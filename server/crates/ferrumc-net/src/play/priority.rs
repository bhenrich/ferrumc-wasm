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
    /// position synchronization, player-info updates, and the entity
    /// spawn/despawn pair (so neither is tail-dropped behind bulk world traffic).
    State,
    /// Bulk world content: chunk data, block updates, and entity movement.
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
    ///
    /// This governs the connection writer's drain ("Layer B") only. The
    /// authoritative must-deliver-or-disconnect signal for the per-player outbound
    /// *channel* ("Layer A", the router's [`mpsc`](tokio::sync::mpsc) sender) is
    /// [`Criticality`], which the router applies per send site.
    pub fn is_drop_fatal(self) -> bool {
        matches!(self, Self::Critical)
    }

    /// The default priority class for `packet`, used by
    /// [`PlayWriter::enqueue_classified`](crate::PlayWriter::enqueue_classified).
    ///
    /// This is a starting policy, not a hard rule: a caller that knows better
    /// (for example, flagging a specific chunk send as cosmetic) may still
    /// enqueue at an explicit priority. The reserved title / particle / sound
    /// packets map to [`Cosmetic`](Self::Cosmetic); the scoreboard, team, and
    /// boss-bar UI packets ride [`State`](Self::State) (a dropped update would
    /// leave stale on-screen UI), and inline block-entity data rides
    /// [`World`](Self::World) with the rest of the bulk world content.
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
            // Player Info Remove is the tab-list counterpart of the add above:
            // the client must reliably see a player leave, so it rides State too.
            | ClientboundPlayPacket::RemovePlayerInfo(_)
            // SpawnEntity / RemoveEntities are the world-side counterparts of the
            // Player Info add / remove: a dropped spawn leaves a tab-list entry with
            // no visible body (later moves for an unknown entity id are ignored
            // client-side) and a dropped despawn leaves a ghost. Both must not be
            // tail-dropped behind bulk chunk traffic, so they ride State, not World.
            | ClientboundPlayPacket::SpawnEntity(_)
            | ClientboundPlayPacket::RemoveEntities(_)
            | ClientboundPlayPacket::GameEvent(_)
            | ClientboundPlayPacket::SetCenterChunk(_)
            // The block-change ack confirms the client's optimistic prediction for
            // a break/place; dropping it would leave that prediction stuck, so it
            // rides the State class (not droppable World traffic) alongside the
            // other client-reconciliation frames.
            | ClientboundPlayPacket::AcknowledgeBlockChange(_)
            // System chat carries command feedback and relayed player chat: the
            // client should reliably see it, so it rides the high-priority State
            // class (still non-fatal if the queue is ever saturated).
            | ClientboundPlayPacket::SystemChat(_)
            // The command graph is part of the join handshake (the client renders
            // commands valid and enables autocomplete from it); the tab-complete
            // response answers a client request and must be reliably delivered.
            // Both ride the State class.
            | ClientboundPlayPacket::Commands(_)
            | ClientboundPlayPacket::TabCompleteResponse(_)
            // Inventory and held-item updates are authoritative state the client
            // must reliably apply; dropping one desyncs its inventory view, so
            // they ride the State class (not droppable World traffic).
            | ClientboundPlayPacket::SetContainerContent(_)
            | ClientboundPlayPacket::SetContainerSlot(_)
            | ClientboundPlayPacket::ClientboundSetHeldItem(_)
            // Player Abilities is join-handshake state (flight + instabuild flags);
            // the client must reliably apply it, so it rides State.
            | ClientboundPlayPacket::PlayerAbilities(_)
            // Scoreboard, team, and boss-bar packets carry persistent on-screen
            // UI: a dropped update leaves the sidebar/tab-color/health bar stale
            // until the next one, so they ride State (ahead of bulk world), not
            // droppable Cosmetic. OpenSignEditor is the direct response that opens
            // the sign-edit GUI after a placement, so it too rides State.
            | ClientboundPlayPacket::UpdateObjectives(_)
            | ClientboundPlayPacket::DisplayObjective(_)
            | ClientboundPlayPacket::UpdateScore(_)
            | ClientboundPlayPacket::SetPlayerTeam(_)
            | ClientboundPlayPacket::BossBar(_)
            | ClientboundPlayPacket::OpenSignEditor(_)
            // OpenScreen opens a container GUI in direct response to an interaction;
            // like OpenSignEditor it rides State so it is not tail-dropped behind
            // bulk world traffic (the connection sends it mandatory at the call site).
            | ClientboundPlayPacket::OpenScreen(_)
            | ClientboundPlayPacket::SetDefaultSpawnPosition(_) => Self::State,
            ClientboundPlayPacket::BlockUpdate(_)
            | ClientboundPlayPacket::ChunkDataAndLight(_)
            | ClientboundPlayPacket::UnloadChunk(_)
            // Inline block-entity data (sign text, banner patterns, …) is bulk
            // world content alongside block updates and chunk data.
            | ClientboundPlayPacket::BlockEntityData(_)
            // Entity movement is bulk position traffic, droppable under load (a
            // later move or the next teleport supersedes a dropped one). The spawn
            // (SpawnEntity) and despawn (RemoveEntities) are deliberately NOT here —
            // both ride State above so neither is tail-dropped behind bulk chunk
            // traffic, which would leave an invisible body or a ghost entity.
            | ClientboundPlayPacket::EntityTeleport(_)
            | ClientboundPlayPacket::UpdateEntityPosition(_)
            | ClientboundPlayPacket::UpdateEntityPositionAndRotation(_)
            | ClientboundPlayPacket::UpdateEntityRotation(_)
            | ClientboundPlayPacket::SetHeadRotation(_)
            // Equipment (held item) is cosmetic viewer state: a later update or the
            // next spawn supersedes a dropped one, so it rides droppable World.
            | ClientboundPlayPacket::SetEquipment(_) => Self::World,
            // Purely visual / audio overlays, safely shed under congestion: the
            // title stack (title / subtitle / action bar / animation times), world
            // particles, and the positional sound effect.
            ClientboundPlayPacket::SetTitleText(_)
            | ClientboundPlayPacket::SetSubtitleText(_)
            | ClientboundPlayPacket::SetActionBarText(_)
            | ClientboundPlayPacket::SetTitleAnimationTimes(_)
            | ClientboundPlayPacket::Particle(_)
            | ClientboundPlayPacket::SoundEffect(_) => Self::Cosmetic,
        }
    }
}

