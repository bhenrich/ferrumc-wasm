//! The single-owner simulation/session driver task.
//!
//! One task owns the [`SessionRouter`] and the single [`SimShard`], so neither
//! is shared behind a lock. Connection tasks reach it only through a bounded
//! [`SimCommand`] channel:
//!
//! ```text
//!   connection --SimCommand::Join-->  driver  -- router.join_player --> shard inbox
//!   connection --SimCommand::Event--> driver  -- router.route_event --> shard inbox
//!                                        |
//!                                   (every tick) drain inbox -> run_tick ->
//!                                   route_output -> player outbound channels
//! ```
//!
//! The driver advances the shard on a fixed interval with **no catch-up**
//! ([`MissedTickBehavior::Skip`]), matching the project's overload rule, and
//! never blocks: channel sends are non-blocking inside the router, and a full
//! inbox defers inputs rather than stalling.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::MissedTickBehavior;

use ferrumc_core::{PlayerId, Tick};
use ferrumc_math::{ChunkPos, Vec3};
use ferrumc_observability::{
    CounterRegistry, MutationKind, MutationResult, ServerClock, TickMetrics,
};
use ferrumc_proto::generated::play::ChunkDataAndLight;
use ferrumc_session::{NetEvent, PlayerSessionHandle, SessionError, SessionRouter};
use ferrumc_sim::{ChunkTicket, GameInput, GameOutput, SimShard, TicketReason};
use ferrumc_storage::{InMemoryStore, WorldStore, MAX_SAVE_BATCH};
use ferrumc_world::FlatWorldGenerator;

use crate::world::chunk_packet;

/// A request from a connection task to the simulation/session driver.
///
/// The enum is the only way a connection influences simulation state; it carries
/// no sockets and no shard handles.
pub(crate) enum SimCommand {
    /// Place a player at `position` and hand back their session handle.
    Join {
        /// The joining player's identity.
        player: PlayerId,
        /// The world-space position to join at.
        position: Vec3,
        /// One-shot channel the driver replies on with the new session handle (or
        /// a classified routing error).
        reply: oneshot::Sender<Result<PlayerSessionHandle, SessionError>>,
    },
    /// Route a translated network event (a play packet or a disconnect) to the
    /// player's shard.
    Event(NetEvent),
    /// Stream chunks around a moving player: release the player tickets on every
    /// chunk in `unload`, then load-or-generate every chunk in `load` (acquiring a
    /// player ticket on each) and reply with the freshly built chunk packets.
    ///
    /// The world mutation (acquire/release through the shard's ticketed chunk map)
    /// happens here on the driver, never on the connection task — the connection
    /// only decides *which* chunks based on the position it observed, and renders
    /// the returned packets to its socket.
    StreamChunks {
        /// Chunk columns to bring into view (load-or-generate, ticket acquired).
        load: Vec<ChunkPos>,
        /// Chunk columns that left view (player ticket released).
        unload: Vec<ChunkPos>,
        /// One-shot channel the driver replies on with the built chunk packets for
        /// the subset of `load` that resolved (a store/encode failure skips just
        /// that chunk, so the connection only records what it actually received).
        reply: oneshot::Sender<Vec<ChunkDataAndLight>>,
    },
    /// Release the player tickets on every chunk in `positions` without sending
    /// anything back. Used when a connection ends so the player's streamed chunks
    /// stop being held resident.
    ReleaseChunks {
        /// Chunk columns whose player ticket should be dropped.
        positions: Vec<ChunkPos>,
    },
}

