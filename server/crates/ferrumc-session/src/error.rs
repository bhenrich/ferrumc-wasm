//! Error types for the session router.

use ferrumc_core::{PlayerId, ServerError};
use ferrumc_math::ShardPos;

/// A classifying error returned by [`SessionRouter`](crate::SessionRouter)
/// operations.
///
/// Each variant names the *kind* of failure so callers can react
/// programmatically rather than parse message strings, per the project's
/// error-classification rule. Convert one into the server-wide [`ServerError`]
/// with the provided [`From`] impl.
///
/// The enum is `#[non_exhaustive]`: more variants will appear as the router
/// grows, so downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SessionError {
    /// An operation referenced a player that has no active session (never joined
    /// or already disconnected).
    #[error("no active session for player {player}")]
    UnknownPlayer {
        /// The player that has no session.
        player: PlayerId,
    },

    /// A join was attempted for a player that already has an active session.
    ///
    /// The join is rejected and the existing session is left untouched; the
    /// caller must disconnect the old session first if a re-join is intended.
    #[error("player {player} already has an active session")]
    DuplicatePlayer {
        /// The player that is already connected.
        player: PlayerId,
    },

    /// No shard is registered to own the target position's shard, so there is
    /// nowhere to route the player's inputs.
    #[error("no shard registered at ({}, {})", shard.x(), shard.z())]
    UnknownShard {
        /// The shard position that has no registered input channel.
        shard: ShardPos,
    },

    /// A shard's bounded input channel was at capacity and rejected the input.
    ///
    /// This is *reject* backpressure mirroring the simulation inbox: the router
    /// never blocks (it must not stall the tick loop) and never silently drops
    /// (that would desync clients). The caller decides what to do — retry next
    /// tick, coalesce, or disconnect a flooding client.
    #[error("input channel for shard ({}, {}) is full", shard.x(), shard.z())]
    ShardInboxFull {
        /// The shard whose input channel overflowed.
        shard: ShardPos,
    },

    /// A shard's input channel is closed because the shard worker is gone.
    #[error("input channel for shard ({}, {}) is closed", shard.x(), shard.z())]
    ShardClosed {
        /// The shard whose input channel is closed.
        shard: ShardPos,
    },

    /// A player's bounded outbound channel was at capacity and rejected the
    /// packet.
    ///
    /// Like the shard inbox this is reject backpressure. A persistently full
    /// outbound queue means the client cannot keep up with must-deliver traffic;
    /// the caller may escalate that to a disconnect (mirroring the network
    /// layer's `OutboundOverflow`).
    #[error("outbound channel for player {player} is full")]
    OutboundFull {
        /// The player whose outbound channel overflowed.
        player: PlayerId,
    },

    /// A player's outbound channel is closed because the connection task is gone.
    #[error("outbound channel for player {player} is closed")]
    OutboundClosed {
        /// The player whose outbound channel is closed.
        player: PlayerId,
    },
}

impl From<SessionError> for ServerError {
    fn from(error: SessionError) -> Self {
        match error {
            SessionError::UnknownPlayer { player } => {
                ServerError::not_found(format!("session for player {player}"))
            }
            SessionError::UnknownShard { shard } => ServerError::not_found(format!(
                "registered shard at ({}, {})",
                shard.x(),
                shard.z()
            )),
            SessionError::DuplicatePlayer { player } => {
                ServerError::invalid_state(format!("player {player} already has a session"))
            }
            SessionError::ShardClosed { shard } => ServerError::invalid_state(format!(
                "shard ({}, {}) input channel closed",
                shard.x(),
                shard.z()
            )),
            SessionError::OutboundClosed { player } => {
                ServerError::invalid_state(format!("player {player} outbound channel closed"))
            }
            SessionError::ShardInboxFull { shard } => ServerError::capacity(format!(
                "shard ({}, {}) input channel full",
                shard.x(),
                shard.z()
            )),
            SessionError::OutboundFull { player } => {
                ServerError::capacity(format!("player {player} outbound channel full"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player() -> PlayerId {
        PlayerId::offline("notch")
    }

    #[test]
    fn display_strings_are_classified() {
        assert_eq!(
            SessionError::UnknownPlayer { player: player() }.to_string(),
            format!("no active session for player {}", player())
        );
        assert_eq!(
            SessionError::UnknownShard {
                shard: ShardPos::new(-1, 2)
            }
            .to_string(),
            "no shard registered at (-1, 2)"
        );
        assert_eq!(
            SessionError::ShardInboxFull {
                shard: ShardPos::new(0, 0)
            }
            .to_string(),
            "input channel for shard (0, 0) is full"
        );
        assert_eq!(
            SessionError::OutboundFull { player: player() }.to_string(),
            format!("outbound channel for player {} is full", player())
        );
    }

    #[test]
    fn unknown_player_maps_to_not_found() {
        let server: ServerError = SessionError::UnknownPlayer { player: player() }.into();
        assert!(matches!(server, ServerError::NotFound(_)));
    }

    #[test]
    fn full_channels_map_to_capacity() {
        let shard: ServerError = SessionError::ShardInboxFull {
            shard: ShardPos::new(1, 1),
        }
        .into();
        assert!(matches!(shard, ServerError::Capacity(_)));
        let outbound: ServerError = SessionError::OutboundFull { player: player() }.into();
        assert!(matches!(outbound, ServerError::Capacity(_)));
    }

    #[test]
    fn closed_and_duplicate_map_to_invalid_state() {
        let closed: ServerError = SessionError::ShardClosed {
            shard: ShardPos::new(0, 0),
        }
        .into();
        assert!(matches!(closed, ServerError::InvalidState(_)));
        let dup: ServerError = SessionError::DuplicatePlayer { player: player() }.into();
        assert!(matches!(dup, ServerError::InvalidState(_)));
    }

    #[test]
    fn variants_compare_by_value() {
        assert_eq!(
            SessionError::UnknownPlayer { player: player() },
            SessionError::UnknownPlayer { player: player() }
        );
        assert_ne!(
            SessionError::ShardInboxFull {
                shard: ShardPos::new(0, 0)
            },
            SessionError::ShardClosed {
                shard: ShardPos::new(0, 0)
            }
        );
    }
}
