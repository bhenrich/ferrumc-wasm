//! [`ConnectionState`]: the protocol state a connection is currently in.

use ferrumc_proto::State;

/// The protocol phase a single connection is in.
///
/// A connection advances through these phases in a fixed lifecycle:
///
/// ```text
/// Handshaking ──▶ Status                 (server-list ping; connection closes after)
///             └─▶ Login ──▶ Configuration ──▶ Play
/// ```
///
/// The handshake's `next_state` field selects the [`Status`](Self::Status) or
/// [`Login`](Self::Login) branch; the remaining transitions are driven by
/// acknowledgement packets. This crate does not own the transition *policy* —
/// the caller (M09) advances the state and feeds the new value back into the
/// decoder for each subsequent frame. The enum exists so framing can apply the
/// correct per-state limits and the correct per-state packet dispatch.
///
/// [`Play`](Self::Play) is modelled for the state machine and frame-size limits,
/// but no typed play packets exist yet (see [`crate::InboundPacket::Play`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ConnectionState {
    /// The initial state every connection opens in; the client immediately
    /// sends a single `Handshake` selecting the next branch.
    #[default]
    Handshaking,
    /// Server-list ping: the client requests status and an optional latency
    /// ping, then disconnects.
    Status,
    /// Login negotiation: authentication and (optionally) compression setup.
    Login,
    /// The 1.20.2+ configuration phase between login and play: client settings
    /// and registry/known-pack exchange.
    Configuration,
    /// In-game play. No typed play packets are defined in this milestone.
    Play,
}

impl ConnectionState {
    /// Maps to the corresponding [`ferrumc_proto::State`], or `None` for
    /// [`Play`](Self::Play), which `ferrumc-proto` does not model yet.
    ///
    /// The names differ deliberately: this crate calls the first phase
    /// `Handshaking` (matching the Minecraft client's own naming) while
    /// `ferrumc-proto` calls it `Handshake`.
    pub fn to_proto_state(self) -> Option<State> {
        match self {
            Self::Handshaking => Some(State::Handshake),
            Self::Status => Some(State::Status),
            Self::Login => Some(State::Login),
            Self::Configuration => Some(State::Configuration),
            Self::Play => None,
        }
    }

    /// `true` when this is the [`Play`](Self::Play) state, whose frames carry no
    /// typed packet in this milestone.
    pub fn is_play(self) -> bool {
        matches!(self, Self::Play)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_handshaking() {
        assert_eq!(ConnectionState::default(), ConnectionState::Handshaking);
    }

    #[test]
    fn proto_state_mapping_is_complete_for_modelled_states() {
        assert_eq!(
            ConnectionState::Handshaking.to_proto_state(),
            Some(State::Handshake)
        );
        assert_eq!(
            ConnectionState::Status.to_proto_state(),
            Some(State::Status)
        );
        assert_eq!(ConnectionState::Login.to_proto_state(), Some(State::Login));
        assert_eq!(
            ConnectionState::Configuration.to_proto_state(),
            Some(State::Configuration)
        );
    }

    #[test]
    fn play_has_no_proto_state() {
        assert_eq!(ConnectionState::Play.to_proto_state(), None);
        assert!(ConnectionState::Play.is_play());
        assert!(!ConnectionState::Handshaking.is_play());
    }
}
