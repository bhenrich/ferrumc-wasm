//! [`DisconnectReason`] and [`DisconnectPolicy`]: why a play connection is torn
//! down, and how.

use crate::error::DisconnectClass;

/// How a play connection should be closed once a [`DisconnectReason`] is decided.
///
/// The reason determines the policy via [`DisconnectReason::policy`]: a clean,
/// server-initiated close tries to deliver a final message, while a hostile or
/// broken peer is dropped without ceremony.
///
/// The enum is `#[non_exhaustive]`: new policies may be added without a breaking
/// change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DisconnectPolicy {
    /// Drop the socket immediately without attempting to flush queued frames.
    /// Used for hostile or already-broken peers where further writes are
    /// pointless or unsafe.
    Immediate,
    /// Best-effort flush the [`Critical`](crate::OutboundPriority::Critical)
    /// queue (so a final Disconnect frame can reach the client) before closing.
    /// Used for clean, server-initiated closes.
    FlushThenClose,
}

/// Why a play connection is being disconnected.
///
/// This classifies the *cause* so the connection layer can pick a
/// [`DisconnectPolicy`], record a metric, and (later) select a user-facing kick
/// message. It deliberately separates hostile/broken causes — which map to an
/// [`Immediate`](DisconnectPolicy::Immediate) drop — from clean,
/// server-initiated ones that warrant a [`FlushThenClose`](DisconnectPolicy::FlushThenClose).
///
/// The enum is `#[non_exhaustive]`: new reasons may be added without a breaking
/// change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// The peer broke the protocol contract (an unknown packet for the state, or
    /// trailing bytes after a complete packet).
    ProtocolViolation,
    /// A frame or packet body was structurally malformed (bad length `VarInt`,
    /// negative length, or an undecodable body).
    MalformedPacket,
    /// The peer advertised or accumulated more bytes than a size cap allows — a
    /// resource-exhaustion attempt.
    FrameTooLarge,
    /// The peer sustained a serverbound frame rate above its
    /// [`PacketBudget`](crate::PacketBudget).
    BudgetExceeded,
    /// An outbound priority queue overflowed in a way the caller deemed fatal
    /// (typically the [`Critical`](crate::OutboundPriority::Critical) queue):
    /// the client cannot keep up with must-deliver traffic.
    OutboundOverflow,
    /// The client missed the keep-alive deadline.
    KeepAliveTimeout,
    /// The server is shutting down and is closing connections cooperatively.
    ServerShutdown,
    /// An operator, plugin, or game-logic action kicked the player.
    Kicked,
}

impl DisconnectReason {
    /// The [`DisconnectPolicy`] this reason calls for.
    ///
    /// Clean, server-initiated reasons ([`ServerShutdown`](Self::ServerShutdown),
    /// [`Kicked`](Self::Kicked), [`KeepAliveTimeout`](Self::KeepAliveTimeout))
    /// flush so a final message can be delivered; hostile or broken reasons drop
    /// immediately.
    pub fn policy(self) -> DisconnectPolicy {
        match self {
            Self::ServerShutdown | Self::Kicked | Self::KeepAliveTimeout => {
                DisconnectPolicy::FlushThenClose
            }
            Self::ProtocolViolation
            | Self::MalformedPacket
            | Self::FrameTooLarge
            | Self::BudgetExceeded
            | Self::OutboundOverflow => DisconnectPolicy::Immediate,
        }
    }

    /// `true` when this reason reflects hostile or broken peer behaviour (as
    /// opposed to a clean, server-initiated close).
    pub fn is_peer_fault(self) -> bool {
        matches!(
            self,
            Self::ProtocolViolation
                | Self::MalformedPacket
                | Self::FrameTooLarge
                | Self::BudgetExceeded
        )
    }

    /// Maps a decode-layer [`DisconnectClass`] onto the matching reason.
    ///
    /// Lets the connection layer turn an inbound framing/decode failure straight
    /// into a disconnect reason. [`DisconnectClass`] is `#[non_exhaustive]`, so
    /// adding a class there surfaces here as a compile error — the deliberate
    /// signal to extend this mapping rather than silently defaulting.
    pub fn from_disconnect_class(class: DisconnectClass) -> Self {
        match class {
            DisconnectClass::FrameTooLarge => Self::FrameTooLarge,
            DisconnectClass::Malformed => Self::MalformedPacket,
            DisconnectClass::ProtocolViolation => Self::ProtocolViolation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_reasons_flush_before_close() {
        assert_eq!(
            DisconnectReason::ServerShutdown.policy(),
            DisconnectPolicy::FlushThenClose
        );
        assert_eq!(
            DisconnectReason::Kicked.policy(),
            DisconnectPolicy::FlushThenClose
        );
        assert_eq!(
            DisconnectReason::KeepAliveTimeout.policy(),
            DisconnectPolicy::FlushThenClose
        );
    }

    #[test]
    fn hostile_reasons_drop_immediately() {
        for reason in [
            DisconnectReason::ProtocolViolation,
            DisconnectReason::MalformedPacket,
            DisconnectReason::FrameTooLarge,
            DisconnectReason::BudgetExceeded,
            DisconnectReason::OutboundOverflow,
        ] {
            assert_eq!(reason.policy(), DisconnectPolicy::Immediate);
        }
    }

    #[test]
    fn peer_fault_excludes_server_initiated_reasons() {
        assert!(DisconnectReason::BudgetExceeded.is_peer_fault());
        assert!(DisconnectReason::ProtocolViolation.is_peer_fault());
        assert!(!DisconnectReason::ServerShutdown.is_peer_fault());
        assert!(!DisconnectReason::Kicked.is_peer_fault());
        // OutboundOverflow is a delivery failure, not classed as a peer fault.
        assert!(!DisconnectReason::OutboundOverflow.is_peer_fault());
    }

    #[test]
    fn disconnect_class_maps_to_reason() {
        assert_eq!(
            DisconnectReason::from_disconnect_class(DisconnectClass::FrameTooLarge),
            DisconnectReason::FrameTooLarge
        );
        assert_eq!(
            DisconnectReason::from_disconnect_class(DisconnectClass::Malformed),
            DisconnectReason::MalformedPacket
        );
        assert_eq!(
            DisconnectReason::from_disconnect_class(DisconnectClass::ProtocolViolation),
            DisconnectReason::ProtocolViolation
        );
    }
}
