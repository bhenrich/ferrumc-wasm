//! A single simulation shard: bounded inbox in, outputs out, at tick
//! boundaries.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroUsize;

use ferrumc_core::{DimensionId, GameMode, PlayerId, WorldId};
use ferrumc_math::{BlockPos, Direction, ShardPos, Vec3};
use ferrumc_placement::{
    compute_fence_connection_state, compute_placement, NeighborQuery, PlacementContext,
    PlacementResult, PlacementRule,
};
use ferrumc_registry::block_state::{block_metadata, state_id_to_block_name};
use ferrumc_world::BlockStateId;

use crate::error::SimError;
use crate::loaded::LoadedChunkMap;
use crate::message::{GameInput, GameOutput};
use crate::mutation::{MutationCause, MutationResult, PendingMutation, RejectionReason};

/// Maximum absolute value allowed for any player position coordinate.
///
/// Mirrors the vanilla server's move-packet sanity bound: a client may not place
/// itself beyond +/-3.0e7 blocks on any axis (just past the maximum world
/// border). Anything larger — or non-finite — is a malformed or hostile position
/// and is rejected at the tick boundary rather than corrupting shard state.
const MAX_POSITION_MAGNITUDE: f64 = 3.0e7;

/// Returns `true` if `position` is a finite, in-range player position.
///
/// Rejects NaN, infinities, and any coordinate whose magnitude exceeds
/// [`MAX_POSITION_MAGNITUDE`]. This is the simulation's only movement check this
/// milestone: no collision, no speed limit, just finite/range sanity so a bad
/// packet can never poison player state.
fn is_valid_position(position: Vec3) -> bool {
    let in_range = |value: f64| value.is_finite() && value.abs() <= MAX_POSITION_MAGNITUDE;
    in_range(position.x) && in_range(position.y) && in_range(position.z)
}

/// Maximum distance, in blocks, between a player and a block they may break or
/// place.
///
/// Measured from the player's position to the centre of the target block. Set a
/// little above vanilla's ~4.5-block interaction range so creative-mode reach is
/// comfortably covered. This is the milestone's only interaction-range check:
/// there is no eye-height offset, line-of-sight, or per-gamemode tuning yet.
const MAX_REACH: f64 = 6.0;

/// A fixed `minecraft:stone` block-state (id `1` in the pinned flat-world
/// registry), used only by the shard's block-edit tests now that an accepted
/// place writes the held item's resolved state threaded through
/// [`GameInput::BlockPlace`] rather than a hardcoded default.
#[cfg(test)]
const DEFAULT_PLACED_STATE: BlockStateId = BlockStateId::new(1);

/// Returns `true` if the block at `block` is within [`MAX_REACH`] of an actor
/// positioned at `actor`.
///
/// Distance is measured from `actor` to the centre of the target block and
/// compared squared to avoid a square root. A non-finite actor position (which
/// movement validation already rejects before it can be stored) can never make
/// this return `true`, so it fails closed.
fn within_reach(actor: Vec3, block: BlockPos) -> bool {
    let centre = Vec3::new(
        f64::from(block.x()) + 0.5,
        f64::from(block.y()) + 0.5,
        f64::from(block.z()) + 0.5,
    );
    (actor - centre).length_squared() <= MAX_REACH * MAX_REACH
}

/// Builds a [`NonZeroUsize`] in const context, falling back to `1` for a zero
/// input.
const fn non_zero_usize(value: usize) -> NonZeroUsize {
    match NonZeroUsize::new(value) {
        Some(v) => v,
        None => NonZeroUsize::MIN,
    }
}

/// Default inbox capacity used by [`SimShard::new`].
///
/// 1024 queued inputs per shard is far above the per-tick volume a well-behaved
/// session router produces (a handful of inputs per player per tick), so
/// reaching it signals upstream misbehaviour or a stall — exactly when reject
/// backpressure should kick in.
const DEFAULT_INBOX_CAPACITY: NonZeroUsize = non_zero_usize(1024);

/// Default world a shard owns chunks for when none is specified.
///
/// The current milestone runs a single overworld shard, so [`SimShard::new`]
/// and friends default to world `0`. Use [`SimShard::in_dimension`] to place a
/// shard in an explicit world/dimension.
const DEFAULT_WORLD: WorldId = WorldId::new(0);

/// Default dimension a shard owns chunks for when none is specified (the
/// overworld, index `0`).
const DEFAULT_DIMENSION: DimensionId = DimensionId::new(0);

/// Upper bound on buffered [`PendingMutation`]s between drains.
///
/// The driver drains the buffer with [`SimShard::take_mutations`] every tick, and
/// at most one mutation is produced per queued block-edit input, so the buffer
/// never exceeds one inbox's worth in normal operation. This cap is a defensive
/// ceiling for the pathological case where the driver stalls without draining:
/// past it, new journal entries are dropped (the journal is best-effort and the
/// authoritative overlay still persists the block) rather than growing unbounded.
const MUTATION_LOG_CAP: usize = 4096;

/// Per-player state owned exclusively by the shard.
#[derive(Debug, Clone, Copy)]
struct PlayerState {
    position: Vec3,
    /// Body yaw in degrees, seeded to `0.0` on join and updated by a
    /// [`GameInput::PlayerMove`] carrying rotation. Broadcast to viewers so a
    /// remote player faces the right way instead of always facing north.
    yaw: f32,
    /// Pitch in degrees, seeded to `0.0` on join and updated by a
    /// [`GameInput::PlayerMove`] carrying rotation.
    pitch: f32,
    /// The authoritative server-side game mode. Seeded to [`GameMode::default`] on
    /// join and mutated by [`GameInput::SetGameMode`]; later milestones read it to
    /// enforce mode-specific rules (creative no-decrement, break speed, flight).
    game_mode: GameMode,
}

/// A movement coalesced within one tick: the latest valid position and/or
/// rotation a player's [`GameInput::PlayerMove`]s carried this tick.
///
/// Each field merges independently — a later input's `Some` component overwrites
/// an earlier one, while a `None` leaves the earlier value — so a position-only
/// move followed by a rotation-only move in the same tick applies both. A
/// `position` of `None` means no (valid) position arrived this tick, so the apply
/// pass emits a rotation-only [`GameOutput::PlayerMoved`].
#[derive(Debug, Clone, Copy, Default)]
struct PendingMove {
    position: Option<Vec3>,
    yaw: Option<f32>,
    pitch: Option<f32>,
}

/// One simulation shard.
///
/// A shard exclusively owns its players, a bounded inbox, and the chunks
/// resident for its world/dimension (a [`LoadedChunkMap`]). It applies queued
/// [`GameInput`]s **only** at tick boundaries and returns the resulting
/// [`GameOutput`]s. Entity ownership arrives in later milestones.
///
/// # Chunk ownership
///
/// The shard owns chunk *data* through [`loaded_chunks`](SimShard::loaded_chunks)
/// / [`loaded_chunks_mut`](SimShard::loaded_chunks_mut) but never a database
/// handle: chunk loading is driven by passing a borrowed
/// [`WorldStore`](ferrumc_storage::WorldStore) to the map's
/// [`acquire`](LoadedChunkMap::acquire). Which chunks are resident is governed
/// entirely by tickets.
///
/// # Tick-boundary application
///
/// [`enqueue`](SimShard::enqueue) only appends to the inbox; it never mutates
/// shard state. State changes happen exclusively inside
/// [`run_tick`](SimShard::run_tick), which drains the whole inbox in FIFO order.
/// An input enqueued after a `run_tick` returns is therefore applied at the
/// *next* tick, never mid-tick.
///
/// # Movement coalescing and validation
///
/// Multiple [`GameInput::PlayerMove`]s for the same player in one tick are
/// *coalesced*: only the latest valid position is applied at the boundary, and a
/// single [`GameOutput::PlayerMoved`] is emitted (overload handling step one —
/// coalesce movement). Coordinates are sanity-checked with [`is_valid_position`]:
/// a non-finite or out-of-range move is rejected without touching state, and if
/// no valid move supersedes it the shard emits a
/// [`GameOutput::PlayerPositionCorrected`] so the desynced client can snap back.
/// A move for an absent player is ignored, and a [`GameInput::PlayerLeave`]
/// cancels any pending move/correction for that player.
///
/// # Block edits
///
/// [`GameInput::BlockBreak`] and [`GameInput::BlockPlace`] mutate the resident
/// chunk at the tick boundary. Each is validated first (see
/// [`apply_block_edit`](SimShard::apply_block_edit)): the actor must be present,
/// the target chunk must be resident in this shard (which also pins the edit to
/// the shard's dimension), and the target must be within [`MAX_REACH`]. Reach is
/// measured against the actor's position *as of the start of the tick* — a move
/// queued in the same tick is coalesced and applied afterwards, so it does not
/// extend reach for an edit earlier in the same inbox. An accepted break writes
/// [`BlockStateId::AIR`]; an accepted place writes the held item's resolved
/// block-state, carried on [`GameInput::BlockPlace`].
/// Either way the owning section is marked dirty (by
/// [`Chunk::set_block`](ferrumc_world::Chunk::set_block)) and a single
/// [`GameOutput::BlockChanged`] is emitted, in inbox order, for the session layer
/// to broadcast and acknowledge. A rejected edit mutates nothing; if there is a
/// client to heal (an in-reach edit refused in a resident chunk) it emits a
/// [`GameOutput::BlockChangeRejected`] so the actor can resync, otherwise (absent
/// actor, unloaded chunk) it emits nothing.
///
/// # Backpressure
///
/// The inbox is bounded to a fixed capacity. When it is full,
/// [`enqueue`](SimShard::enqueue) returns [`SimError::InboxFull`] and leaves the
/// inbox untouched: it neither blocks (the shard runs on a sim worker that must
/// never stall) nor silently drops (that would desync clients). Deciding what to
/// do on rejection is the caller's responsibility.
///
/// # Determinism
///
/// Given the same starting state and the same sequence of enqueued inputs, a
/// shard produces an identical sequence of outputs. The inbox is strictly FIFO
/// and player state lives in an ordered [`BTreeMap`], so no iteration order or
/// hashing randomness can leak into results.
#[derive(Debug, Clone)]
pub struct SimShard {
    shard_pos: ShardPos,
    inbox: VecDeque<GameInput>,
    inbox_capacity: usize,
    players: BTreeMap<PlayerId, PlayerState>,
    chunks: LoadedChunkMap,
    /// Accepted gameplay mutations buffered for the storage journal, drained each
    /// tick by the driver. Bounded by [`MUTATION_LOG_CAP`].
    mutation_log: Vec<PendingMutation>,
}

