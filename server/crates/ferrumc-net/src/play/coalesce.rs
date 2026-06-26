//! [`MovementCoalescer`]: a per-tick movement-collapsing hook.
//!
//! A client streams many position updates between server ticks, but only the
//! most recent one matters when the simulation finally reads the player's
//! position. This hook keeps **only the latest** offered movement packet for the
//! current tick and discards the superseded ones, so the simulation processes at
//! most one positional update per player per tick.
//!
//! This is the placeholder wiring for that policy: it collapses by "latest wins"
//! only and does no interpolation, distance validation, or anti-cheat — those
//! belong to a later simulation milestone. The hook lives in `ferrumc-net`
//! because it operates purely on decoded serverbound frames and never touches
//! world or simulation state.

use ferrumc_proto::generated::play::ServerboundPlayPacket;

/// `true` when `packet` is a player-movement packet whose effect is fully
/// captured by the latest update in a tick.
///
/// Only the two positional packets qualify: [`SetPlayerPosition`] and
/// [`SetPlayerPositionAndRotation`]. Actions, chat, and keep-alives are *not*
/// movement — each is independently meaningful and must never be coalesced away.
///
/// [`SetPlayerPosition`]: ferrumc_proto::generated::play::SetPlayerPosition
/// [`SetPlayerPositionAndRotation`]: ferrumc_proto::generated::play::SetPlayerPositionAndRotation
pub fn is_movement(packet: &ServerboundPlayPacket) -> bool {
    matches!(
        packet,
        ServerboundPlayPacket::SetPlayerPosition(_)
            | ServerboundPlayPacket::SetPlayerPositionAndRotation(_)
    )
}

/// The result of [`offer`](MovementCoalescer::offer)ing a packet to the
/// coalescer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferOutcome {
    /// The first movement packet of the tick was stored as the pending latest.
    Stored,
    /// A movement packet replaced an earlier pending one, which was dropped.
    Superseded,
    /// The packet was not a movement packet; the coalescer ignored it and the
    /// caller must route it normally.
    NotMovement,
}

impl OfferOutcome {
    /// `true` when the offered packet was taken by the coalescer (either stored
    /// or superseding a previous one).
    pub fn was_coalesced(self) -> bool {
        matches!(self, Self::Stored | Self::Superseded)
    }
}

/// Keeps only the latest movement packet offered within a tick.
///
/// Feed every decoded serverbound packet to [`offer`](Self::offer); movement
/// packets replace the pending latest (counting the superseded one), everything
/// else is reported as [`OfferOutcome::NotMovement`] for the caller to route. At
/// the tick boundary, [`take`](Self::take) yields the single surviving movement
/// packet (if any) and clears the slot for the next tick.
#[derive(Debug, Clone, Default)]
pub struct MovementCoalescer {
    latest: Option<ServerboundPlayPacket>,
    superseded: u64,
}

impl MovementCoalescer {
    /// Creates an empty coalescer with no pending movement.
    pub fn new() -> Self {
        Self::default()
    }

    /// Offers `packet` to the coalescer.
    ///
    /// A movement packet (see [`is_movement`]) becomes the pending latest,
    /// dropping and counting any earlier pending movement. A non-movement packet
    /// is returned as [`OfferOutcome::NotMovement`] and left untouched for the
    /// caller to handle.
    pub fn offer(&mut self, packet: ServerboundPlayPacket) -> OfferOutcome {
        if !is_movement(&packet) {
            return OfferOutcome::NotMovement;
        }
        if self.latest.replace(packet).is_some() {
            self.superseded += 1;
            OfferOutcome::Superseded
        } else {
            OfferOutcome::Stored
        }
    }

    /// Takes the latest pending movement packet and clears the slot for the next
    /// tick, returning `None` when no movement was offered this tick.
    pub fn take(&mut self) -> Option<ServerboundPlayPacket> {
        self.latest.take()
    }

    /// `true` when a movement packet is pending for the current tick.
    pub fn has_pending(&self) -> bool {
        self.latest.is_some()
    }

    /// The cumulative number of movement packets dropped because a later one
    /// superseded them. A running metric; it is not reset by [`take`](Self::take).
    pub fn superseded_count(&self) -> u64 {
        self.superseded
    }
}

#[cfg(test)]
mod tests {
    // Position coordinates are exact, representable values, so exact float
    // comparison is intentional here.
    #![allow(clippy::float_cmp)]

    use super::*;

    fn position(x: f64) -> ServerboundPlayPacket {
        use ferrumc_proto::generated::play::SetPlayerPosition;
        ServerboundPlayPacket::SetPlayerPosition(SetPlayerPosition::new(x, 64.0, 0.0, 0))
    }

    fn keep_alive() -> ServerboundPlayPacket {
        use ferrumc_proto::generated::play::ServerboundKeepAlive;
        ServerboundPlayPacket::ServerboundKeepAlive(ServerboundKeepAlive::new(1))
    }

    #[test]
    fn is_movement_only_matches_positional_packets() {
        assert!(is_movement(&position(1.0)));
        assert!(!is_movement(&keep_alive()));
    }

    #[test]
    fn coalescing_keeps_only_the_latest_position() {
        let mut coalescer = MovementCoalescer::new();
        assert_eq!(coalescer.offer(position(1.0)), OfferOutcome::Stored);
        assert_eq!(coalescer.offer(position(2.0)), OfferOutcome::Superseded);
        assert_eq!(coalescer.offer(position(3.0)), OfferOutcome::Superseded);
        assert!(coalescer.has_pending());

        let taken = coalescer.take();
        let Some(ServerboundPlayPacket::SetPlayerPosition(pos)) = taken else {
            panic!("expected the latest position");
        };
        assert_eq!(pos.x(), 3.0);
        // Two earlier positions were superseded.
        assert_eq!(coalescer.superseded_count(), 2);
        // The slot is cleared after taking.
        assert!(!coalescer.has_pending());
        assert!(coalescer.take().is_none());
    }

    #[test]
    fn non_movement_is_not_coalesced() {
        let mut coalescer = MovementCoalescer::new();
        assert_eq!(coalescer.offer(keep_alive()), OfferOutcome::NotMovement);
        assert!(!coalescer.has_pending());
        assert_eq!(coalescer.superseded_count(), 0);
    }

    #[test]
    fn take_resets_between_ticks() {
        let mut coalescer = MovementCoalescer::new();
        coalescer.offer(position(1.0));
        assert!(coalescer.take().is_some());
        // Next tick: a fresh single offer is stored, not superseded.
        assert_eq!(coalescer.offer(position(9.0)), OfferOutcome::Stored);
        let Some(ServerboundPlayPacket::SetPlayerPosition(pos)) = coalescer.take() else {
            panic!("expected the new tick's position");
        };
        assert_eq!(pos.x(), 9.0);
        // The superseded counter is cumulative across ticks (none here).
        assert_eq!(coalescer.superseded_count(), 0);
    }

    #[test]
    fn offer_outcome_reports_coalescing() {
        assert!(OfferOutcome::Stored.was_coalesced());
        assert!(OfferOutcome::Superseded.was_coalesced());
        assert!(!OfferOutcome::NotMovement.was_coalesced());
    }
}
