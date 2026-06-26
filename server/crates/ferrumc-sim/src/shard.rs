//! A single simulation shard: bounded inbox in, outputs out, at tick
//! boundaries.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroUsize;

use ferrumc_core::{DimensionId, PlayerId, WorldId};
use ferrumc_math::{BlockPos, ShardPos, Vec3};
use ferrumc_world::BlockStateId;

use crate::error::SimError;
use crate::loaded::LoadedChunkMap;
use crate::message::{GameInput, GameOutput};

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

/// The block-state the simulation writes for an accepted
/// [`GameInput::BlockPlace`].
///
/// This milestone ignores held-item and tool rules, so every place drops the
/// same fixed block: `minecraft:stone` (block-state id `1` in the pinned
/// flat-world registry). A break is the inverse, writing [`BlockStateId::AIR`].
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

/// Per-player state owned exclusively by the shard.
#[derive(Debug, Clone, Copy)]
struct PlayerState {
    position: Vec3,
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
/// [`BlockStateId::AIR`]; an accepted place writes [`DEFAULT_PLACED_STATE`].
/// Either way the owning section is marked dirty (by
/// [`Chunk::set_block`](ferrumc_world::Chunk::set_block)) and a single
/// [`GameOutput::BlockChanged`] is emitted, in inbox order, for the session
/// layer to broadcast. A rejected edit mutates nothing and emits nothing.
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

    /// Returns `true` if `player` is currently present in the shard.
    pub fn contains_player(&self, player: PlayerId) -> bool {
        self.players.contains_key(&player)
    }

    /// Returns the current position of `player`, or `None` if absent.
    pub fn player_position(&self, player: PlayerId) -> Option<Vec3> {
        self.players.get(&player).map(|state| state.position)
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
    pub fn run_tick(&mut self) -> Vec<GameOutput> {
        let mut outputs = Vec::new();
        // Coalesce movement: keep only the latest *valid* position per player.
        let mut pending_moves: BTreeMap<PlayerId, Vec3> = BTreeMap::new();
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
                        slot.insert(PlayerState { position });
                        outputs.push(GameOutput::PlayerSpawned { player, position });
                    }
                }
                GameInput::PlayerMove { player, position } => {
                    // Movement for an unknown player is ignored rather than
                    // implicitly spawning one; there is also nothing to correct.
                    if !self.players.contains_key(&player) {
                        continue;
                    }
                    if is_valid_position(position) {
                        // Coalesce: overwrite any earlier move this tick. A valid
                        // move also clears a pending correction — it supersedes
                        // the rejected one.
                        pending_moves.insert(player, position);
                        corrections.remove(&player);
                    } else if !pending_moves.contains_key(&player) {
                        // Reject out-of-range / non-finite coords. Request a
                        // correction only if no valid move is queued to override
                        // the client's bad position.
                        corrections.insert(player);
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
                GameInput::BlockBreak { player, position } => {
                    // Break -> air. Applied in FIFO order during the drain so the
                    // BlockChanged output keeps the inbox ordering.
                    if let Some(output) = self.apply_block_edit(player, position, BlockStateId::AIR)
                    {
                        outputs.push(output);
                    }
                }
                GameInput::BlockPlace { player, position } => {
                    // Place -> the fixed default block (no held-item rules yet).
                    if let Some(output) =
                        self.apply_block_edit(player, position, DEFAULT_PLACED_STATE)
                    {
                        outputs.push(output);
                    }
                }
            }
        }

