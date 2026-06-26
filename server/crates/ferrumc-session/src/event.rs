//! [`NetEvent`]: the network-side events the session router consumes.
//!
//! A `NetEvent` is the typed product of the networking layer's play phase: a
//! decoded serverbound packet, or a connection teardown. The router translates
//! each into a simulation [`GameInput`](ferrumc_sim::GameInput) (see
//! [`net_event_to_input`](crate::net_event_to_input)) and routes it to the
//! player's shard. The networking layer never reaches into the simulation
//! directly; it only emits these messages.

use ferrumc_core::PlayerId;
use ferrumc_net::DisconnectReason;
use ferrumc_proto::generated::play::ServerboundPlayPacket;

/// A network-side event addressed to a specific player's session.
///
/// This is the input vocabulary of the [`SessionRouter`](crate::SessionRouter).
/// The networking layer decodes, validates, and budgets serverbound traffic,
/// then hands the router these already-typed, already-trusted events; the router
/// is responsible only for translation and routing, never for parsing bytes.
///
/// The enum is `#[non_exhaustive]`: more event kinds (chat, interactions,
/// inventory) will be added as the bridge grows, so downstream `match`es must
/// include a wildcard arm.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum NetEvent {
    /// A serverbound play packet decoded from a connected player.
    ///
    /// The packet is already framed, size-capped, and budget-charged by the
    /// networking layer; the router only maps it to a simulation input.
    Play {
        /// The player the packet came from.
        player: PlayerId,
        /// The decoded serverbound play packet.
        packet: ServerboundPlayPacket,
    },

    /// The player's connection ended, for the given classified reason.
    ///
    /// The router treats this as a request to despawn the player from their
    /// shard and to drop the player<->shard mapping (disconnect cleanup).
    Disconnected {
        /// The player whose connection ended.
        player: PlayerId,
        /// Why the connection was torn down.
        reason: DisconnectReason,
    },
}

impl NetEvent {
    /// Builds a [`NetEvent::Play`] for `player` carrying `packet`.
    pub fn play(player: PlayerId, packet: ServerboundPlayPacket) -> Self {
        Self::Play { player, packet }
    }

    /// Builds a [`NetEvent::Disconnected`] for `player` with `reason`.
    pub fn disconnected(player: PlayerId, reason: DisconnectReason) -> Self {
        Self::Disconnected { player, reason }
    }

    /// The player this event is addressed to.
    pub fn player(&self) -> PlayerId {
        match self {
            Self::Play { player, .. } | Self::Disconnected { player, .. } => *player,
        }
    }
}

#[cfg(test)]
mod tests {
    use ferrumc_proto::generated::play::ServerboundKeepAlive;

    use super::*;

    #[test]
    fn constructors_and_player_accessor() {
        let p = PlayerId::offline("notch");
        let play = NetEvent::play(
            p,
            ServerboundPlayPacket::ServerboundKeepAlive(ServerboundKeepAlive::new(1)),
        );
        assert_eq!(play.player(), p);

        let bye = NetEvent::disconnected(p, DisconnectReason::ServerShutdown);
        assert_eq!(bye.player(), p);
        assert_ne!(play, bye);
    }
}