impl SimShard {
    /// Creates an empty shard for `shard_pos` with the default inbox capacity in
    /// the default single overworld (world `0`, dimension `0`). Use
    /// [`in_dimension`](SimShard::in_dimension) to place the shard elsewhere.
    pub fn new(shard_pos: ShardPos) -> Self {
        Self::build(
            shard_pos,
            DEFAULT_WORLD,
            DEFAULT_DIMENSION,
            DEFAULT_INBOX_CAPACITY,
        )
    }

    /// Creates an empty shard for `shard_pos` with an explicit inbox `capacity`
    /// in the default world/dimension.
    ///
    /// `capacity` is a [`NonZeroUsize`] so a zero-capacity (permanently full)
    /// inbox is unrepresentable. The inbox pre-allocates this capacity once and
    /// never grows beyond it.
    pub fn with_inbox_capacity(shard_pos: ShardPos, capacity: NonZeroUsize) -> Self {
        Self::build(shard_pos, DEFAULT_WORLD, DEFAULT_DIMENSION, capacity)
    }

    /// Creates an empty shard for `shard_pos` owning chunks in an explicit
    /// `world` and `dimension`, with the default inbox capacity.
    pub fn in_dimension(shard_pos: ShardPos, world: WorldId, dimension: DimensionId) -> Self {
        Self::build(shard_pos, world, dimension, DEFAULT_INBOX_CAPACITY)
    }

    /// Shared constructor: builds a shard with every field initialized.
    fn build(
        shard_pos: ShardPos,
        world: WorldId,
        dimension: DimensionId,
        capacity: NonZeroUsize,
    ) -> Self {
        Self {
            shard_pos,
            inbox: VecDeque::with_capacity(capacity.get()),
            inbox_capacity: capacity.get(),
            players: BTreeMap::new(),
            chunks: LoadedChunkMap::new(world, dimension),
            mutation_log: Vec::new(),
        }
    }

    /// Returns the position of this shard in shard coordinates.
    pub const fn shard_pos(&self) -> ShardPos {
        self.shard_pos
    }

    /// Returns the chunks this shard currently owns in memory.
    pub const fn loaded_chunks(&self) -> &LoadedChunkMap {
        &self.chunks
    }

    /// Returns a mutable handle to the shard's chunks, used to acquire/release
    /// tickets and collect dirty chunks for saving.
    pub fn loaded_chunks_mut(&mut self) -> &mut LoadedChunkMap {
        &mut self.chunks
    }

    /// Returns the fixed inbox capacity.
    pub const fn inbox_capacity(&self) -> usize {
        self.inbox_capacity
    }

    /// Returns the number of inputs currently queued in the inbox.
    pub fn inbox_len(&self) -> usize {
        self.inbox.len()
    }

    /// Returns `true` if the inbox is at capacity and will reject new inputs.
    pub fn is_inbox_full(&self) -> bool {
        self.inbox.len() >= self.inbox_capacity
    }

    /// Returns the number of players currently present in the shard.
    pub fn player_count(&self) -> usize {
        self.players.len()
    }

    /// Returns `true` if any accepted gameplay mutations are buffered for the
    /// storage journal.
    pub fn has_pending_mutations(&self) -> bool {
        !self.mutation_log.is_empty()
    }

    /// Drains and returns the buffered gameplay mutations for the storage
    /// journal, leaving the buffer empty.
    ///
    /// Called by the driver each tick; it stamps each entry with the current tick
    /// and a monotonic id when building the journal records, so the deterministic
    /// shard never reads a clock or allocates an id itself.
    #[must_use]
    pub fn take_mutations(&mut self) -> Vec<PendingMutation> {
        std::mem::take(&mut self.mutation_log)
    }

    /// Returns `true` if `player` is currently present in the shard.
    pub fn contains_player(&self, player: PlayerId) -> bool {
        self.players.contains_key(&player)
    }

    /// Returns the current position of `player`, or `None` if absent.
    pub fn player_position(&self, player: PlayerId) -> Option<Vec3> {
        self.players.get(&player).map(|state| state.position)
    }

    /// Returns the authoritative game mode of `player`, or `None` if absent.
    pub fn player_game_mode(&self, player: PlayerId) -> Option<GameMode> {
        self.players.get(&player).map(|state| state.game_mode)
    }

    /// Enqueues `input` for application at the next tick boundary.
    ///
    /// Returns [`SimError::InboxFull`] without modifying the inbox if it is
    /// already at capacity (reject backpressure — see the type docs).
    pub fn enqueue(&mut self, input: GameInput) -> Result<(), SimError> {
        if self.inbox.len() >= self.inbox_capacity {
            return Err(SimError::InboxFull {
                capacity: self.inbox_capacity,
            });
        }
        self.inbox.push_back(input);
        Ok(())
    }