/// Whether a clientbound play packet **must** reach the client or may be shed
/// under backpressure.
///
/// This is the authoritative droppability signal for the per-player outbound
/// *channel* ("Layer A": the router's [`mpsc`](tokio::sync::mpsc) sender),
/// distinct from [`OutboundPriority`], which only orders the connection writer's
/// drain ("Layer B"). A [`Mandatory`](Self::Mandatory) packet is delivered or the
/// slow client is disconnected — never silently dropped — because losing it
/// corrupts the client's later state (a missing ack strands a prediction, a
/// missing spawn leaves an invisible body, a missing despawn ghosts an entity). A
/// [`Droppable`](Self::Droppable) packet may
/// be coalesced or dropped when the channel is full, because a later update
/// supersedes it (a movement delta) or its loss is merely cosmetic.
///
/// Some packets are context-dependent: a `BlockUpdate` is mandatory when it
/// resyncs the acting player after a rejected edit but droppable as a viewer
/// broadcast. [`for_packet`](Self::for_packet) gives only the **default** class;
/// the router passes the criticality explicitly at such call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Criticality {
    /// Must be delivered; a full or closed channel disconnects the client instead
    /// of dropping the packet.
    Mandatory,
    /// May be coalesced or dropped under backpressure without corrupting later
    /// client state.
    Droppable,
}

