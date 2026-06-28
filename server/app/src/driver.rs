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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::MissedTickBehavior;

use ferrumc_core::{GameMode, PlayerId, TextComponent, Tick};
use ferrumc_math::{BlockPos, ChunkPos, Cuboid, Direction, Vec3};
use ferrumc_observability::{
    ChunkPosSnapshot, CounterRegistry, MutationKind, MutationResult, NetTelemetryHub,
    PlayerSnapshot, ServerClock, ServerSnapshotParts, SnapshotPublisher, TickMetrics, Vec3Snapshot,
    DEFAULT_TOP_N,
};
use ferrumc_proto::generated::play::{ChunkDataAndLight, ClientboundPlayPacket};
use ferrumc_session::{
    sign_block_entity_data, NetEvent, PlayerSessionHandle, SessionError, SessionRouter,
};
use ferrumc_sim::{
    BlockStateId, ChunkTicket, GameInput, GameOutput, MutationCause, PendingMutation, RegionOp,
    SimShard, TicketReason,
};
use ferrumc_storage::{
    BlockMutationLogRecord, MutationActor, MutationLogCause, SchemaVersion, WorldStore,
};
use ferrumc_world::{BlockEntity, Chunk, FlatWorldGenerator};

use crate::plugins::BlockEventDispatcher;
use crate::storage_worker::StorageFlushRequest;
use crate::world::chunk_packet;

/// Schema version stamped on every [`BlockMutationLogRecord`] the driver appends.
///
/// Independent of the chunk/overlay record versions (the journal is its own
/// versioned format), per the versioned-record rule.
const MUTATION_LOG_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Upper bound on the number of recent tick timestamps the effective-TPS window
/// retains. At 20 TPS only ~21 fall inside the one-second window, so this is
/// generous headroom that keeps the deque strictly bounded under any tick rate.
const TPS_WINDOW_CAP: usize = 128;

/// Driver-owned state used to build and publish the read-only [`ServerSnapshot`]
/// once per tick.
///
/// It carries the snapshot publisher (a clone of the handle the dashboard reads),
/// the fixed build/start context, a bounded roster of connected players keyed on
/// join/disconnect, and a bounded window of recent tick timestamps for the
/// effective-TPS gauge. None of it touches the forbidden net/session internals:
/// the roster is filled from [`SimCommand::Join`] and pruned against the router's
/// public connection check, and every other field comes from the registry or
/// public read-only shard queries.
struct SnapshotCtx {
    /// The write side of the shared snapshot cell; the dashboard holds clones.
    publisher: SnapshotPublisher,
    /// Build string reported in every snapshot (computed once at startup).
    build: String,
    /// Server start time as a Unix timestamp in seconds.
    started_at_unix: u64,
    /// Monotonic start instant used to derive uptime.
    start_instant: Instant,
    /// Bounded roster of connected players (`PlayerId -> display name`), filled on
    /// join and pruned each tick against the router's public connection state.
    roster: BTreeMap<PlayerId, String>,
    /// Recent tick timestamps within the last wall-second, used to derive the
    /// effective TPS. Bounded by [`TPS_WINDOW_CAP`].
    tps_window: VecDeque<Instant>,
    /// The shared per-connection network-telemetry hub. Connection tasks publish
    /// into it off the hot path; the driver prunes and folds it each tick.
    net_telemetry: Arc<NetTelemetryHub>,
    /// The long-lived block-event dispatcher, read once per tick for the
    /// per-plugin block-edit decision counts.
    block_events: Arc<BlockEventDispatcher>,
}

impl SnapshotCtx {
    /// Builds the publish context, capturing the fixed build/start fields once.
    fn new(
        publisher: SnapshotPublisher,
        net_telemetry: Arc<NetTelemetryHub>,
        block_events: Arc<BlockEventDispatcher>,
    ) -> Self {
        Self {
            publisher,
            build: format!("ferrumc {}", env!("CARGO_PKG_VERSION")),
            started_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or(0),
            start_instant: Instant::now(),
            roster: BTreeMap::new(),
            tps_window: VecDeque::new(),
            net_telemetry,
            block_events,
        }
    }