    /// Applies every queued input at this tick boundary and returns the outputs.
    ///
    /// This is the only method that mutates player state, so all queued inputs
    /// take effect exactly at this boundary. The inbox is empty on return.
    ///
    /// Joins and leaves apply in FIFO order; movement is coalesced (latest valid
    /// position per player) and validated, then applied after the drain — see the
    /// type-level docs. Spawn/despawn outputs are emitted in inbox order;
    /// move/correction outputs follow, ordered by [`PlayerId`] so the result is
    /// fully deterministic for a given inbox.
    #[allow(clippy::too_many_lines)] // one tick drain: join/leave/move + every block-edit input arm
    pub fn run_tick(&mut self) -> Vec<GameOutput> {
        let mut outputs = Vec::new();
        // Coalesce movement: keep only the latest *valid* position and rotation
        // per player (each component merged independently).
        let mut pending_moves: BTreeMap<PlayerId, PendingMove> = BTreeMap::new();
        // Players whose move was rejected this tick and still need a snap-back
        // correction (a later valid move removes them again).
        let mut corrections: BTreeSet<PlayerId> = BTreeSet::new();

        while let Some(input) = self.inbox.pop_front() {
            match input {
                GameInput::PlayerJoin { player, position } => {
                    // A duplicate join for an already-present player is ignored:
                    // the first join wins and re-joining produces no output,
                    // keeping the result deterministic regardless of retries.
                    if let Entry::Vacant(slot) = self.players.entry(player) {
                        slot.insert(PlayerState {
                            position,
                            yaw: 0.0,
                            pitch: 0.0,
                            game_mode: GameMode::default(),
                        });
                        outputs.push(GameOutput::PlayerSpawned { player, position });
                    }
                }
                GameInput::SetGameMode { player, mode } => {
                    // Mutate the authoritative mode in place; a mode change for an
                    // absent player is a silent no-op and emits nothing.
                    if let Some(state) = self.players.get_mut(&player) {
                        state.game_mode = mode;
                    }
                }
                GameInput::PlayerMove {
                    player,
                    position,
                    yaw,
                    pitch,
                } => {
                    // Movement for an unknown player is ignored rather than
                    // implicitly spawning one; there is also nothing to correct.
                    if !self.players.contains_key(&player) {
                        continue;
                    }
                    // A position is accepted only if finite and in range.
                    let valid_position = position.filter(|p| is_valid_position(*p));
                    if position.is_some() && valid_position.is_none() {
                        // Reject out-of-range / non-finite coords. Request a
                        // correction only if no valid position is queued to
                        // override the client's bad one.
                        let valid_pending = pending_moves
                            .get(&player)
                            .is_some_and(|m| m.position.is_some());
                        if !valid_pending {
                            corrections.insert(player);
                        }
                    }
                    // Coalesce each component independently: a later input's
                    // `Some` overwrites, a `None` leaves the earlier value. Only
                    // touch the entry when there is something to record so a
                    // purely-invalid move leaves no empty entry to apply.
                    if valid_position.is_some() || yaw.is_some() || pitch.is_some() {
                        let merged = pending_moves.entry(player).or_default();
                        if valid_position.is_some() {
                            merged.position = valid_position;
                            // A valid move supersedes a queued correction.
                            corrections.remove(&player);
                        }
                        if yaw.is_some() {
                            merged.yaw = yaw;
                        }
                        if pitch.is_some() {
                            merged.pitch = pitch;
                        }
                    }
                }
                GameInput::PlayerLeave { player } => {
                    // A leave cancels any queued movement/correction: there is no
                    // point moving or correcting a player who is gone.
                    pending_moves.remove(&player);
                    corrections.remove(&player);
                    if self.players.remove(&player).is_some() {
                        outputs.push(GameOutput::PlayerDespawned { player });
                    }
                }
                GameInput::BlockBreak {
                    player,
                    position,
                    sequence,
                } => {
                    // Break -> air. Applied in FIFO order during the drain so the
                    // output keeps the inbox ordering.
                    let cause = MutationCause::PlayerCreative { player };
                    let result = self.apply_block_edit(cause, position, BlockStateId::AIR);
                    if let Some(output) =
                        block_change_output(cause, sequence, position, BlockStateId::AIR, result)
                    {
                        outputs.push(output);
                    }
                }
                GameInput::BlockPlace {
                    player,
                    position,
                    sequence,
                    state,
                    clicked_face,
                    cursor_position,
                    player_yaw,
                } => {
                    // Refine the held item's default state into the correct placed
                    // state (rotation/facing/half/fence connectivity) against an
                    // immutable view of the resident chunks. The borrow ends before
                    // the mutable write below. The same refinement backs the
                    // off-tick `preview_placement` the driver uses to report the
                    // final state to the after-hook, so the two never diverge.
                    let computed = self.refine_placement(
                        state,
                        clicked_face,
                        cursor_position,
                        player_yaw,
                        position,
                    );
                    // Unsupported/unrecognised -> safe default (the held state).
                    let placed = computed.map_or(state, |r| BlockStateId::new(r.state_id));
                    let is_fence = computed.is_some_and(|r| r.rule == PlacementRule::FenceLike);

                    let cause = MutationCause::PlayerCreative { player };
                    let result = self.apply_block_edit(cause, position, placed);
                    if let Some(output) =
                        block_change_output(cause, sequence, position, placed, result)
                    {
                        outputs.push(output);
                    }
                    // A placed fence updates its same-fence cardinal neighbours so
                    // they connect back to it (broadcast-only, no extra ack).
                    if is_fence {
                        if let MutationResult::Applied { .. } = result {
                            // The reverse lookup yields a `'static` name, so it does
                            // not borrow `self` and is free to pass into the mutable
                            // neighbour-update pass below.
                            if let Some(fence_name) = state_id_to_block_name(placed.as_u32()) {
                                self.update_fence_neighbors(&mut outputs, position, fence_name);
                            }
                        }
                    }
                }
                GameInput::SetBlockExact {
                    player,
                    position,
                    sequence,
                    state,
                } => {
                    // An authoritative plugin/command exact write: store `state`
                    // verbatim through the same edit funnel, with NO
                    // compute_placement refinement and NO fence-neighbour pass. The
                    // plugin already chose the final state (e.g. a rotated
                    // `oak_log axis=x`), so re-deriving it would corrupt it.
                    let cause = MutationCause::PlayerCreative { player };
                    let result = self.apply_block_edit(cause, position, state);
                    if let Some(output) =
                        block_change_output(cause, sequence, position, state, result)
                    {
                        outputs.push(output);
                    }
                }
                GameInput::RejectBlockEdit {
                    player,
                    position,
                    sequence,
                    requested_state,
                } => {
                    // An edit refused upstream (plugin Deny / veto): the world is
                    // never touched. Read the authoritative state at the target and
                    // emit the same rejection output an in-sim refusal produces, so
                    // the actor is healed (mandatory resync + ack) through one
                    // funnel. Read-only: no `set_block`, no journal entry, so the
                    // tick stays deterministic.
                    outputs.push(GameOutput::BlockChangeRejected {
                        player,
                        position,
                        sequence,
                        requested_state,
                        authoritative_state: self.authoritative_state(position),
                    });
                }
            }
        }

        // Apply the coalesced moves at the boundary, in deterministic player
        // order. Every player here was present at coalesce time and cannot have
        // left (a leave clears the entry), so the lookup always succeeds. Every
        // entry carries at least one component, so each yields a PlayerMoved.
        for (player, merged) in pending_moves {
            if let Some(state) = self.players.get_mut(&player) {
                let position_changed = merged.position.is_some();
                if let Some(position) = merged.position {
                    state.position = position;
                }
                if let Some(yaw) = merged.yaw {
                    state.yaw = yaw;
                }
                if let Some(pitch) = merged.pitch {
                    state.pitch = pitch;
                }
                outputs.push(GameOutput::PlayerMoved {
                    player,
                    position: state.position,
                    yaw: state.yaw,
                    pitch: state.pitch,
                    position_changed,
                });
            }
        }

        // Snap clients back for rejected moves with no superseding valid move.
        for player in corrections {
            if let Some(state) = self.players.get(&player) {
                outputs.push(GameOutput::PlayerPositionCorrected {
                    player,
                    position: state.position,
                });
            }
        }

        outputs
    }

    /// Validates and applies a single block edit at the tick boundary — the one
    /// and only block-write funnel.
    ///
    /// Returns the structured [`MutationResult`]: [`Applied`](MutationResult::Applied)
    /// (the write happened and `set_block` marked the section dirty) or
    /// [`Rejected`](MutationResult::Rejected) with a [`RejectionReason`] and the
    /// authoritative state the client must heal to. An edit is rejected, in this
    /// precedence, when:
    /// - the acting player is not present in the shard ([`ActorAbsent`](RejectionReason::ActorAbsent));
    /// - the target chunk is not resident in this shard ([`ChunkNotLoaded`](RejectionReason::ChunkNotLoaded),
    ///   which also covers another dimension, since the map is dimension-scoped) —
    ///   checked before reach so an edit aimed at an absent chunk is rejected
    ///   silently rather than healed to a fabricated air state;
    /// - the target is beyond [`MAX_REACH`] of the actor ([`OutOfReach`](RejectionReason::OutOfReach)); or
    /// - the target `y` is outside the buildable range ([`YOutOfBounds`](RejectionReason::YOutOfBounds),
    ///   rejected by [`Chunk::set_block`](ferrumc_world::Chunk::set_block) without panic).
    ///
    /// Only [`MutationCause::PlayerCreative`] is reach-checked (it carries the
    /// actor); other causes bypass the actor/reach checks. A rejected edit never
    /// mutates chunk state.
    fn apply_block_edit(
        &mut self,
        cause: MutationCause,
        position: BlockPos,
        requested_state: BlockStateId,
    ) -> MutationResult {
        // Only a player edit has an actor; other causes (command/plugin/test) are
        // authoritative and skip the actor/reach checks. Resolve the actor first so
        // an edit by an absent player is rejected before anything else.
        let actor = match cause {
            MutationCause::PlayerCreative { player } => {
                let Some(actor_position) = self.players.get(&player).map(|state| state.position)
                else {
                    return MutationResult::Rejected {
                        reason: RejectionReason::ActorAbsent,
                        authoritative_state: self.authoritative_state(position),
                    };
                };
                Some(actor_position)
            }
            _ => None,
        };

        // The target chunk must be resident: it is where the write lands and the
        // only source of the authoritative state a rejection heals to. Checked
        // *before* reach so an edit aimed at an absent chunk is rejected silently
        // (no client to heal — `block_change_output` drops it) rather than
        // "healing" the client to a fabricated air state. The real column corrects
        // the client when it streams in.
        if !self.chunks.is_loaded(position.to_chunk_pos()) {
            return MutationResult::Rejected {
                reason: RejectionReason::ChunkNotLoaded,
                authoritative_state: BlockStateId::AIR,
            };
        }

        // Reach is validated only for a player edit, against the actor's
        // start-of-tick position; the chunk is resident, so the resync carries the
        // real authoritative state.
        if let Some(actor) = actor {
            if !within_reach(actor, position) {
                return MutationResult::Rejected {
                    reason: RejectionReason::OutOfReach,
                    authoritative_state: self.authoritative_state(position),
                };
            }
        }

        let Some(chunk) = self.chunks.get_mut(position.to_chunk_pos()) else {
            // Unreachable: residency was confirmed above. Fail closed rather than
            // panic if the invariant ever changes.
            return MutationResult::Rejected {
                reason: RejectionReason::ChunkNotLoaded,
                authoritative_state: BlockStateId::AIR,
            };
        };
        let old_state = chunk.get_block(position).unwrap_or(BlockStateId::AIR);
        // The chunk was looked up by `position`'s own column, so `set_block` can
        // only fail on an out-of-range `y`; treat that as a rejected edit rather
        // than mutating or panicking.
        match chunk.set_block(position, requested_state) {
            Ok(()) => {
                // A non-test gameplay edit drives the *persistence* signal: mark
                // the owning section persist-dirty (so only player-modified chunks
                // ever produce an overlay) and journal the mutation. A `Test` cause
                // is excluded so deterministic test/replay edits never persist.
                // `set_block` already marked the network dirty mask for everyone.
                if !matches!(cause, MutationCause::Test) {
                    chunk.mark_persist_dirty(position);
                    // Defensive bound only: the driver drains this every tick, so
                    // it is cleared long before reaching the cap. Past the cap a
                    // journal entry is dropped (best-effort; the overlay still
                    // persists the block) rather than growing the buffer unbounded.
                    if self.mutation_log.len() < MUTATION_LOG_CAP {
                        self.mutation_log.push(PendingMutation::new(
                            cause,
                            position,
                            old_state,
                            requested_state,
                        ));
                    }
                }
                MutationResult::Applied {
                    new_state: requested_state,
                }
            }
            Err(_) => MutationResult::Rejected {
                reason: RejectionReason::YOutOfBounds,
                authoritative_state: old_state,
            },
        }
    }

