//! [`OutboundPriority`]: the clientbound play-packet priority classes and the
//! per-class queue capacities the [`PlayWriter`](crate::PlayWriter) enforces.

use ferrumc_proto::generated::play::ClientboundPlayPacket;

/// The number of distinct [`OutboundPriority`] classes.
///
/// Used to size the fixed per-priority queue and counter arrays inside the
/// [`PlayWriter`](crate::PlayWriter).
pub const PRIORITY_COUNT: usize = 4;

/// Default capacity (in packets) of the [`Critical`](OutboundPriority::Critical)
/// queue: 64.
///
/// Critical traffic — keep-alives and disconnects — is small and infrequent, so
/// a shallow queue suffices. A *full* Critical queue means the client is not
/// draining even the most important frames; the caller should treat that as a
/// fatal [`OutboundOverflow`](crate::DisconnectReason::OutboundOverflow) rather
/// than tolerate the drop.
pub const DEFAULT_CRITICAL_CAPACITY: usize = 64;

/// Default capacity (in packets) of the [`State`](OutboundPriority::State)
/// queue: 128.
///
/// State changes (join, player-info, position sync) are bursty but bounded; the
/// queue absorbs a normal burst while still capping a backlog.
pub const DEFAULT_STATE_CAPACITY: usize = 128;

/// Default capacity (in packets) of the [`World`](OutboundPriority::World)
/// queue: 512.
///
/// World traffic is the bulk of play output: a view-distance change can enqueue
/// hundreds of chunk frames at once, so this queue is the deepest.
pub const DEFAULT_WORLD_CAPACITY: usize = 512;

/// Default capacity (in packets) of the [`Cosmetic`](OutboundPriority::Cosmetic)
/// queue: 256.
///
/// Cosmetic traffic (particles, sounds) is purely visual; dropping it under load
/// is harmless, so the queue is moderate and the first to shed frames.
pub const DEFAULT_COSMETIC_CAPACITY: usize = 256;

/// The priority class of a clientbound play packet.
///
/// The [`PlayWriter`](crate::PlayWriter) keeps one bounded queue per class and
/// drains them in **strict priority order** — `Critical`, then `State`, then
/// `World`, then `Cosmetic` — so the most important frames always leave first
/// when the link is congested.
///
/// The derived ordering follows that same ranking: `Critical < State < World <
/// Cosmetic`, i.e. the highest-priority class compares *least*. Prefer
/// [`OutboundPriority::ALL`] when you need to iterate classes in drain order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OutboundPriority {
    /// Must-deliver control traffic: keep-alives and disconnects. Drained first;
    /// a full Critical queue is a fatal condition, not a droppable one.
    Critical,
    /// Connection/world state the client needs to stay consistent: join game,
    /// position synchronization, player-info updates.
    State,
    /// Bulk world content: chunk data, block updates, entity spawns.
    World,
    /// Purely visual, safely droppable traffic: particles and sounds.
    Cosmetic,
}

impl OutboundPriority {
    /// Every priority class in strict drain order, highest priority first.
    pub const ALL: [Self; PRIORITY_COUNT] =
        [Self::Critical, Self::State, Self::World, Self::Cosmetic];

    /// The class's index into a [`PRIORITY_COUNT`]-wide array, matching its
    /// position in [`ALL`](Self::ALL).
    pub fn index(self) -> usize {
        match self {
            Self::Critical => 0,
            Self::State => 1,
            Self::World => 2,
            Self::Cosmetic => 3,
        }
    }

    /// `true` when a *full* queue of this class is a fatal condition the caller
    /// must escalate to a disconnect rather than absorb as a drop.
    ///
    /// Only [`Critical`](Self::Critical) traffic is undroppable: losing a
    /// keep-alive or a disconnect frame breaks the connection contract, whereas
    /// state, world, and cosmetic frames can be shed (the client recovers via a
    /// later full update or simply misses a visual effect).
    pub fn is_drop_fatal(self) -> bool {
        matches!(self, Self::Critical)
    }

