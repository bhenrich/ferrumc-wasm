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
//! simulation inbox leaves excess inputs in the bounded router channel for the
//! next tick rather than consuming and dropping them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::MissedTickBehavior;

use ferrumc_core::{GameMode, PlayerId, Result as ServerResult, ServerError, TextComponent, Tick};
use ferrumc_items::ItemStack;
use ferrumc_math::{BlockPos, ChunkPos, Cuboid, Direction, Vec3};
use ferrumc_observability::{
    ChunkPosSnapshot, CounterRegistry, MutationKind, MutationResult, NetTelemetryHub,
    PlayerSnapshot, ServerClock, ServerSnapshotParts, SnapshotPublisher, TickMetrics, Vec3Snapshot,
    DEFAULT_TOP_N,
};
use ferrumc_proto::generated::play::{
    ChunkDataAndLight, ClientboundPlayPacket, GameEvent, UpdateTime,
};
use ferrumc_session::{
    sign_block_entity_data, DeliveryPolicy, InputDeliveryError, NetEvent, PlayerSessionHandle,
    SessionError, SessionRouter,
};
use ferrumc_sim::{
    BlockStateId, ChunkTicket, GameInput, GameOutput, MutationCause, PendingMutation, RegionOp,
    SimShard, TicketReason, WorldTime,
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

/// `GameEvent` reason for `change_game_mode` (clientbound Game Event, protocol
/// 772): the recipient's own game mode switches to the carried mode id. Sent to a
/// `/gamemode <mode> <player>` target — the issuer's own switch is sent by the
/// connection.
const GAME_EVENT_CHANGE_GAMEMODE: u8 = 3;
/// `GameEvent` reason for `start_raining` — begins the client-visible rain a
/// `/weather rain` broadcasts.
const GAME_EVENT_START_RAINING: u8 = 1;
/// `GameEvent` reason for `stop_raining` — clears the weather a `/weather clear`
/// broadcasts.
const GAME_EVENT_STOP_RAINING: u8 = 2;
/// The `GameEvent` `value` field carried by the weather toggles, which use no
/// value (unlike `change_game_mode`, whose value is the mode id).
const GAME_EVENT_WEATHER_VALUE: f32 = 0.0;

/// How often the driver broadcasts the world time to every player: once per second
/// (every 20 ticks at the 20 TPS target). Clients interpolate the sun/moon between
/// updates, so a one-second cadence animates the sky smoothly without sending a
/// packet every tick.
const TIME_BROADCAST_INTERVAL_TICKS: i64 = 20;

/// The `time_of_day_increasing` flag (the 1.21.2+ `tickDayTime` bool) sent on every
/// Update Time. Always `true`: the daylight cycle runs by default (the
/// `doDaylightCycle` equivalent), so the client keeps advancing the sky locally
/// between the server's periodic updates. A configurable gamerule toggle is
/// deferred.
const DAYLIGHT_CYCLE_INCREASING: bool = true;

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
    /// per-plugin event-decision counts (block edits, chat, and interactions).
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
                .map_or(0, |since| since.as_secs()),
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

/// Resolves an online player by display `name` against the driver-owned roster,
/// returning their [`PlayerId`], or `None` if no connected player matches.
///
/// Names are matched exactly (case-sensitive, as Minecraft names are). Used to
/// resolve the `<player>` argument of `/tp <player>` and `/gamemode <mode>
/// <player>` — the connection has no roster, so name resolution happens here on
/// the driver, which owns it.
fn resolve_online_player(roster: &BTreeMap<PlayerId, String>, name: &str) -> Option<PlayerId> {
    roster
        .iter()
        .find(|(_, display)| display.as_str() == name)
        .map(|(id, _)| *id)
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

/// Builds the clientbound Update Time packet from the driver-owned [`WorldTime`].
///
/// Carries the current monotonic world age and day-night phase plus the always-on
/// [`DAYLIGHT_CYCLE_INCREASING`] flag; used for the per-join send and the periodic
/// and `/time` broadcasts.
fn update_time_packet(world_time: &WorldTime) -> ClientboundPlayPacket {
    ClientboundPlayPacket::UpdateTime(UpdateTime::new(
        world_time.world_age(),
        world_time.time_of_day(),
        DAYLIGHT_CYCLE_INCREASING,
    ))
}

/// A one-shot acknowledgement of bounded shard-input admission.
type DeliveryReply = oneshot::Sender<Result<(), SessionError>>;

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
        /// The pre-encoded `SetEquipment` body (the joiner's full equipment set —
        /// main hand, off hand, and armor — as continuation-terminated slot+Slot
        /// entries), cached by the router at join so viewers entering view see it
        /// without a follow-up (which would race).
        equipment: Vec<u8>,
        /// One-shot channel the driver replies on with the new session handle (or
        /// a classified routing error).
        reply: oneshot::Sender<Result<PlayerSessionHandle, SessionError>>,
    },
    /// Route a translated network event (a play packet or a disconnect) to the
    /// player's shard.
    Event {
        /// The validated network event to route.
        event: NetEvent,
        /// Optional acceptance reply used when a caller must not publish an
        /// after-event or client preview before bounded shard admission.
        acceptance: Option<DeliveryReply>,
    },
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
        /// Optional bounded-shard acceptance reply.
        acceptance: Option<DeliveryReply>,
    },
    /// Place the held block at `position` after a player `UseItemOn` (the only
    /// caller — plugin/command exact writes use [`SetBlockExact`](Self::SetBlockExact)).
    ///
    /// The connection resolved `state` from the player's selected hotbar slot
    /// (the simulation stays inventory-free), then routed it here. The driver
    /// first admits the edit to the block's owning shard as a
    /// [`GameInput::BlockPlace`]. Only after bounded admission does it preview the
    /// refined placement on the resident chunk and reply with the final computed
    /// state (so the connection can fire its `after_block_place` hook with the
    /// state the world will hold). The simulation validates the admitted input
    /// (actor present, chunk resident, in reach) at the tick boundary and, on
    /// acceptance, writes the refined state. Spawn-protection veto and the
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
        /// One-shot reply carrying the final computed block-state only after
        /// bounded shard acceptance, or the classified rejection that terminated
        /// the overloaded session.
        reply: oneshot::Sender<Result<BlockStateId, SessionError>>,
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
        /// Optional bounded-shard acceptance reply.
        acceptance: Option<DeliveryReply>,
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
        /// Optional bounded-shard acceptance reply.
        acceptance: Option<DeliveryReply>,
    },
    /// Undo `player`'s most recent region edit — the `/undo` command.
    ///
    /// Routed to the player's shard as a [`GameInput::RegionUndo`], which restores
    /// the prior block-states the last edit captured. A no-op if the player has no
    /// recorded edits.
    RegionUndo {
        /// The player whose most recent region edit is undone.
        player: PlayerId,
        /// Optional bounded-shard acceptance reply.
        acceptance: Option<DeliveryReply>,
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
        /// Optional bounded-shard acceptance reply.
        acceptance: Option<DeliveryReply>,
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
        /// Optional bounded-shard acceptance reply.
        acceptance: Option<DeliveryReply>,
    },
    /// Teleport `player` to the current position of the online player named
    /// `target` — the `/tp <player>` command.
    ///
    /// The connection (net layer) has no roster or world access, so it cannot
    /// resolve a player name or read a position. It routes the name here and the
    /// driver resolves `target` against the live roster, reads that player's
    /// authoritative position (sim, then the router's join-seeded fallback), and
    /// reuses [`SimCommand::TeleportPlayer`]'s path to snap `player`. An unknown or
    /// position-less target is a logged no-op.
    TeleportToPlayer {
        /// The player to move (the command issuer).
        player: PlayerId,
        /// The display name of the online player to teleport to.
        target: String,
        /// Optional bounded-shard acceptance reply.
        acceptance: Option<DeliveryReply>,
    },
    /// Set the authoritative game mode of the online player named `target` and
    /// switch their client — the `/gamemode <mode> <player>` command.
    ///
    /// The targeted counterpart to [`SimCommand::SetGameMode`] (which the
    /// connection uses for the issuer's own mode). The connection cannot resolve a
    /// name or reach another player's channel, so the driver resolves `target`
    /// against the live roster, routes a [`GameInput::SetGameMode`] to that
    /// player's shard (the authoritative state), and sends them a clientbound
    /// `GameEvent` (`change_game_mode`) so their client switches. An unknown target
    /// is a logged no-op.
    SetGameModeFor {
        /// The display name of the online player whose mode changes.
        target: String,
        /// The new game mode.
        mode: GameMode,
        /// Optional bounded-shard acceptance reply.
        acceptance: Option<DeliveryReply>,
    },
    /// Broadcast a weather change to every connected player — the `/weather`
    /// command.
    ///
    /// Sent server-wide as a clientbound `GameEvent` (`start_raining` /
    /// `stop_raining`); only the driver-owned [`SessionRouter`] can reach every
    /// player's channel. No rain is simulated in this slice — this toggles the
    /// client-visible weather state only.
    SetWeather {
        /// `true` begins rain (`start_raining`); `false` clears it (`stop_raining`).
        raining: bool,
    },
    /// Set the absolute world time-of-day — the `/time set <phase|ticks>` command.
    ///
    /// The driver applies it to the authoritative [`WorldTime`] it owns (wrapping
    /// into a single day) and immediately broadcasts the new time to every player
    /// as an Update Time, so every sky jumps at once. Only the driver-owned
    /// [`SessionRouter`] can reach every player's channel.
    SetTime {
        /// The absolute day-night phase in ticks (wrapped to `0..24000`).
        time_of_day: i64,
    },
    /// Add ticks to the world time-of-day — the `/time add <ticks>` command.
    ///
    /// The relative counterpart to [`SimCommand::SetTime`]: the driver applies the
    /// (signed, wrapping) delta to its [`WorldTime`] and broadcasts the adjusted
    /// time to every player.
    AddTime {
        /// The signed tick delta to add (wrapping within a day).
        ticks: i64,
    },
    /// Report the current day-night phase to `player` — the `/time query daytime`
    /// command.
    ///
    /// The command layer runs on the connection, which has no world state, so it
    /// cannot read the live clock. The query routes here and the driver — the owner
    /// of the authoritative [`WorldTime`] — replies with a System Chat Message
    /// naming the current `time_of_day`.
    QueryTime {
        /// The player who asked, and who receives the answer.
        player: PlayerId,
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
    /// Update `player`'s broadcast equipment (the full visible set) after a
    /// held-item or worn-equipment change.
    ///
    /// The inventory is connection-local, so the connection encodes the new
    /// `SetEquipment` body (main hand, off hand, and armor) and routes it here; the
    /// driver-owned [`SessionRouter`] caches it and broadcasts it (droppable) to the
    /// viewers that currently have `player` spawned. A gone player is a no-op.
    SetEquipment {
        /// The player whose equipment changed.
        player: PlayerId,
        /// The pre-encoded `SetEquipment` body (the full equipment set as
        /// continuation-terminated slot+Slot entries).
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
        /// Optional bounded-shard acceptance reply.
        acceptance: Option<DeliveryReply>,
    },
    /// Probe whether the block at `position` is an openable chest and, if so,
    /// return a snapshot of its container for the player to open.
    ///
    /// Net never reads the world directly: the connection routes a right-click on a
    /// block here, and the driver asks the block's owning shard
    /// ([`SimShard::container_open`]) — which validates reach/residency, confirms
    /// the block is a chest, lazily creates a missing container, and replies with a
    /// 27-slot snapshot (or `None` for a non-chest / out-of-reach / absent-player
    /// case, in which the connection falls through to placement). The snapshot is
    /// the authoritative copy the connection mirrors into the opened window.
    OpenContainer {
        /// The player opening the container.
        player: PlayerId,
        /// Absolute position of the clicked block.
        position: BlockPos,
        /// One-shot reply: the chest's slot snapshot, or `None` if not openable.
        reply: oneshot::Sender<Option<Vec<ItemStack>>>,
    },
    /// Apply a left-click on chest slot `slot` of the chest at `position` with the
    /// carried `cursor`, returning the post-click cursor and the chest's updated
    /// snapshot.
    ///
    /// The mutation is applied atomically against the world's authoritative chest
    /// by [`SimShard::container_left_click`] using the item-count-conserving
    /// exchange, so a click can never dupe or lose an item even with concurrent
    /// viewers. The connection sends its current cursor and adopts the returned
    /// cursor + snapshot; a `None` reply (chest gone / out of reach / bad slot)
    /// leaves the connection's cursor untouched and triggers a resync.
    ContainerLeftClick {
        /// The clicking player.
        player: PlayerId,
        /// Absolute position of the open chest.
        position: BlockPos,
        /// The chest slot index clicked (`0..27`).
        slot: usize,
        /// The player's carried item at click time (server-authoritative).
        cursor: ItemStack,
        /// One-shot reply: the `(new_cursor, snapshot)` after the click, or `None`
        /// if the click could not be applied (the caller keeps its cursor).
        reply: oneshot::Sender<Option<(ItemStack, Vec<ItemStack>)>>,
    },
}