    /// Records a tick timestamp and returns the effective TPS: the number of ticks
    /// observed within the trailing wall-second.
    fn record_tps(&mut self, now: Instant) -> f64 {
        self.tps_window.push_back(now);
        while self.tps_window.len() > TPS_WINDOW_CAP {
            self.tps_window.pop_front();
        }
        while let Some(&front) = self.tps_window.front() {
            if now.duration_since(front) > Duration::from_secs(1) {
                self.tps_window.pop_front();
            } else {
                break;
            }
        }
        self.tps_window.len() as f64
    }
}

/// Maps a [`GameMode`] onto its lowercase protocol label for the snapshot.
fn gamemode_label(mode: GameMode) -> String {
    match mode {
        GameMode::Survival => "survival",
        GameMode::Creative => "creative",
        GameMode::Adventure => "adventure",
        GameMode::Spectator => "spectator",
    }
    .to_string()
}

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
        /// The pre-encoded main-hand `SetEquipment` body (slot byte + trusted Slot)
        /// for the joiner's initial held item, cached by the router at join so
        /// viewers entering view see it without a follow-up (which would race).
        equipment: Vec<u8>,
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
        /// Each [`StreamedChunk`] carries the column packet plus any block-entity
        /// (sign) render packets for that chunk, so a (re)joining or streaming
        /// player sees existing signs render as the chunk enters view.
        reply: oneshot::Sender<Vec<StreamedChunk>>,
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
    /// Place the held block at `position` after a player `UseItemOn` (the only
    /// caller — plugin/command exact writes use [`SetBlockExact`](Self::SetBlockExact)).
    ///
    /// The connection resolved `state` from the player's selected hotbar slot
    /// (the simulation stays inventory-free), then routed it here. The driver
    /// previews the refined placement on the resident chunk, replies with the
    /// final computed state over `reply` (so the connection can fire its
    /// `after_block_place` hook with the state the world will hold), then forwards
    /// the edit to the block's owning shard as a [`GameInput::BlockPlace`], which
    /// validates it (actor present, chunk resident, in reach) at the tick boundary
    /// and, on acceptance, writes the refined state. Spawn-protection veto and the
    /// empty-hand / non-placeable cases are handled at the connection before this
    /// command is ever sent.
    PlaceBlock {
        /// The placing player.
        player: PlayerId,
        /// The block position to place at (already stepped off the clicked face).
        position: BlockPos,
        /// The block-action sequence to acknowledge on accept/reject.
        sequence: i32,
        /// The held item's default block-state (the placement input the sim
        /// refines into the final rotated/faced/halved state).
        state: BlockStateId,
        /// The face of the targeted block the player clicked.
        clicked_face: Direction,
        /// The cursor hit point inside the targeted block (`0.0..=1.0` per axis).
        cursor_position: Vec3,
        /// The player's yaw in degrees at place time.
        player_yaw: f32,
        /// One-shot reply carrying the final computed block-state (the refinement
        /// the tick will apply), so the connection fires `after_block_place` with
        /// the state the world holds rather than the held default. `None` is never
        /// sent today (an unsupported block previews to the held state), but the
        /// channel is `Option` so a dropped reply still has a safe fallback.
        reply: oneshot::Sender<Option<BlockStateId>>,
    },
    /// Write an exact, authoritative block-state at `position`, applied verbatim
    /// (NOT refined by `compute_placement`).
    ///
    /// Routes a plugin/command exact-state write — a `before_block_*` `Replace`
    /// decision or a `WorldIntent::SetBlock` — to the block's owning shard as a
    /// [`GameInput::SetBlockExact`]. The plugin already chose the final state, so
    /// the simulation stores it byte-for-byte rather than re-deriving
    /// axis/half/facing (which would corrupt a rotated state). Validated like any
    /// edit; on accept the acting `player` gets the ack/resync.
    SetBlockExact {
        /// The player on whose behalf the exact write is applied (receives the
        /// ack/resync).
        player: PlayerId,
        /// The block position to write at.
        position: BlockPos,
        /// The block-action sequence to acknowledge on accept/reject.
        sequence: i32,
        /// The exact block-state to write, applied verbatim.
        state: BlockStateId,
    },
    /// Apply a region (cuboid) block edit on behalf of `player` — the `/fill` and
    /// `/replace` commands.
    ///
    /// Routed to the player's shard as a single [`GameInput::RegionEdit`], so the
    /// whole cuboid is applied at one tick boundary through the shard-owned
    /// block-edit funnel (persist + broadcast, no ack), capturing the prior states
    /// for `/undo`. The region is bounded: the command layer rejects an over-cap
    /// cuboid before this is ever sent, and the shard re-checks defensively.
    RegionEdit {
        /// The player on whose behalf the edit is applied (keys the undo history).
        player: PlayerId,
        /// The cuboid of blocks the edit addresses.
        region: Cuboid,
        /// How every block in the cuboid changes.
        op: RegionOp,
    },
    /// Undo `player`'s most recent region edit — the `/undo` command.
    ///
    /// Routed to the player's shard as a [`GameInput::RegionUndo`], which restores
    /// the prior block-states the last edit captured. A no-op if the player has no
    /// recorded edits.
    RegionUndo {
        /// The player whose most recent region edit is undone.
        player: PlayerId,
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
    /// Update `player`'s broadcast equipment (held item) after a hotbar change.
    ///
    /// The inventory is connection-local, so the connection encodes the new
    /// main-hand `SetEquipment` body and routes it here; the driver-owned
    /// [`SessionRouter`] caches it and broadcasts it (droppable) to the viewers that
    /// currently have `player` spawned. A gone player is a no-op.
    SetEquipment {
        /// The player whose held item changed.
        player: PlayerId,
        /// The pre-encoded main-hand `SetEquipment` body (slot byte + trusted Slot).
        equipment: Vec<u8>,
    },
    /// Apply a player's edit to a sign's text after a serverbound `UpdateSign`.
    ///
    /// Routed to the block's owning shard as a [`GameInput::UpdateSign`]. The
    /// simulation validates it (actor present, chunk resident, in reach, a
    /// non-waxed sign present) at the tick boundary and, on acceptance, stores the
    /// new lines and emits a `SignUpdated` the router broadcasts as
    /// `BlockEntityData`. Net never writes the world directly — this is the only
    /// path a sign edit reaches the simulation.
    UpdateSign {
        /// The editing player.
        player: PlayerId,
        /// Absolute position of the sign being edited.
        position: BlockPos,
        /// `true` to edit the front face, `false` the back.
        is_front: bool,
        /// The four new text lines, top to bottom.
        lines: [String; ferrumc_world::SIGN_LINES],
    },
}

