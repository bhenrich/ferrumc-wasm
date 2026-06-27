//! Messages crossing the simulation boundary.
//!
//! [`GameInput`] flows *into* a shard — upstream the session router produces it
//! from validated network events — and is applied only at tick boundaries.
//! [`GameOutput`] flows *out* of a shard after a tick and is routed back to
//! sessions/network. The simulation never touches sockets; it only exchanges
//! these typed messages.

use ferrumc_core::{GameMode, PlayerId};
use ferrumc_math::{BlockPos, Vec3};
use ferrumc_world::BlockStateId;

use crate::mutation::MutationCause;

/// An input applied to the simulation at the next tick boundary.
///
/// Inputs are intentionally minimal for this milestone: player presence and
/// movement. The enum is `#[non_exhaustive]` because new variants will be added
/// as the simulation grows, so downstream `match`es must include a wildcard arm.
///
/// `PartialEq` (but not `Eq`) is derived because positions are floating-point
/// [`Vec3`]s.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GameInput {
    /// A player entered the shard.
    PlayerJoin {
        /// Identity of the joining player.
        player: PlayerId,
        /// World-space position the player joins at.
        position: Vec3,
    },
    /// A player moved.
    PlayerMove {
        /// Identity of the moving player.
        player: PlayerId,
        /// New world-space position.
        position: Vec3,
    },
    /// A player left the shard.
    PlayerLeave {
        /// Identity of the leaving player.
        player: PlayerId,
    },
    /// A player broke (destroyed) the block at `position`.
    ///
    /// Decoded upstream from a serverbound `PlayerAction` "start destroying
    /// block". The simulation validates the edit at the tick boundary
    /// (actor present, target chunk resident, target within reach) and, on
    /// acceptance, replaces the block with [`BlockStateId::AIR`]. Held-tool and
    /// drop rules are out of scope this milestone.
    BlockBreak {
        /// Identity of the player breaking the block.
        player: PlayerId,
        /// Absolute position of the block to break.
        position: BlockPos,
        /// The block-action sequence the client stamped on the originating
        /// `PlayerAction`, echoed back in an `AcknowledgeBlockChange` on accept.
        sequence: i32,
    },
    /// A player placed a block at `position`.
    ///
    /// Decoded upstream from a serverbound `UseItemOn`; `position` is the block
    /// adjacent to the clicked face. The simulation validates the edit at the
    /// tick boundary (the same checks as [`BlockBreak`](GameInput::BlockBreak))
    /// and, on acceptance, sets a fixed default block — held-item rules are out
    /// of scope this milestone.
    BlockPlace {
        /// Identity of the player placing the block.
        player: PlayerId,
        /// Absolute position the block is placed at.
        position: BlockPos,
        /// The block-action sequence the client stamped on the originating
        /// `UseItemOn`, echoed back in an `AcknowledgeBlockChange` on accept.
        sequence: i32,
    },
    /// Set the authoritative server-side game mode of a player.
    ///
    /// Produced by the app's `/gamemode` command path (in addition to the
    /// clientbound `GameEvent` that switches the client visually) so the
    /// simulation owns the mode that later enforcement reads (creative
    /// no-decrement, block-break speed, flight). Applied at the tick boundary;
    /// a `SetGameMode` for an absent player is a silent no-op. Emits no output.
    SetGameMode {
        /// Identity of the player whose mode changes.
        player: PlayerId,
        /// The new authoritative game mode.
        mode: GameMode,
    },
}

/// An output produced by the simulation during a tick.
///
/// Outputs are deterministic given the inbox contents: identical input
/// sequences yield identical output sequences. The enum is `#[non_exhaustive]`
/// for the same forward-compatibility reasons as [`GameInput`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum GameOutput {
    /// A player became present in the shard.
    PlayerSpawned {
        /// Identity of the spawned player.
        player: PlayerId,
        /// World-space position the player spawned at.
        position: Vec3,
    },
    /// A player's position changed.
    PlayerMoved {
        /// Identity of the moved player.
        player: PlayerId,
        /// New world-space position.
        position: Vec3,
    },
    /// A player's movement was rejected; the client should snap back to this
    /// authoritative position.
    ///
    /// Emitted at the tick boundary when a [`GameInput::PlayerMove`] carried
    /// non-finite or out-of-range coordinates and no valid move superseded it in
    /// the same tick. Carries the player's last accepted position so the session
    /// layer can send a correction to the desynced client.
    PlayerPositionCorrected {
        /// Identity of the player to correct.
        player: PlayerId,
        /// Authoritative position the client must return to.
        position: Vec3,
    },
    /// A player was removed from the shard.
    PlayerDespawned {
        /// Identity of the despawned player.
        player: PlayerId,
    },
    /// The block at `position` changed to `state`.
    ///
    /// Emitted at the tick boundary after the simulation applies an accepted
    /// block break (`state` is [`BlockStateId::AIR`]) or place. The session
    /// layer broadcasts it to viewers in range as a clientbound `BlockUpdate`
    /// and, for a [`MutationCause::PlayerCreative`] edit, echoes `sequence` back
    /// to the acting player as an `AcknowledgeBlockChange`.
    BlockChanged {
        /// Absolute position of the changed block.
        position: BlockPos,
        /// The block's new state.
        state: BlockStateId,
        /// The originating block-action sequence to acknowledge to the actor.
        sequence: i32,
        /// What caused the mutation (carries the acting player for the ack).
        cause: MutationCause,
    },
    /// A player's block edit was rejected; the actor's predicted change must heal
    /// to the authoritative state.
    ///
    /// Emitted at the tick boundary when an edit that has a client to heal is
    /// refused — the actor is present and the target chunk is resident, so the
    /// authoritative state is readable (e.g. the target is out of reach). The
    /// session layer sends only the actor a `BlockUpdate` carrying
    /// `authoritative_state` at `position` *followed by* an
    /// `AcknowledgeBlockChange` echoing `sequence`: the `BlockUpdate` sets the
    /// client's known server state and the ack ends its pending prediction so
    /// that state is what it displays. The ack is what actually reverts the ghost
    /// block on a real 1.21.8 client (a `BlockUpdate` alone is swallowed while a
    /// prediction is pending), so it is mandatory on reject just as on accept.
    /// Nothing is broadcast (viewers never saw the predicted change). Rejections
    /// with no client to heal (absent actor, unloaded chunk) emit nothing.
    BlockChangeRejected {
        /// The player whose predicted edit must be undone.
        player: PlayerId,
        /// Absolute position of the rejected edit.
        position: BlockPos,
        /// The originating block-action sequence to acknowledge to the actor, so
        /// its pending client-side prediction ends and reverts to the
        /// authoritative state.
        sequence: i32,
        /// The state the client optimistically predicted (air for a break, the
        /// placed block for a place); used by the app to classify the metric.
        requested_state: BlockStateId,
        /// The authoritative state to resync the actor to.
        authoritative_state: BlockStateId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_compare_by_value() {
        let p = PlayerId::offline("notch");
        let a = GameInput::PlayerJoin {
            player: p,
            position: Vec3::new(1.0, 2.0, 3.0),
        };
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(
            a,
            GameInput::PlayerMove {
                player: p,
                position: Vec3::new(1.0, 2.0, 3.0),
            }
        );
    }

    #[test]
    fn outputs_compare_by_value() {
        let p = PlayerId::offline("jeb_");
        let spawned = GameOutput::PlayerSpawned {
            player: p,
            position: Vec3::ZERO,
        };
        assert_eq!(spawned.clone(), spawned);
        assert_ne!(spawned, GameOutput::PlayerDespawned { player: p });
    }
}