impl SimCommand {
    /// Returns whether this command crosses the bounded shard-input boundary and
    /// can acknowledge that admission to its caller.
    pub(crate) fn supports_delivery_acceptance(&self) -> bool {
        matches!(
            self,
            Self::Event { .. }
                | Self::SetGameMode { .. }
                | Self::SetBlockExact { .. }
                | Self::RegionEdit { .. }
                | Self::RegionUndo { .. }
                | Self::RejectBlockEdit { .. }
                | Self::TeleportPlayer { .. }
                | Self::TeleportToPlayer { .. }
                | Self::SetGameModeFor { .. }
                | Self::UpdateSign { .. }
        )
    }

    /// Attaches a one-shot bounded-shard acceptance reply.
    ///
    /// Returns the untouched sender when this command has no shard-input
    /// admission step. Callers use
    /// [`supports_delivery_acceptance`](Self::supports_delivery_acceptance)
    /// before requesting one.
    pub(crate) fn request_delivery_acceptance(
        &mut self,
        reply: DeliveryReply,
    ) -> Result<(), DeliveryReply> {
        let (Self::Event { acceptance, .. }
        | Self::SetGameMode { acceptance, .. }
        | Self::SetBlockExact { acceptance, .. }
        | Self::RegionEdit { acceptance, .. }
        | Self::RegionUndo { acceptance, .. }
        | Self::RejectBlockEdit { acceptance, .. }
        | Self::TeleportPlayer { acceptance, .. }
        | Self::TeleportToPlayer { acceptance, .. }
        | Self::SetGameModeFor { acceptance, .. }
        | Self::UpdateSign { acceptance, .. }) = self
        else {
            return Err(reply);
        };
        *acceptance = Some(reply);
        Ok(())
    }
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
/// with `await`s into the chunk map. Loads use the current in-memory path; ticket
/// releases await the off-tick storage worker's durability result because a last
/// ticket cannot safely disappear before its overlay commits. The per-update load
/// count is capped by the connection (see `connection.rs`), bounding the work one
/// command can request.
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
    fatal_shutdown: watch::Sender<bool>,
    mut shutdown: watch::Receiver<bool>,
) -> ServerResult<()> {
    let mut ticker = tokio::time::interval(tick_period);
    // Lag must not trigger catch-up ticks: skip missed deadlines instead.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // The driver owns the authoritative tick counter: it advances once per
    // `run_tick` and publishes the value through `clock` so connection tasks can
    // stamp their packet traces with the current tick.
    let mut tick = Tick::ZERO;

    // The driver also owns the deterministic day-night clock: it advances once per
    // `run_tick` (alongside `tick`) and is mutated by `/time`. The world starts at
    // age 0 / phase 0; the first periodic broadcast (and every join) seeds clients.
    let mut world_time = WorldTime::new();

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
                // Reserve capacity before taking dirty state. If the worker has
                // failed and closed the channel, the shard keeps its last
                // recoverable copy and shutdown reports failure.
                if has_pending_persistence(&shard) {
                    if let Ok(permit) = storage_tx.reserve().await {
                        if let Some(request) =
                            build_flush_request(&mut shard, tick, &mut next_mutation_id)
                        {
                            permit.send(request);
                        }
                    } else {
                        let error = storage_worker_closed("final shutdown flush");
                        let _ = fatal_shutdown.send(true);
                        return Err(error);
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
                    &mut world_time,
                    &mut snap_ctx,
                )?;
                // End-of-tick flush: hand the tick's player edits to the storage
                // worker without ever blocking the tick (see the helper).
                if let Err(error) =
                    try_flush_persist_dirty(&mut shard, &storage_tx, tick, &mut next_mutation_id)
                {
                    let _ = fatal_shutdown.send(true);
                    return Err(error);
                }
            }
            maybe_command = commands.recv() => match maybe_command {
                Some(command) => {
                    if let Err(error) = handle_command(
                        &mut router,
                        &mut shard,
                        &*store,
                        &generator,
                        &storage_tx,
                        tick,
                        &mut next_mutation_id,
                        &mut world_time,
                        &mut snap_ctx.roster,
                        command,
                    )
                    .await
                    {
                        let _ = fatal_shutdown.send(true);
                        return Err(error);
                    }
                }
                None => break,
            },
        }
    }

    Ok(())
}

/// Returns whether taking a flush request would remove recoverable shard state.
fn has_pending_persistence(shard: &SimShard) -> bool {
    shard.loaded_chunks().has_persist_dirty() || shard.has_pending_mutations()
}

