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

use ferrumc_core::{GameMode, PlayerId, TextComponent, Tick};
use ferrumc_math::{BlockPos, ChunkPos, Vec3};
use ferrumc_observability::{
    CounterRegistry, MutationKind, MutationResult, ServerClock, TickMetrics,
};
use ferrumc_proto::generated::play::ChunkDataAndLight;
use ferrumc_session::{NetEvent, PlayerSessionHandle, SessionError, SessionRouter};
use ferrumc_sim::{
    BlockStateId, ChunkTicket, GameInput, GameOutput, MutationCause, PendingMutation, SimShard,
    TicketReason,
};
use ferrumc_storage::{
    BlockMutationLogRecord, MutationActor, MutationLogCause, SchemaVersion, WorldStore,
};
use ferrumc_world::FlatWorldGenerator;

use crate::storage_worker::StorageFlushRequest;
use crate::world::chunk_packet;

/// Schema version stamped on every [`BlockMutationLogRecord`] the driver appends.
///
/// Independent of the chunk/overlay record versions (the journal is its own
/// versioned format), per the versioned-record rule.
const MUTATION_LOG_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// A request from a connection task to the simulation/session driver.
///
/// The enum is the only way a connection influences simulation state; it carries
/// no sockets and no shard handles.
pub(crate) enum SimCommand {
    /// Place a player at `position` and hand back their session handle.
    Join {
        /// The joining player's identity.
        player: PlayerId,
        /// The joining player's display name (shown on viewers' tab list and the
        /// nameplate above the spawned entity).
        name: String,
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
    /// Broadcast a System Chat Message to every connected player.
    ///
    /// The connection task cannot reach other players' outbound channels (only the
    /// driver-owned [`SessionRouter`] holds them), so relayed player chat routes
    /// through here. `overlay = true` renders the message on the action bar.
    BroadcastSystemChat {
        /// The message to render, as a structured text component.
        content: TextComponent,
        /// Whether to render above the hotbar (action bar) rather than the chat box.
        overlay: bool,
    },
    /// Mutate the authoritative server-side game mode of a player after a
    /// `/gamemode` command.
    ///
    /// Routed to the player's shard as a [`GameInput::SetGameMode`] so the
    /// simulation owns the mode that later enforcement reads. The clientbound
    /// `GameEvent` that switches the client visually is sent separately by the
    /// connection task; this only updates authoritative state.
    SetGameMode {
        /// The player whose authoritative mode changes.
        player: PlayerId,
        /// The new game mode.
        mode: GameMode,
    },
    /// Place the held block at `position` after a `UseItemOn`.
    ///
    /// The connection resolved `state` from the player's selected hotbar slot
    /// (the simulation stays inventory-free), then routed it here. The driver
    /// forwards it to the block's owning shard as a [`GameInput::BlockPlace`],
    /// which validates the edit (actor present, chunk resident, in reach) at the
    /// tick boundary and, on acceptance, writes `state`. Spawn-protection veto and
    /// the empty-hand / non-placeable cases are handled at the connection before
    /// this command is ever sent.
    PlaceBlock {
        /// The placing player.
        player: PlayerId,
        /// The block position to place at (already stepped off the clicked face).
        position: BlockPos,
        /// The block-action sequence to acknowledge on accept/reject.
        sequence: i32,
        /// The block-state the held item places.
        state: BlockStateId,
    },
    /// Resync + acknowledge a block edit refused at the connection (a plugin
    /// `Deny` / spawn-protection veto) without mutating the world.
    ///
    /// The connection (net layer) has no world access, so it cannot read the
    /// authoritative block state needed to heal the actor's optimistic prediction.
    /// It routes the refusal here as a [`GameInput::RejectBlockEdit`] to the
    /// block's owning shard, which reads the authoritative state and emits the same
    /// ack + mandatory resync a sim-side rejection (out of reach, unloaded chunk)
    /// already produces — one funnel for every rejection site, so a Deny no longer
    /// leaves a ghost block. The block-edit metric is counted once, on the
    /// resulting [`GameOutput::BlockChangeRejected`] in [`run_tick`].
    RejectBlockEdit {
        /// The player whose predicted edit must be healed.
        player: PlayerId,
        /// The block position of the refused edit.
        position: BlockPos,
        /// The block-action sequence to acknowledge so the prediction ends.
        sequence: i32,
        /// The state the client predicted (air for a break, the held block for a
        /// place); used only to classify the metric.
        requested_state: BlockStateId,
    },
    /// Teleport `player` to `position`.
    ///
    /// Fulfils a plugin's `Teleport` intent: the connection task cannot reach
    /// another player's outbound channel, so it routes the teleport here and the
    /// driver-owned [`SessionRouter`] snaps the target's client (mandatory
    /// `SynchronizePlayerPosition`) and routes an authoritative move so viewers and
    /// simulation state follow.
    TeleportPlayer {
        /// The player to move.
        player: PlayerId,
        /// The destination position.
        position: Vec3,
    },
    /// Send a System Chat Message to a single `player`.
    ///
    /// Fulfils a plugin's `Message` intent aimed at a player other than the acting
    /// connection: the connection cannot reach another player's outbound channel,
    /// so the targeted delivery routes through the driver-owned [`SessionRouter`].
    /// `overlay = true` renders the message on the action bar.
    SendSystemChat {
        /// The recipient.
        player: PlayerId,
        /// The message to render, as a structured text component.
        content: TextComponent,
        /// Whether to render above the hotbar (action bar) rather than the chat box.
        overlay: bool,
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
    store: Arc<dyn WorldStore>,
    generator: FlatWorldGenerator,
    mut shard_rx: mpsc::Receiver<GameInput>,
    mut commands: mpsc::Receiver<SimCommand>,
    tick_period: Duration,
    metrics: Arc<CounterRegistry>,
    clock: ServerClock,
    storage_tx: mpsc::Sender<StorageFlushRequest>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(tick_period);
    // Lag must not trigger catch-up ticks: skip missed deadlines instead.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // The driver owns the authoritative tick counter: it advances once per
    // `run_tick` and publishes the value through `clock` so connection tasks can
    // stamp their packet traces with the current tick.
    let mut tick = Tick::ZERO;

    // Monotonic id stamped on each journal entry so the append-only mutation log
    // stays ordered across the server's lifetime.
    let mut next_mutation_id: u64 = 0;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => {
                // Final flush before stopping: capture any remaining persist-dirty
                // chunks and journal entries and send them, awaiting briefly if the
                // channel is momentarily full so a graceful shutdown loses nothing.
                if let Some(request) = build_flush_request(&mut shard, tick, &mut next_mutation_id) {
                    if let Err(err) = storage_tx.send(request).await {
                        tracing::warn!(%err, "failed to send final storage flush on shutdown");
                    }
                }
                // Dropping `storage_tx` (on return) closes the channel, which the
                // storage worker observes to drain and exit.
                break;
            }
            _ = ticker.tick() => {
                run_tick(&mut router, &mut shard, &mut shard_rx, &metrics, &clock, &mut tick);
                // End-of-tick flush: hand the tick's player edits to the storage
                // worker without ever blocking the tick (see the helper).
                try_flush_persist_dirty(&mut shard, &storage_tx, tick, &mut next_mutation_id);
            }
            maybe_command = commands.recv() => match maybe_command {
                Some(command) => {
                    handle_command(
                        &mut router,
                        &mut shard,
                        &*store,
                        &generator,
                        &storage_tx,
                        tick,
                        &mut next_mutation_id,
                        command,
                    )
                    .await;
                }
                None => break,
            },
        }
    }
}