impl Criticality {
    /// The default criticality class for `packet`.
    ///
    /// Mandatory: the block-change ack, the entity spawn ([`SpawnEntity`]) and its
    /// despawns ([`RemoveEntities`] / [`RemovePlayerInfo`]), the player-info add,
    /// the keep-alive, and the join / login-kit framing ([`JoinGame`],
    /// [`GameEvent`], [`SetCenterChunk`], [`SynchronizePlayerPosition`],
    /// [`SetDefaultSpawnPosition`]). Droppable: relative movement deltas, teleports,
    /// chunk data, ambient block updates, system chat, and the command/tab-complete
    /// graph.
    ///
    /// This is the default only: a `BlockUpdate` carried as an actor resync is
    /// mandatory despite the droppable default here, so the router classifies such
    /// context-dependent sends explicitly rather than calling this.
    ///
    /// [`SpawnEntity`]: ClientboundPlayPacket::SpawnEntity
    /// [`RemoveEntities`]: ClientboundPlayPacket::RemoveEntities
    /// [`RemovePlayerInfo`]: ClientboundPlayPacket::RemovePlayerInfo
    /// [`JoinGame`]: ClientboundPlayPacket::JoinGame
    /// [`GameEvent`]: ClientboundPlayPacket::GameEvent
    /// [`SetCenterChunk`]: ClientboundPlayPacket::SetCenterChunk
    /// [`SynchronizePlayerPosition`]: ClientboundPlayPacket::SynchronizePlayerPosition
    /// [`SetDefaultSpawnPosition`]: ClientboundPlayPacket::SetDefaultSpawnPosition
    pub fn for_packet(packet: &ClientboundPlayPacket) -> Self {
        match packet {
            ClientboundPlayPacket::ClientboundKeepAlive(_)
            | ClientboundPlayPacket::JoinGame(_)
            | ClientboundPlayPacket::SynchronizePlayerPosition(_)
            | ClientboundPlayPacket::PlayerInfoUpdate(_)
            | ClientboundPlayPacket::RemovePlayerInfo(_)
            | ClientboundPlayPacket::RemoveEntities(_)
            | ClientboundPlayPacket::GameEvent(_)
            | ClientboundPlayPacket::SetCenterChunk(_)
            | ClientboundPlayPacket::AcknowledgeBlockChange(_)
            // A dropped spawn leaves a tab-list entry with no visible body, the
            // inverse of the ghost a dropped despawn leaves, and is just as
            // unrecoverable (later moves for an unknown entity id are ignored
            // client-side), so SpawnEntity is mandatory alongside the despawns.
            | ClientboundPlayPacket::SpawnEntity(_)
            // Inventory/held-item updates are authoritative state: dropping one
            // leaves the client's view of its inventory desynced until the next
            // full container resync, so they are mandatory, not droppable.
            | ClientboundPlayPacket::SetContainerContent(_)
            | ClientboundPlayPacket::SetContainerSlot(_)
            | ClientboundPlayPacket::ClientboundSetHeldItem(_)
            // Player Abilities carries the creative flight/instabuild flags the
            // client needs on join; dropping it leaves the client without them.
            | ClientboundPlayPacket::PlayerAbilities(_)
            | ClientboundPlayPacket::SetDefaultSpawnPosition(_) => Self::Mandatory,
            ClientboundPlayPacket::BlockUpdate(_)
            | ClientboundPlayPacket::ChunkDataAndLight(_)
            | ClientboundPlayPacket::UnloadChunk(_)
            | ClientboundPlayPacket::EntityTeleport(_)
            | ClientboundPlayPacket::UpdateEntityPosition(_)
            | ClientboundPlayPacket::UpdateEntityPositionAndRotation(_)
            | ClientboundPlayPacket::UpdateEntityRotation(_)
            | ClientboundPlayPacket::SetHeadRotation(_)
            // Equipment is cosmetic: a dropped held-item update is superseded by the
            // next one (or re-sent with the spawn on re-enter), so it is droppable.
            | ClientboundPlayPacket::SetEquipment(_)
            | ClientboundPlayPacket::SystemChat(_)
            | ClientboundPlayPacket::Commands(_)
            | ClientboundPlayPacket::TabCompleteResponse(_)
            // The reserved title / particle / sound / scoreboard / team / boss-bar
            // / block-entity / sign packets default to droppable: a drop leaves a
            // missed visual or a stale UI element that the next update heals, never
            // corrupting later client state. A lane that needs an exception (e.g. a
            // boss-bar *remove*, which would otherwise ghost) passes the criticality
            // explicitly at the send site, exactly like a BlockUpdate resync.
            | ClientboundPlayPacket::SetTitleText(_)
            | ClientboundPlayPacket::SetSubtitleText(_)
            | ClientboundPlayPacket::SetActionBarText(_)
            | ClientboundPlayPacket::SetTitleAnimationTimes(_)
            | ClientboundPlayPacket::Particle(_)
            | ClientboundPlayPacket::SoundEffect(_)
            | ClientboundPlayPacket::UpdateObjectives(_)
            | ClientboundPlayPacket::DisplayObjective(_)
            | ClientboundPlayPacket::UpdateScore(_)
            | ClientboundPlayPacket::SetPlayerTeam(_)
            | ClientboundPlayPacket::BossBar(_)
            | ClientboundPlayPacket::BlockEntityData(_)
            | ClientboundPlayPacket::OpenSignEditor(_)
            // OpenScreen defaults to droppable like the other GUI-open packets; the
            // chest-open path sends it mandatory explicitly so a busy client still
            // reliably gets the window it just asked to open.
            | ClientboundPlayPacket::OpenScreen(_) => Self::Droppable,
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

    #[test]
    fn despawn_rides_state_not_world() {
        use ferrumc_proto::generated::play::RemoveEntities;
        // A despawn must not be tail-dropped behind bulk World chunk traffic.
        let despawn = ClientboundPlayPacket::RemoveEntities(RemoveEntities::new(vec![2]));
        assert_eq!(
            OutboundPriority::for_packet(&despawn),
            OutboundPriority::State
        );
    }

    #[test]
    fn spawn_entity_rides_state_and_is_mandatory() {
        use ferrumc_proto::generated::play::{EntityVelocity, SpawnEntity};
        // A spawn is the world-side counterpart of a despawn: a dropped spawn
        // leaves a tab-list entry with no visible body, so it must never be
        // tail-dropped behind bulk World chunks (it rides State) nor silently
        // dropped on a full channel (it is Mandatory).
        let spawn = ClientboundPlayPacket::SpawnEntity(SpawnEntity::new(
            2,
            uuid::Uuid::nil(),
            149,
            0.0,
            0.0,
            0.0,
            0,
            0,
            0,
            0,
            EntityVelocity::new(0, 0, 0),
        ));
        assert_eq!(
            OutboundPriority::for_packet(&spawn),
            OutboundPriority::State
        );
        assert_eq!(Criticality::for_packet(&spawn), Criticality::Mandatory);
    }

    #[test]
    fn mandatory_packets_are_classified_mandatory() {
        use ferrumc_proto::generated::play::{AcknowledgeBlockChange, RemoveEntities};
        // The block-change ack and despawns must never be silently dropped.
        let ack = ClientboundPlayPacket::AcknowledgeBlockChange(AcknowledgeBlockChange::new(1));
        let despawn = ClientboundPlayPacket::RemoveEntities(RemoveEntities::new(vec![2]));
        assert_eq!(Criticality::for_packet(&ack), Criticality::Mandatory);
        assert_eq!(Criticality::for_packet(&despawn), Criticality::Mandatory);
    }

    #[test]
    fn reserved_visual_packets_are_cosmetic_and_droppable() {
        use ferrumc_proto::generated::play::{Particle, SoundEffect};
        // Particles and sounds are visual/audio-only and safely shed under
        // congestion (the title/action-bar overlays share this Cosmetic arm).
        let particle = ClientboundPlayPacket::Particle(Particle::new(
            false,
            false,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            0.0,
            1,
            0,
            Vec::new(),
        ));
        let sound = ClientboundPlayPacket::SoundEffect(SoundEffect::new(Vec::new()));
        for packet in [&particle, &sound] {
            assert_eq!(
                OutboundPriority::for_packet(packet),
                OutboundPriority::Cosmetic
            );
            assert_eq!(Criticality::for_packet(packet), Criticality::Droppable);
        }
    }

    #[test]
    fn reserved_ui_packets_ride_state() {
        use ferrumc_codec::BoundedString;
        use ferrumc_proto::generated::play::DisplayObjective;
        // Persistent on-screen UI (here a sidebar objective) must not be
        // tail-dropped behind bulk World chunk traffic.
        let display = ClientboundPlayPacket::DisplayObjective(DisplayObjective::new(
            1,
            BoundedString::<32_767>::new("health".to_string()).unwrap(),
        ));
        assert_eq!(
            OutboundPriority::for_packet(&display),
            OutboundPriority::State
        );
    }

    #[test]
    fn movement_and_chunk_packets_are_droppable() {
        use ferrumc_proto::generated::play::{UnloadChunk, UpdateEntityPosition};
        // A later move or the next stream pass supersedes these, so they may shed.
        let mov = ClientboundPlayPacket::UpdateEntityPosition(UpdateEntityPosition::new(
            2, 1, 0, 0, true,
        ));
        let unload = ClientboundPlayPacket::UnloadChunk(UnloadChunk::new(0, 0));
        assert_eq!(Criticality::for_packet(&mov), Criticality::Droppable);
        assert_eq!(Criticality::for_packet(&unload), Criticality::Droppable);
    }
}