/// Classifies an unexpectedly unavailable storage worker at a durability edge.
fn storage_worker_closed(operation: &str) -> ServerError {
    ServerError::invalid_state(format!(
        "cannot complete {operation}: storage flush worker channel is closed"
    ))
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
    Some(StorageFlushRequest::new(overlays, mutations))
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
) -> ServerResult<()> {
    if !has_pending_persistence(shard) {
        return Ok(());
    }
    match storage_tx.try_reserve() {
        Ok(permit) => {
            if let Some(request) = build_flush_request(shard, tick, next_mutation_id) {
                permit.send(request);
            }
            Ok(())
        }
        Err(mpsc::error::TrySendError::Full(())) => {
            tracing::trace!("storage flush channel full; deferring dirty chunks to next tick");
            Ok(())
        }
        Err(mpsc::error::TrySendError::Closed(())) => {
            Err(storage_worker_closed("end-of-tick flush"))
        }
    }
}

/// Turns one router delivery into either acceptance, a classified caller
/// rejection after explicit session teardown, or a fatal driver error.
fn enforce_input_delivery(
    router: &mut SessionRouter,
    player: PlayerId,
    delivery: Result<(), InputDeliveryError>,
    allow_retained_leave: bool,
    operation: &str,
) -> ServerResult<Result<(), SessionError>> {
    let Err(rejection) = delivery else {
        return Ok(Ok(()));
    };
    let policy = rejection.policy().policy();
    let is_leave = matches!(rejection.input(), GameInput::PlayerLeave { .. });
    let error = rejection.into_error();

    if matches!(&error, SessionError::UnknownPlayer { .. }) {
        return Ok(Err(error));
    }
    if !matches!(&error, SessionError::ShardInboxFull { .. }) {
        return Err(ServerError::invalid_state(format!(
            "{operation} could not reach its shard: {error}"
        )));
    }

    // A connection teardown already has no live source to stop. SessionRouter
    // retained its leave and mapping in the player-bounded pending set; the tick
    // loop retries it after freeing shard capacity.
    if is_leave && allow_retained_leave {
        return Ok(Err(error));
    }
    if is_leave {
        return Err(ServerError::capacity(format!(
            "{operation} was accepted by simulation but its overloaded session could not enqueue PlayerLeave: {error}"
        )));
    }

    match policy {
        DeliveryPolicy::BestEffort => Ok(Ok(())),
        DeliveryPolicy::Authoritative | DeliveryPolicy::Coalescible => {
            match router.disconnect_player_owned(player) {
                Ok(_) => Ok(Err(error)),
                Err(disconnect_error) => Err(ServerError::capacity(format!(
                    "{operation} was rejected by bounded shard capacity and the overloaded session could not enqueue PlayerLeave: {disconnect_error}"
                ))),
            }
        }
    }
}

/// Sends an optional caller acknowledgement for one classified delivery result.
fn reply_delivery(acceptance: Option<DeliveryReply>, outcome: Result<(), SessionError>) {
    if let Some(reply) = acceptance {
        // A gone connection no longer needs the classification; the router has
        // already accepted the input or enforced its overload outcome.
        let _ = reply.send(outcome);
    }
}

/// Routes an ordinary simulation input and enforces its typed overload policy.
fn route_input_command(
    router: &mut SessionRouter,
    player: PlayerId,
    input: GameInput,
    acceptance: Option<DeliveryReply>,
    operation: &str,
) -> ServerResult<()> {
    let delivery = router.route_game_input_owned(player, input);
    let outcome = enforce_input_delivery(router, player, delivery, false, operation)?;
    reply_delivery(acceptance, outcome);
    Ok(())
}

