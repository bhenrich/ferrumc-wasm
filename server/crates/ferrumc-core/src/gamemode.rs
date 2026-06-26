//! Player game modes and their numeric protocol ids.

use crate::error::ServerError;

/// A player's game mode.
///
/// The numeric ids match the Minecraft protocol: `Survival = 0`, `Creative = 1`,
/// `Adventure = 2`, `Spectator = 3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "snake_case")
)]
pub enum GameMode {
    /// Standard survival: limited resources, health, and hunger.
    #[default]
    Survival,
    /// Unlimited resources and flight.
    Creative,
    /// Like survival, but blocks can only be broken with the right tools.
    Adventure,
    /// Non-interactive observer with free flight and no collision.
    Spectator,
}

impl GameMode {
    /// Returns the protocol id for this game mode.
    pub const fn as_id(self) -> u8 {
        match self {
            Self::Survival => 0,
            Self::Creative => 1,
            Self::Adventure => 2,
            Self::Spectator => 3,
        }
    }

    /// Returns the game mode for a protocol `id`, or `None` if it is out of range.
    pub const fn from_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(Self::Survival),
            1 => Some(Self::Creative),
            2 => Some(Self::Adventure),
            3 => Some(Self::Spectator),
            _ => None,
        }
    }
}

impl TryFrom<u8> for GameMode {
    type Error = InvalidGameModeId;

    fn try_from(id: u8) -> core::result::Result<Self, Self::Error> {
        Self::from_id(id).ok_or(InvalidGameModeId(id))
    }
}

/// Error returned when converting an out-of-range numeric id into a [`GameMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid game mode id: {0}")]
pub struct InvalidGameModeId(u8);

impl InvalidGameModeId {
    /// Returns the rejected id.
    pub const fn id(self) -> u8 {
        self.0
    }
}

impl From<InvalidGameModeId> for ServerError {
    fn from(err: InvalidGameModeId) -> Self {
        Self::invalid_state(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_survival() {
        assert_eq!(GameMode::default(), GameMode::Survival);
    }

    #[test]
    fn id_round_trip() {
        for mode in [
            GameMode::Survival,
            GameMode::Creative,
            GameMode::Adventure,
            GameMode::Spectator,
        ] {
            assert_eq!(GameMode::from_id(mode.as_id()), Some(mode));
            assert_eq!(GameMode::try_from(mode.as_id()), Ok(mode));
        }
    }

    #[test]
    fn ids_have_protocol_values() {
        assert_eq!(GameMode::Survival.as_id(), 0);
        assert_eq!(GameMode::Creative.as_id(), 1);
        assert_eq!(GameMode::Adventure.as_id(), 2);
        assert_eq!(GameMode::Spectator.as_id(), 3);
    }

    #[test]
    fn invalid_id_is_rejected() {
        assert_eq!(GameMode::from_id(4), None);
        let err = GameMode::try_from(255).expect_err("255 is not a game mode");
        assert_eq!(err.id(), 255);
        assert_eq!(err.to_string(), "invalid game mode id: 255");
    }

    #[test]
    fn invalid_id_converts_into_server_error() {
        let err: ServerError = GameMode::try_from(9).unwrap_err().into();
        assert!(matches!(err, ServerError::InvalidState(_)));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_rejects_unknown_variant_string() {
        // A valid mode round-trips via its snake_case protocol name...
        assert_eq!(
            serde_json::from_str::<GameMode>("\"creative\"").expect("valid mode"),
            GameMode::Creative
        );
        // ...but an unknown string ("god") is rejected rather than silently
        // defaulting, so bad input never masquerades as a real game mode.
        assert!(
            serde_json::from_str::<GameMode>("\"god\"").is_err(),
            "unknown game mode string must be rejected"
        );
    }
}