    /// The default priority class for `packet`, used by
    /// [`PlayWriter::enqueue_classified`](crate::PlayWriter::enqueue_classified).
    ///
    /// This is a starting policy, not a hard rule: a caller that knows better
    /// (for example, flagging a specific chunk send as cosmetic) may still
    /// enqueue at an explicit priority. No generated clientbound packet currently
    /// maps to [`Cosmetic`](Self::Cosmetic); that class is reserved for the
    /// particle and sound packets a later milestone adds.
    pub fn for_packet(packet: &ClientboundPlayPacket) -> Self {
        match packet {
            ClientboundPlayPacket::ClientboundKeepAlive(_) => Self::Critical,
            // The login/join handshake packets are all connection state the client
            // needs to enter the world consistently: `GameEvent` (the
            // chunks-load-start cue), `SetCenterChunk`, and the default-spawn and
            // position syncs frame the bulk chunk stream that follows.
            ClientboundPlayPacket::JoinGame(_)
            | ClientboundPlayPacket::SynchronizePlayerPosition(_)
            | ClientboundPlayPacket::PlayerInfoUpdate(_)
            | ClientboundPlayPacket::GameEvent(_)
            | ClientboundPlayPacket::SetCenterChunk(_)
            | ClientboundPlayPacket::SetDefaultSpawnPosition(_) => Self::State,
            ClientboundPlayPacket::SpawnEntity(_)
            | ClientboundPlayPacket::BlockUpdate(_)
            | ClientboundPlayPacket::ChunkDataAndLight(_) => Self::World,
        }
    }
}

#[cfg(test)]
mod tests {
    use ferrumc_proto::generated::play::{ClientboundKeepAlive, JoinGame};

    use super::*;

    #[test]
    fn all_is_in_strict_drain_order() {
        assert_eq!(
            OutboundPriority::ALL,
            [
                OutboundPriority::Critical,
                OutboundPriority::State,
                OutboundPriority::World,
                OutboundPriority::Cosmetic,
            ]
        );
    }

    #[test]
    fn indices_are_dense_and_match_all() {
        for (i, prio) in OutboundPriority::ALL.into_iter().enumerate() {
            assert_eq!(prio.index(), i);
        }
    }

    #[test]
    fn ordering_ranks_critical_highest() {
        assert!(OutboundPriority::Critical < OutboundPriority::State);
        assert!(OutboundPriority::State < OutboundPriority::World);
        assert!(OutboundPriority::World < OutboundPriority::Cosmetic);
    }

    #[test]
    fn only_critical_is_drop_fatal() {
        assert!(OutboundPriority::Critical.is_drop_fatal());
        assert!(!OutboundPriority::State.is_drop_fatal());
        assert!(!OutboundPriority::World.is_drop_fatal());
        assert!(!OutboundPriority::Cosmetic.is_drop_fatal());
    }

    #[test]
    fn keep_alive_classifies_as_critical() {
        let packet = ClientboundPlayPacket::ClientboundKeepAlive(ClientboundKeepAlive::new(7));
        assert_eq!(
            OutboundPriority::for_packet(&packet),
            OutboundPriority::Critical
        );
    }

    #[test]
    fn join_game_classifies_as_state() {
        use ferrumc_codec::BoundedString;
        use ferrumc_proto::generated::play::SpawnInfo;

        let spawn = SpawnInfo::new(
            0,
            BoundedString::<32_767>::new("overworld".to_string()).unwrap(),
            0,
            0,
            0,
            false,
            true,
            None,
            0,
            63,
        );
        let packet = ClientboundPlayPacket::JoinGame(JoinGame::new(
            1,
            false,
            Vec::new(),
            20,
            8,
            8,
            false,
            true,
            false,
            spawn,
            false,
        ));
        assert_eq!(
            OutboundPriority::for_packet(&packet),
            OutboundPriority::State
        );
    }
}