/// Builds a [`StorageFlushRequest`] from the shard's pending persist-dirty chunks
/// and journaled mutations, stamping the overlays' capture tick and assigning a
/// monotonic id to each journal entry from `next_mutation_id`.
///
/// Returns `None` (taking nothing, clearing nothing) when there is nothing to
/// flush. Otherwise it drains the shard's persist-dirty masks and mutation buffer,
/// so the caller is committed to delivering the returned request.
fn build_flush_request(
    shard: &mut SimShard,
    tick: Tick,
    next_mutation_id: &mut u64,
) -> Option<StorageFlushRequest> {
    if !shard.loaded_chunks().has_persist_dirty() && !shard.has_pending_mutations() {
        return None;
    }
    let tick_n = tick.get();
    let overlays = shard.loaded_chunks_mut().take_persist_dirty(tick_n);
    let mutations = shard
        .take_mutations()
        .into_iter()
        .map(|mutation| {
            let id = *next_mutation_id;
            *next_mutation_id = next_mutation_id.saturating_add(1);
            build_mutation_record(id, tick_n, &mutation)
        })
        .collect();
    Some(StorageFlushRequest {
        overlays,
        mutations,
        // Per-tick/shutdown flushes are fire-and-forget; only the disconnect
        // barrier (`release_chunks_acked`) attaches an ack.
        ack: None,
    })
}

