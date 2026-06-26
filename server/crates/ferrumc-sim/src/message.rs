//! Messages crossing the simulation boundary.
//!
//! [`GameInput`] flows *into* a shard — upstream the session router produces it
//! from validated network events — and is applied only at tick boundaries.
//! [`GameOutput`] flows *out* of a shard after a tick and is routed back to
//! sessions/network. The simulation never touches sockets; it only exchanges
//! these typed messages.

use ferrumc_core::PlayerId;
use ferrumc_math::Vec3;

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
    /// A player was removed from the shard.
    PlayerDespawned {
        /// Identity of the despawned player.
        player: PlayerId,
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