/// Routes a server-driven teleport and withholds its client sync until bounded
/// shard admission succeeds.
fn route_teleport_command(
    router: &mut SessionRouter,
    player: PlayerId,
    position: Vec3,
    acceptance: Option<DeliveryReply>,
    operation: &str,
) -> ServerResult<()> {
    let delivery = router.teleport_player_owned(player, position);
    let outcome = enforce_input_delivery(router, player, delivery, false, operation)?;
    reply_delivery(acceptance, outcome);
    Ok(())
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
    world_time: &mut WorldTime,
    player_roster: &mut BTreeMap<PlayerId, String>,
    command: SimCommand,
) -> ServerResult<()> {
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
                // Seed the joiner's sky with the current world time so the
                // day-night cycle starts correct immediately, rather than waiting
                // up to a second for the next periodic broadcast. Queued on the
                // player's outbound channel, it is drained after the join kit, so
                // it lands once the client is in the play state.
                router.send_play_packet_to(player, update_time_packet(world_time));
            }
            // The connection task may have already gone away; a failed reply send
            // means the join handle is simply discarded.
            let _ = reply.send(result);
        }
        SimCommand::Event { event, acceptance } => {
            let player = event.player();
            let allow_retained_leave = matches!(&event, NetEvent::Disconnected { .. });
            let delivery = router.route_event_owned(&event);
            let outcome = enforce_input_delivery(
                router,
                player,
                delivery,
                allow_retained_leave,
                "network event",
            )?;
            reply_delivery(acceptance, outcome);
        }
        SimCommand::StreamChunks {
            load,
            unload,
            reply,
        } => {
            // Release first (frees tickets the new view no longer needs) before
            // acquiring, so a chunk that both left and re-entered nets out cleanly.
            release_chunks(shard, storage_tx, tick, next_mutation_id, &unload).await?;
            let packets = load_chunks(shard, store, generator, &load).await;
            // A gone connection just discards the packets; nothing to clean up.
            let _ = reply.send(packets);
        }
        SimCommand::ReleaseChunks { positions } => {
            // Disconnect path: await the worker's commit before releasing tickets so
            // a fast rejoin cannot read a stale baseline (Bug A barrier).
            release_chunks_acked(shard, storage_tx, tick, next_mutation_id, &positions).await?;
        }
        SimCommand::BroadcastSystemChat { content, overlay } => {
            router.broadcast_system_chat(&content, overlay);
        }
        SimCommand::SetGameMode {
            player,
            mode,
            acceptance,
        } => {
            route_input_command(
                router,
                player,
                GameInput::SetGameMode { player, mode },
                acceptance,
                "set game mode",
            )?;
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
            let delivery = router.route_game_input_owned(
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
            );
            let outcome =
                enforce_input_delivery(router, player, delivery, false, "block placement")?;
            match outcome {
                Ok(()) => {
                    // Preview only after admission. The tick recomputes through the
                    // same helper, so the after-hook receives the final refined
                    // state without previewing a rejected mutation.
                    let computed = shard.preview_placement(
                        state,
                        clicked_face,
                        cursor_position,
                        player_yaw,
                        position,
                    );
                    let _ = reply.send(Ok(computed));
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
        }
        SimCommand::SetBlockExact {
            player,
            position,
            sequence,
            state,
            acceptance,
        } => {
            route_input_command(
                router,
                player,
                GameInput::SetBlockExact {
                    player,
                    position,
                    sequence,
                    state,
                },
                acceptance,
                "exact block set",
            )?;
        }
        SimCommand::RegionEdit {
            player,
            region,
            op,
            acceptance,
        } => {
            route_input_command(
                router,
                player,
                GameInput::RegionEdit { player, region, op },
                acceptance,
                "region edit",
            )?;
        }
        SimCommand::RegionUndo { player, acceptance } => {
            route_input_command(
                router,
                player,
                GameInput::RegionUndo { player },
                acceptance,
                "region undo",
            )?;
        }
        SimCommand::RejectBlockEdit {
            player,
            position,
            sequence,
            requested_state,
            acceptance,
        } => {
            route_input_command(
                router,
                player,
                GameInput::RejectBlockEdit {
                    player,
                    position,
                    sequence,
                    requested_state,
                },
                acceptance,
                "block-edit rejection",
            )?;
        }
        SimCommand::TeleportPlayer {
            player,
            position,
            acceptance,
        } => {
            route_teleport_command(router, player, position, acceptance, "teleport")?;
        }
        SimCommand::TeleportToPlayer {
            player,
            target,
            acceptance,
        } => {
            // Resolve the destination player by name against the live roster, read
            // their authoritative position (sim first, then the router's
            // join-seeded fallback for the tick before the join applies), and reuse
            // the teleport path to snap the issuer. An offline or position-less
            // target is a logged no-op; the acceptance reply lets command feedback
            // follow only after that outcome is known.
            let Some(target_id) = resolve_online_player(player_roster, &target) else {
                tracing::trace!(%target, "tp target is not online");
                reply_delivery(acceptance, Ok(()));
                return Ok(());
            };
            let Some(position) = shard
                .player_position(target_id)
                .or_else(|| router.player_position(target_id))
            else {
                tracing::trace!(%target, "tp target has no known position");
                reply_delivery(acceptance, Ok(()));
                return Ok(());
            };
            route_teleport_command(router, player, position, acceptance, "teleport to player")?;
        }
        SimCommand::SetGameModeFor {
            target,
            mode,
            acceptance,
        } => {
            // Resolve the target by name, route the authoritative mode change to
            // their shard, then switch their client with a GameEvent. An offline
            // target is a logged no-op.
            let Some(target_id) = resolve_online_player(player_roster, &target) else {
                tracing::trace!(%target, "gamemode target is not online");
                reply_delivery(acceptance, Ok(()));
                return Ok(());
            };
            let delivery = router.route_game_input_owned(
                target_id,
                GameInput::SetGameMode {
                    player: target_id,
                    mode,
                },
            );
            let outcome = enforce_input_delivery(
                router,
                target_id,
                delivery,
                false,
                "targeted set game mode",
            )?;
            if outcome.is_ok() {
                router.send_play_packet_to(
                    target_id,
                    ClientboundPlayPacket::GameEvent(GameEvent::new(
                        GAME_EVENT_CHANGE_GAMEMODE,
                        f32::from(mode.as_id()),
                    )),
                );
            }
            reply_delivery(acceptance, outcome);
        }
        SimCommand::SetWeather { raining } => {
            // Server-wide, client-visible weather toggle (no rain simulation in this
            // slice): broadcast the matching GameEvent to every connected player.
            let reason = if raining {
                GAME_EVENT_START_RAINING
            } else {
                GAME_EVENT_STOP_RAINING
            };
            router.broadcast_play_packet(&ClientboundPlayPacket::GameEvent(GameEvent::new(
                reason,
                GAME_EVENT_WEATHER_VALUE,
            )));
        }
        SimCommand::SetTime { time_of_day } => {
            // Apply the absolute phase to the authoritative clock (wrapped into a
            // single day) and broadcast it so every client's sky jumps at once.
            world_time.set_time_of_day(time_of_day);
            router.broadcast_play_packet(&update_time_packet(world_time));
        }
        SimCommand::AddTime { ticks } => {
            // Relative counterpart of SetTime: adjust the clock and broadcast.
            world_time.add_time(ticks);
            router.broadcast_play_packet(&update_time_packet(world_time));
        }
        SimCommand::QueryTime { player } => {
            // Only the driver holds the live clock; answer the asker directly with
            // the current day-night phase. A gone player is a no-op in the router.
            let message = TextComponent::text(format!("The time is {}", world_time.time_of_day()));
            router.send_system_chat_to(player, &message, false);
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
            acceptance,
        } => {
            route_input_command(
                router,
                player,
                GameInput::UpdateSign {
                    player,
                    position,
                    is_front,
                    lines,
                },
                acceptance,
                "sign update",
            )?;
        }
        SimCommand::OpenContainer {
            player,
            position,
            reply,
        } => {
            // Read (and lazily create) the chest container directly off the resident
            // shard, like `preview_placement`: a request/response read that does not
            // wait for the next tick. A gone connection just discards the reply.
            let snapshot = shard.container_open(player, position);
            let _ = reply.send(snapshot);
        }
        SimCommand::ContainerLeftClick {
            player,
            position,
            slot,
            cursor,
            reply,
        } => {
            // Apply the conserving slot/cursor exchange atomically against the
            // authoritative chest while we hold the shard, so concurrent viewers are
            // serialised and a click can never dupe or lose an item. The connection
            // adopts the returned cursor + snapshot (or keeps its cursor on `None`).
            let outcome = shard.container_left_click(player, position, slot, cursor);
            let _ = reply.send(outcome);
        }
    }

    Ok(())
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
/// Before dropping any tickets, it runs the same receipt-backed durability barrier
/// as disconnect. A movement unload can also remove the last player ticket, so
/// merely enqueueing an overlay would allow a fast reacquire to read stale storage
/// if that accepted write then failed.
async fn release_chunks(
    shard: &mut SimShard,
    storage_tx: &mpsc::Sender<StorageFlushRequest>,
    tick: Tick,
    next_mutation_id: &mut u64,
    positions: &[ChunkPos],
) -> ServerResult<()> {
    release_chunks_acked(shard, storage_tx, tick, next_mutation_id, positions).await
}

/// Releases the player ticket on each chunk in `positions`, but **only after** the
/// storage worker confirms every buffered edit is committed (the Bug A barrier).
///
/// Used by both disconnect ([`SimCommand::ReleaseChunks`]) and movement-driven
/// unloads. It always sends a flush request carrying a single-shot ack — even when
/// [`build_flush_request`] returns `None`. That is deliberate: a prior per-tick
/// [`try_flush_persist_dirty`] may already have drained the placed-block overlay
/// into the worker's *uncommitted* buffer, so there may be nothing fresh to capture
/// here yet the write is still not durable. The worker force-commits its entire
/// pending buffer before acking, and only then are tickets dropped — so a later
/// `acquire`/`load_or_generate` reads the persisted baseline instead of stale data.
///
/// This `await`s a redb commit, which is allowed because it runs from
/// [`handle_command`], never inside [`run_tick`].
async fn release_chunks_acked(
    shard: &mut SimShard,
    storage_tx: &mpsc::Sender<StorageFlushRequest>,
    tick: Tick,
    next_mutation_id: &mut u64,
    positions: &[ChunkPos],
) -> ServerResult<()> {
    // This admission must happen before `build_flush_request`: a closed worker
    // cannot consume the batch, so the shard must keep both its dirty state and
    // final ticket for recovery.
    let permit = storage_tx
        .reserve()
        .await
        .map_err(|_| storage_worker_closed("chunk-release durability barrier"))?;
    let (ack_tx, ack_rx) = oneshot::channel();
    // Always send an acked request, even with nothing fresh to flush: the overlay
    // may already be buffered uncommitted in the worker.
    let mut request = build_flush_request(shard, tick, next_mutation_id)
        .unwrap_or_else(|| StorageFlushRequest::new(Vec::new(), Vec::new()));
    request.ack = Some(ack_tx);
    permit.send(request);

    // A canceled acknowledgement means the worker terminated without proving
    // durability. Propagate that failure and retain every requested ticket.
    ack_rx.await.map_err(|_| {
        ServerError::invalid_state("chunk-release durability barrier acknowledgement was canceled")
    })??;

    let ticket = ChunkTicket::of(TicketReason::Player);
    for &pos in positions {
        let _ = shard.loaded_chunks_mut().release(pos, ticket);
    }

    Ok(())
}

/// Drains queued inputs into the shard, advances one tick, and routes outputs.
///
/// Also records the per-tick observability metrics: it times the tick
/// (`ferrumc_tick_ms{shard}`), counts accepted and sim-rejected block edits
/// (`ferrumc_block_mutation_total{kind,result}`), advances and publishes the
/// authoritative tick through `clock`, and emits a structured tick event.
#[allow(clippy::too_many_arguments)] // the driver threads its per-tick state (tick, world clock, snapshot) through
fn run_tick(
    router: &mut SessionRouter,
    shard: &mut SimShard,
    shard_rx: &mut mpsc::Receiver<GameInput>,
    metrics: &CounterRegistry,
    clock: &ServerClock,
    tick: &mut Tick,
    world_time: &mut WorldTime,
    snap: &mut SnapshotCtx,
) -> ServerResult<()> {
    let start = Instant::now();

    // Move only what the simulation inbox can accept. Capacity is checked before
    // receiving, so an excess authoritative input remains owned by the bounded
    // router channel and is retried next tick.
    let inputs_drained = drain_shard_inputs(shard, shard_rx)?;
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
        if let Err(error) = router.disconnect_player_owned(player) {
            match error.error() {
                SessionError::ShardInboxFull { .. } | SessionError::UnknownPlayer { .. } => {
                    // A full leave is retained in SessionRouter's player-bounded
                    // pending set. The retry below (and each later tick) owns the
                    // outcome; an already-gone player needs no further action.
                }
                _ => {
                    return Err(ServerError::invalid_state(format!(
                        "mandatory outbound teardown could not reach its shard: {error}"
                    )));
                }
            }
        }
    }

    // The channel was drained at the start of this tick, so retry retained
    // lifecycle leaves now. A still-full control lane remains explicitly pending
    // for the next tick; a closed/invalid shard is fatal rather than log-only.
    if let Err(error) = router.retry_pending_disconnects() {
        match error.error() {
            SessionError::ShardInboxFull { .. } | SessionError::UnknownPlayer { .. } => {}
            _ => {
                return Err(ServerError::invalid_state(format!(
                    "retained PlayerLeave retry could not reach its shard: {error}"
                )));
            }
        }
    }

    // Advance and publish the authoritative tick (saturating: it never wraps
    // silently), then record the tick metrics for this shard.
    *tick = tick.saturating_add(1);
    clock.set(*tick);

    // Advance the deterministic day-night clock in lockstep with the tick, then
    // broadcast the world time to every player once per second so their skies keep
    // animating between client-side interpolation. A zero-player broadcast is a
    // no-op, and a dropped (Cosmetic) update is healed by the next one.
    world_time.advance();
    if world_time.world_age() % TIME_BROADCAST_INTERVAL_TICKS == 0 {
        router.broadcast_play_packet(&update_time_packet(world_time));
    }

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
    Ok(())
}