/// Runs the driver loop until `shutdown` flips or every command sender drops.
///
/// Owns `router`, `shard`, the shard input receiver `shard_rx` (drained each
/// tick), the chunk `store` and `generator` (used to stream chunks in around
/// moving players), and the `commands` channel. The loop prioritises shutdown,
/// then a due tick, then a pending command, so a command flood can never starve
/// the simulation.
///
/// # Blocking
///
/// A [`SimCommand::StreamChunks`]/[`SimCommand::ReleaseChunks`] is handled inline
/// with `await`s into the chunk map (load-or-generate, persist-on-unload). With
/// the current in-memory store these resolve without yielding to real I/O, so the
/// next tick is not meaningfully delayed; a future disk-backed store would move
/// that work off the driver. The per-update load count is capped by the
/// connection (see `connection.rs`), bounding the work one command can request.
#[allow(clippy::too_many_arguments)] // the driver owns several distinct pieces wired once at startup
pub(crate) async fn run(
    mut router: SessionRouter,
    mut shard: SimShard,
    store: InMemoryStore,
    generator: FlatWorldGenerator,
    mut shard_rx: mpsc::Receiver<GameInput>,
    mut commands: mpsc::Receiver<SimCommand>,
    tick_period: Duration,
    metrics: Arc<CounterRegistry>,
    clock: ServerClock,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(tick_period);
    // Lag must not trigger catch-up ticks: skip missed deadlines instead.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // The driver owns the authoritative tick counter: it advances once per
    // `run_tick` and publishes the value through `clock` so connection tasks can
    // stamp their packet traces with the current tick.
    let mut tick = Tick::ZERO;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            _ = ticker.tick() => {
                run_tick(&mut router, &mut shard, &mut shard_rx, &metrics, &clock, &mut tick);
            }
            maybe_command = commands.recv() => match maybe_command {
                Some(command) => {
                    handle_command(&mut router, &mut shard, &store, &generator, &metrics, command).await;
                }
                None => break,
            },
        }
    }
}

/// Applies one command against the router and/or the shard's chunk map.
async fn handle_command(
    router: &mut SessionRouter,
    shard: &mut SimShard,
    store: &InMemoryStore,
    generator: &FlatWorldGenerator,
    metrics: &CounterRegistry,
    command: SimCommand,
) {
    match command {
        SimCommand::Join {
            player,
            position,
            reply,
        } => {
            let result = router.join_player(player, position);
            // The connection task may have already gone away; a failed reply send
            // means the join handle is simply discarded.
            let _ = reply.send(result);
        }
        SimCommand::Event(event) => {
            if let Err(err) = router.route_event(&event) {
                tracing::trace!(%err, "dropping network event");
            }
        }
        SimCommand::StreamChunks {
            load,
            unload,
            reply,
        } => {
            // Release first (frees tickets the new view no longer needs) before
            // acquiring, so a chunk that both left and re-entered nets out cleanly.
            release_chunks(shard, store, metrics, &unload).await;
            let packets = load_chunks(shard, store, generator, &load).await;
            // A gone connection just discards the packets; nothing to clean up.
            let _ = reply.send(packets);
        }
        SimCommand::ReleaseChunks { positions } => {
            release_chunks(shard, store, metrics, &positions).await;
        }
    }
}

/// Load-or-generates each chunk in `positions`, acquiring a player ticket on it,
/// and returns the built [`ChunkDataAndLight`] packet for each that resolved.
///
/// A chunk whose store read fails, or whose packet cannot be encoded, is skipped
/// (logged) so one bad chunk never stalls the rest; its ticket is dropped again
/// so a skipped chunk is not left pinned with no client tracking it.
async fn load_chunks(
    shard: &mut SimShard,
    store: &InMemoryStore,
    generator: &FlatWorldGenerator,
    positions: &[ChunkPos],
) -> Vec<ChunkDataAndLight> {
    let ticket = ChunkTicket::of(TicketReason::Player);
    let mut packets = Vec::with_capacity(positions.len());
    for &pos in positions {
        if let Err(err) = shard
            .loaded_chunks_mut()
            .acquire(store, generator, pos, ticket)
            .await
        {
            tracing::warn!(%err, x = pos.x(), z = pos.z(), "failed to acquire streamed chunk");
            continue;
        }
        // `acquire` just made the chunk resident, so the lookup cannot miss; the
        // built packet is owned, so the immutable borrow ends before the arms.
        match shard
            .loaded_chunks()
            .get(pos)
            .map(|chunk| chunk_packet(pos, chunk))
        {
            Some(Ok(packet)) => packets.push(packet),
            other => {
                if let Some(Err(err)) = other {
                    tracing::warn!(%err, x = pos.x(), z = pos.z(), "failed to encode streamed chunk");
                }
                // Drop the ticket we just took: the connection never learns about
                // this chunk, so it would otherwise stay pinned forever.
                let _ = shard.loaded_chunks_mut().release(pos, ticket);
            }
        }
    }
    packets
}