    /// Refines a player placement's held state into the final block-state using
    /// the resident chunks as the neighbour query, or `None` for an
    /// unsupported/unrecognised block.
    ///
    /// This is the single source of placement refinement, shared by the
    /// tick-boundary [`GameInput::BlockPlace`] apply path and the off-tick
    /// read-only [`preview_placement`](Self::preview_placement). Sharing it
    /// guarantees the state previewed to the after-hook equals the state the tick
    /// applies for the common single-edit case (chunks mutate only at tick
    /// boundaries).
    fn refine_placement(
        &self,
        state: BlockStateId,
        clicked_face: Direction,
        cursor_position: Vec3,
        player_yaw: f32,
        position: BlockPos,
    ) -> Option<PlacementResult> {
        let query = ShardNeighborQuery {
            chunks: &self.chunks,
        };
        let ctx = PlacementContext {
            item_block_state: state.as_u32(),
            clicked_face,
            cursor_position,
            player_yaw,
            position,
        };
        compute_placement(&ctx, &query)
    }

    /// Computes the final placed block-state for a player placement *without*
    /// mutating anything — the refinement [`GameInput::BlockPlace`] would apply.
    ///
    /// Read-only and off-tick: the driver calls it to report the final computed
    /// state back to the connection so the `after_block_place` hook fires with the
    /// state the world will hold, not the held item's bare default. An
    /// unsupported/unrecognised block falls back to the held `state` unchanged,
    /// matching the apply path. Because it shares
    /// [`refine_placement`](Self::refine_placement) with the tick and chunks
    /// mutate only at tick boundaries, the preview equals the applied state for
    /// the common single-edit case (neighbour-dependent fences placed in the same
    /// tick are the documented exception).
    #[must_use]
    pub fn preview_placement(
        &self,
        state: BlockStateId,
        clicked_face: Direction,
        cursor_position: Vec3,
        player_yaw: f32,
        position: BlockPos,
    ) -> BlockStateId {
        self.refine_placement(state, clicked_face, cursor_position, player_yaw, position)
            .map_or(state, |r| BlockStateId::new(r.state_id))
    }

    /// Reads the authoritative block state at `position` from the resident chunk,
    /// falling back to [`BlockStateId::AIR`] when the chunk is not resident.
    fn authoritative_state(&self, position: BlockPos) -> BlockStateId {
        self.chunks
            .get(position.to_chunk_pos())
            .and_then(|chunk| chunk.get_block(position))
            .unwrap_or(BlockStateId::AIR)
    }

    /// After a fence is placed at `center`, recomputes each cardinal neighbour that
    /// is the *same* fence so it connects back to the new block, and broadcasts any
    /// that changed.
    ///
    /// Neighbour writes go through the same [`apply_block_edit`](Self::apply_block_edit)
    /// funnel under [`MutationCause::Command`]: they persist and broadcast a viewer
    /// `BlockUpdate` (via [`block_change_output`]) but carry no player, so the router
    /// never acks them (only [`MutationCause::PlayerCreative`] is acked). Cross-shard
    /// neighbours are out of scope this milestone; a neighbour in a non-resident
    /// chunk is simply skipped (stale connectivity at the shard edge).
    fn update_fence_neighbors(
        &mut self,
        outputs: &mut Vec<GameOutput>,
        center: BlockPos,
        fence_name: &str,
    ) {
        for dir in [
            Direction::North,
            Direction::South,
            Direction::East,
            Direction::West,
        ] {
            let npos = center.offset(dir);
            // Only update a neighbour that is the very same fence.
            let Some(neighbor_state) = self
                .chunks
                .get(npos.to_chunk_pos())
                .and_then(|chunk| chunk.get_block(npos))
            else {
                continue;
            };
            if state_id_to_block_name(neighbor_state.as_u32()) != Some(fence_name) {
                continue;
            }
            // Recompute its connectivity now that the placed fence is visible.
            let recomputed = {
                let query = ShardNeighborQuery {
                    chunks: &self.chunks,
                };
                compute_fence_connection_state(fence_name, npos, &query)
            };
            let Some(new_state) = recomputed else {
                continue;
            };
            if new_state == neighbor_state.as_u32() {
                continue; // already connected; no write, no broadcast
            }
            let cause = MutationCause::Command;
            let result = self.apply_block_edit(cause, npos, BlockStateId::new(new_state));
            // Sequence 0: a Command edit is never acked, so the value is unused.
            if let Some(output) =
                block_change_output(cause, 0, npos, BlockStateId::new(new_state), result)
            {
                outputs.push(output);
            }
        }
    }
}

/// A [`NeighborQuery`] backed by a shard's resident chunks.
///
/// Resolves a neighbour's block-state id to a name via the registry and reports it
/// as fence-connectable when it is the same fence or a solid full cube. Air,
/// unknown, and non-resident neighbours are not connectable.
struct ShardNeighborQuery<'a> {
    chunks: &'a LoadedChunkMap,
}

impl NeighborQuery for ShardNeighborQuery<'_> {
    fn is_fence_connectable(&self, position: BlockPos, fence_block_name: &str) -> bool {
        let Some(state) = self
            .chunks
            .get(position.to_chunk_pos())
            .and_then(|chunk| chunk.get_block(position))
        else {
            return false;
        };
        if state.is_air() {
            return false;
        }
        let Some(name) = state_id_to_block_name(state.as_u32()) else {
            return false;
        };
        // A fence connects to the same fence or to any solid full cube.
        name == fence_block_name || block_metadata(name).is_some_and(|m| m.is_solid_cube)
    }

    fn block_state_at(&self, position: BlockPos) -> Option<u32> {
        // Resolve the resident neighbour's state for the placement rules that read
        // it (stair auto-corner `shape`, fence-gate `in_wall`). Air and
        // non-resident cells report `None`, matching the trait contract; a
        // neighbour in another shard's chunk is simply unseen (stale corner at the
        // shard edge, healed when that column streams in).
        self.chunks
            .get(position.to_chunk_pos())
            .and_then(|chunk| chunk.get_block(position))
            .filter(|state| !state.is_air())
            .map(BlockStateId::as_u32)
    }
}

