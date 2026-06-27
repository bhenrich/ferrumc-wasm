//! [`SessionRouter`] and [`PlayerSessionHandle`]: the player<->shard mapping and
//! the message-based bridge between connections and simulation shards.

use std::collections::BTreeMap;

use tokio::sync::mpsc::{self, error::TrySendError};

use ferrumc_core::{PlayerId, TextComponent};
use ferrumc_math::{BlockPos, ChunkPos, ShardPos, Vec3};
use ferrumc_proto::generated::play::ClientboundPlayPacket;
use ferrumc_sim::{BlockStateId, GameInput, GameOutput, MutationCause};

use crate::error::SessionError;
use crate::event::NetEvent;
use crate::translate::{
    ack_shell, block_update_shell, chunk_for_position, entity_spawn_shell, entity_teleport_shell,
    move_shell, play_packet_to_input, player_info_add, player_info_remove, remove_entities_shell,
    shard_for_position, update_entity_position_shell,
};

/// Default capacity of each shard's input channel.
///
/// Sized to mirror the simulation shard inbox (1024 queued inputs): far above
/// the per-tick volume a well-behaved router produces, so hitting it signals a
/// stall or a flood — exactly when [reject backpressure](SessionError::ShardInboxFull)
/// should engage.
pub const DEFAULT_SHARD_INPUT_CAPACITY: usize = 1024;

/// Default capacity of each player's outbound channel.
///
/// A connection's writer task drains this; a moderate bound absorbs a normal
/// burst of clientbound shells while still capping a backlog. A persistently
/// full queue is the caller's cue to disconnect a client that cannot keep up.
pub const DEFAULT_OUTBOUND_CAPACITY: usize = 256;

/// Default view distance, in chunks, scoping which viewers a player is broadcast
/// to.
///
/// Matches the app's default play view distance. Visibility is a square
/// (Chebyshev) chunk-distance test: a subject is shown to a viewer when their
/// chunks differ by at most this many on both axes.
pub const DEFAULT_VIEW_DISTANCE: i32 = 10;

/// First network entity id handed to a joining player.
///
/// Starts at `2` to stay clear of `0` and of the id `1` the app assigns a client
/// to *itself* in `JoinGame`, so a remote player's entity can never collide with
/// a viewer's own.
const FIRST_ENTITY_ID: i32 = 2;

/// Squared move distance, in blocks, at or below which a move is broadcast as a
/// *relative* `UpdateEntityPosition` rather than an absolute `EntityTeleport`.
///
/// A relative move encodes each axis delta in 1/4096-block fixed point, which
/// tops out near 8 blocks per axis, so a larger jump must teleport. Comparing the
/// squared distance keeps the selection allocation- and `sqrt`-free.
const MAX_RELATIVE_MOVE_DISTANCE_SQ: f64 = 8.0 * 8.0;

/// The router's private per-player record.
///
/// Holds the routing target (`shard` + `outbound` channel) plus the lightweight
/// view state the router needs to scope and address visibility broadcasts: the
/// display `name` shown on the tab list and nameplate, the network `entity_id`
/// other clients see this player as, and the last-known `position` (seeded at
/// join, refreshed from every movement output). This is routing metadata
/// mirrored from simulation outputs, not authoritative world state — the
/// simulation still owns the real positions.
#[derive(Debug)]
struct SessionEntry {
    shard: ShardPos,
    outbound: mpsc::Sender<ClientboundPlayPacket>,
    name: String,
    entity_id: i32,
    position: Vec3,
}

/// A handle to one player's session, returned by
/// [`SessionRouter::join_player`].
///
/// The connection's writer task owns this handle and drains the outbound channel
/// — with [`recv`](Self::recv) (await) or [`try_recv`](Self::try_recv)
/// (non-blocking) — to deliver clientbound packets to the client. The handle
/// also records which [`shard`](Self::shard) the player joined.
///
/// Dropping the handle closes the outbound channel; the router observes this as a
/// [`SessionError::OutboundClosed`] the next time it tries to route an output to
/// the player, and the player should then be disconnected.
#[derive(Debug)]
pub struct PlayerSessionHandle {
    player: PlayerId,
    shard: ShardPos,
    outbound: mpsc::Receiver<ClientboundPlayPacket>,
}

impl PlayerSessionHandle {
    /// The player this session belongs to.
    pub fn player(&self) -> PlayerId {
        self.player
    }

    /// The shard the player was routed to on join.
    pub fn shard(&self) -> ShardPos {
        self.shard
    }

    /// Awaits the next clientbound packet, or `None` once the router has dropped
    /// the session and the channel is drained.
    pub async fn recv(&mut self) -> Option<ClientboundPlayPacket> {
        self.outbound.recv().await
    }

    /// Returns the next queued clientbound packet without waiting, or `None` if
    /// none is ready (the queue is empty or the router has dropped the session).
    pub fn try_recv(&mut self) -> Option<ClientboundPlayPacket> {
        self.outbound.try_recv().ok()
    }
}

/// Routes network events to simulation shards and simulation outputs back to
/// connections, owning the player<->shard mapping.
///
/// The router is the only component that knows where each player lives. It never
/// holds a [`SimShard`](ferrumc_sim::SimShard), a chunk, a socket, or a database
/// handle: it communicates purely by [`GameInput`]/[`GameOutput`] messages over
/// bounded [`mpsc`] channels.
///
/// # Wiring
///
/// - [`register_shard`](Self::register_shard) creates a shard's bounded input
///   channel and hands back the receiving half for the shard worker to drain.
/// - [`join_player`](Self::join_player) places a player on the shard owning their
///   spawn position, sends a [`GameInput::PlayerJoin`], and returns a
///   [`PlayerSessionHandle`] for the connection.
/// - [`route_event`](Self::route_event) translates a [`NetEvent`] and forwards it
///   to the player's shard.
/// - [`route_output`](Self::route_output) turns a [`GameOutput`] into the
///   clientbound packets it implies and delivers them to the relevant
///   connection(s).
/// - [`disconnect_player`](Self::disconnect_player) drops the mapping and
///   notifies the shard to despawn the player.
///
/// # Backpressure
///
/// Every channel is bounded and routing uses non-blocking sends, so the router
/// never blocks the tick loop. A full *shard input* channel surfaces as a
/// classified [`SessionError::ShardInboxFull`] for the caller to act on. Outbound
/// position broadcasts and corrections are instead lossy under backpressure — a
/// full recipient misses the update, which the next one supersedes — and a
/// recipient whose channel has *closed* is reported by
/// [`route_output`](Self::route_output) for disconnection.
///
/// # Single-shard binding (this milestone)
///
/// A player stays bound to the shard they joined for the lifetime of the
/// session: movement routes to that shard even if the new position lies in
/// another shard's region. Cross-shard handoff is a later milestone.
///
/// # Visibility broadcasts (this milestone)
///
/// Beyond echoing a player's own outputs, the router gives players sight of one
/// another. On [`join_player`](Self::join_player) it exchanges a player-list add
/// plus an entity spawn between the newcomer and every existing player within
/// view distance; on [`disconnect_player`](Self::disconnect_player) it sends a
/// player-list remove *and* an entity despawn for the departing player; and
/// [`route_output`](Self::route_output) broadcasts a mover's new position to the
/// viewers in range (relative move or teleport by distance). Visibility is scoped
/// to [`view_distance`](Self::view_distance) chunks. The router keeps a per-player
/// last-known position to make this chunk-distance test, refreshed from
/// simulation outputs — never authored here.
#[derive(Debug)]
pub struct SessionRouter {
    shards: BTreeMap<ShardPos, mpsc::Sender<GameInput>>,
    players: BTreeMap<PlayerId, SessionEntry>,
    shard_input_capacity: usize,
    outbound_capacity: usize,
    view_distance: i32,
    next_entity_id: i32,
}