/// Transfers bounded router inputs without consuming past the simulation
/// inbox's available capacity.
fn drain_shard_inputs(
    shard: &mut SimShard,
    shard_rx: &mut mpsc::Receiver<GameInput>,
) -> ServerResult<usize> {
    let mut inputs_drained = 0usize;
    while !shard.is_inbox_full() {
        let Ok(input) = shard_rx.try_recv() else {
            break;
        };
        shard.enqueue(input).map_err(|error| {
            // This task is the shard's sole owner, so no producer can fill the
            // inbox between the capacity check and enqueue. Treat any violation
            // as fatal instead of logging and continuing after consuming input.
            ServerError::invalid_state(format!(
                "simulation inbox rejected input after reporting capacity: {error}"
            ))
        })?;
        inputs_drained += 1;
    }
    Ok(inputs_drained)
}

/// Builds and publishes the read-only [`ServerSnapshot`] for this tick.
///
/// Folds the metric-derived fields from `metrics` with app-side context: the
/// effective TPS (from a bounded timestamp window), uptime, the per-player list
/// assembled from the driver-owned roster plus public read-only shard/router
/// queries, the per-player network counters and server-wide packet-trace
/// summaries aggregated from the network-telemetry hub, and the per-plugin
/// event-decision counts (block edits, chat, and interactions) read from the
/// block-event dispatcher.
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
        // Per-plugin event-decision counts (block edits, chat, and interactions),
        // read from the shared dispatcher.
        plugin_decisions: snap.block_events.decision_snapshots(),
        network_per_player: net.per_player,
        inbound_trace_summary: net.inbound,
        outbound_trace_summary: net.outbound,
    };

    snap.publisher.publish(metrics.server_snapshot(parts));
}

#[cfg(test)]
mod tests {
    // Teleport/weather assertions compare exact, representable shell coordinates
    // and game-event values, so exact float comparison is intentional here.
    #![allow(clippy::float_cmp)]
    // The test setup pairs a `router` with a `roster`; the names are deliberately
    // close, mirroring the production `player_roster`.
    #![allow(clippy::similar_names)]

    use std::collections::BTreeMap;
    use std::num::NonZeroUsize;

    use ferrumc_core::{GameMode, PlayerId, ServerError, Tick};
    use ferrumc_math::{BlockPos, ChunkPos, Cuboid, Direction, ShardPos, Vec3};
    use ferrumc_net::DisconnectReason;
    use ferrumc_proto::generated::play::ClientboundPlayPacket;
    use ferrumc_session::{NetEvent, PlayerSessionHandle, SessionError, SessionRouter};
    use ferrumc_sim::{
        BlockStateId, ChunkTicket, GameInput, RegionOp, SimShard, TicketReason, WorldTime,
        TIME_DAY, TIME_NOON,
    };
    use ferrumc_storage::InMemoryStore;
    use ferrumc_world::FlatWorldGenerator;
    use tokio::sync::{mpsc, oneshot};

    use crate::storage_worker::StorageFlushRequest;

    use super::{
        drain_shard_inputs, handle_command, release_chunks, release_chunks_acked,
        route_input_command, try_flush_persist_dirty, SimCommand,
    };

    /// Builds one shard-mutating driver command for the saturation matrix.
    type CommandBuilder = fn(PlayerId) -> SimCommand;

    /// Builds one direct simulation input for the saturation matrix.
    type InputBuilder = fn(PlayerId) -> GameInput;

    /// Drains every queued outbound packet on `handle` (used to discard
    /// join-visibility traffic before asserting on a command's effect).
    fn drain(handle: &mut PlayerSessionHandle) {
        while handle.try_recv().is_some() {}
    }

    /// Runs one driver command with deterministic in-memory dependencies.
    async fn dispatch_test_command(
        router: &mut SessionRouter,
        roster: &mut BTreeMap<PlayerId, String>,
        command: SimCommand,
    ) -> Result<(), ServerError> {
        let mut shard = SimShard::new(ShardPos::new(0, 0));
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        let (storage_tx, _storage_rx) = mpsc::channel(1);
        let mut next_mutation_id = 0u64;
        let mut world_time = WorldTime::new();
        handle_command(
            router,
            &mut shard,
            &store,
            &generator,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &mut world_time,
            roster,
            command,
        )
        .await
    }

    /// Builds a connected player whose tiny data lane is stopped at one reserved
    /// control slot.
    fn data_saturated_router(
        label: &str,
    ) -> (
        SessionRouter,
        mpsc::Receiver<GameInput>,
        PlayerSessionHandle,
        PlayerId,
    ) {
        let mut router = SessionRouter::with_capacities_and_control_reserve(2, 8, 1);
        let inbox = router.register_shard(ShardPos::new(0, 0));
        let player = PlayerId::offline(label);
        let handle = router
            .join_player(player, label, spawn())
            .expect("join occupies the data portion of the tiny queue");
        (router, inbox, handle, player)
    }