/// Maps a sim-layer [`PendingMutation`] (plus an assigned `id` and `tick`) into a
/// storage [`BlockMutationLogRecord`].
fn build_mutation_record(id: u64, tick: u64, mutation: &PendingMutation) -> BlockMutationLogRecord {
    let (actor, cause) = match mutation.cause() {
        MutationCause::PlayerCreative { player } => (
            MutationActor::Player(player),
            MutationLogCause::PlayerCreative,
        ),
        MutationCause::Plugin => (MutationActor::System, MutationLogCause::Plugin),
        MutationCause::Test => (MutationActor::System, MutationLogCause::Test),
        // `MutationCause::Command` and any future (non-exhaustive) source are
        // attributed to the system with a command-like cause rather than dropped.
        _ => (MutationActor::System, MutationLogCause::Command),
    };
    BlockMutationLogRecord::new(
        MUTATION_LOG_SCHEMA_VERSION,
        id,
        tick,
        actor,
        mutation.position(),
        mutation.old_state(),
        mutation.new_state(),
        cause,
    )
}

/// End-of-tick flush, run on the tick hot path and therefore strictly
/// non-blocking.
///
/// If there is anything to persist, it reserves a slot on the bounded storage
/// channel up front; only on success does it drain the shard's persist-dirty
/// chunks and send them. If the channel is full it leaves the chunks marked
/// persist-dirty and retries next tick, so backpressure can never block the tick
/// or lose an edit.
fn try_flush_persist_dirty(
    shard: &mut SimShard,
    storage_tx: &mpsc::Sender<StorageFlushRequest>,
    tick: Tick,
    next_mutation_id: &mut u64,
) {
    if !shard.loaded_chunks().has_persist_dirty() && !shard.has_pending_mutations() {
        return;
    }
    match storage_tx.try_reserve() {
        Ok(permit) => {
            if let Some(request) = build_flush_request(shard, tick, next_mutation_id) {
                permit.send(request);
            }
        }
        Err(_) => {
            tracing::trace!("storage flush channel full; deferring dirty chunks to next tick");
        }
    }
}

