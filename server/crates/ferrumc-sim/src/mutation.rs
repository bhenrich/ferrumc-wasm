//! The cause and structured outcome of a block mutation.
//!
//! Every block write funnels through
//! [`SimShard::apply_block_edit`](crate::SimShard) carrying a [`MutationCause`]
//! (who asked) and returning a [`MutationResult`] (what happened). Tagging each
//! mutation with its cause lets the session layer ack/resync the right player and
//! lets future auditing/metrics attribute the change.

use ferrumc_core::PlayerId;
use ferrumc_math::BlockPos;
use ferrumc_world::BlockStateId;

/// Who requested a block mutation.
///
/// Only [`PlayerCreative`](MutationCause::PlayerCreative) is produced this
/// milestone; the command/plugin/test variants reserve the other sources so the
/// funnel signature is stable as they come online. The acting player rides the
/// player variant so the session layer can target the ack (on accept) or the
/// resync (on reject) at exactly that player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MutationCause {
    /// A creative-mode player break/place.
    PlayerCreative {
        /// The player who requested the edit.
        player: PlayerId,
    },
    /// A command execution (e.g. `/setblock`).
    Command,
    /// A plugin submission via a command sink.
    Plugin,
    /// A test or replay harness.
    Test,
}

/// An accepted block mutation captured for the storage journal.
///
/// Buffered by [`SimShard`](crate::SimShard) on every non-[`Test`](MutationCause::Test)
/// accepted edit and drained each tick by the driver, which stamps it with a tick
/// and a monotonic id to build a `BlockMutationLogRecord`. It records the cause,
/// the block position, and the before/after states so a future crash-replay can
/// re-apply (or undo) the edit. Carries no tick/id itself: those are assigned at
/// drain time so the deterministic sim never reads a clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingMutation {
    cause: MutationCause,
    position: BlockPos,
    old_state: BlockStateId,
    new_state: BlockStateId,
}

impl PendingMutation {
    /// Builds a pending mutation record.
    pub(crate) fn new(
        cause: MutationCause,
        position: BlockPos,
        old_state: BlockStateId,
        new_state: BlockStateId,
    ) -> Self {
        Self {
            cause,
            position,
            old_state,
            new_state,
        }
    }

    /// Returns who caused the mutation.
    #[must_use]
    pub fn cause(&self) -> MutationCause {
        self.cause
    }

    /// Returns the mutated block position.
    #[must_use]
    pub fn position(&self) -> BlockPos {
        self.position
    }

    /// Returns the block state before the mutation.
    #[must_use]
    pub fn old_state(&self) -> BlockStateId {
        self.old_state
    }

    /// Returns the block state after the mutation.
    #[must_use]
    pub fn new_state(&self) -> BlockStateId {
        self.new_state
    }
}

/// Why a block mutation was rejected.
///
/// Kept internal to the shard: the structured rejection is mapped to a
/// [`GameOutput`](crate::GameOutput) at the tick boundary, so nothing outside the
/// crate names these reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RejectionReason {
    /// The acting player is not present in the shard.
    ActorAbsent,
    /// The target block is beyond the actor's reach.
    OutOfReach,
    /// The target chunk is not resident in this shard.
    ChunkNotLoaded,
    /// The target `y` is outside the buildable range.
    YOutOfBounds,
}

/// The structured outcome of a single `apply_block_edit` call.
///
/// Internal to the shard (`pub(crate)`): the tick boundary maps it to the public
/// [`GameOutput`](crate::GameOutput) carriers. Kept un-exported deliberately so
/// the app never confuses it with the observability crate's like-named
/// `MutationResult` (accepted/rejected metric label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationResult {
    /// The block changed; `set_block` marked its section dirty.
    Applied {
        /// The block's new state, echoed to the actor and broadcast to viewers.
        new_state: BlockStateId,
    },
    /// The edit was refused; the actor must heal to `authoritative_state` (the
    /// chunk's real block, or air when the chunk is not resident).
    Rejected {
        /// Why the edit was refused.
        reason: RejectionReason,
        /// The authoritative state the desynced client must snap back to.
        authoritative_state: BlockStateId,
    },
}