    /// Proves an overload path enqueued no mutation: only the original join and
    /// the reserved-lane leave reached the shard.
    fn assert_join_then_overload_leave(inbox: &mut mpsc::Receiver<GameInput>, player: PlayerId) {
        assert!(matches!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin { player: joined, .. }) if joined == player
        ));
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::PlayerLeave { player }),
            "the rejected mutation is replaced only by explicit session teardown",
        );
        assert!(inbox.try_recv().is_err());
    }

    /// The `(reason, value)` of the next outbound `GameEvent` on `handle`.
    fn next_game_event(handle: &mut PlayerSessionHandle) -> (u8, f32) {
        let ClientboundPlayPacket::GameEvent(event) =
            handle.try_recv().expect("a queued packet").into_packet()
        else {
            panic!("expected a GameEvent");
        };
        (event.reason(), event.value())
    }

    /// The `(world_age, time_of_day, increasing)` of the next outbound
    /// `UpdateTime` on `handle`, skipping any other queued packets.
    fn next_update_time(handle: &mut PlayerSessionHandle) -> (i64, i64, bool) {
        while let Some(message) = handle.try_recv() {
            if let ClientboundPlayPacket::UpdateTime(update) = message.into_packet() {
                return (
                    update.world_age(),
                    update.time_of_day(),
                    update.time_of_day_increasing(),
                );
            }
        }
        panic!("expected a queued UpdateTime");
    }

    /// Builds one edited chunk held by exactly one player ticket.
    async fn dirty_player_chunk() -> (SimShard, ChunkPos) {
        let mut shard = SimShard::new(ShardPos::new(0, 0));
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        let pos = ChunkPos::new(0, 0);
        let ticket = ChunkTicket::of(TicketReason::Player);
        shard
            .loaded_chunks_mut()
            .acquire(&store, &generator, pos, ticket)
            .await
            .expect("load deterministic flat chunk");

        let edited = BlockPos::new(1, 65, 1);
        let chunk = shard
            .loaded_chunks_mut()
            .get_mut(pos)
            .expect("acquired chunk is resident");
        chunk
            .set_block(edited, BlockStateId::new(1))
            .expect("edit is inside the resident chunk");
        chunk.mark_persist_dirty(edited);

        assert_eq!(shard.loaded_chunks().ticket_count(pos), 1);
        assert!(shard.loaded_chunks().has_persist_dirty());
        (shard, pos)
    }

    #[tokio::test]
    async fn end_of_tick_flush_defers_full_but_rejects_closed_without_draining() {
        let (mut shard, _pos) = dirty_player_chunk().await;
        // One occupied slot deterministically represents storage backpressure
        // without scheduling a consumer.
        let (full_tx, _full_rx) = mpsc::channel::<StorageFlushRequest>(1);
        full_tx
            .try_send(StorageFlushRequest::new(Vec::new(), Vec::new()))
            .expect("fill the sole storage slot");
        let mut next_mutation_id = 0;

        try_flush_persist_dirty(&mut shard, &full_tx, Tick::ZERO, &mut next_mutation_id)
            .expect("a full queue is lossless backpressure, not worker failure");
        assert!(shard.loaded_chunks().has_persist_dirty());

        let (closed_tx, closed_rx) = mpsc::channel::<StorageFlushRequest>(1);
        drop(closed_rx);
        let error =
            try_flush_persist_dirty(&mut shard, &closed_tx, Tick::ZERO, &mut next_mutation_id)
                .expect_err("a closed queue means the worker is unavailable");

        assert!(matches!(error, ServerError::InvalidState(_)));
        assert!(shard.loaded_chunks().has_persist_dirty());
    }

    #[tokio::test]
    async fn release_chunks_closed_channel_retains_dirty_state_and_last_ticket() {
        let (mut shard, pos) = dirty_player_chunk().await;
        let (storage_tx, storage_rx) = mpsc::channel::<StorageFlushRequest>(1);
        drop(storage_rx);
        let mut next_mutation_id = 0;

        let error = release_chunks(
            &mut shard,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &[pos],
        )
        .await
        .expect_err("a closed worker must reject the flush");

        assert!(matches!(error, ServerError::InvalidState(_)));
        assert!(shard.loaded_chunks().has_persist_dirty());
        assert_eq!(shard.loaded_chunks().ticket_count(pos), 1);
        assert!(shard.loaded_chunks().is_loaded(pos));
    }

    #[tokio::test]
    async fn failed_movement_release_ack_retains_last_ticket() {
        let (mut shard, pos) = dirty_player_chunk().await;
        let (storage_tx, mut storage_rx) = mpsc::channel::<StorageFlushRequest>(1);
        let mut next_mutation_id = 0;

        let worker = tokio::spawn(async move {
            let mut request = storage_rx.recv().await.expect("movement barrier request");
            request
                .ack
                .take()
                .expect("movement barrier has an acknowledgement")
                .send(Err(ServerError::internal(
                    "injected movement commit failure",
                )))
                .expect("driver is awaiting the movement acknowledgement");
        });

        let error = release_chunks(
            &mut shard,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &[pos],
        )
        .await
        .expect_err("a failed movement flush cannot release the last ticket");

        assert!(matches!(error, ServerError::Internal { .. }));
        assert_eq!(shard.loaded_chunks().ticket_count(pos), 1);
        assert!(shard.loaded_chunks().is_loaded(pos));
        worker.await.expect("movement worker task completed");
    }

    #[tokio::test]
    async fn closed_durability_barrier_retains_dirty_state_and_last_ticket() {
        let (mut shard, pos) = dirty_player_chunk().await;
        let (storage_tx, storage_rx) = mpsc::channel::<StorageFlushRequest>(1);
        drop(storage_rx);
        let mut next_mutation_id = 0;

        let error = release_chunks_acked(
            &mut shard,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &[pos],
        )
        .await
        .expect_err("a closed worker cannot prove durability");

        assert!(matches!(error, ServerError::InvalidState(_)));
        assert!(shard.loaded_chunks().has_persist_dirty());
        assert_eq!(shard.loaded_chunks().ticket_count(pos), 1);
        assert!(shard.loaded_chunks().is_loaded(pos));
    }

    #[tokio::test]
    async fn failed_durability_ack_retains_last_ticket() {
        let (mut shard, pos) = dirty_player_chunk().await;
        let (storage_tx, mut storage_rx) = mpsc::channel::<StorageFlushRequest>(1);
        let mut next_mutation_id = 0;

        let worker = tokio::spawn(async move {
            let mut request = storage_rx.recv().await.expect("barrier request");
            let overlay_count = request.overlays.len();
            request
                .ack
                .take()
                .expect("barrier has an acknowledgement")
                .send(Err(ServerError::internal("injected commit failure")))
                .expect("driver is awaiting the acknowledgement");
            overlay_count
        });

        let error = release_chunks_acked(
            &mut shard,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &[pos],
        )
        .await
        .expect_err("a failed commit must fail the barrier");

        assert!(matches!(error, ServerError::Internal { .. }));
        assert_eq!(shard.loaded_chunks().ticket_count(pos), 1);
        assert!(shard.loaded_chunks().is_loaded(pos));
        assert_eq!(
            worker.await.expect("worker task completed"),
            1,
            "the failed barrier carried the recoverable overlay to the worker"
        );
    }

    #[tokio::test]
    async fn canceled_durability_ack_retains_last_ticket() {
        let (mut shard, pos) = dirty_player_chunk().await;
        let (storage_tx, mut storage_rx) = mpsc::channel::<StorageFlushRequest>(1);
        let mut next_mutation_id = 0;

        let worker = tokio::spawn(async move {
            let request = storage_rx.recv().await.expect("barrier request");
            assert_eq!(request.overlays.len(), 1);
            drop(request);
        });

        let error = release_chunks_acked(
            &mut shard,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &[pos],
        )
        .await
        .expect_err("a canceled acknowledgement cannot prove durability");

        assert!(matches!(error, ServerError::InvalidState(_)));
        assert_eq!(shard.loaded_chunks().ticket_count(pos), 1);
        assert!(shard.loaded_chunks().is_loaded(pos));
        worker.await.expect("worker task completed");
    }

    #[tokio::test]
    async fn successful_durability_ack_releases_last_ticket() {
        let (mut shard, pos) = dirty_player_chunk().await;
        let (storage_tx, mut storage_rx) = mpsc::channel::<StorageFlushRequest>(1);
        let mut next_mutation_id = 0;

        let worker = tokio::spawn(async move {
            let mut request = storage_rx.recv().await.expect("barrier request");
            let overlay_count = request.overlays.len();
            request
                .ack
                .take()
                .expect("barrier has an acknowledgement")
                .send(Ok(()))
                .expect("driver is awaiting the acknowledgement");
            overlay_count
        });

        release_chunks_acked(
            &mut shard,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &[pos],
        )
        .await
        .expect("a successful commit permits release");

        assert_eq!(worker.await.expect("worker task completed"), 1);
        assert_eq!(shard.loaded_chunks().ticket_count(pos), 0);
        assert!(!shard.loaded_chunks().is_loaded(pos));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one named regression covers every authoritative input family
    async fn full_shard_queue_never_silently_drops_authoritative_input() {
        // Placement has a success-preview reply. Saturation must classify it as
        // rejected, send no preview, and terminate through the reserved leave.
        let (mut router, mut inbox, _handle, player) = data_saturated_router("overloaded-placer");
        let mut roster = BTreeMap::new();
        let (reply_tx, reply_rx) = oneshot::channel();
        dispatch_test_command(
            &mut router,
            &mut roster,
            SimCommand::PlaceBlock {
                player,
                position: BlockPos::new(8, 65, 8),
                sequence: 7,
                state: BlockStateId::new(137),
                clicked_face: Direction::East,
                cursor_position: Vec3::new(0.5, 0.5, 0.5),
                player_yaw: 0.0,
                reply: reply_tx,
            },
        )
        .await
        .expect("overload is handled without killing the driver");

        let rejection = reply_rx
            .await
            .expect("the driver classifies the placement outcome")
            .expect_err("a success preview must not precede shard acceptance");
        assert!(
            matches!(rejection, SessionError::ShardInboxFull { .. }),
            "the caller receives the classified capacity rejection",
        );
        assert!(
            !router.is_player_connected(player),
            "an authoritative overload must explicitly terminate the session",
        );
        assert_join_then_overload_leave(&mut inbox, player);

        // Every command/input branch that mutates simulation state uses the same
        // enforcement funnel. Each fresh tiny queue proves the rejected command
        // itself never appears between PlayerJoin and PlayerLeave.
        let command_cases: [(&str, CommandBuilder); 6] = [
            ("exact-edit", |player| SimCommand::SetBlockExact {
                player,
                position: BlockPos::new(8, 65, 8),
                sequence: 11,
                state: BlockStateId::new(1),
                acceptance: None,
            }),
            ("game-mode", |player| SimCommand::SetGameMode {
                player,
                mode: GameMode::Adventure,
                acceptance: None,
            }),
            ("region-edit", |player| SimCommand::RegionEdit {
                player,
                region: Cuboid::new(BlockPos::new(8, 65, 8), BlockPos::new(9, 65, 9)),
                op: RegionOp::Fill {
                    state: BlockStateId::new(1),
                },
                acceptance: None,
            }),
            ("region-undo", |player| SimCommand::RegionUndo {
                player,
                acceptance: None,
            }),
            ("sign-edit", |player| SimCommand::UpdateSign {
                player,
                position: BlockPos::new(8, 65, 8),
                is_front: true,
                lines: std::array::from_fn(|index| format!("line {index}")),
                acceptance: None,
            }),
            ("teleport-command", |player| SimCommand::TeleportPlayer {
                player,
                position: Vec3::new(24.0, 70.0, 24.0),
                acceptance: None,
            }),
        ];
        for (label, build) in command_cases {
            let (mut router, mut inbox, mut handle, player) = data_saturated_router(label);
            dispatch_test_command(&mut router, &mut BTreeMap::new(), build(player))
                .await
                .expect("data-lane overload is a session outcome, not a driver failure");
            assert!(!router.is_player_connected(player), "{label}");
            if label == "teleport-command" {
                assert!(
                    handle.try_recv().is_none(),
                    "a rejected teleport must not preview a position sync",
                );
            }
            assert_join_then_overload_leave(&mut inbox, player);
        }

        // The raw break/movement event classes also cannot disappear. Movement is
        // coalescible by policy; this app deliberately classifies saturation as a
        // flooding-session disconnect rather than retaining an unbounded latest
        // value.
        let input_cases: [(&str, InputBuilder); 2] = [
            ("block-break", |player| GameInput::BlockBreak {
                player,
                position: BlockPos::new(8, 65, 8),
                sequence: 13,
            }),
            ("movement", |player| GameInput::PlayerMove {
                player,
                position: Some(Vec3::new(9.0, 64.0, 9.0)),
                yaw: None,
                pitch: None,
            }),
        ];
        for (label, build) in input_cases {
            let (mut router, mut inbox, _handle, player) = data_saturated_router(label);
            route_input_command(&mut router, player, build(player), None, label)
                .expect("overload explicitly disconnects the source");
            assert!(!router.is_player_connected(player), "{label}");
            assert_join_then_overload_leave(&mut inbox, player);
        }

        // Rejection healing is control traffic: after ordinary data has stopped,
        // it consumes the reserve instead of being dropped and leaves the session
        // connected for the sim's resync+ack output.
        let (mut router, mut inbox, _handle, player) = data_saturated_router("rejection-control");
        let rejection = GameInput::RejectBlockEdit {
            player,
            position: BlockPos::new(8, 65, 8),
            sequence: 17,
            requested_state: BlockStateId::AIR,
        };
        route_input_command(
            &mut router,
            player,
            rejection.clone(),
            None,
            "block-edit rejection",
        )
        .expect("the reserved control slot accepts rejection healing");
        assert!(router.is_player_connected(player));
        assert!(matches!(inbox.try_recv(), Ok(GameInput::PlayerJoin { .. })));
        assert_eq!(inbox.try_recv(), Ok(rejection));

        // A second join has no session to terminate; it is explicitly rejected
        // through the existing join reply while the first mapping stays intact.
        let mut router = SessionRouter::with_capacities(1, 8);
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let first = PlayerId::offline("first-join");
        let _first_handle = router
            .join_player(first, "first-join", spawn())
            .expect("first join fills the tiny control queue");
        let second = PlayerId::offline("second-join");
        let (reply_tx, reply_rx) = oneshot::channel();
        dispatch_test_command(
            &mut router,
            &mut BTreeMap::new(),
            SimCommand::Join {
                player: second,
                name: "second-join".to_string(),
                position: spawn(),
                equipment: Vec::new(),
                reply: reply_tx,
            },
        )
        .await
        .expect("join rejection is returned to its caller");
        assert!(matches!(
            reply_rx.await.expect("join reply"),
            Err(SessionError::ShardInboxFull { .. })
        ));
        assert!(router.is_player_connected(first));
        assert!(!router.is_player_connected(second));
    }

    #[test]
    fn simulation_handoff_leaves_excess_authoritative_input_queued() {
        let capacity = NonZeroUsize::new(1).expect("test capacity is non-zero");
        let mut shard = SimShard::with_inbox_capacity(ShardPos::new(0, 0), capacity);
        let (tx, mut rx) = mpsc::channel(2);
        let player = PlayerId::offline("handoff");
        let first = GameInput::PlayerJoin {
            player,
            position: spawn(),
        };
        let second = GameInput::SetGameMode {
            player,
            mode: GameMode::Adventure,
        };
        tx.try_send(first).expect("first bounded input");
        tx.try_send(second.clone()).expect("second bounded input");

        assert_eq!(
            drain_shard_inputs(&mut shard, &mut rx).expect("handoff respects capacity"),
            1,
        );
        assert_eq!(shard.inbox_len(), 1);
        assert_eq!(
            rx.try_recv(),
            Ok(second),
            "the input beyond simulation capacity remains in the router channel",
        );
    }

    #[test]
    fn exhausted_control_lane_fails_closed_and_retains_the_leave() {
        let mut router = SessionRouter::with_capacities(1, 8);
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let player = PlayerId::offline("control-exhausted");
        let _handle = router
            .join_player(player, "control-exhausted", spawn())
            .expect("join physically fills the capacity-one queue");

        let error = route_input_command(
            &mut router,
            player,
            GameInput::RejectBlockEdit {
                player,
                position: BlockPos::new(8, 65, 8),
                sequence: 19,
                requested_state: BlockStateId::AIR,
            },
            None,
            "block-edit rejection",
        )
        .expect_err("physical control-lane exhaustion must fail the driver closed");
        assert!(matches!(error, ServerError::Capacity(_)));
        assert!(router.is_player_connected(player));
        assert_eq!(router.pending_disconnect_count(), 1);

        assert!(matches!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin { player: joined, .. }) if joined == player
        ));
        router
            .retry_pending_disconnects()
            .expect("freed capacity accepts the retained leave");
        assert!(!router.is_player_connected(player));
        assert_eq!(router.pending_disconnect_count(), 0);
        assert_eq!(inbox.try_recv(), Ok(GameInput::PlayerLeave { player }),);
    }

    #[tokio::test]
    async fn full_disconnect_is_retained_until_control_capacity_returns() {
        let mut router = SessionRouter::with_capacities(1, 8);
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let player = PlayerId::offline("disconnect-retry");
        let _handle = router
            .join_player(player, "disconnect-retry", spawn())
            .expect("join physically fills the capacity-one queue");

        dispatch_test_command(
            &mut router,
            &mut BTreeMap::new(),
            SimCommand::Event {
                event: NetEvent::disconnected(player, DisconnectReason::ServerShutdown),
                acceptance: None,
            },
        )
        .await
        .expect("a saturated teardown remains explicit pending work");
        assert!(router.is_player_connected(player));
        assert_eq!(router.pending_disconnect_count(), 1);

        assert!(matches!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin { player: joined, .. }) if joined == player
        ));
        router
            .retry_pending_disconnects()
            .expect("freed capacity accepts the retained disconnect");
        assert!(!router.is_player_connected(player));
        assert_eq!(inbox.try_recv(), Ok(GameInput::PlayerLeave { player }),);
    }

    #[tokio::test]
    async fn place_block_command_replies_with_the_refined_state() {
        // After bounded routing succeeds, the driver previews the FINAL computed
        // state (an east-face oak_log refines to axis=x, state 136), so the
        // connection fires after_block_place with the state the world will hold
        // rather than the held default (137). An empty shard is enough: the
        // axis-from-face rule consults the clicked face, not neighbours.
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let player = PlayerId::offline("placer");
        let _handle = router
            .join_player(player, "placer", spawn())
            .expect("join placer");
        assert!(matches!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin { player: joined, .. }) if joined == player
        ));
        let mut shard = SimShard::new(ShardPos::new(0, 0));
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        let (storage_tx, _storage_rx) = mpsc::channel(1);
        let mut next_mutation_id = 0u64;
        let mut world_time = WorldTime::new();
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
            &mut world_time,
            &mut player_roster,
            SimCommand::PlaceBlock {
                player,
                position: BlockPos::new(8, 65, 8),
                sequence: 1,
                state: BlockStateId::new(137), // oak_log default (axis=y)
                clicked_face: Direction::East,
                cursor_position: Vec3::new(0.5, 0.5, 0.5),
                player_yaw: 0.0,
                reply: reply_tx,
            },
        )
        .await
        .expect("placement command does not cross a durability barrier");

        assert_eq!(
            reply_rx
                .await
                .expect("driver replied with the computed state"),
            Ok(BlockStateId::new(136)),
        );
    }

    /// A fixed in-shard spawn used by the teleport/weather/gamemode driver tests.
    fn spawn() -> Vec3 {
        Vec3::new(8.0, 64.0, 8.0)
    }

    #[tokio::test]
    async fn weather_command_broadcasts_a_game_event_to_everyone() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let mut a = router
            .join_player(PlayerId::offline("a"), "a", spawn())
            .expect("join a");
        let mut b = router
            .join_player(PlayerId::offline("b"), "b", spawn())
            .expect("join b");
        drain(&mut a);
        drain(&mut b);

        let mut shard = SimShard::new(ShardPos::new(0, 0));
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        let (storage_tx, _storage_rx) = mpsc::channel(1);
        let mut next_mutation_id = 0u64;
        let mut world_time = WorldTime::new();
        let mut roster = BTreeMap::new();

        handle_command(
            &mut router,
            &mut shard,
            &store,
            &generator,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &mut world_time,
            &mut roster,
            SimCommand::SetWeather { raining: true },
        )
        .await
        .expect("weather command does not cross a durability barrier");

        // Both players receive start_raining (reason 1, no value). clear would be 2.
        assert_eq!(next_game_event(&mut a), (1, 0.0));
        assert_eq!(next_game_event(&mut b), (1, 0.0));
    }

    #[tokio::test]
    async fn set_time_sets_the_phase_and_broadcasts_to_everyone() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let mut a = router
            .join_player(PlayerId::offline("a"), "a", spawn())
            .expect("join a");
        let mut b = router
            .join_player(PlayerId::offline("b"), "b", spawn())
            .expect("join b");
        drain(&mut a);
        drain(&mut b);

        let mut shard = SimShard::new(ShardPos::new(0, 0));
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        let (storage_tx, _storage_rx) = mpsc::channel(1);
        let mut next_mutation_id = 0u64;
        let mut world_time = WorldTime::new();
        let mut roster = BTreeMap::new();

        handle_command(
            &mut router,
            &mut shard,
            &store,
            &generator,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &mut world_time,
            &mut roster,
            SimCommand::SetTime {
                time_of_day: TIME_DAY,
            },
        )
        .await
        .expect("time command does not cross a durability barrier");

        // `/time set day` sets the authoritative phase to 1000 (age unchanged) ...
        assert_eq!(world_time.time_of_day(), 1_000);
        assert_eq!(world_time.world_age(), 0);
        // ... and every player receives an Update Time carrying it (increasing).
        assert_eq!(next_update_time(&mut a), (0, 1_000, true));
        assert_eq!(next_update_time(&mut b), (0, 1_000, true));
    }

    #[tokio::test]
    async fn join_sends_the_current_world_time() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));

        let mut shard = SimShard::new(ShardPos::new(0, 0));
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        let (storage_tx, _storage_rx) = mpsc::channel(1);
        let mut next_mutation_id = 0u64;
        // A clock already wound to noon: the join send must carry that phase so the
        // client's sky is correct from the first frame, not the default phase 0.
        let mut world_time = WorldTime::new();
        world_time.set_time_of_day(TIME_NOON);
        let mut roster = BTreeMap::new();
        let (reply_tx, reply_rx) = oneshot::channel();

        handle_command(
            &mut router,
            &mut shard,
            &store,
            &generator,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &mut world_time,
            &mut roster,
            SimCommand::Join {
                player: PlayerId::offline("joiner"),
                name: "joiner".to_string(),
                position: spawn(),
                equipment: Vec::new(),
                reply: reply_tx,
            },
        )
        .await
        .expect("join command does not cross a durability barrier");

        let mut handle = reply_rx
            .await
            .expect("driver replied to the join")
            .expect("join accepted");
        assert_eq!(next_update_time(&mut handle), (0, TIME_NOON, true));
    }

    #[tokio::test]
    async fn query_time_sends_a_chat_to_the_asker() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let asker = PlayerId::offline("asker");
        let mut handle = router
            .join_player(asker, "asker", spawn())
            .expect("join asker");
        drain(&mut handle);

        let mut shard = SimShard::new(ShardPos::new(0, 0));
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        let (storage_tx, _storage_rx) = mpsc::channel(1);
        let mut next_mutation_id = 0u64;
        let mut world_time = WorldTime::new();
        world_time.set_time_of_day(TIME_DAY);
        let mut roster = BTreeMap::new();

        handle_command(
            &mut router,
            &mut shard,
            &store,
            &generator,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &mut world_time,
            &mut roster,
            SimCommand::QueryTime { player: asker },
        )
        .await
        .expect("query command does not cross a durability barrier");

        // The driver answers the asker directly with a System Chat Message.
        assert!(matches!(
            handle.try_recv().expect("a queued packet").into_packet(),
            ClientboundPlayPacket::SystemChat(_),
        ));
    }

    #[tokio::test]
    async fn set_game_mode_for_routes_authoritative_change_and_notifies_target() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let issuer = PlayerId::offline("Op");
        let target = PlayerId::offline("Joe");
        let _op = router.join_player(issuer, "Op", spawn()).expect("join op");
        let mut joe = router
            .join_player(target, "Joe", spawn())
            .expect("join joe");
        drain(&mut joe);
        // Discard the PlayerJoin inputs so the inbox below only holds the command.
        while inbox.try_recv().is_ok() {}

        let mut shard = SimShard::new(ShardPos::new(0, 0));
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        let (storage_tx, _storage_rx) = mpsc::channel(1);
        let mut next_mutation_id = 0u64;
        let mut world_time = WorldTime::new();
        let mut roster = BTreeMap::new();
        roster.insert(target, "Joe".to_string());
        let (acceptance_tx, acceptance_rx) = oneshot::channel();

        handle_command(
            &mut router,
            &mut shard,
            &store,
            &generator,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &mut world_time,
            &mut roster,
            SimCommand::SetGameModeFor {
                target: "Joe".to_string(),
                mode: GameMode::Creative,
                acceptance: Some(acceptance_tx),
            },
        )
        .await
        .expect("game-mode command does not cross a durability barrier");
        assert_eq!(
            acceptance_rx.await.expect("driver replies to command"),
            Ok(())
        );

        // The authoritative change is routed to the target's shard ...
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::SetGameMode {
                player: target,
                mode: GameMode::Creative,
            })
        );
        // ... and the target's client is switched with change_game_mode (reason 3)
        // carrying creative (1.0).
        assert_eq!(next_game_event(&mut joe), (3, 1.0));
    }

    #[tokio::test]
    async fn targeted_game_mode_preview_waits_for_shard_acceptance() {
        let mut router = SessionRouter::with_capacities_and_control_reserve(3, 8, 1);
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let issuer = PlayerId::offline("ModeOp");
        let target = PlayerId::offline("ModeTarget");
        let mut op = router
            .join_player(issuer, "ModeOp", spawn())
            .expect("join issuer");
        let mut target_handle = router
            .join_player(target, "ModeTarget", spawn())
            .expect("join target");
        drain(&mut op);
        drain(&mut target_handle);
        let mut roster = BTreeMap::new();
        roster.insert(target, "ModeTarget".to_string());
        let (acceptance_tx, acceptance_rx) = oneshot::channel();

        dispatch_test_command(
            &mut router,
            &mut roster,
            SimCommand::SetGameModeFor {
                target: "ModeTarget".to_string(),
                mode: GameMode::Adventure,
                acceptance: Some(acceptance_tx),
            },
        )
        .await
        .expect("data-lane overload terminates only the target session");

        assert!(matches!(
            acceptance_rx.await.expect("classified command reply"),
            Err(SessionError::ShardInboxFull { .. })
        ));
        assert!(!router.is_player_connected(target));
        assert!(
            target_handle.try_recv().is_none(),
            "the client mode switch must not precede authoritative acceptance",
        );
        assert!(matches!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin { player, .. }) if player == issuer
        ));
        assert!(matches!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin { player, .. }) if player == target
        ));
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::PlayerLeave { player: target })
        );
    }

    #[tokio::test]
    async fn teleport_to_player_snaps_the_issuer_to_the_target() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let issuer = PlayerId::offline("Op");
        let target = PlayerId::offline("Joe");
        let mut op = router.join_player(issuer, "Op", spawn()).expect("join op");
        // Inside shard (0, 0) (chunks 0..8 on each axis) so the join is accepted.
        let target_pos = Vec3::new(20.0, 70.0, 40.0);
        let _joe = router
            .join_player(target, "Joe", target_pos)
            .expect("join joe");
        drain(&mut op);

        let mut shard = SimShard::new(ShardPos::new(0, 0));
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        let (storage_tx, _storage_rx) = mpsc::channel(1);
        let mut next_mutation_id = 0u64;
        let mut world_time = WorldTime::new();
        let mut roster = BTreeMap::new();
        roster.insert(target, "Joe".to_string());

        handle_command(
            &mut router,
            &mut shard,
            &store,
            &generator,
            &storage_tx,
            Tick::ZERO,
            &mut next_mutation_id,
            &mut world_time,
            &mut roster,
            SimCommand::TeleportToPlayer {
                player: issuer,
                target: "Joe".to_string(),
                acceptance: None,
            },
        )
        .await
        .expect("teleport command does not cross a durability barrier");

        // The issuer's client is snapped to the target's position (the sim has not
        // ticked the join, so resolution falls back to the router's seeded pos).
        let ClientboundPlayPacket::SynchronizePlayerPosition(sync) =
            op.try_recv().expect("a teleport sync").into_packet()
        else {
            panic!("expected a SynchronizePlayerPosition");
        };
        assert_eq!((sync.x(), sync.y(), sync.z()), (20.0, 70.0, 40.0));
    }
}