/// Maps an [`apply_block_edit`](SimShard::apply_block_edit) result into the
/// [`GameOutput`] the session layer routes, or `None` when nothing should be
/// sent.
///
/// An [`Applied`](MutationResult::Applied) edit becomes a
/// [`GameOutput::BlockChanged`] (broadcast to viewers, acked to the actor). A
/// [`Rejected`](MutationResult::Rejected) edit by a present player becomes a
/// [`GameOutput::BlockChangeRejected`] (a targeted resync + ack to the actor) —
/// including a [`ChunkNotLoaded`](RejectionReason::ChunkNotLoaded) rejection,
/// which only a *present* actor can reach (residency is checked after the actor),
/// so its prediction must be ended rather than ghosted. The single case that
/// emits nothing is an [`ActorAbsent`](RejectionReason::ActorAbsent) rejection:
/// there is no client session to heal. A non-player cause also emits nothing.
fn block_change_output(
    cause: MutationCause,
    sequence: i32,
    position: BlockPos,
    requested_state: BlockStateId,
    result: MutationResult,
) -> Option<GameOutput> {
    match result {
        MutationResult::Applied { new_state } => Some(GameOutput::BlockChanged {
            position,
            state: new_state,
            sequence,
            cause,
        }),
        MutationResult::Rejected {
            reason,
            authoritative_state,
        } => {
            // Only a player edit has an actor to resync; non-player causes
            // (command/plugin/test) have no client to heal this milestone.
            let MutationCause::PlayerCreative { player } = cause else {
                return None;
            };
            // A genuinely absent actor has no session to ack or resync — stay
            // silent. But ChunkNotLoaded is NOT silenced here: apply_block_edit
            // checks ActorAbsent *before* ChunkNotLoaded, so a ChunkNotLoaded
            // rejection means the actor WAS present. Silencing it stranded that
            // client's optimistic prediction as a ghost block; instead emit a
            // rejection so the actor gets the ack (+ best-known resync) that ends
            // the prediction. The real column corrects it when it streams in.
            if matches!(reason, RejectionReason::ActorAbsent) {
                return None;
            }
            Some(GameOutput::BlockChangeRejected {
                player,
                position,
                sequence,
                requested_state,
                authoritative_state,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use ferrumc_math::ChunkPos;
    use ferrumc_storage::InMemoryStore;
    use ferrumc_world::FlatWorldGenerator;

    use super::*;
    use crate::ticket::{ChunkTicket, TicketReason};

    fn player(name: &str) -> PlayerId {
        PlayerId::offline(name)
    }

    fn shard() -> SimShard {
        SimShard::new(ShardPos::new(0, 0))
    }

    /// A spawn-position helper: the default world spawn used across block-edit
    /// tests, comfortably in reach of the flat surface around it.
    fn spawn() -> Vec3 {
        Vec3::new(8.0, 64.0, 8.0)
    }

    /// Builds a shard with `chunk` generated and resident, with its
    /// freshly-generated dirtiness cleared so later assertions see only the
    /// edits the test makes.
    async fn shard_with_loaded_chunk(chunk: ChunkPos) -> SimShard {
        let mut s = shard();
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        s.loaded_chunks_mut()
            .acquire(
                &store,
                &generator,
                chunk,
                ChunkTicket::of(TicketReason::Player),
            )
            .await
            .expect("acquire chunk");
        // A generated chunk is dirty for its initial save; clear that so a later
        // dirty check reflects only the test's own edit.
        let _ = s.loaded_chunks_mut().take_dirty();
        s
    }

    /// Reads the block at `pos` from the resident chunk owning it.
    fn block_at(s: &SimShard, pos: BlockPos) -> Option<BlockStateId> {
        s.loaded_chunks()
            .get(pos.to_chunk_pos())
            .and_then(|c| c.get_block(pos))
    }

    #[test]
    fn new_uses_default_capacity_and_is_empty() {
        let s = shard();
        assert_eq!(s.shard_pos(), ShardPos::new(0, 0));
        assert_eq!(s.inbox_capacity(), 1024);
        assert_eq!(s.inbox_len(), 0);
        assert_eq!(s.player_count(), 0);
        assert!(!s.is_inbox_full());
    }

    #[test]
    fn enqueue_does_not_mutate_state_until_tick() {
        let mut s = shard();
        let p = player("alice");
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::new(1.0, 64.0, 2.0),
        })
        .expect("room");

        // Queued but not applied yet.
        assert_eq!(s.inbox_len(), 1);
        assert_eq!(s.player_count(), 0);
        assert!(!s.contains_player(p));
        assert_eq!(s.player_position(p), None);

        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::PlayerSpawned {
                player: p,
                position: Vec3::new(1.0, 64.0, 2.0)
            }]
        );
        assert_eq!(s.inbox_len(), 0);
        assert_eq!(s.player_count(), 1);
        assert!(s.contains_player(p));
        assert_eq!(s.player_position(p), Some(Vec3::new(1.0, 64.0, 2.0)));
    }

    #[test]
    fn multiple_moves_in_one_tick_coalesce_to_latest() {
        let mut s = shard();
        let p = player("bob");
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::ZERO,
        })
        .expect("room");
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: Some(Vec3::new(5.0, 0.0, 0.0)),
            yaw: None,
            pitch: None,
        })
        .expect("room");
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: Some(Vec3::new(9.0, 0.0, 0.0)),
            yaw: None,
            pitch: None,
        })
        .expect("room");

        let outputs = s.run_tick();
        // The two moves coalesce: only the latest position is applied, and a
        // single PlayerMoved (after the spawn) is emitted.
        assert_eq!(
            outputs,
            vec![
                GameOutput::PlayerSpawned {
                    player: p,
                    position: Vec3::ZERO
                },
                GameOutput::PlayerMoved {
                    player: p,
                    position: Vec3::new(9.0, 0.0, 0.0),
                    yaw: 0.0,
                    pitch: 0.0,
                    position_changed: true,
                },
            ]
        );
        assert_eq!(s.player_position(p), Some(Vec3::new(9.0, 0.0, 0.0)));
    }

    #[test]
    fn move_with_rotation_stores_and_emits_yaw_pitch() {
        let mut s = shard();
        let p = player("turner");
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::ZERO,
        })
        .expect("room");
        let _ = s.run_tick();

        // A position+rotation move stores both and emits a position-changed
        // PlayerMoved carrying the new yaw/pitch.
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: Some(Vec3::new(3.0, 0.0, 0.0)),
            yaw: Some(90.0),
            pitch: Some(-30.0),
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::PlayerMoved {
                player: p,
                position: Vec3::new(3.0, 0.0, 0.0),
                yaw: 90.0,
                pitch: -30.0,
                position_changed: true,
            }]
        );
    }

    #[test]
    fn rotation_only_move_keeps_position_and_flags_no_position_change() {
        let mut s = shard();
        let p = player("looker");
        let spawn = Vec3::new(8.0, 64.0, 8.0);
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn,
        })
        .expect("room");
        let _ = s.run_tick();

        // A rotation-only move (no position) updates yaw/pitch but leaves the
        // position untouched and reports position_changed = false.
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: None,
            yaw: Some(45.0),
            pitch: Some(10.0),
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::PlayerMoved {
                player: p,
                position: spawn,
                yaw: 45.0,
                pitch: 10.0,
                position_changed: false,
            }]
        );
        // The stored position is unchanged by a rotation-only move.
        assert_eq!(s.player_position(p), Some(spawn));
    }

    #[test]
    fn position_only_move_leaves_rotation_unchanged() {
        let mut s = shard();
        let p = player("strafer");
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::ZERO,
        })
        .expect("room");
        let _ = s.run_tick();

        // First turn in place, then move position-only: the second move must keep
        // the yaw the first one set (a None component leaves the stored value).
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: None,
            yaw: Some(120.0),
            pitch: Some(5.0),
        })
        .expect("room");
        let _ = s.run_tick();
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: Some(Vec3::new(1.0, 0.0, 0.0)),
            yaw: None,
            pitch: None,
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::PlayerMoved {
                player: p,
                position: Vec3::new(1.0, 0.0, 0.0),
                yaw: 120.0,
                pitch: 5.0,
                position_changed: true,
            }]
        );
    }

    #[test]
    fn invalid_move_is_rejected_and_emits_a_correction() {
        let mut s = shard();
        let p = player("nan");
        let spawn = Vec3::new(8.0, 64.0, 8.0);
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn,
        })
        .expect("room");
        let _ = s.run_tick();

        // Every flavour of bad coordinate is rejected.
        for bad in [
            Vec3::new(f64::NAN, 64.0, 8.0),
            Vec3::new(8.0, f64::INFINITY, 8.0),
            Vec3::new(8.0, 64.0, f64::NEG_INFINITY),
            Vec3::new(3.0e7 + 1.0, 64.0, 8.0),
            Vec3::new(8.0, 64.0, -3.0e7 - 1.0),
        ] {
            s.enqueue(GameInput::PlayerMove {
                player: p,
                position: Some(bad),
                yaw: None,
                pitch: None,
            })
            .expect("room");
            let outputs = s.run_tick();
            // The rejected move never changes state and yields a snap-back
            // correction to the last accepted position.
            assert_eq!(
                outputs,
                vec![GameOutput::PlayerPositionCorrected {
                    player: p,
                    position: spawn,
                }]
            );
            assert_eq!(s.player_position(p), Some(spawn));
        }
    }

    #[test]
    fn valid_move_supersedes_a_rejected_one_without_correcting() {
        let mut s = shard();
        let p = player("oscar");
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::ZERO,
        })
        .expect("room");
        let _ = s.run_tick();

        // A rejected move followed by a valid one: the valid position wins and no
        // correction is emitted (the PlayerMoved is the authoritative update).
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: Some(Vec3::new(f64::NAN, 0.0, 0.0)),
            yaw: None,
            pitch: None,
        })
        .expect("room");
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: Some(Vec3::new(3.0, 4.0, 5.0)),
            yaw: None,
            pitch: None,
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::PlayerMoved {
                player: p,
                position: Vec3::new(3.0, 4.0, 5.0),
                yaw: 0.0,
                pitch: 0.0,
                position_changed: true,
            }]
        );
        assert_eq!(s.player_position(p), Some(Vec3::new(3.0, 4.0, 5.0)));
    }

    #[test]
    fn boundary_coordinates_are_accepted() {
        let mut s = shard();
        let p = player("edge");
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::ZERO,
        })
        .expect("room");
        let _ = s.run_tick();

        // Exactly at the magnitude limit is in range (inclusive bound).
        let edge = Vec3::new(3.0e7, -3.0e7, 0.0);
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: Some(edge),
            yaw: None,
            pitch: None,
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::PlayerMoved {
                player: p,
                position: edge,
                yaw: 0.0,
                pitch: 0.0,
                position_changed: true,
            }]
        );
        assert_eq!(s.player_position(p), Some(edge));
    }

    #[test]
    fn leave_cancels_a_pending_move_in_the_same_tick() {
        let mut s = shard();
        let p = player("quinn");
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::ZERO,
        })
        .expect("room");
        let _ = s.run_tick();

        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: Some(Vec3::new(2.0, 0.0, 0.0)),
            yaw: None,
            pitch: None,
        })
        .expect("room");
        s.enqueue(GameInput::PlayerLeave { player: p })
            .expect("room");
        // The leave wins: only a despawn, no stale move for a gone player.
        let outputs = s.run_tick();
        assert_eq!(outputs, vec![GameOutput::PlayerDespawned { player: p }]);
        assert_eq!(s.player_count(), 0);
    }

    #[test]
    fn invalid_move_for_absent_player_is_silent() {
        let mut s = shard();
        let ghost = player("ghost");
        s.enqueue(GameInput::PlayerMove {
            player: ghost,
            position: Some(Vec3::new(f64::NAN, 0.0, 0.0)),
            yaw: None,
            pitch: None,
        })
        .expect("room");
        // No player present: no correction, no output at all.
        assert!(s.run_tick().is_empty());
        assert_eq!(s.player_count(), 0);
    }

    #[test]
    fn duplicate_join_is_ignored() {
        let mut s = shard();
        let p = player("carol");
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::ZERO,
        })
        .expect("room");
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::new(100.0, 0.0, 0.0),
        })
        .expect("room");

        let outputs = s.run_tick();
        // Only the first join spawns; the second is a no-op and the position is
        // unchanged.
        assert_eq!(
            outputs,
            vec![GameOutput::PlayerSpawned {
                player: p,
                position: Vec3::ZERO
            }]
        );
        assert_eq!(s.player_position(p), Some(Vec3::ZERO));
    }

    #[test]
    fn move_and_leave_for_unknown_player_are_ignored() {
        let mut s = shard();
        let ghost = player("ghost");
        s.enqueue(GameInput::PlayerMove {
            player: ghost,
            position: Some(Vec3::new(1.0, 1.0, 1.0)),
            yaw: None,
            pitch: None,
        })
        .expect("room");
        s.enqueue(GameInput::PlayerLeave { player: ghost })
            .expect("room");

        assert!(s.run_tick().is_empty());
        assert_eq!(s.player_count(), 0);
    }

    #[test]
    fn leave_removes_present_player() {
        let mut s = shard();
        let p = player("dave");
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::ZERO,
        })
        .expect("room");
        let _ = s.run_tick();
        assert_eq!(s.player_count(), 1);

        s.enqueue(GameInput::PlayerLeave { player: p })
            .expect("room");
        let outputs = s.run_tick();
        assert_eq!(outputs, vec![GameOutput::PlayerDespawned { player: p }]);
        assert_eq!(s.player_count(), 0);
        assert!(!s.contains_player(p));
    }

    #[test]
    fn inbox_rejects_when_full_then_recovers_after_drain() {
        let cap = NonZeroUsize::new(2).expect("nonzero");
        let mut s = SimShard::with_inbox_capacity(ShardPos::new(3, -1), cap);
        let p = player("erin");

        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::ZERO,
        })
        .expect("first");
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: Some(Vec3::new(1.0, 0.0, 0.0)),
            yaw: None,
            pitch: None,
        })
        .expect("second");
        assert!(s.is_inbox_full());

        // Third is rejected with a classified error; inbox is left untouched.
        let err = s
            .enqueue(GameInput::PlayerMove {
                player: p,
                position: Some(Vec3::new(2.0, 0.0, 0.0)),
                yaw: None,
                pitch: None,
            })
            .expect_err("inbox is full");
        assert_eq!(err, SimError::InboxFull { capacity: 2 });
        assert_eq!(s.inbox_len(), 2);

        // Draining at the tick boundary frees the inbox; enqueue works again.
        let outputs = s.run_tick();
        assert_eq!(outputs.len(), 2);
        assert!(!s.is_inbox_full());
        s.enqueue(GameInput::PlayerLeave { player: p })
            .expect("room after drain");
    }

    #[test]
    fn empty_tick_produces_no_outputs() {
        let mut s = shard();
        assert!(s.run_tick().is_empty());
    }

    #[tokio::test]
    async fn break_replaces_block_with_air_marks_dirty_and_emits_change() {
        let chunk = ChunkPos::new(0, 0);
        let mut s = shard_with_loaded_chunk(chunk).await;
        let p = player("breaker");
        let target = BlockPos::new(8, 63, 8); // flat-world grass surface
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn(),
        })
        .expect("room");
        let _ = s.run_tick();
        // The generated surface block starts non-air, and the chunk is clean.
        assert_ne!(block_at(&s, target), Some(BlockStateId::AIR));
        assert!(!s
            .loaded_chunks()
            .get(chunk)
            .expect("resident")
            .dirty_sections()
            .any());

        s.enqueue(GameInput::BlockBreak {
            player: p,
            position: target,
            sequence: 7,
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::BlockChanged {
                position: target,
                state: BlockStateId::AIR,
                sequence: 7,
                cause: MutationCause::PlayerCreative { player: p },
            }]
        );
        // The chunk reflects the break and the owning section is now dirty for
        // BOTH the network mask and the persistence (persist-dirty) mask, since a
        // PlayerCreative edit is a real gameplay mutation.
        assert_eq!(block_at(&s, target), Some(BlockStateId::AIR));
        let resident = s.loaded_chunks().get(chunk).expect("resident");
        assert!(resident.dirty_sections().any());
        assert!(
            resident.persist_dirty_sections().any(),
            "a player break must mark the chunk persist-dirty"
        );
        // The edit is also journaled for the storage worker.
        assert!(s.has_pending_mutations());
    }

    #[tokio::test]
    async fn place_marks_persist_dirty_and_journals_the_mutation() {
        let chunk = ChunkPos::new(0, 0);
        let mut s = shard_with_loaded_chunk(chunk).await;
        let p = player("placer");
        let target = BlockPos::new(8, 65, 8);
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn(),
        })
        .expect("room");
        let _ = s.run_tick();
        assert!(!s.has_pending_mutations());

        s.enqueue(GameInput::BlockPlace {
            player: p,
            position: target,
            sequence: 1,
            state: DEFAULT_PLACED_STATE,
            clicked_face: Direction::Up,
            cursor_position: Vec3::new(0.5, 0.0, 0.5),
            player_yaw: 0.0,
        })
        .expect("room");
        let _ = s.run_tick();

        assert!(s
            .loaded_chunks()
            .get(chunk)
            .expect("resident")
            .persist_dirty_sections()
            .any());
        let mutations = s.take_mutations();
        assert_eq!(mutations.len(), 1);
        assert_eq!(mutations[0].position(), target);
        assert_eq!(mutations[0].new_state(), DEFAULT_PLACED_STATE);
        assert_eq!(mutations[0].old_state(), BlockStateId::AIR);
        // Draining empties the buffer.
        assert!(!s.has_pending_mutations());
    }

    #[tokio::test]
    async fn place_sets_the_default_block_and_emits_change() {
        let chunk = ChunkPos::new(0, 0);
        let mut s = shard_with_loaded_chunk(chunk).await;
        let p = player("placer");
        let target = BlockPos::new(8, 65, 8); // air just above the surface
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn(),
        })
        .expect("room");
        let _ = s.run_tick();
        assert_eq!(block_at(&s, target), Some(BlockStateId::AIR));

        s.enqueue(GameInput::BlockPlace {
            player: p,
            position: target,
            sequence: 12,
            state: DEFAULT_PLACED_STATE,
            clicked_face: Direction::Up,
            cursor_position: Vec3::new(0.5, 0.0, 0.5),
            player_yaw: 0.0,
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::BlockChanged {
                position: target,
                state: DEFAULT_PLACED_STATE,
                sequence: 12,
                cause: MutationCause::PlayerCreative { player: p },
            }]
        );
        assert_eq!(block_at(&s, target), Some(DEFAULT_PLACED_STATE));
    }

    #[tokio::test]
    async fn place_writes_the_threaded_held_block_state_not_a_default() {
        // The held item's resolved block-state is threaded on the input, so a
        // place must write exactly that state — here glass (562), proving the old
        // hardcoded stone default is gone.
        let chunk = ChunkPos::new(0, 0);
        let mut s = shard_with_loaded_chunk(chunk).await;
        let p = player("builder");
        let target = BlockPos::new(8, 65, 8); // air just above the surface
        let glass = BlockStateId::new(562);
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn(),
        })
        .expect("room");
        let _ = s.run_tick();

        s.enqueue(GameInput::BlockPlace {
            player: p,
            position: target,
            sequence: 8,
            state: glass,
            clicked_face: Direction::Up,
            cursor_position: Vec3::new(0.5, 0.0, 0.5),
            player_yaw: 0.0,
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::BlockChanged {
                position: target,
                state: glass,
                sequence: 8,
                cause: MutationCause::PlayerCreative { player: p },
            }]
        );
        assert_eq!(block_at(&s, target), Some(glass));
        assert_ne!(block_at(&s, target), Some(DEFAULT_PLACED_STATE));
    }

    #[tokio::test]
    async fn break_in_unloaded_chunk_by_present_actor_is_rejected_with_resync() {
        // No chunk resident, but the actor IS present: residency is checked after
        // the actor, so this reaches a ChunkNotLoaded rejection. A present actor
        // optimistically predicted the break, so it must be healed (ack + best-known
        // resync) rather than ghosted; the real column corrects it when it streams
        // in. The authoritative state is air (the chunk is absent).
        let mut s = shard();
        let p = player("homeless");
        let target = BlockPos::new(8, 63, 8);
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn(),
        })
        .expect("room");
        let _ = s.run_tick();
        s.enqueue(GameInput::BlockBreak {
            player: p,
            position: target,
            sequence: 1,
        })
        .expect("room");
        assert_eq!(
            s.run_tick(),
            vec![GameOutput::BlockChangeRejected {
                player: p,
                position: target,
                sequence: 1,
                requested_state: BlockStateId::AIR,
                authoritative_state: BlockStateId::AIR,
            }]
        );
    }

    #[tokio::test]
    async fn reject_block_edit_for_a_place_resyncs_air_without_mutating() {
        // A place refused upstream (plugin Deny): the client predicted the held
        // block at an empty cell. RejectBlockEdit reads the authoritative state
        // (air — the cell is empty) and emits a BlockChangeRejected healing the
        // actor to air, writing nothing to the chunk.
        let chunk = ChunkPos::new(0, 0);
        let mut s = shard_with_loaded_chunk(chunk).await;
        let p = player("denied-placer");
        let target = BlockPos::new(8, 65, 8); // air just above the surface
        let glass = BlockStateId::new(562);
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn(),
        })
        .expect("room");
        let _ = s.run_tick();
        assert_eq!(block_at(&s, target), Some(BlockStateId::AIR));

        s.enqueue(GameInput::RejectBlockEdit {
            player: p,
            position: target,
            sequence: 7,
            requested_state: glass,
        })
        .expect("room");
        assert_eq!(
            s.run_tick(),
            vec![GameOutput::BlockChangeRejected {
                player: p,
                position: target,
                sequence: 7,
                requested_state: glass,
                authoritative_state: BlockStateId::AIR,
            }]
        );
        // The world was never touched.
        assert_eq!(block_at(&s, target), Some(BlockStateId::AIR));
    }

    #[tokio::test]
    async fn reject_block_edit_for_a_break_resyncs_the_surface_without_mutating() {
        // A break refused upstream: the client predicted air at the surface.
        // RejectBlockEdit reads the authoritative surface block and emits a
        // BlockChangeRejected healing the actor back to it, writing nothing.
        let chunk = ChunkPos::new(0, 0);
        let mut s = shard_with_loaded_chunk(chunk).await;
        let p = player("denied-breaker");
        let target = BlockPos::new(8, 63, 8); // flat-world grass surface
        let authoritative = block_at(&s, target).expect("resident surface block");
        assert_ne!(authoritative, BlockStateId::AIR);
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn(),
        })
        .expect("room");
        let _ = s.run_tick();

        s.enqueue(GameInput::RejectBlockEdit {
            player: p,
            position: target,
            sequence: 3,
            requested_state: BlockStateId::AIR,
        })
        .expect("room");
        assert_eq!(
            s.run_tick(),
            vec![GameOutput::BlockChangeRejected {
                player: p,
                position: target,
                sequence: 3,
                requested_state: BlockStateId::AIR,
                authoritative_state: authoritative,
            }]
        );
        // The surface block is untouched.
        assert_eq!(block_at(&s, target), Some(authoritative));
    }

    #[test]
    fn set_game_mode_mutates_present_player_and_ignores_absent() {
        let mut s = shard();
        let p = player("modeswitcher");
        let ghost = player("ghost");
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::ZERO,
        })
        .expect("room");
        let _ = s.run_tick();
        // The authoritative mode starts at the default and survives a tick.
        assert_eq!(s.player_game_mode(p), Some(GameMode::default()));

        s.enqueue(GameInput::SetGameMode {
            player: p,
            mode: GameMode::Creative,
        })
        .expect("room");
        // Setting an absent player's mode is a silent no-op (no panic, no spawn).
        s.enqueue(GameInput::SetGameMode {
            player: ghost,
            mode: GameMode::Creative,
        })
        .expect("room");
        // A mode change emits no output.
        assert!(s.run_tick().is_empty());
        assert_eq!(s.player_game_mode(p), Some(GameMode::Creative));
        assert_eq!(s.player_game_mode(ghost), None);
        assert_eq!(s.player_count(), 1);
    }

    #[tokio::test]
    async fn edit_out_of_reach_is_rejected_with_a_resync_when_chunk_is_loaded() {
        // Block (100, 63, 8) lives in chunk (6, 0); load exactly that chunk so
        // the only reason to reject is the ~92-block distance from spawn. Because
        // the chunk is resident the authoritative state is readable, so the actor
        // gets a targeted resync rather than silence.
        let far_chunk = ChunkPos::new(6, 0);
        let mut s = shard_with_loaded_chunk(far_chunk).await;
        let p = player("shortarms");
        let target = BlockPos::new(100, 63, 8);
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn(),
        })
        .expect("room");
        let _ = s.run_tick();

        // The authoritative (untouched) surface state the resync must carry.
        let authoritative = block_at(&s, target).expect("resident surface block");
        assert_ne!(authoritative, BlockStateId::AIR);

        s.enqueue(GameInput::BlockBreak {
            player: p,
            position: target,
            sequence: 3,
        })
        .expect("room");
        assert_eq!(
            s.run_tick(),
            vec![GameOutput::BlockChangeRejected {
                player: p,
                position: target,
                sequence: 3,
                requested_state: BlockStateId::AIR,
                authoritative_state: authoritative,
            }]
        );
        // The block is untouched (still the generated surface, not air).
        assert_eq!(block_at(&s, target), Some(authoritative));
    }

    #[tokio::test]
    async fn block_edit_for_absent_player_is_ignored() {
        let chunk = ChunkPos::new(0, 0);
        let mut s = shard_with_loaded_chunk(chunk).await;
        let ghost = player("ghost");
        s.enqueue(GameInput::BlockBreak {
            player: ghost,
            position: BlockPos::new(8, 63, 8),
            sequence: 1,
        })
        .expect("room");
        s.enqueue(GameInput::BlockPlace {
            player: ghost,
            position: BlockPos::new(8, 65, 8),
            sequence: 2,
            state: DEFAULT_PLACED_STATE,
            clicked_face: Direction::Up,
            cursor_position: Vec3::new(0.5, 0.0, 0.5),
            player_yaw: 0.0,
        })
        .expect("room");
        // An absent actor has no session to ack or resync, so nothing is emitted.
        assert!(s.run_tick().is_empty());
    }

    #[tokio::test]
    async fn block_edit_applies_only_at_tick_boundary() {
        let chunk = ChunkPos::new(0, 0);
        let mut s = shard_with_loaded_chunk(chunk).await;
        let p = player("patient");
        let target = BlockPos::new(8, 63, 8);
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn(),
        })
        .expect("room");
        let _ = s.run_tick();

        s.enqueue(GameInput::BlockBreak {
            player: p,
            position: target,
            sequence: 5,
        })
        .expect("room");
        // Enqueued but not yet applied: the block is unchanged until the tick.
        assert_ne!(block_at(&s, target), Some(BlockStateId::AIR));
        let _ = s.run_tick();
        assert_eq!(block_at(&s, target), Some(BlockStateId::AIR));
    }

    // --- placement integration (compute_placement in the place funnel) ---

    /// `oak_log` default (axis=y); the integration tests derive axis/half/facing
    /// from the placement inputs threaded on the place.
    const OAK_LOG: u32 = 137;
    /// `oak_slab` default (type=bottom).
    const OAK_SLAB: u32 = 12054;
    /// `oak_stairs` default (facing=north, half=bottom).
    const OAK_STAIRS: u32 = 2949;
    /// `torch` (single floor state).
    const TORCH: u32 = 2401;
    /// `oak_fence` default (all sides disconnected).
    const OAK_FENCE: u32 = 6027;

    /// Builds a shard with chunk (0,0) resident and `p` joined at spawn.
    async fn shard_with_player(p: PlayerId) -> SimShard {
        let mut s = shard_with_loaded_chunk(ChunkPos::new(0, 0)).await;
        s.enqueue(GameInput::PlayerJoin {
            player: p,
            position: spawn(),
        })
        .expect("room");
        let _ = s.run_tick();
        s
    }

    /// Enqueues one place of `held` at `target` with the given placement inputs and
    /// returns the tick's outputs.
    #[allow(clippy::too_many_arguments)] // a test helper mirroring the place input's fields
    fn place_block(
        s: &mut SimShard,
        p: PlayerId,
        target: BlockPos,
        held: u32,
        face: Direction,
        cursor_y: f64,
        yaw: f32,
        sequence: i32,
    ) -> Vec<GameOutput> {
        s.enqueue(GameInput::BlockPlace {
            player: p,
            position: target,
            sequence,
            state: BlockStateId::new(held),
            clicked_face: face,
            cursor_position: Vec3::new(0.5, cursor_y, 0.5),
            player_yaw: yaw,
        })
        .expect("room");
        s.run_tick()
    }

    #[tokio::test]
    async fn place_log_on_side_face_sets_axis() {
        let p = player("logger");
        let mut s = shard_with_player(p).await;
        let target = BlockPos::new(8, 65, 8);
        // Clicking an east/west face lays the log along the x axis (136), not the
        // default vertical y (137).
        let _ = place_block(&mut s, p, target, OAK_LOG, Direction::East, 0.5, 0.0, 1);
        assert_eq!(block_at(&s, target), Some(BlockStateId::new(136)));
    }

    #[tokio::test]
    async fn place_slab_bottom_or_top_from_cursor() {
        let p = player("slabber");
        let mut s = shard_with_player(p).await;
        // Top-face click -> bottom slab (default 12054).
        let bottom = BlockPos::new(8, 65, 8);
        let _ = place_block(&mut s, p, bottom, OAK_SLAB, Direction::Up, 0.0, 0.0, 1);
        assert_eq!(block_at(&s, bottom), Some(BlockStateId::new(12054)));
        // Side click in the upper half -> top slab (12052).
        let top = BlockPos::new(9, 65, 8);
        let _ = place_block(&mut s, p, top, OAK_SLAB, Direction::North, 0.8, 0.0, 2);
        assert_eq!(block_at(&s, top), Some(BlockStateId::new(12052)));
    }

    #[tokio::test]
    async fn place_stairs_facing_from_yaw_and_half_from_cursor() {
        let p = player("stairer");
        let mut s = shard_with_player(p).await;
        // yaw 180 -> facing north, top-face click -> bottom half: the default 2949.
        let a = BlockPos::new(8, 65, 8);
        let _ = place_block(&mut s, p, a, OAK_STAIRS, Direction::Up, 0.0, 180.0, 1);
        assert_eq!(block_at(&s, a), Some(BlockStateId::new(2949)));
        // yaw 90 -> facing west, bottom-face click -> top half: 2979.
        let b = BlockPos::new(9, 65, 8);
        let _ = place_block(&mut s, p, b, OAK_STAIRS, Direction::Down, 0.0, 90.0, 2);
        assert_eq!(block_at(&s, b), Some(BlockStateId::new(2979)));
    }

    #[tokio::test]
    async fn place_torch_floor_vs_wall() {
        let p = player("torcher");
        let mut s = shard_with_player(p).await;
        // Top-face click keeps the floor torch (2401).
        let floor = BlockPos::new(8, 65, 8);
        let _ = place_block(&mut s, p, floor, TORCH, Direction::Up, 0.5, 0.0, 1);
        assert_eq!(block_at(&s, floor), Some(BlockStateId::new(2401)));
        // North-face click becomes a wall torch facing north (2402).
        let wall = BlockPos::new(9, 65, 8);
        let _ = place_block(&mut s, p, wall, TORCH, Direction::North, 0.5, 0.0, 2);
        assert_eq!(block_at(&s, wall), Some(BlockStateId::new(2402)));
    }

    #[tokio::test]
    async fn set_block_exact_writes_a_rotated_state_verbatim_bypassing_refinement() {
        // A plugin/command exact write of a rotated state must be stored as-is.
        // The contrast place below proves the player path still refines, so the
        // exact path is genuinely bypassing compute_placement (not a no-op).
        let p = player("plugin-actor");
        let mut s = shard_with_player(p).await;
        let exact_target = BlockPos::new(8, 65, 8);
        // oak_log axis=x (136): a neutral place would re-derive this to axis=y.
        s.enqueue(GameInput::SetBlockExact {
            player: p,
            position: exact_target,
            sequence: 1,
            state: BlockStateId::new(136),
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::BlockChanged {
                position: exact_target,
                state: BlockStateId::new(136),
                sequence: 1,
                cause: MutationCause::PlayerCreative { player: p },
            }]
        );
        assert_eq!(block_at(&s, exact_target), Some(BlockStateId::new(136)));

        // Contrast: the player place path refines the SAME held id 136 with a
        // neutral top-face click back to the default vertical axis=y (137).
        let refined_target = BlockPos::new(9, 65, 8);
        let _ = place_block(&mut s, p, refined_target, 136, Direction::Up, 0.5, 0.0, 2);
        assert_eq!(block_at(&s, refined_target), Some(BlockStateId::new(137)));
    }

    #[tokio::test]
    async fn preview_placement_matches_the_applied_state() {
        // preview_placement (off-tick, read-only) must return the same state the
        // BlockPlace tick applies, so the after-hook reports the final state.
        let p = player("previewer");
        let mut s = shard_with_player(p).await;
        let target = BlockPos::new(8, 65, 8);
        // An east-face log preview -> axis=x (136), the rotated state.
        let preview = s.preview_placement(
            BlockStateId::new(OAK_LOG),
            Direction::East,
            Vec3::new(0.5, 0.5, 0.5),
            0.0,
            target,
        );
        assert_eq!(preview, BlockStateId::new(136));
        // Applying the same place yields exactly the previewed state.
        let _ = place_block(&mut s, p, target, OAK_LOG, Direction::East, 0.5, 0.0, 1);
        assert_eq!(block_at(&s, target), Some(preview));
    }

    #[tokio::test]
    async fn preview_placement_falls_back_to_the_held_state_for_a_simple_cube() {
        // A simple cube (stone, 1) has no placement-derived properties, so the
        // preview is the held state unchanged.
        let p = player("cube-previewer");
        let s = shard_with_player(p).await;
        let target = BlockPos::new(8, 65, 8);
        let preview = s.preview_placement(
            BlockStateId::new(1),
            Direction::North,
            Vec3::new(0.5, 0.9, 0.5),
            200.0,
            target,
        );
        assert_eq!(preview, BlockStateId::new(1));
    }

    #[tokio::test]
    async fn place_simple_cube_is_unchanged() {
        let p = player("mason");
        let mut s = shard_with_player(p).await;
        let target = BlockPos::new(8, 65, 8);
        // Stone (1) is a simple cube: the placement inputs never alter it.
        let _ = place_block(&mut s, p, target, 1, Direction::North, 0.9, 200.0, 1);
        assert_eq!(block_at(&s, target), Some(BlockStateId::new(1)));
    }

    #[tokio::test]
    async fn place_fence_connects_to_neighbor_and_broadcasts_the_update() {
        let p = player("fencer");
        let mut s = shard_with_player(p).await;

        // Place an isolated fence at A: all sides disconnected (6027).
        let a = BlockPos::new(8, 65, 8);
        let _ = place_block(&mut s, p, a, OAK_FENCE, Direction::Up, 0.5, 0.0, 1);
        assert_eq!(block_at(&s, a), Some(BlockStateId::new(6027)));

        // Place a second fence at B, one east of A. B connects west to A (6026),
        // and A is recomputed to connect east to B (6011).
        let b = BlockPos::new(9, 65, 8);
        let outputs = place_block(&mut s, p, b, OAK_FENCE, Direction::Up, 0.5, 0.0, 2);
        assert_eq!(block_at(&s, b), Some(BlockStateId::new(6026)));
        assert_eq!(block_at(&s, a), Some(BlockStateId::new(6011)));

        // The actor's own place is acked (PlayerCreative); the neighbour update at A
        // is broadcast under a non-acking Command cause.
        assert!(outputs.contains(&GameOutput::BlockChanged {
            position: b,
            state: BlockStateId::new(6026),
            sequence: 2,
            cause: MutationCause::PlayerCreative { player: p },
        }));
        assert!(outputs.contains(&GameOutput::BlockChanged {
            position: a,
            state: BlockStateId::new(6011),
            sequence: 0,
            cause: MutationCause::Command,
        }));
    }
}
