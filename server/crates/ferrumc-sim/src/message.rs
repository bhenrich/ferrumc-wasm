//! Messages crossing the simulation boundary.
//!
//! [`GameInput`] flows *into* a shard — upstream the session router produces it
//! from validated network events — and is applied only at tick boundaries.
//! [`GameOutput`] flows *out* of a shard after a tick and is routed back to
//! sessions/network. The simulation never touches sockets; it only exchanges
//! these typed messages.

use ferrumc_core::PlayerId;
use ferrumc_math::{BlockPos, Vec3};
use ferrumc_world::BlockStateId;

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
    /// layer broadcasts it to viewers in range as a clientbound `BlockUpdate`.
    /// A rejected edit produces no such output.
    BlockChanged {
        /// Absolute position of the changed block.
        position: BlockPos,
        /// The block's new state.
        state: BlockStateId,
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