/// One chunk built for a [`SimCommand::StreamChunks`] request: the column
/// data+light packet plus any block-entity render packets for that chunk.
///
/// The block-entity packets (sign `BlockEntityData`) are sent right after the
/// column so the client renders existing signs as the chunk enters view — the
/// chunk-enter half of the sign loop, covering both a (re)joining player's spawn
/// columns and a moving player streaming new columns.
pub(crate) struct StreamedChunk {
    /// The chunk column data + light packet.
    pub(crate) chunk: ChunkDataAndLight,
    /// Block-entity render packets for this chunk, in deterministic position
    /// order; empty when the chunk holds no block entities.
    pub(crate) block_entities: Vec<ClientboundPlayPacket>,
}

/// Builds the block-entity render packets for `chunk`: one `BlockEntityData` per
/// sign block-entity, in ascending [`BlockPos`] order.
fn chunk_block_entity_packets(chunk: &Chunk) -> Vec<ClientboundPlayPacket> {
    chunk
        .block_entities()
        .filter_map(|(pos, entity)| {
            let BlockEntity::Sign(sign) = entity else {
                return None;
            };
            Some(sign_block_entity_data(pos, sign))
        })
        .collect()
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
    snapshots: SnapshotPublisher,
    net_telemetry: Arc<NetTelemetryHub>,
    block_events: Arc<BlockEventDispatcher>,
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

    // Read-only snapshot publishing state (roster, TPS window, build/start
    // context). Updated only from this task, never holding a lock across a tick.
    let mut snap_ctx = SnapshotCtx::new(snapshots, net_telemetry, block_events);

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
                run_tick(
                    &mut router,
                    &mut shard,
                    &mut shard_rx,
                    &metrics,
                    &clock,
                    &mut tick,
                    &mut snap_ctx,
                );
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
                        &mut snap_ctx.roster,
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
#[allow(clippy::too_many_lines)] // one dispatch over every SimCommand variant
async fn handle_command(
    router: &mut SessionRouter,
    shard: &mut SimShard,
    store: &dyn WorldStore,
    generator: &FlatWorldGenerator,
    storage_tx: &mpsc::Sender<StorageFlushRequest>,
    tick: Tick,
    next_mutation_id: &mut u64,
    player_roster: &mut BTreeMap<PlayerId, String>,
    command: SimCommand,
) {
    match command {
        SimCommand::Join {
            player,
            name,
            position,
            equipment,
            reply,
        } => {
            let result = router.join_player_with_equipment(player, &name, position, equipment);
            // Record the joiner in the driver-owned roster for the dashboard
            // snapshot only once the router accepted the join; a rejected join must
            // not leave a phantom roster entry. The roster is pruned each tick
            // against the router's public connection check, so it stays bounded.
            if result.is_ok() {
                player_roster.insert(player, name);
            }
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
            clicked_face,
            cursor_position,
            player_yaw,
            reply,
        } => {
            // Preview the refined placement against the resident chunk and hand it
            // back to the connection BEFORE routing, so its after_block_place hook
            // fires with the final computed state (not the held default). The
            // preview shares run_tick's refinement helper and chunks mutate only at
            // tick boundaries, so it matches what the tick applies for this single
            // edit. The reply is sent here, off the tick — the connection does not
            // wait ~50 ms for the next tick.
            let computed =
                shard.preview_placement(state, clicked_face, cursor_position, player_yaw, position);
            let _ = reply.send(Some(computed));
            // Route the UNCHANGED place to the block's owning shard (the same
            // routing as any other block edit); the tick recomputes via the shared
            // helper to an identical result, with the fence-neighbour pass intact. A
            // gone player has no shard to route to.
            if let Err(err) = router.route_game_input(
                player,
                GameInput::BlockPlace {
                    player,
                    position,
                    sequence,
                    state,
                    clicked_face,
                    cursor_position,
                    player_yaw,
                },
            ) {
                tracing::trace!(%err, "dropping block place");
            }
        }
        SimCommand::SetBlockExact {
            player,
            position,
            sequence,
            state,
        } => {
            // Route the exact write to the block's owning shard as a verbatim
            // SetBlockExact (no compute_placement). A gone player has no shard.
            if let Err(err) = router.route_game_input(
                player,
                GameInput::SetBlockExact {
                    player,
                    position,
                    sequence,
                    state,
                },
            ) {
                tracing::trace!(%err, "dropping exact block set");
            }
        }
        SimCommand::RegionEdit { player, region, op } => {
            // Route the whole cuboid as one input so it applies at a single tick
            // boundary (and never blows the bounded shard inbox). A gone player has
            // no shard to route to.
            if let Err(err) =
                router.route_game_input(player, GameInput::RegionEdit { player, region, op })
            {
                tracing::trace!(%err, "dropping region edit");
            }
        }
        SimCommand::RegionUndo { player } => {
            // Route the undo to the player's shard, which restores the prior states
            // its last region edit captured. A gone player has no shard to route to.
            if let Err(err) = router.route_game_input(player, GameInput::RegionUndo { player }) {
                tracing::trace!(%err, "dropping region undo");
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
        SimCommand::SetEquipment { player, equipment } => {
            // Best-effort cosmetic broadcast to in-view viewers; a gone player is a
            // no-op inside the router.
            router.set_equipment(player, equipment);
        }
        SimCommand::UpdateSign {
            player,
            position,
            is_front,
            lines,
        } => {
            // Route the sign edit to the block's owning shard. A gone player has no
            // shard to route to; a full inbox drops it (the client's next edit
            // retries), exactly like any other block-edit input under load.
            if let Err(err) = router.route_game_input(
                player,
                GameInput::UpdateSign {
                    player,
                    position,
                    is_front,
                    lines,
                },
            ) {
                tracing::trace!(%err, "dropping sign update");
            }
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
) -> Vec<StreamedChunk> {
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
        // built packets are owned, so the immutable borrow ends before the arms.
        match shard.loaded_chunks().get(pos).map(|chunk| {
            chunk_packet(pos, chunk).map(|chunk_packet| StreamedChunk {
                chunk: chunk_packet,
                block_entities: chunk_block_entity_packets(chunk),
            })
        }) {
            Some(Ok(streamed)) => packets.push(streamed),
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
    snap: &mut SnapshotCtx,
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

    // Publish the read-only snapshot for the dashboard. This runs at the very end
    // of the tick and only swaps an `Arc` pointer, so it never holds a lock across
    // the simulation work above.
    publish_snapshot(router, shard, metrics, *tick, snap);
}

/// Builds and publishes the read-only [`ServerSnapshot`] for this tick.
///
/// Folds the metric-derived fields from `metrics` with app-side context: the
/// effective TPS (from a bounded timestamp window), uptime, the per-player list
/// assembled from the driver-owned roster plus public read-only shard/router
/// queries, the per-player network counters and server-wide packet-trace
/// summaries aggregated from the network-telemetry hub, and the per-plugin
/// block-edit decision counts read from the block-event dispatcher.
///
/// The roster is pruned against the router's public connection check and the
/// telemetry hub is pruned against the surviving roster, so neither grows without
/// bound. All folds are cheap and bounded (a handful of sessions, a top-N
/// summary, one row per plugin), and none touches the simulation hot path.
fn publish_snapshot(
    router: &SessionRouter,
    shard: &SimShard,
    metrics: &CounterRegistry,
    tick: Tick,
    snap: &mut SnapshotCtx,
) {
    let now = Instant::now();
    let tps = snap.record_tps(now);

    // Drop any roster entries the router no longer considers connected (covers
    // every disconnect path without reaching into session internals).
    snap.roster
        .retain(|player, _| router.is_player_connected(*player));

    // Prune the telemetry hub against the surviving roster so disconnected
    // sessions never linger, then aggregate the rest into the per-player counters
    // and the server-wide top-N packet-trace summaries.
    let connected_names: BTreeSet<String> = snap.roster.values().cloned().collect();
    snap.net_telemetry.retain_sessions(&connected_names);
    let net = snap.net_telemetry.aggregate(DEFAULT_TOP_N);

    let players: Vec<PlayerSnapshot> = snap
        .roster
        .iter()
        .map(|(player, name)| {
            // Prefer the authoritative sim position; fall back to the router's
            // join-seeded position for the tick before the join is applied.
            let position = shard
                .player_position(*player)
                .or_else(|| router.player_position(*player))
                .unwrap_or(Vec3::ZERO);
            let mode = shard.player_game_mode(*player).unwrap_or_default();
            // Chunk column = floor(block / 16); floor the float to a block first.
            let chunk_x = (position.x.floor() as i32) >> 4;
            let chunk_z = (position.z.floor() as i32) >> 4;
            // Fold this player's network counters in, keyed by the session label
            // (the player name); absent until the player's first flush publishes.
            let counters = net.by_session.get(name).copied().unwrap_or_default();
            PlayerSnapshot {
                player_id: player.as_uuid().as_u128(),
                name: name.clone(),
                position: Vec3Snapshot {
                    x: position.x,
                    y: position.y,
                    z: position.z,
                },
                chunk: ChunkPosSnapshot {
                    x: chunk_x,
                    z: chunk_z,
                },
                gamemode: gamemode_label(mode),
                outbound_queue_len: counters.outbound_queue_len,
                network_in_bytes: counters.network_in_bytes,
                network_out_bytes: counters.network_out_bytes,
                frames_decoded: counters.frames_decoded,
                frames_encoded: counters.frames_encoded,
                packets_dropped_total: counters.packets_dropped_total,
            }
        })
        .collect();

    let parts = ServerSnapshotParts {
        build: snap.build.clone(),
        started_at: snap.started_at_unix,
        uptime_secs: now.duration_since(snap.start_instant).as_secs(),
        tick: tick.get(),
        tps,
        players_online: players.len(),
        players,
        chunks_loaded: shard.loaded_chunks().loaded_count(),
        // The chunk map exposes only a persist-dirty flag today, not exact dirty
        // counts; surface the flag as a 0/1 approximation and leave dirty at 0.
        chunks_dirty: 0,
        chunks_persist_dirty: usize::from(shard.loaded_chunks().has_persist_dirty()),
        // Per-plugin block-edit decision counts, read from the shared dispatcher.
        plugin_decisions: snap.block_events.decision_snapshots(),
        network_per_player: net.per_player,
        inbound_trace_summary: net.inbound,
        outbound_trace_summary: net.outbound,
    };

    snap.publisher.publish(metrics.server_snapshot(parts));
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ferrumc_core::{PlayerId, Tick};
    use ferrumc_math::{BlockPos, Direction, ShardPos, Vec3};
    use ferrumc_session::SessionRouter;
    use ferrumc_sim::{BlockStateId, SimShard};
    use ferrumc_storage::InMemoryStore;
    use ferrumc_world::FlatWorldGenerator;
    use tokio::sync::{mpsc, oneshot};

    use super::{handle_command, SimCommand};

    #[tokio::test]
    async fn place_block_command_replies_with_the_refined_state() {
        // The driver previews the placement and replies with the FINAL computed
        // state (an east-face oak_log refines to axis=x, state 136) BEFORE routing
        // the tick edit, so the connection fires after_block_place with the state
        // the world will hold rather than the held default (137). An empty shard is
        // enough: the axis-from-face rule consults the clicked face, not neighbours.
        let mut router = SessionRouter::new();
        let mut shard = SimShard::new(ShardPos::new(0, 0));
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        let (storage_tx, _storage_rx) = mpsc::channel(1);
        let mut next_mutation_id = 0u64;
        let mut player_roster = BTreeMap::new();
        let (reply_tx, reply_rx) = oneshot::channel();

        handle_command(
            &mut router,
            &mut shard,
            &store,
            &generator,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &mut player_roster,
            SimCommand::PlaceBlock {
                player: PlayerId::offline("placer"),
                position: BlockPos::new(8, 65, 8),
                sequence: 1,
                state: BlockStateId::new(137), // oak_log default (axis=y)
                clicked_face: Direction::East,
                cursor_position: Vec3::new(0.5, 0.5, 0.5),
                player_yaw: 0.0,
                reply: reply_tx,
            },
        )
        .await;

        assert_eq!(
            reply_rx
                .await
                .expect("driver replied with the computed state"),
            Some(BlockStateId::new(136)),
        );
    }
}