/// Releases the player ticket on each chunk in `positions`, persisting the save
/// record of any chunk that unloads with unsaved edits so a streamed-out chunk is
/// not lost.
async fn release_chunks(
    shard: &mut SimShard,
    store: &InMemoryStore,
    metrics: &CounterRegistry,
    positions: &[ChunkPos],
) {
    let ticket = ChunkTicket::of(TicketReason::Player);
    let mut dirty = Vec::new();
    for &pos in positions {
        if let Some(record) = shard.loaded_chunks_mut().release(pos, ticket).into_dirty() {
            dirty.push(record);
        }
    }

    // Persist in store-bounded batches so a large unload (e.g. a disconnect that
    // drops a whole view square of freshly generated chunks) never exceeds the
    // store's per-call batch limit.
    while !dirty.is_empty() {
        let take = dirty.len().min(MAX_SAVE_BATCH);
        let batch: Vec<_> = dirty.drain(..take).collect();
        // Time each persist call for ferrumc_storage_flush_ms.
        let start = Instant::now();
        let result = store.save_chunks(batch).await;
        metrics.record_storage_flush_ms(start.elapsed().as_millis() as u64);
        if let Err(err) = result {
            tracing::warn!(%err, "failed to persist streamed-out chunks");
        }
    }
}

/// Drains queued inputs into the shard, advances one tick, and routes outputs.
///
/// Also records the per-tick observability metrics: it times the tick
/// (`ferrumc_tick_ms{shard}`), counts accepted and sim-rejected block edits
/// (`ferrumc_block_mutation_total{kind,result}`), advances and publishes the
/// authoritative tick through `clock`, and emits a structured tick event.
fn run_tick(
    router: &mut SessionRouter,
    shard: &mut SimShard,
    shard_rx: &mut mpsc::Receiver<GameInput>,
    metrics: &CounterRegistry,
    clock: &ServerClock,
    tick: &mut Tick,
) {
    let start = Instant::now();

    // Move everything the router queued since the last tick into the inbox; a
    // full inbox stops the drain (reject backpressure) and retries next tick.
    let mut inputs_drained = 0usize;
    while let Ok(input) = shard_rx.try_recv() {
        if let Err(err) = shard.enqueue(input) {
            tracing::warn!(%err, "shard inbox full; deferring inputs to next tick");
            break;
        }
        inputs_drained += 1;
    }
    let inbox_len = shard.inbox_len();

    let outputs = shard.run_tick();
    // Routing an output may fan out to many viewers; any whose connection has
    // closed are returned so we can schedule a clean despawn for each.
    let mut closed = Vec::new();
    for output in &outputs {
        // Classify the block-edit metric from the requested/new state: air means a
        // break, any other state a place. An accepted edit surfaces as BlockChanged;
        // a sim-side rejection (out of reach, etc.) now surfaces as
        // BlockChangeRejected and is counted here too (spawn-protect vetoes are
        // still counted at the connection veto site, before the sim ever sees them).
        match output {
            GameOutput::BlockChanged { state, .. } => {
                let kind = if state.is_air() {
                    MutationKind::Break
                } else {
                    MutationKind::Place
                };
                metrics.record_block_mutation(kind, MutationResult::Accepted);
            }
            GameOutput::BlockChangeRejected {
                requested_state, ..
            } => {
                let kind = if requested_state.is_air() {
                    MutationKind::Break
                } else {
                    MutationKind::Place
                };
                metrics.record_block_mutation(kind, MutationResult::Rejected);
            }
            _ => {}
        }
        closed.extend(router.route_output(output));
    }
    for player in closed {
        let _ = router.disconnect_player(player);
    }

    // Advance and publish the authoritative tick (saturating: it never wraps
    // silently), then record the tick metrics for this shard.
    *tick = tick.saturating_add(1);
    clock.set(*tick);
    let shard_pos = shard.shard_pos();
    let tick_metrics = TickMetrics {
        shard_x: shard_pos.x(),
        shard_z: shard_pos.z(),
        tick: *tick,
        duration_us: start.elapsed().as_micros() as u64,
        inputs_drained,
        outputs_emitted: outputs.len(),
        players: shard.player_count(),
        inbox_len,
    };
    metrics.record_tick(&tick_metrics);
    tracing::debug!(
        target: "ferrumc::observability::tick",
        shard_x = tick_metrics.shard_x,
        shard_z = tick_metrics.shard_z,
        tick = tick_metrics.tick.get(),
        duration_us = tick_metrics.duration_us,
        inputs_drained = tick_metrics.inputs_drained,
        outputs_emitted = tick_metrics.outputs_emitted,
        players = tick_metrics.players,
        inbox_len = tick_metrics.inbox_len,
        "tick"
    );
}
