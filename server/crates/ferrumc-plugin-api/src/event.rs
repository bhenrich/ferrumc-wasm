//! Event types delivered to plugins, and the registrar used to subscribe.

use std::collections::BTreeSet;

use ferrumc_core::PlayerId;
use ferrumc_math::BlockPos;

/// A discriminant identifying a kind of [`PluginEvent`] without its payload.
///
/// Plugins subscribe to event *kinds* via an [`EventRegistrar`] so the host can
/// skip delivering events nobody is listening for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EventKind {
    /// A player finished joining the server.
    PlayerJoin,
    /// A player left the server.
    PlayerLeave,
    /// A player broke a block.
    BlockBreak,
}

/// An event the host dispatches to subscribed plugins.
///
/// Events are read-only notifications: a plugin reacts to them through the
/// capability-gated facades on its [`EventContext`](crate::EventContext), never
/// by mutating the event. The enum is `#[non_exhaustive]`; new variants may be
/// added, so `match`es must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginEvent {
    /// A player finished joining the server.
    PlayerJoin {
        /// The player that joined.
        player: PlayerId,
    },
    /// A player left the server.
    PlayerLeave {
        /// The player that left.
        player: PlayerId,
    },
    /// A player broke a block.
    BlockBreak {
        /// The player that broke the block.
        player: PlayerId,
        /// The position of the broken block.
        pos: BlockPos,
    },
}

impl PluginEvent {
    /// Returns the [`EventKind`] discriminant of this event.
    pub const fn kind(&self) -> EventKind {
        match self {
            PluginEvent::PlayerJoin { .. } => EventKind::PlayerJoin,
            PluginEvent::PlayerLeave { .. } => EventKind::PlayerLeave,
            PluginEvent::BlockBreak { .. } => EventKind::BlockBreak,
        }
    }
}

/// Collects the [`EventKind`]s a plugin subscribes to during setup.
///
/// The host hands a plugin a registrar (subject to the
/// [`ReceiveEvents`](crate::Capability::ReceiveEvents) capability) in
/// [`Plugin::on_enable`](crate::Plugin::on_enable); the plugin records its
/// interests, and the host later dispatches only matching events.
#[derive(Debug, Default, Clone)]
pub struct EventRegistrar {
    subscriptions: BTreeSet<EventKind>,
}

impl EventRegistrar {
    /// Creates an empty registrar with no subscriptions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribes to `kind`, returning the registrar for chaining.
    ///
    /// Subscribing to the same kind twice is a no-op.
    pub fn subscribe(&mut self, kind: EventKind) -> &mut Self {
        self.subscriptions.insert(kind);
        self
    }

    /// Returns whether `kind` is currently subscribed.
    pub fn is_subscribed(&self, kind: EventKind) -> bool {
        self.subscriptions.contains(&kind)
    }

    /// Returns the number of distinct subscribed kinds.
    pub fn len(&self) -> usize {
        self.subscriptions.len()
    }

    /// Returns whether nothing is subscribed.
    pub fn is_empty(&self) -> bool {
        self.subscriptions.is_empty()
    }

    /// Iterates over the subscribed kinds in a stable order.
    pub fn subscriptions(&self) -> impl Iterator<Item = EventKind> + '_ {
        self.subscriptions.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_matches_payload() {
        let join = PluginEvent::PlayerJoin {
            player: PlayerId::offline("Steve"),
        };
        assert_eq!(join.kind(), EventKind::PlayerJoin);

        let broke = PluginEvent::BlockBreak {
            player: PlayerId::offline("Steve"),
            pos: BlockPos::ORIGIN,
        };
        assert_eq!(broke.kind(), EventKind::BlockBreak);
    }

    #[test]
    fn registrar_tracks_subscriptions() {
        let mut registrar = EventRegistrar::new();
        assert!(registrar.is_empty());
        registrar
            .subscribe(EventKind::PlayerJoin)
            .subscribe(EventKind::PlayerJoin)
            .subscribe(EventKind::BlockBreak);

        assert_eq!(registrar.len(), 2);
        assert!(registrar.is_subscribed(EventKind::PlayerJoin));
        assert!(registrar.is_subscribed(EventKind::BlockBreak));
        assert!(!registrar.is_subscribed(EventKind::PlayerLeave));

        let collected: Vec<EventKind> = registrar.subscriptions().collect();
        assert_eq!(
            collected,
            vec![EventKind::PlayerJoin, EventKind::BlockBreak]
        );
    }
}