impl SessionRouter {
    /// Creates an empty router with the default channel capacities
    /// ([`DEFAULT_SHARD_INPUT_CAPACITY`] and [`DEFAULT_OUTBOUND_CAPACITY`]) and
    /// the default [`DEFAULT_VIEW_DISTANCE`].
    pub fn new() -> Self {
        Self::with_capacities(DEFAULT_SHARD_INPUT_CAPACITY, DEFAULT_OUTBOUND_CAPACITY)
    }

    /// Creates an empty router with explicit channel capacities and the default
    /// [`DEFAULT_VIEW_DISTANCE`].
    ///
    /// Each capacity is clamped to at least `1`, since a bounded
    /// [`mpsc`] channel cannot have zero capacity.
    pub fn with_capacities(shard_input_capacity: usize, outbound_capacity: usize) -> Self {
        Self {
            shards: BTreeMap::new(),
            players: BTreeMap::new(),
            shard_input_capacity: shard_input_capacity.max(1),
            outbound_capacity: outbound_capacity.max(1),
            view_distance: DEFAULT_VIEW_DISTANCE,
            next_entity_id: FIRST_ENTITY_ID,
        }
    }

    /// The configured per-shard input channel capacity.
    pub fn shard_input_capacity(&self) -> usize {
        self.shard_input_capacity
    }

    /// The configured per-player outbound channel capacity.
    pub fn outbound_capacity(&self) -> usize {
        self.outbound_capacity
    }

    /// The view distance, in chunks, used to scope visibility broadcasts.
    pub fn view_distance(&self) -> i32 {
        self.view_distance
    }

    /// Sets the view distance, in chunks, used to scope visibility broadcasts.
    ///
    /// Clamped to `0` at minimum (a negative distance would hide every player,
    /// including those sharing a chunk). The app sets this from its configured
    /// play view distance at startup.
    pub fn set_view_distance(&mut self, chunks: i32) {
        self.view_distance = chunks.max(0);
    }

    /// The number of shards currently registered.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// `true` if a shard is registered at `shard`.
    pub fn is_shard_registered(&self, shard: ShardPos) -> bool {
        self.shards.contains_key(&shard)
    }

    /// The number of players with an active session.
    pub fn player_count(&self) -> usize {
        self.players.len()
    }

    /// `true` if `player` has an active session.
    pub fn is_player_connected(&self, player: PlayerId) -> bool {
        self.players.contains_key(&player)
    }

    /// The shard `player` is bound to, or `None` if they have no session.
    pub fn player_shard(&self, player: PlayerId) -> Option<ShardPos> {
        self.players.get(&player).map(|entry| entry.shard)
    }

    /// Registers a shard at `shard`, returning the receiving half of its bounded
    /// input channel.
    ///
    /// The caller (the shard worker) drains the returned receiver into the
    /// shard's inbox each tick. Registering an already-registered shard replaces
    /// its sender, closing the previous channel — so the previous receiver, if
    /// still held, observes the channel as closed.
    pub fn register_shard(&mut self, shard: ShardPos) -> mpsc::Receiver<GameInput> {
        let (tx, rx) = mpsc::channel(self.shard_input_capacity);
        self.shards.insert(shard, tx);
        rx
    }

    /// The network entity id a `player` is broadcast to other clients as, or
    /// `None` if they have no session.
    pub fn player_entity_id(&self, player: PlayerId) -> Option<i32> {
        self.players.get(&player).map(|entry| entry.entity_id)
    }

    /// The last-known position the router holds for `player`, or `None` if they
    /// have no session.
    ///
    /// This is routing metadata (seeded at join, refreshed from movement
    /// outputs), not the simulation's authoritative position.
    pub fn player_position(&self, player: PlayerId) -> Option<Vec3> {
        self.players.get(&player).map(|entry| entry.position)
    }

    /// Joins `player` (displayed as `name`) at `position`, routing them to the
    /// owning shard and making them mutually visible to nearby players.
    ///
    /// Determines the shard from `position`, sends a [`GameInput::PlayerJoin`] to
    /// it, records the mapping (storing the display `name`, allocating the
    /// player's network entity id, and seeding their position), exchanges
    /// player-list + spawn packets with every existing player within view
    /// distance, and returns a [`PlayerSessionHandle`] carrying the player's
    /// outbound channel. The `name` is what other clients show on the tab list
    /// and the nameplate above the spawned entity.
    ///
    /// # Errors
    ///
    /// - [`SessionError::UnknownShard`] if no shard owns `position`.
    /// - [`SessionError::DuplicatePlayer`] if `player` already has a session.
    /// - [`SessionError::ShardInboxFull`] / [`SessionError::ShardClosed`] if the
    ///   join could not be delivered to the shard.
    ///
    /// On any error nothing is registered, so the join can be retried cleanly.
    /// The visibility broadcast is best-effort: a viewer whose outbound channel
    /// is full or closed simply misses the update (a closed viewer is cleaned up
    /// when its own connection ends), so it never fails the join.
    pub fn join_player(
        &mut self,
        player: PlayerId,
        name: &str,
        position: Vec3,
    ) -> Result<PlayerSessionHandle, SessionError> {
        let shard = shard_for_position(position);
        if !self.shards.contains_key(&shard) {
            return Err(SessionError::UnknownShard { shard });
        }
        if self.players.contains_key(&player) {
            return Err(SessionError::DuplicatePlayer { player });
        }

        // Notify the shard before recording anything: if the join cannot be
        // delivered, the player must not be left half-registered.
        self.send_to_shard(shard, GameInput::PlayerJoin { player, position })?;

        let (tx, rx) = mpsc::channel(self.outbound_capacity);
        let entity_id = self.allocate_entity_id();
        self.players.insert(
            player,
            SessionEntry {
                shard,
                outbound: tx,
                name: name.to_owned(),
                entity_id,
                position,
            },
        );
        self.broadcast_join_visibility(player, position);
        Ok(PlayerSessionHandle {
            player,
            shard,
            outbound: rx,
        })
    }

    /// Allocates the next network entity id, advancing the counter.
    fn allocate_entity_id(&mut self) -> i32 {
        let id = self.next_entity_id;
        // Wrap rather than overflow-panic; a process churning through 2^31 joins
        // is not a real concern, but a panic in the router would be.
        self.next_entity_id = self.next_entity_id.wrapping_add(1);
        id
    }