        // Apply the coalesced moves at the boundary, in deterministic player
        // order. Every player here was present at coalesce time and cannot have
        // left (a leave clears the entry), so the lookup always succeeds.
        for (player, position) in pending_moves {
            if let Some(state) = self.players.get_mut(&player) {
                state.position = position;
                outputs.push(GameOutput::PlayerMoved { player, position });
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

    /// Validates and applies a single block edit at the tick boundary.
    ///
    /// Returns the [`GameOutput::BlockChanged`] to broadcast on acceptance, or
    /// `None` if the edit is rejected. An edit is rejected when:
    /// - the actor is not present in the shard;
    /// - the target is beyond [`MAX_REACH`] of the actor's position;
    /// - the target chunk is not resident in this shard (which also covers a
    ///   block in another dimension, since the shard's map is dimension-scoped);
    ///   or
    /// - the target `y` is outside the buildable range (rejected by
    ///   [`Chunk::set_block`](ferrumc_world::Chunk::set_block) without panic).
    ///
    /// A rejected edit never mutates chunk state. On acceptance the owning
    /// section is marked dirty by `set_block`, so the chunk is included in the
    /// next dirty-chunk drain.
    fn apply_block_edit(
        &mut self,
        player: PlayerId,
        position: BlockPos,
        state: BlockStateId,
    ) -> Option<GameOutput> {
        let actor = self.players.get(&player)?.position;
        if !within_reach(actor, position) {
            return None;
        }
        let chunk = self.chunks.get_mut(position.to_chunk_pos())?;
        // The chunk was looked up by `position`'s own column, so `set_block` can
        // only fail on an out-of-range `y`; treat that as a rejected edit rather
        // than mutating or panicking.
        chunk.set_block(position, state).ok()?;
        Some(GameOutput::BlockChanged { position, state })
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
            position: Vec3::new(5.0, 0.0, 0.0),
        })
        .expect("room");
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: Vec3::new(9.0, 0.0, 0.0),
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
                    position: Vec3::new(9.0, 0.0, 0.0)
                },
            ]
        );
        assert_eq!(s.player_position(p), Some(Vec3::new(9.0, 0.0, 0.0)));
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
                position: bad,
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
            position: Vec3::new(f64::NAN, 0.0, 0.0),
        })
        .expect("room");
        s.enqueue(GameInput::PlayerMove {
            player: p,
            position: Vec3::new(3.0, 4.0, 5.0),
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::PlayerMoved {
                player: p,
                position: Vec3::new(3.0, 4.0, 5.0),
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
            position: edge,
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::PlayerMoved {
                player: p,
                position: edge,
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
            position: Vec3::new(2.0, 0.0, 0.0),
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
            position: Vec3::new(f64::NAN, 0.0, 0.0),
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
            position: Vec3::new(1.0, 1.0, 1.0),
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
            position: Vec3::new(1.0, 0.0, 0.0),
        })
        .expect("second");
        assert!(s.is_inbox_full());

        // Third is rejected with a classified error; inbox is left untouched.
        let err = s
            .enqueue(GameInput::PlayerMove {
                player: p,
                position: Vec3::new(2.0, 0.0, 0.0),
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
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::BlockChanged {
                position: target,
                state: BlockStateId::AIR,
            }]
        );
        // The chunk reflects the break and the owning section is now dirty.
        assert_eq!(block_at(&s, target), Some(BlockStateId::AIR));
        assert!(s
            .loaded_chunks()
            .get(chunk)
            .expect("resident")
            .dirty_sections()
            .any());
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
        })
        .expect("room");
        let outputs = s.run_tick();
        assert_eq!(
            outputs,
            vec![GameOutput::BlockChanged {
                position: target,
                state: DEFAULT_PLACED_STATE,
            }]
        );
        assert_eq!(block_at(&s, target), Some(DEFAULT_PLACED_STATE));
    }

    #[tokio::test]
    async fn break_in_unloaded_chunk_is_rejected() {
        // No chunk resident: an otherwise in-reach break is rejected.
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
        })
        .expect("room");
        assert!(s.run_tick().is_empty());
    }

    #[tokio::test]
    async fn edit_out_of_reach_is_rejected_even_when_chunk_is_loaded() {
        // Block (100, 63, 8) lives in chunk (6, 0); load exactly that chunk so
        // the only reason to reject is the ~92-block distance from spawn.
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

        s.enqueue(GameInput::BlockBreak {
            player: p,
            position: target,
        })
        .expect("room");
        assert!(s.run_tick().is_empty());
        // The block is untouched (still the generated surface, not air).
        assert_ne!(block_at(&s, target), Some(BlockStateId::AIR));
    }

    #[tokio::test]
    async fn block_edit_for_absent_player_is_ignored() {
        let chunk = ChunkPos::new(0, 0);
        let mut s = shard_with_loaded_chunk(chunk).await;
        let ghost = player("ghost");
        s.enqueue(GameInput::BlockBreak {
            player: ghost,
            position: BlockPos::new(8, 63, 8),
        })
        .expect("room");
        s.enqueue(GameInput::BlockPlace {
            player: ghost,
            position: BlockPos::new(8, 65, 8),
        })
        .expect("room");
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
        })
        .expect("room");
        // Enqueued but not yet applied: the block is unchanged until the tick.
        assert_ne!(block_at(&s, target), Some(BlockStateId::AIR));
        let _ = s.run_tick();
        assert_eq!(block_at(&s, target), Some(BlockStateId::AIR));
    }
}
