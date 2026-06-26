//! A single simulation shard: bounded inbox in, outputs out, at tick
//! boundaries.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;

use ferrumc_core::PlayerId;
use ferrumc_math::{ShardPos, Vec3};

use crate::error::SimError;
use crate::message::{GameInput, GameOutput};

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

/// Per-player state owned exclusively by the shard.
#[derive(Debug, Clone, Copy)]
struct PlayerState {
    position: Vec3,
}

/// One simulation shard.
///
/// A shard exclusively owns its players and a bounded inbox, applies queued
/// [`GameInput`]s **only** at tick boundaries, and returns the resulting
/// [`GameOutput`]s. This skeleton tracks player presence and position; chunk and
/// entity ownership arrive in later milestones.
///
/// # Tick-boundary application
///
/// [`enqueue`](SimShard::enqueue) only appends to the inbox; it never mutates
/// shard state. State changes happen exclusively inside
/// [`run_tick`](SimShard::run_tick), which drains the whole inbox in FIFO order.
/// An input enqueued after a `run_tick` returns is therefore applied at the
/// *next* tick, never mid-tick.
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
}

impl SimShard {
    /// Creates an empty shard for `shard_pos` with the default inbox capacity.
    pub fn new(shard_pos: ShardPos) -> Self {
        Self::with_inbox_capacity(shard_pos, DEFAULT_INBOX_CAPACITY)
    }

    /// Creates an empty shard for `shard_pos` with an explicit inbox `capacity`.
    ///
    /// `capacity` is a [`NonZeroUsize`] so a zero-capacity (permanently full)
    /// inbox is unrepresentable. The inbox pre-allocates this capacity once and
    /// never grows beyond it.
    pub fn with_inbox_capacity(shard_pos: ShardPos, capacity: NonZeroUsize) -> Self {
        Self {
            shard_pos,
            inbox: VecDeque::with_capacity(capacity.get()),
            inbox_capacity: capacity.get(),
            players: BTreeMap::new(),
        }
    }

    /// Returns the position of this shard in shard coordinates.
    pub const fn shard_pos(&self) -> ShardPos {
        self.shard_pos
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

    /// Applies every queued input in FIFO order and returns the outputs.
    ///
    /// This is the only method that mutates player state, so all queued inputs
    /// take effect exactly at this tick boundary. The inbox is empty on return.
    pub fn run_tick(&mut self) -> Vec<GameOutput> {
        let mut outputs = Vec::new();
        while let Some(input) = self.inbox.pop_front() {
            self.apply(&input, &mut outputs);
        }
        outputs
    }

    /// Applies a single input, pushing any resulting outputs.
    ///
    /// Takes the input by reference; every field it reads is `Copy`, so no
    /// ownership is required.
    fn apply(&mut self, input: &GameInput, outputs: &mut Vec<GameOutput>) {
        match *input {
            GameInput::PlayerJoin { player, position } => {
                // A duplicate join for an already-present player is ignored: the
                // first join wins and re-joining produces no output, keeping the
                // result deterministic regardless of upstream retries.
                if let Entry::Vacant(slot) = self.players.entry(player) {
                    slot.insert(PlayerState { position });
                    outputs.push(GameOutput::PlayerSpawned { player, position });
                }
            }
            GameInput::PlayerMove { player, position } => {
                // Movement for an unknown player is ignored rather than
                // implicitly spawning one; only an explicit join adds a player.
                if let Some(state) = self.players.get_mut(&player) {
                    state.position = position;
                    outputs.push(GameOutput::PlayerMoved { player, position });
                }
            }
            GameInput::PlayerLeave { player } => {
                if self.players.remove(&player).is_some() {
                    outputs.push(GameOutput::PlayerDespawned { player });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(name: &str) -> PlayerId {
        PlayerId::offline(name)
    }

    fn shard() -> SimShard {
        SimShard::new(ShardPos::new(0, 0))
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
    fn queued_inputs_apply_in_fifo_order_in_one_tick() {
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
        assert_eq!(
            outputs,
            vec![
                GameOutput::PlayerSpawned {
                    player: p,
                    position: Vec3::ZERO
                },
                GameOutput::PlayerMoved {
                    player: p,
                    position: Vec3::new(5.0, 0.0, 0.0)
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
}