    /// Exchanges player-list-add + spawn packets between the joiner and every
    /// existing player within view distance.
    ///
    /// Each in-range existing player is told about the joiner, and the joiner is
    /// told about each in-range existing player, so visibility is symmetric after
    /// a single join. All sends are best-effort (see [`join_player`](Self::join_player)).
    fn broadcast_join_visibility(&self, joiner: PlayerId, joiner_position: Vec3) {
        let Some(joiner_entry) = self.players.get(&joiner) else {
            return;
        };
        let joiner_chunk = chunk_for_position(joiner_position);
        for (&other, other_entry) in &self.players {
            if other == joiner {
                continue;
            }
            if !within_view(
                joiner_chunk,
                chunk_for_position(other_entry.position),
                self.view_distance,
            ) {
                continue;
            }
            // Show the joiner to the existing player.
            let _ = other_entry
                .outbound
                .try_send(player_info_add(joiner, &joiner_entry.name));
            let _ = other_entry.outbound.try_send(entity_spawn_shell(
                joiner_entry.entity_id,
                joiner,
                joiner_position,
            ));
            // Show the existing player to the joiner.
            let _ = joiner_entry
                .outbound
                .try_send(player_info_add(other, &other_entry.name));
            let _ = joiner_entry.outbound.try_send(entity_spawn_shell(
                other_entry.entity_id,
                other,
                other_entry.position,
            ));
        }
    }

    /// Translates and routes a [`NetEvent`] to the player's shard.
    ///
    /// A [`NetEvent::Disconnected`] runs the full [disconnect
    /// cleanup](Self::disconnect_player). A [`NetEvent::Play`] is translated with
    /// [`net_event_to_input`](crate::net_event_to_input); events with no
    /// simulation effect (keep-alives and the like) are a no-op `Ok(())`.
    ///
    /// # Errors
    ///
    /// - [`SessionError::UnknownPlayer`] if the event targets a player with no
    ///   session.
    /// - [`SessionError::ShardInboxFull`] / [`SessionError::ShardClosed`] if the
    ///   input could not be delivered to the shard.
    pub fn route_event(&mut self, event: &NetEvent) -> Result<(), SessionError> {
        match event {
            NetEvent::Disconnected { player, .. } => self.disconnect_player(*player).map(|_| ()),
            NetEvent::Play { player, packet } => match play_packet_to_input(*player, packet) {
                Some(input) => self.route_input(*player, input),
                None => Ok(()),
            },
        }
    }

