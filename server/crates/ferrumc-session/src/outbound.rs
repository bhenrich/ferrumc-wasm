//! [`OutboundMessage`]: the per-player outbound channel payload — a clientbound
//! play packet tagged with the outbound classification the router assigns it at
//! the send site.

use ferrumc_net::{Criticality, OutboundPriority};
use ferrumc_proto::generated::play::ClientboundPlayPacket;

/// One clientbound play packet plus the outbound classification the router
/// assigns it **at the send site**: its [`Criticality`] (droppable vs mandatory)
/// and its [`OutboundPriority`] (the Layer-B drain class).
///
/// This is the payload of the per-player outbound channel. Carrying the
/// classification alongside the packet is what lets the connection writer
/// (Layer B, [`PlayWriter`](ferrumc_net::PlayWriter)) honor the router's intent
/// instead of **re-inferring** criticality from packet *type* — which is wrong
/// for context-dependent packets. A [`BlockUpdate`](ClientboundPlayPacket::BlockUpdate)
/// is a *droppable* viewer broadcast in one send and a *mandatory* actor resync
/// in another; only the router (Layer A) knows which, so it decides once, here,
/// and the writer obeys.
///
/// Constructors cover the three send patterns the router needs:
/// - [`classified`](Self::classified) — derive both fields from the packet type,
///   for sends whose criticality is *not* context-dependent.
/// - [`mandatory`](Self::mandatory) / [`droppable`](Self::droppable) — force the
///   criticality while keeping the type's default priority.
/// - [`new`](Self::new) — fully explicit criticality *and* priority, used for the
///   atomic resync that is forced onto the ack's [`State`](OutboundPriority::State)
///   queue so the pair is co-queued and FIFO-ordered (resync before ack).
#[derive(Debug, Clone)]
pub struct OutboundMessage {
    packet: ClientboundPlayPacket,
    criticality: Criticality,
    priority: OutboundPriority,
}

impl OutboundMessage {
    /// Classifies `packet` by its type, taking both the default
    /// [`Criticality`] and the default [`OutboundPriority`] from
    /// [`Criticality::for_packet`] / [`OutboundPriority::for_packet`].
    ///
    /// Use this only for packets whose criticality is **not** context-dependent
    /// (a despawn, a position correction, a keep-alive). For a context-dependent
    /// packet the caller must state its intent with [`mandatory`](Self::mandatory),
    /// [`droppable`](Self::droppable), or [`new`](Self::new).
    pub fn classified(packet: ClientboundPlayPacket) -> Self {
        let criticality = Criticality::for_packet(&packet);
        let priority = OutboundPriority::for_packet(&packet);
        Self {
            packet,
            criticality,
            priority,
        }
    }

    /// Marks `packet` as [`Mandatory`](Criticality::Mandatory) — delivered or the
    /// slow client is disconnected — at the packet type's default
    /// [`OutboundPriority`].
    pub fn mandatory(packet: ClientboundPlayPacket) -> Self {
        Self {
            criticality: Criticality::Mandatory,
            priority: OutboundPriority::for_packet(&packet),
            packet,
        }
    }

    /// Marks `packet` as [`Droppable`](Criticality::Droppable) — may be shed under
    /// backpressure — at the packet type's default [`OutboundPriority`].
    pub fn droppable(packet: ClientboundPlayPacket) -> Self {
        Self {
            criticality: Criticality::Droppable,
            priority: OutboundPriority::for_packet(&packet),
            packet,
        }
    }

    /// Wraps `packet` with an explicit `criticality` **and** `priority`, both
    /// chosen by the caller.
    ///
    /// Used for the rejected-edit resync: a [`BlockUpdate`](ClientboundPlayPacket::BlockUpdate)
    /// is forced onto [`Criticality::Mandatory`] and the ack's
    /// [`OutboundPriority::State`] queue (overriding its droppable/`World`
    /// defaults) so it shares a queue with the [`AcknowledgeBlockChange`](ClientboundPlayPacket::AcknowledgeBlockChange)
    /// that follows it. With the channel FIFO and the writer draining one message
    /// at a time, the resync is then guaranteed to be processed before the ack —
    /// so the ack can never survive a dropped resync.
    pub fn new(
        packet: ClientboundPlayPacket,
        criticality: Criticality,
        priority: OutboundPriority,
    ) -> Self {
        Self {
            packet,
            criticality,
            priority,
        }
    }

    /// The criticality the router assigned this packet at the send site.
    pub fn criticality(&self) -> Criticality {
        self.criticality
    }

    /// The Layer-B drain priority the router assigned this packet at the send site.
    pub fn priority(&self) -> OutboundPriority {
        self.priority
    }

    /// The wrapped packet, borrowed.
    pub fn packet(&self) -> &ClientboundPlayPacket {
        &self.packet
    }

    /// Consumes the envelope, returning the wrapped packet.
    pub fn into_packet(self) -> ClientboundPlayPacket {
        self.packet
    }
}

#[cfg(test)]
mod tests {
    use ferrumc_proto::generated::play::BlockUpdate;
    use ferrumc_proto::types::BlockPosition;

    use super::*;

    /// A `BlockUpdate` packet: droppable / `World` by type, the context-dependent
    /// packet the envelope exists to reclassify.
    fn block_update() -> ClientboundPlayPacket {
        ClientboundPlayPacket::BlockUpdate(BlockUpdate::new(BlockPosition::new(1, 2, 3), 0))
    }

    #[test]
    fn classified_takes_both_defaults_from_the_packet_type() {
        let msg = OutboundMessage::classified(block_update());
        assert_eq!(msg.criticality(), Criticality::Droppable);
        assert_eq!(msg.priority(), OutboundPriority::World);
    }

    #[test]
    fn mandatory_forces_criticality_but_keeps_default_priority() {
        let msg = OutboundMessage::mandatory(block_update());
        assert_eq!(msg.criticality(), Criticality::Mandatory);
        // Priority is still the type default: only `new` overrides it.
        assert_eq!(msg.priority(), OutboundPriority::World);
    }

    #[test]
    fn droppable_forces_criticality_but_keeps_default_priority() {
        let msg = OutboundMessage::droppable(block_update());
        assert_eq!(msg.criticality(), Criticality::Droppable);
        assert_eq!(msg.priority(), OutboundPriority::World);
    }

    #[test]
    fn new_round_trips_explicit_criticality_and_priority() {
        let msg = OutboundMessage::new(
            block_update(),
            Criticality::Mandatory,
            OutboundPriority::State,
        );
        assert_eq!(msg.criticality(), Criticality::Mandatory);
        assert_eq!(msg.priority(), OutboundPriority::State);
        assert!(matches!(
            msg.into_packet(),
            ClientboundPlayPacket::BlockUpdate(_)
        ));
    }

    #[test]
    fn carried_criticality_diverges_from_the_packet_type_default() {
        // The whole point of the envelope: a router-mandatory `BlockUpdate` resync
        // carries `Mandatory` even though the packet type's default is `Droppable`,
        // so Layer B never re-infers the wrong class from the type.
        let msg = OutboundMessage::new(
            block_update(),
            Criticality::Mandatory,
            OutboundPriority::State,
        );
        assert_eq!(msg.criticality(), Criticality::Mandatory);
        assert_eq!(
            Criticality::for_packet(msg.packet()),
            Criticality::Droppable,
            "the type default must diverge from the carried criticality"
        );
    }
}