/// Applies one command against the router and/or the shard's chunk map.
#[allow(clippy::too_many_arguments)] // threads the storage flush channel + tick context through
async fn handle_command(
    router: &mut SessionRouter,
    shard: &mut SimShard,
    store: &dyn WorldStore,
    generator: &FlatWorldGenerator,
    storage_tx: &mpsc::Sender<StorageFlushRequest>,
    tick: Tick,
    next_mutation_id: &mut u64,
    command: SimCommand,
) {
    match command {
        SimCommand::Join {
            player,
            name,
            position,
            reply,
        } => {
            let result = router.join_player(player, &name, position);
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
            release_chunks(shard, storage_tx, tick, next_mutation_id, &unload).await;
            let packets = load_chunks(shard, store, generator, &load).await;
            // A gone connection just discards the packets; nothing to clean up.
            let _ = reply.send(packets);
        }
        SimCommand::ReleaseChunks { positions } => {
            // Disconnect path: await the worker's commit before releasing tickets so
            // a fast rejoin cannot read a stale baseline (Bug A barrier).
            release_chunks_acked(shard, storage_tx, tick, next_mutation_id, &positions).await;
        }
        SimCommand::BroadcastSystemChat { content, overlay } => {
            router.broadcast_system_chat(&content, overlay);
        }
        SimCommand::SetGameMode { player, mode } => {
            // Route the authoritative mode change to the player's shard. A gone
            // player (already disconnected) simply has no shard to route to.
            if let Err(err) =
                router.route_game_input(player, GameInput::SetGameMode { player, mode })
            {
                tracing::trace!(%err, "dropping set-game-mode");
            }
        }
        SimCommand::PlaceBlock {
            player,
            position,
            sequence,
            state,
        } => {
            // Route the place to the block's owning shard (the same routing as any
            // other block edit). A gone player has no shard to route to.
            if let Err(err) = router.route_game_input(
                player,
                GameInput::BlockPlace {
                    player,
                    position,
                    sequence,
                    state,
                },
            ) {
                tracing::trace!(%err, "dropping block place");
            }
        }
        SimCommand::RejectBlockEdit {
            player,
            position,
            sequence,
            requested_state,
        } => {
            // Route the refusal to the block's owning shard, which reads the
            // authoritative state and emits a BlockChangeRejected (the actor's
            // mandatory resync + ack). A gone player has no shard to route to.
            // Best-effort: if the shard inbox is saturated the rejection is dropped
            // (the ghost then persists until the client's next interaction), exactly
            // as the original BlockBreak/BlockPlace inputs behave under the same load.
            if let Err(err) = router.route_game_input(
                player,
                GameInput::RejectBlockEdit {
                    player,
                    position,
                    sequence,
                    requested_state,
                },
            ) {
                tracing::trace!(%err, "dropping block-edit rejection");
            }
        }
        SimCommand::TeleportPlayer { player, position } => {
            // Snap the target's client and route an authoritative move. A gone
            // player simply has no session to teleport.
            if let Err(err) = router.teleport_player(player, position) {
                tracing::trace!(%err, "dropping teleport");
            }
        }
        SimCommand::SendSystemChat {
            player,
            content,
            overlay,
        } => {
            router.send_system_chat_to(player, &content, overlay);
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
    store: &dyn WorldStore,
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

/// Releases the player ticket on each chunk in `positions`.
///
/// Before dropping any tickets, it flushes the shard's pending player edits to the
/// storage worker, so a chunk that unloads with unsaved edits has them captured as
/// an overlay first. A generated-but-unmodified chunk has no persist-dirty
/// sections, so it produces nothing and unloads for free. Persistence itself
/// happens off-tick on the storage worker; this only enqueues it.
async fn release_chunks(
    shard: &mut SimShard,
    storage_tx: &mpsc::Sender<StorageFlushRequest>,
    tick: Tick,
    next_mutation_id: &mut u64,
    positions: &[ChunkPos],
) {
    // Capture any unsaved edits (on these chunks or any other resident chunk)
    // before unloading. `await` here is fine: this runs between ticks on a
    // command, not on the tick hot path, so blocking briefly is acceptable
    // backpressure and keeps the flush lossless.
    if let Some(request) = build_flush_request(shard, tick, next_mutation_id) {
        if let Err(err) = storage_tx.send(request).await {
            tracing::warn!(%err, "failed to flush edits before releasing chunks");
        }
    }

    let ticket = ChunkTicket::of(TicketReason::Player);
    for &pos in positions {
        let _ = shard.loaded_chunks_mut().release(pos, ticket);
    }
}

/// Releases the player ticket on each chunk in `positions`, but **only after** the
/// storage worker confirms every buffered edit is committed (the Bug A barrier).
///
/// Used exclusively on the disconnect path ([`SimCommand::ReleaseChunks`]). Unlike
/// the per-movement [`release_chunks`], this always sends a flush request carrying
/// a single-shot ack — even when [`build_flush_request`] returns `None`. That is
/// deliberate: a prior per-tick [`try_flush_persist_dirty`] has usually already
/// drained the placed-block overlay into the worker's *uncommitted* buffer, so
/// there may be nothing fresh to capture here yet the write is still not durable.
/// The worker force-commits its entire pending buffer before acking, and only then
/// are the tickets dropped — so the next player's `acquire`/`load_or_generate`
/// reads the freshly persisted baseline instead of the stale one.
///
/// This `await`s a redb commit, which is allowed because it runs off the tick (from
/// [`handle_command`], never `run_tick`); the per-movement unload deliberately does
/// **not** route through here, so a chunk-boundary crossing never forces a commit.
async fn release_chunks_acked(
    shard: &mut SimShard,
    storage_tx: &mpsc::Sender<StorageFlushRequest>,
    tick: Tick,
    next_mutation_id: &mut u64,
    positions: &[ChunkPos],
) {
    let (ack_tx, ack_rx) = oneshot::channel();
    // Always send an acked request, even with nothing fresh to flush: the overlay
    // may already be buffered uncommitted in the worker.
    let mut request =
        build_flush_request(shard, tick, next_mutation_id).unwrap_or(StorageFlushRequest {
            overlays: Vec::new(),
            mutations: Vec::new(),
            ack: None,
        });
    request.ack = Some(ack_tx);

    if storage_tx.send(request).await.is_ok() {
        // Block this command (not the tick) until the commit lands.
        let _ = ack_rx.await;
    } else {
        tracing::warn!("failed to send acked flush before releasing chunks; releasing best-effort");
    }

    let ticket = ChunkTicket::of(TicketReason::Player);
    for &pos in positions {
        let _ = shard.loaded_chunks_mut().release(pos, ticket);
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
    // closed — or that overflowed a *mandatory* packet (the slow-client policy) —
    // are returned so we can schedule a clean despawn for each. Each
    // `disconnect_player` drains its own leave-broadcast cascade iteratively, so
    // this loop terminates and never recurses.
    let mut closed = Vec::new();
    for output in &outputs {
        // Classify the block-edit metric from the requested/new state: air means a
        // break, any other state a place. An accepted edit surfaces as BlockChanged;
        // every rejection surfaces as BlockChangeRejected and is counted here — both
        // sim-side refusals (out of reach, unloaded chunk) and edits refused upstream
        // (plugin Deny / spawn-protection veto), which now route through the sim's
        // RejectBlockEdit path rather than being counted at the connection.
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