    /// Routes a simulation [`GameOutput`] to its network recipient(s), returning
    /// the players whose connection has closed and should be disconnected.
    ///
    /// The mapping per output:
    /// - [`GameOutput::PlayerSpawned`] refreshes the player's cached position;
    ///   the spawn itself was already broadcast to viewers at
    ///   [`join_player`](Self::join_player) time, so nothing else is sent.
    /// - [`GameOutput::PlayerMoved`] broadcasts the move to every *other* player
    ///   within view distance — a relative `UpdateEntityPosition` for a step of at
    ///   most 8 blocks, an absolute `EntityTeleport` for a larger jump — then
    ///   refreshes the cached position (the mover is authoritative for its own
    ///   position and is not echoed). The broadcast runs first so the delta is
    ///   measured against the previous position.
    /// - [`GameOutput::PlayerPositionCorrected`] snaps the player itself back to
    ///   the authoritative position; it is not a broadcast.
    /// - [`GameOutput::BlockChanged`] broadcasts a `BlockUpdate` to every
    ///   player whose chunk is within view distance of the changed block's
    ///   chunk (the actor is included — the change is authoritative for everyone
    ///   who can see it) and, for a [`MutationCause::PlayerCreative`] edit, also
    ///   sends the acting player alone an `AcknowledgeBlockChange` echoing the
    ///   sequence.
    /// - [`GameOutput::BlockChangeRejected`] sends only the acting player a
    ///   `BlockUpdate` carrying the authoritative state followed by an
    ///   `AcknowledgeBlockChange` echoing the rejected sequence: the `BlockUpdate`
    ///   sets the client's known server state and the ack ends its pending
    ///   prediction so the ghost block reverts. The ack is what actually heals a
    ///   real client (a `BlockUpdate` alone is swallowed while a prediction is
    ///   pending), so it is mandatory on reject as on accept. Never a broadcast,
    ///   since viewers never saw the predicted change.
    /// - [`GameOutput::PlayerDespawned`] (and any future variant) sends nothing:
    ///   leave notifications are issued by
    ///   [`disconnect_player`](Self::disconnect_player), where the departing
    ///   player's identity is still known.
    ///
    /// # Backpressure
    ///
    /// Position broadcasts and corrections are lossy under backpressure: a
    /// recipient whose outbound channel is *full* misses this update, which the
    /// next move/correction supersedes (so the router never blocks the tick
    /// loop). A recipient whose channel is *closed* is returned for the caller to
    /// disconnect.
    pub fn route_output(&mut self, output: &GameOutput) -> Vec<PlayerId> {
        let mut closed = Vec::new();
        match output {
            GameOutput::PlayerSpawned { player, position } => {
                self.update_position(*player, *position);
            }
            GameOutput::PlayerMoved { player, position } => {
                // Broadcast BEFORE refreshing the cached position: the move is
                // relative to the *last* position, so the delta must be computed
                // against the old value still held in the entry. Updating first
                // would zero every delta.
                self.broadcast_move(*player, *position, &mut closed);
                self.update_position(*player, *position);
            }
            GameOutput::PlayerPositionCorrected { player, position } => {
                if let Some(entry) = self.players.get(player) {
                    match entry.outbound.try_send(move_shell(*position)) {
                        Ok(()) | Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Closed(_)) => closed.push(*player),
                    }
                }
            }
            GameOutput::BlockChanged {
                position,
                state,
                sequence,
                cause,
            } => {
                self.broadcast_block_update(*position, *state, &mut closed);
                // Acknowledge the sequence to the acting player alone so its
                // client-side prediction is confirmed. Only a player edit has an
                // actor to ack; other causes broadcast to viewers but ack no one.
                if let MutationCause::PlayerCreative { player } = cause {
                    if let Some(entry) = self.players.get(player) {
                        match entry.outbound.try_send(ack_shell(*sequence)) {
                            Ok(()) | Err(TrySendError::Full(_)) => {}
                            Err(TrySendError::Closed(_)) => closed.push(*player),
                        }
                    }
                }
            }
            GameOutput::BlockChangeRejected {
                player,
                position,
                sequence,
                authoritative_state,
                ..
            } => {
                // Heal only the actor; viewers never saw the predicted change. The
                // `BlockUpdate` sets the client's known server state at the block
                // and the `AcknowledgeBlockChange` then ends its pending prediction
                // so that authoritative state is displayed. On a real 1.21.8 client
                // the ack is what actually reverts the ghost block
                // (endPredictionsUpTo); a `BlockUpdate` alone is swallowed while the
                // prediction is pending, so the ack is mandatory here too.
                if let Some(entry) = self.players.get(player) {
                    match entry
                        .outbound
                        .try_send(block_update_shell(*position, *authoritative_state))
                    {
                        // The resync queued (or was dropped under backpressure); the
                        // ack must still follow to end the client's prediction.
                        Ok(()) | Err(TrySendError::Full(_)) => {
                            match entry.outbound.try_send(ack_shell(*sequence)) {
                                Ok(()) | Err(TrySendError::Full(_)) => {}
                                Err(TrySendError::Closed(_)) => closed.push(*player),
                            }
                        }
                        Err(TrySendError::Closed(_)) => closed.push(*player),
                    }
                }
            }
            // A despawn carries no wire packet here; leave handling lives in
            // disconnect_player.
            _ => {}
        }
        closed
    }

    /// Refreshes the cached position the router routes visibility against.
    fn update_position(&mut self, player: PlayerId, position: Vec3) {
        if let Some(entry) = self.players.get_mut(&player) {
            entry.position = position;
        }
    }

    /// Broadcasts `mover`'s new `position` to every other player within view
    /// distance, recording any closed recipients in `closed`.
    ///
    /// The carrier is chosen by distance from the mover's *previous* (cached)
    /// position: a relative [`update_entity_position_shell`] for a step within
    /// [`MAX_RELATIVE_MOVE_DISTANCE_SQ`], otherwise an absolute
    /// [`entity_teleport_shell`]. The same packet is sent to every in-range
    /// viewer (built once, cloned per send). This must run before the cached
    /// position is refreshed, or the relative delta would be zero.
    fn broadcast_move(&self, mover: PlayerId, position: Vec3, closed: &mut Vec<PlayerId>) {
        let Some(mover_entry) = self.players.get(&mover) else {
            return;
        };
        let mover_chunk = chunk_for_position(position);
        let last = mover_entry.position;
        let (dx, dy, dz) = (
            position.x - last.x,
            position.y - last.y,
            position.z - last.z,
        );
        let packet = if dx * dx + dy * dy + dz * dz <= MAX_RELATIVE_MOVE_DISTANCE_SQ {
            update_entity_position_shell(mover_entry.entity_id, dx, dy, dz)
        } else {
            entity_teleport_shell(mover_entry.entity_id, position)
        };
        for (&other, entry) in &self.players {
            if other == mover
                || !within_view(
                    mover_chunk,
                    chunk_for_position(entry.position),
                    self.view_distance,
                )
            {
                continue;
            }
            match entry.outbound.try_send(packet.clone()) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Closed(_)) => closed.push(other),
            }
        }
    }

    /// Broadcasts a block change at `position` to every player whose chunk is
    /// within view distance of the block's chunk, recording closed recipients.
    ///
    /// Unlike a movement broadcast there is no actor to exclude: the change is
    /// authoritative for everyone who can see the block, including the player who
    /// caused it. Sends are non-blocking; a recipient whose channel is *full*
    /// simply misses this update (the simulation still holds the correct world
    /// state) while a *closed* recipient is recorded in `closed` for the caller
    /// to disconnect.
    fn broadcast_block_update(
        &self,
        position: BlockPos,
        state: BlockStateId,
        closed: &mut Vec<PlayerId>,
    ) {
        let block_chunk = position.to_chunk_pos();
        for (&player, entry) in &self.players {
            if !within_view(
                block_chunk,
                chunk_for_position(entry.position),
                self.view_distance,
            ) {
                continue;
            }
            match entry.outbound.try_send(block_update_shell(position, state)) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Closed(_)) => closed.push(player),
            }
        }
    }

    /// Disconnects `player`: removes them from the player list of every remaining
    /// player, drops the player<->shard mapping, and notifies the shard to
    /// despawn them. Returns the shard the player was on.
    ///
    /// The mapping is removed first — cleanup is the priority — but the departed
    /// entity id is captured beforehand so the despawn can still be addressed.
    /// Both a player-list remove and an entity despawn are then broadcast to every
    /// other player (best-effort; the tab list is not range-scoped) while the
    /// departing identity is still known, and only then is the despawn
    /// [`GameInput::PlayerLeave`] sent. A shard send failure is surfaced so the
    /// caller knows the despawn notice was lost, but the mapping stays removed
    /// regardless.
    ///
    /// # Errors
    ///
    /// - [`SessionError::UnknownPlayer`] if `player` had no session.
    /// - [`SessionError::ShardInboxFull`] / [`SessionError::ShardClosed`] if the
    ///   despawn could not be delivered (mapping already removed).
    pub fn disconnect_player(&mut self, player: PlayerId) -> Result<ShardPos, SessionError> {
        let entry = self
            .players
            .remove(&player)
            .ok_or(SessionError::UnknownPlayer { player })?;
        self.broadcast_leave_visibility(player, entry.entity_id);
        self.send_to_shard(entry.shard, GameInput::PlayerLeave { player })?;
        Ok(entry.shard)
    }

    /// Tells every remaining player to drop `departed` (whose network id is
    /// `entity_id`) from both their tab list and their world.
    ///
    /// Sends two packets per viewer: a [`player_info_remove`] (Player Info
    /// Remove, `0x3E`) to clear the tab-list entry, and a
    /// [`remove_entities_shell`] (Remove Entities, `0x46`) to despawn the entity
    /// so it does not linger as a ghost. Best-effort, like the join broadcast: a
    /// viewer that cannot receive it simply keeps a stale entry until it too
    /// leaves.
    fn broadcast_leave_visibility(&self, departed: PlayerId, entity_id: i32) {
        for entry in self.players.values() {
            let _ = entry.outbound.try_send(player_info_remove(departed));
            let _ = entry.outbound.try_send(remove_entities_shell(&[entity_id]));
        }
    }

    /// Broadcasts a System Chat Message carrying `component` to **every**
    /// connected player (including the sender, matching vanilla chat relay).
    ///
    /// `overlay = true` renders the message above the hotbar (action bar);
    /// `overlay = false` renders it in the chat box. The packet is built once via
    /// [`crate::system_chat`] and a clone is delivered to each player's outbound
    /// channel.
    ///
    /// # Backpressure
    ///
    /// Best-effort and lossy, like the visibility broadcasts: a recipient whose
    /// outbound channel is *full* or *closed* simply misses this message (a closed
    /// channel is cleaned up when that connection's own loop ends). A dropped chat
    /// line is never worth stalling the driver, so this never blocks and never
    /// fails.
    pub fn broadcast_system_chat(&self, component: &TextComponent, overlay: bool) {
        let packet = crate::system_chat(component, overlay);
        for entry in self.players.values() {
            let _ = entry.outbound.try_send(packet.clone());
        }
    }

    /// Routes an already-translated input to the shard that should apply it.
    ///
    /// A block edit is routed by the *block's* chunk (see
    /// [`shard_for_block`](Self::shard_for_block)); every other input routes to
    /// the player's bound shard.
    fn route_input(&self, player: PlayerId, input: GameInput) -> Result<(), SessionError> {
        let entry = self
            .players
            .get(&player)
            .ok_or(SessionError::UnknownPlayer { player })?;
        let shard = match &input {
            GameInput::BlockBreak { position, .. } | GameInput::BlockPlace { position, .. } => {
                self.shard_for_block(*position, entry.shard)
            }
            _ => entry.shard,
        };
        self.send_to_shard(shard, input)
    }

    /// Resolves the shard that should apply a block edit at `position`.
    ///
    /// The minimal multi-shard seam: a block edit belongs to the shard owning the
    /// block's *chunk*, not to the acting player's bound shard — the two diverge
    /// once view distance exceeds a shard's 8-chunk span and a player edits a
    /// block another shard owns. When that owning shard is registered it wins;
    /// otherwise the edit falls back to the player's bound shard (`fallback`). In
    /// this single-shard milestone both resolve to the same shard, so routing is
    /// unchanged — the seam only positions the router for real multi-shard later.
    fn shard_for_block(&self, position: BlockPos, fallback: ShardPos) -> ShardPos {
        let owner = position.to_chunk_pos().to_shard_pos();
        if self.shards.contains_key(&owner) {
            owner
        } else {
            fallback
        }
    }

    /// Non-blocking send of `input` to `shard`'s input channel.
    fn send_to_shard(&self, shard: ShardPos, input: GameInput) -> Result<(), SessionError> {
        let sender = self
            .shards
            .get(&shard)
            .ok_or(SessionError::UnknownShard { shard })?;
        sender.try_send(input).map_err(|err| match err {
            TrySendError::Full(_) => SessionError::ShardInboxFull { shard },
            TrySendError::Closed(_) => SessionError::ShardClosed { shard },
        })
    }
}

/// Returns `true` if chunks `a` and `b` are within `view_distance` chunks on
/// both axes (a square/Chebyshev view, as Minecraft scopes view distance).
fn within_view(a: ChunkPos, b: ChunkPos, view_distance: i32) -> bool {
    let dx = (a.x() - b.x()).abs();
    let dz = (a.z() - b.z()).abs();
    dx.max(dz) <= view_distance
}

impl Default for SessionRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    // Decoded shell coordinates are exact, representable values, so exact float
    // comparison is intentional in these assertions.
    #![allow(clippy::float_cmp)]

    use ferrumc_net::DisconnectReason;
    use ferrumc_proto::generated::play::{ServerboundKeepAlive, ServerboundPlayPacket};

    use super::*;
    use crate::translate::PLAYER_INFO_ADD;

    fn player(name: &str) -> PlayerId {
        PlayerId::offline(name)
    }

    /// Spawn position inside shard (0, 0): chunk 0, well within blocks 0..128.
    fn spawn_pos() -> Vec3 {
        Vec3::new(8.0, 64.0, 8.0)
    }

    #[test]
    fn join_routes_player_to_a_shard() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("alice");

        let handle = router.join_player(p, "alice", spawn_pos()).expect("join");
        assert_eq!(handle.player(), p);
        assert_eq!(handle.shard(), ShardPos::new(0, 0));

        // The mapping records the player on the shard.
        assert!(router.is_player_connected(p));
        assert_eq!(router.player_shard(p), Some(ShardPos::new(0, 0)));
        assert_eq!(router.player_count(), 1);

        // The shard's inbox received exactly the PlayerJoin input.
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin {
                player: p,
                position: spawn_pos(),
            })
        );
    }

    #[test]
    fn join_without_a_shard_is_rejected() {
        let mut router = SessionRouter::new();
        let err = router
            .join_player(player("bob"), "bob", spawn_pos())
            .expect_err("no shard registered");
        assert_eq!(
            err,
            SessionError::UnknownShard {
                shard: ShardPos::new(0, 0)
            }
        );
        assert_eq!(router.player_count(), 0);
    }

    #[test]
    fn duplicate_join_is_rejected_without_touching_state() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("carol");

        let _handle = router
            .join_player(p, "carol", spawn_pos())
            .expect("first join");
        let _ = inbox.try_recv();

        let err = router
            .join_player(p, "carol", spawn_pos())
            .expect_err("already joined");
        assert_eq!(err, SessionError::DuplicatePlayer { player: p });
        // No second PlayerJoin was sent.
        assert!(inbox.try_recv().is_err());
    }

    #[test]
    fn movement_event_routes_to_the_single_shard_as_player_move() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("dave");
        let _handle = router.join_player(p, "dave", spawn_pos()).expect("join");
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin {
                player: p,
                position: spawn_pos(),
            })
        );

        // A movement packet far inside the same shard's region.
        let move_event = NetEvent::play(
            p,
            ServerboundPlayPacket::SetPlayerPosition(
                ferrumc_proto::generated::play::SetPlayerPosition::new(20.0, 64.0, 20.0, 0),
            ),
        );
        router.route_event(&move_event).expect("route move");
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::PlayerMove {
                player: p,
                position: Vec3::new(20.0, 64.0, 20.0),
            })
        );
    }

    #[test]
    fn keep_alive_event_is_a_no_op() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("erin");
        let _handle = router.join_player(p, "erin", spawn_pos()).expect("join");
        let _ = inbox.try_recv();

        router
            .route_event(&NetEvent::play(
                p,
                ServerboundPlayPacket::ServerboundKeepAlive(ServerboundKeepAlive::new(1)),
            ))
            .expect("route keep-alive");
        // Nothing reached the shard.
        assert!(inbox.try_recv().is_err());
    }

    #[test]
    fn play_event_for_unknown_player_is_rejected() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("ghost");
        let err = router
            .route_event(&NetEvent::play(
                p,
                ServerboundPlayPacket::SetPlayerPosition(
                    ferrumc_proto::generated::play::SetPlayerPosition::new(0.0, 0.0, 0.0, 0),
                ),
            ))
            .expect_err("no session");
        assert_eq!(err, SessionError::UnknownPlayer { player: p });
    }

    #[test]
    fn own_spawn_and_move_are_not_echoed_to_self() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("frank");
        let mut handle = router.join_player(p, "frank", spawn_pos()).expect("join");

        // A lone player is never shown their own entity, and an accepted move is
        // authoritative client-side: neither is echoed back.
        assert!(router
            .route_output(&GameOutput::PlayerSpawned {
                player: p,
                position: spawn_pos(),
            })
            .is_empty());
        assert!(router
            .route_output(&GameOutput::PlayerMoved {
                player: p,
                position: Vec3::new(4.0, 5.0, 6.0),
            })
            .is_empty());
        assert!(handle.try_recv().is_none());
        // The move still refreshed the cached position used for routing.
        assert_eq!(router.player_position(p), Some(Vec3::new(4.0, 5.0, 6.0)));
    }

    #[test]
    fn correction_output_sends_a_synchronize_position_packet() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("nora");
        let mut handle = router.join_player(p, "nora", spawn_pos()).expect("join");

        let closed = router.route_output(&GameOutput::PlayerPositionCorrected {
            player: p,
            position: Vec3::new(8.0, 64.0, 8.0),
        });
        assert!(closed.is_empty());
        let ClientboundPlayPacket::SynchronizePlayerPosition(sync) =
            handle.try_recv().expect("a sync packet")
        else {
            panic!("expected a SynchronizePlayerPosition packet");
        };
        assert_eq!((sync.x(), sync.y(), sync.z()), (8.0, 64.0, 8.0));
    }

    #[test]
    fn despawn_output_sends_nothing() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("grace");
        let mut handle = router.join_player(p, "grace", spawn_pos()).expect("join");

        let closed = router.route_output(&GameOutput::PlayerDespawned { player: p });
        assert!(closed.is_empty());
        assert!(handle.try_recv().is_none());
    }

    #[test]
    fn output_for_unknown_player_is_a_no_op() {
        let mut router = SessionRouter::new();
        let closed = router.route_output(&GameOutput::PlayerMoved {
            player: player("nobody"),
            position: Vec3::ZERO,
        });
        assert!(closed.is_empty());
    }

    #[test]
    fn disconnect_cleans_up_the_mapping_and_despawns() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("heidi");
        let _handle = router.join_player(p, "heidi", spawn_pos()).expect("join");
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::PlayerJoin {
                player: p,
                position: spawn_pos(),
            })
        );
        assert!(router.is_player_connected(p));

        let shard = router.disconnect_player(p).expect("disconnect");
        assert_eq!(shard, ShardPos::new(0, 0));

        // Mapping is gone.
        assert!(!router.is_player_connected(p));
        assert_eq!(router.player_shard(p), None);
        assert_eq!(router.player_count(), 0);

        // The shard was told to despawn the player.
        assert_eq!(inbox.try_recv(), Ok(GameInput::PlayerLeave { player: p }));
    }

    #[test]
    fn disconnect_event_routes_through_cleanup() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("ivan");
        let _handle = router.join_player(p, "ivan", spawn_pos()).expect("join");
        let _ = inbox.try_recv();

        router
            .route_event(&NetEvent::disconnected(p, DisconnectReason::ServerShutdown))
            .expect("route disconnect");
        assert!(!router.is_player_connected(p));
        assert_eq!(inbox.try_recv(), Ok(GameInput::PlayerLeave { player: p }));
    }

    #[test]
    fn disconnect_unknown_player_is_rejected() {
        let mut router = SessionRouter::new();
        let err = router
            .disconnect_player(player("nobody"))
            .expect_err("no session");
        assert_eq!(
            err,
            SessionError::UnknownPlayer {
                player: player("nobody")
            }
        );
    }

    #[test]
    fn shard_inbox_full_is_classified_reject_backpressure() {
        // Capacity 1: the join fills the channel, the next input is rejected.
        let mut router = SessionRouter::with_capacities(1, 16);
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("judy");
        let _handle = router.join_player(p, "judy", spawn_pos()).expect("join");

        // The channel now holds the join; a movement input overflows it.
        let err = router
            .route_event(&NetEvent::play(
                p,
                ServerboundPlayPacket::SetPlayerPosition(
                    ferrumc_proto::generated::play::SetPlayerPosition::new(1.0, 1.0, 1.0, 0),
                ),
            ))
            .expect_err("inbox full");
        assert_eq!(
            err,
            SessionError::ShardInboxFull {
                shard: ShardPos::new(0, 0)
            }
        );
    }

    #[test]
    fn broadcast_move_is_lossy_when_a_viewer_is_full() {
        // A capacity-1 outbound channel: the viewer's join visibility already
        // fills it, so a later move broadcast is dropped (lossy backpressure)
        // without reporting the viewer as closed.
        let mut router = SessionRouter::with_capacities(16, 1);
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let viewer = player("kate");
        let mover = player("kurt");
        let _viewer_handle = router
            .join_player(viewer, "kate", spawn_pos())
            .expect("viewer join");
        let _mover_handle = router
            .join_player(mover, "kurt", spawn_pos())
            .expect("mover join");

        let closed = router.route_output(&GameOutput::PlayerMoved {
            player: mover,
            position: Vec3::new(9.0, 64.0, 9.0),
        });
        // The viewer is full, not closed: nothing to disconnect.
        assert!(closed.is_empty());
    }

    #[test]
    fn closed_viewer_is_reported_for_disconnect() {
        // Two players in range. If a viewer drops its handle, a broadcast move
        // reports it as closed so the caller can disconnect it.
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let viewer = player("leo");
        let mover = player("mona");
        let viewer_handle = router
            .join_player(viewer, "leo", spawn_pos())
            .expect("viewer join");
        let _mover_handle = router
            .join_player(mover, "mona", spawn_pos())
            .expect("mover join");
        drop(viewer_handle);

        let closed = router.route_output(&GameOutput::PlayerMoved {
            player: mover,
            position: Vec3::new(9.0, 64.0, 9.0),
        });
        assert_eq!(closed, vec![viewer]);
    }

    #[test]
    fn defaults_match_documented_capacities() {
        let router = SessionRouter::default();
        assert_eq!(router.shard_input_capacity(), DEFAULT_SHARD_INPUT_CAPACITY);
        assert_eq!(router.outbound_capacity(), DEFAULT_OUTBOUND_CAPACITY);
        assert_eq!(router.shard_count(), 0);
        assert_eq!(router.player_count(), 0);
    }

    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let router = SessionRouter::with_capacities(0, 0);
        assert_eq!(router.shard_input_capacity(), 1);
        assert_eq!(router.outbound_capacity(), 1);
    }

    #[tokio::test]
    async fn handle_recv_awaits_a_routed_correction() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let p = player("mallory");
        let mut handle = router.join_player(p, "mallory", spawn_pos()).expect("join");

        let closed = router.route_output(&GameOutput::PlayerPositionCorrected {
            player: p,
            position: Vec3::new(7.0, 8.0, 9.0),
        });
        assert!(closed.is_empty());

        let packet = handle.recv().await.expect("a packet");
        let ClientboundPlayPacket::SynchronizePlayerPosition(sync) = packet else {
            panic!("expected a SynchronizePlayerPosition packet");
        };
        assert_eq!((sync.x(), sync.y(), sync.z()), (7.0, 8.0, 9.0));
    }

    #[test]
    fn two_players_see_each_other_on_join() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let a = player("aaa");
        let b = player("bbb");

        let mut a_handle = router.join_player(a, "aaa", spawn_pos()).expect("a join");
        // `a` is alone: no visibility packets yet.
        assert!(a_handle.try_recv().is_none());

        let mut b_handle = router.join_player(b, "bbb", spawn_pos()).expect("b join");

        let a_eid = router.player_entity_id(a).expect("a entity id");
        let b_eid = router.player_entity_id(b).expect("b entity id");
        // Distinct ids per player, both clear of the local-player id (1).
        assert_ne!(a_eid, b_eid);
        assert!(a_eid >= FIRST_ENTITY_ID && b_eid >= FIRST_ENTITY_ID);

        // `a` learns about `b`; `b` learns about `a` (list add + entity spawn).
        assert_player_info_add(&mut a_handle, b);
        assert_entity_spawn(&mut a_handle, b, b_eid, spawn_pos());
        assert_player_info_add(&mut b_handle, a);
        assert_entity_spawn(&mut b_handle, a, a_eid, spawn_pos());

        // Both joins reached the shard.
        assert!(
            matches!(inbox.try_recv(), Ok(GameInput::PlayerJoin { player, .. }) if player == a)
        );
        assert!(
            matches!(inbox.try_recv(), Ok(GameInput::PlayerJoin { player, .. }) if player == b)
        );
    }

    #[test]
    fn system_chat_reaches_every_connected_player() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let a = player("aaa");
        let b = player("bbb");
        let mut a_handle = router.join_player(a, "aaa", spawn_pos()).expect("a join");
        let mut b_handle = router.join_player(b, "bbb", spawn_pos()).expect("b join");
        // Drain the mutual join-visibility packets so only the chat remains.
        let a_eid = router.player_entity_id(a).expect("a entity id");
        let b_eid = router.player_entity_id(b).expect("b entity id");
        assert_player_info_add(&mut a_handle, b);
        assert_entity_spawn(&mut a_handle, b, b_eid, spawn_pos());
        assert_player_info_add(&mut b_handle, a);
        assert_entity_spawn(&mut b_handle, a, a_eid, spawn_pos());

        let message = TextComponent::text("<aaa> hi everyone");
        router.broadcast_system_chat(&message, false);

        // Both players (the sender included) receive the same chat-box SystemChat.
        for handle in [&mut a_handle, &mut b_handle] {
            let ClientboundPlayPacket::SystemChat(chat) =
                handle.try_recv().expect("a system chat packet")
            else {
                panic!("expected a SystemChat");
            };
            assert!(
                !chat.overlay(),
                "chat relay targets the chat box, not the action bar"
            );
        }
    }

    #[test]
    fn small_move_broadcasts_as_relative_update() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let viewer = player("viewer");
        let mover = player("mover");
        let mut viewer_handle = router
            .join_player(viewer, "viewer", spawn_pos())
            .expect("viewer join");
        let _mover_handle = router
            .join_player(mover, "mover", spawn_pos())
            .expect("mover join");
        let mover_eid = router.player_entity_id(mover).expect("mover entity id");
        // Drain the join-visibility packets the viewer already received.
        assert_player_info_add(&mut viewer_handle, mover);
        assert_entity_spawn(&mut viewer_handle, mover, mover_eid, spawn_pos());

        // A step of (2, 0, 1) from spawn — within 8 blocks — is a relative move.
        let closed = router.route_output(&GameOutput::PlayerMoved {
            player: mover,
            position: Vec3::new(10.0, 64.0, 9.0),
        });
        assert!(closed.is_empty());
        let ClientboundPlayPacket::UpdateEntityPosition(rel) =
            viewer_handle.try_recv().expect("a relative move")
        else {
            panic!("expected an UpdateEntityPosition for a small move");
        };
        assert_eq!(rel.entity_id(), mover_eid);
        // Deltas are `(new - last) * 4096`: dx = 2, dy = 0, dz = 1.
        assert_eq!(
            (rel.delta_x(), rel.delta_y(), rel.delta_z()),
            (8192, 0, 4096)
        );
    }

    #[test]
    fn large_move_broadcasts_as_teleport() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let viewer = player("viewer");
        let mover = player("mover");
        let mut viewer_handle = router
            .join_player(viewer, "viewer", spawn_pos())
            .expect("viewer join");
        let _mover_handle = router
            .join_player(mover, "mover", spawn_pos())
            .expect("mover join");
        let mover_eid = router.player_entity_id(mover).expect("mover entity id");
        // Drain the join-visibility packets the viewer already received.
        assert_player_info_add(&mut viewer_handle, mover);
        assert_entity_spawn(&mut viewer_handle, mover, mover_eid, spawn_pos());

        // A jump of (12, 0, 12) from spawn — over 8 blocks — teleports absolutely.
        let new_pos = Vec3::new(20.0, 64.0, 20.0);
        let closed = router.route_output(&GameOutput::PlayerMoved {
            player: mover,
            position: new_pos,
        });
        assert!(closed.is_empty());
        let ClientboundPlayPacket::EntityTeleport(tp) =
            viewer_handle.try_recv().expect("a teleport")
        else {
            panic!("expected an EntityTeleport for a large jump");
        };
        assert_eq!(tp.entity_id(), mover_eid);
        assert_eq!((tp.x(), tp.y(), tp.z()), (new_pos.x, new_pos.y, new_pos.z));
    }

    #[test]
    fn far_viewer_is_excluded_by_view_distance() {
        let mut router = SessionRouter::new();
        router.set_view_distance(1);
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let near = player("near");
        let far = player("far");
        let mut near_handle = router
            .join_player(near, "near", spawn_pos())
            .expect("near join");
        // Block 80 -> chunk 5, still inside shard (0, 0) (blocks 0..128) but five
        // chunks from the spawn chunk: out of a view distance of one.
        let far_pos = Vec3::new(80.0, 64.0, 80.0);
        let mut far_handle = router.join_player(far, "far", far_pos).expect("far join");

        // Out of range on join: neither learns about the other.
        assert!(near_handle.try_recv().is_none());
        assert!(far_handle.try_recv().is_none());

        // A far move stays out of range and is not broadcast to the near player.
        let closed = router.route_output(&GameOutput::PlayerMoved {
            player: far,
            position: Vec3::new(81.0, 64.0, 81.0),
        });
        assert!(closed.is_empty());
        assert!(near_handle.try_recv().is_none());
    }

    #[test]
    fn block_change_broadcasts_block_update_to_in_range_viewer() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let viewer = player("viewer");
        let mut viewer_handle = router
            .join_player(viewer, "viewer", spawn_pos())
            .expect("viewer join");

        // A change in the viewer's chunk reaches them as a BlockUpdate (the lone
        // player got no join-visibility packets, so this is the first one). A
        // command cause has no actor, so no ack competes with the broadcast.
        let closed = router.route_output(&GameOutput::BlockChanged {
            position: BlockPos::new(8, 63, 8),
            state: BlockStateId::AIR,
            sequence: 0,
            cause: MutationCause::Command,
        });
        assert!(closed.is_empty());
        let ClientboundPlayPacket::BlockUpdate(update) =
            viewer_handle.try_recv().expect("a block update")
        else {
            panic!("expected a BlockUpdate");
        };
        let loc = update.location();
        assert_eq!((loc.x(), loc.y(), loc.z()), (8, 63, 8));
        assert_eq!(update.block_state(), 0);
    }

    #[test]
    fn block_change_excludes_a_far_viewer() {
        let mut router = SessionRouter::new();
        router.set_view_distance(1);
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let viewer = player("viewer");
        let mut viewer_handle = router
            .join_player(viewer, "viewer", spawn_pos())
            .expect("viewer join");

        // Block (88, 63, 8) is in chunk x = 5, five chunks from the spawn chunk:
        // out of a view distance of one, so nothing is broadcast.
        let closed = router.route_output(&GameOutput::BlockChanged {
            position: BlockPos::new(88, 63, 8),
            state: BlockStateId::AIR,
            sequence: 0,
            cause: MutationCause::Command,
        });
        assert!(closed.is_empty());
        assert!(viewer_handle.try_recv().is_none());
    }

    #[test]
    fn block_change_reports_a_closed_viewer() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let viewer = player("viewer");
        let handle = router
            .join_player(viewer, "viewer", spawn_pos())
            .expect("viewer join");
        // Dropping the handle closes the outbound channel.
        drop(handle);

        let closed = router.route_output(&GameOutput::BlockChanged {
            position: BlockPos::new(8, 63, 8),
            state: BlockStateId::AIR,
            sequence: 0,
            cause: MutationCause::Command,
        });
        assert_eq!(closed, vec![viewer]);
    }

    #[test]
    fn accepted_player_edit_acks_only_the_actor() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let actor = player("actor");
        let viewer = player("viewer");
        let mut actor_handle = router
            .join_player(actor, "actor", spawn_pos())
            .expect("actor join");
        let mut viewer_handle = router
            .join_player(viewer, "viewer", spawn_pos())
            .expect("viewer join");
        // Drain the mutual join-visibility packets so only the edit remains.
        let viewer_eid = router.player_entity_id(viewer).expect("viewer entity id");
        let actor_eid = router.player_entity_id(actor).expect("actor entity id");
        assert_player_info_add(&mut actor_handle, viewer);
        assert_entity_spawn(&mut actor_handle, viewer, viewer_eid, spawn_pos());
        assert_player_info_add(&mut viewer_handle, actor);
        assert_entity_spawn(&mut viewer_handle, actor, actor_eid, spawn_pos());

        let closed = router.route_output(&GameOutput::BlockChanged {
            position: BlockPos::new(8, 63, 8),
            state: BlockStateId::AIR,
            sequence: 55,
            cause: MutationCause::PlayerCreative { player: actor },
        });
        assert!(closed.is_empty());

        // The actor sees the authoritative BlockUpdate (it is in range too) and
        // then the AcknowledgeBlockChange echoing its sequence.
        let ClientboundPlayPacket::BlockUpdate(_) =
            actor_handle.try_recv().expect("actor block update")
        else {
            panic!("expected a BlockUpdate for the actor first");
        };
        let ClientboundPlayPacket::AcknowledgeBlockChange(ack) =
            actor_handle.try_recv().expect("actor ack")
        else {
            panic!("expected an AcknowledgeBlockChange for the actor");
        };
        assert_eq!(ack.sequence(), 55);

        // The viewer sees the broadcast BlockUpdate but never an ack.
        let ClientboundPlayPacket::BlockUpdate(_) =
            viewer_handle.try_recv().expect("viewer block update")
        else {
            panic!("expected a BlockUpdate for the viewer");
        };
        assert!(viewer_handle.try_recv().is_none());
    }

    #[test]
    fn rejected_player_edit_resyncs_only_the_actor() {
        let mut router = SessionRouter::new();
        let _inbox = router.register_shard(ShardPos::new(0, 0));
        let actor = player("actor");
        let viewer = player("viewer");
        let mut actor_handle = router
            .join_player(actor, "actor", spawn_pos())
            .expect("actor join");
        let mut viewer_handle = router
            .join_player(viewer, "viewer", spawn_pos())
            .expect("viewer join");
        // Drain the mutual join-visibility packets so only the resync remains.
        let viewer_eid = router.player_entity_id(viewer).expect("viewer entity id");
        let actor_eid = router.player_entity_id(actor).expect("actor entity id");
        assert_player_info_add(&mut actor_handle, viewer);
        assert_entity_spawn(&mut actor_handle, viewer, viewer_eid, spawn_pos());
        assert_player_info_add(&mut viewer_handle, actor);
        assert_entity_spawn(&mut viewer_handle, actor, actor_eid, spawn_pos());

        // A rejected break: only the actor is healed — first the authoritative
        // state, then the ack that ends its pending prediction (the ack is what
        // actually reverts the ghost block on a real client).
        let closed = router.route_output(&GameOutput::BlockChangeRejected {
            player: actor,
            position: BlockPos::new(8, 63, 8),
            sequence: 77,
            requested_state: BlockStateId::AIR,
            authoritative_state: BlockStateId::new(1),
        });
        assert!(closed.is_empty());

        let ClientboundPlayPacket::BlockUpdate(update) =
            actor_handle.try_recv().expect("actor resync")
        else {
            panic!("expected a BlockUpdate resync for the actor");
        };
        let loc = update.location();
        assert_eq!((loc.x(), loc.y(), loc.z()), (8, 63, 8));
        assert_eq!(update.block_state(), 1);
        // The ack follows the resync, echoing the rejected sequence so the client
        // ends its prediction and displays the authoritative state.
        let ClientboundPlayPacket::AcknowledgeBlockChange(ack) =
            actor_handle.try_recv().expect("actor ack")
        else {
            panic!("expected an AcknowledgeBlockChange after the resync");
        };
        assert_eq!(ack.sequence(), 77);
        assert!(actor_handle.try_recv().is_none());
        // The viewer never saw the predicted change, so it gets nothing.
        assert!(viewer_handle.try_recv().is_none());
    }

    #[test]
    fn disconnect_broadcasts_player_remove_and_entity_despawn() {
        let mut router = SessionRouter::new();
        let mut inbox = router.register_shard(ShardPos::new(0, 0));
        let stay = player("stay");
        let leave = player("leave");
        let mut stay_handle = router
            .join_player(stay, "stay", spawn_pos())
            .expect("stay join");
        let _leave_handle = router
            .join_player(leave, "leave", spawn_pos())
            .expect("leave join");
        let leave_eid = router.player_entity_id(leave).expect("leave entity id");
        // Drain the staying player's join visibility and the two shard joins.
        assert_player_info_add(&mut stay_handle, leave);
        assert_entity_spawn(&mut stay_handle, leave, leave_eid, spawn_pos());
        let _ = inbox.try_recv();
        let _ = inbox.try_recv();

        router.disconnect_player(leave).expect("disconnect");

        // The staying player is told to drop the leaver from the tab list, via the
        // dedicated Player Info Remove (0x3E) packet carrying the leaver's UUID.
        let ClientboundPlayPacket::RemovePlayerInfo(remove) =
            stay_handle.try_recv().expect("a player-remove packet")
        else {
            panic!("expected a RemovePlayerInfo");
        };
        assert_eq!(remove.players(), [leave.as_uuid()].as_slice());
        // ...and to despawn the leaver's entity so it does not linger as a ghost.
        let ClientboundPlayPacket::RemoveEntities(despawn) =
            stay_handle.try_recv().expect("a remove-entities packet")
        else {
            panic!("expected a RemoveEntities");
        };
        assert_eq!(despawn.entity_ids(), [leave_eid].as_slice());
        // The shard was told to despawn the leaver.
        assert_eq!(
            inbox.try_recv(),
            Ok(GameInput::PlayerLeave { player: leave })
        );
    }

    #[test]
    fn view_distance_default_and_setter() {
        let mut router = SessionRouter::new();
        assert_eq!(router.view_distance(), DEFAULT_VIEW_DISTANCE);
        router.set_view_distance(4);
        assert_eq!(router.view_distance(), 4);
        // A negative distance clamps to zero (chunk-mates still see each other).
        router.set_view_distance(-3);
        assert_eq!(router.view_distance(), 0);
    }

    /// Asserts the next packet on `handle` is a player-list add for `expected`.
    ///
    /// The Add Player body leads with a count byte (`1`) then the 16-byte UUID;
    /// the name / properties / listed fields follow (asserted in `translate.rs`).
    fn assert_player_info_add(handle: &mut PlayerSessionHandle, expected: PlayerId) {
        let ClientboundPlayPacket::PlayerInfoUpdate(info) =
            handle.try_recv().expect("a player-info packet")
        else {
            panic!("expected a PlayerInfoUpdate");
        };
        assert_eq!(info.action(), PLAYER_INFO_ADD);
        assert_eq!(info.entries()[0], 1);
        assert_eq!(&info.entries()[1..17], expected.as_uuid().as_bytes());
    }

    /// Asserts the next packet on `handle` is a spawn of `expected` with the
    /// given entity id at `pos`, rendered as the player entity type.
    fn assert_entity_spawn(
        handle: &mut PlayerSessionHandle,
        expected: PlayerId,
        eid: i32,
        pos: Vec3,
    ) {
        let ClientboundPlayPacket::SpawnEntity(spawn) = handle.try_recv().expect("a spawn packet")
        else {
            panic!("expected a SpawnEntity packet");
        };
        assert_eq!(spawn.entity_uuid(), expected.as_uuid());
        assert_eq!(spawn.entity_id(), eid);
        assert_eq!(
            spawn.entity_type(),
            149,
            "remote players spawn as minecraft:player"
        );
        assert_eq!((spawn.x(), spawn.y(), spawn.z()), (pos.x, pos.y, pos.z));
    }
}
