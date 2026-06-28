//! Event types delivered to plugins, the registrar used to subscribe, and the
//! block-edit decision surface plugins return at the intent boundary.
//!
//! # Block-edit decision surface (UNSTABLE / dev-only)
//!
//! Beyond the read-only [`PluginEvent`] notifications, this module defines the
//! *decision* surface a plugin uses to veto, rewrite, or augment a block edit
//! before it reaches the simulation: [`BlockPlaceAttempt`], [`BlockBreakAttempt`],
//! and the [`PluginBlockDecision`] a `before_*` hook returns (see
//! [`Plugin::before_block_place`](crate::Plugin::before_block_place) /
//! [`Plugin::before_block_break`](crate::Plugin::before_block_break)). This
//! surface is **unstable and for development only**: its shape (and the in-process
//! ABI it rides on) may change without a compatibility guarantee while the plugin
//! system is being built out. It is gated behind the
//! [`VetoBlockEdits`](crate::Capability::VetoBlockEdits) capability.

use std::collections::BTreeSet;

use ferrumc_core::{PlayerId, TextComponent};
use ferrumc_math::{BlockPos, WorldIntent};

/// Maximum number of [`WorldIntent`]s a single plugin may emit from one
/// `before_*` decision via [`PluginBlockDecision::EmitIntents`].
///
/// The host (and the application that routes the result) treats this as a hard
/// cap: intents beyond it are dropped rather than queued, so a misbehaving plugin
/// cannot flood the simulation from one block event. Bounded by construction, per
/// the project's "every buffer is bounded" rule.
pub const MAX_EMITTED_INTENTS: usize = 64;

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
    /// A player attempted to place a block.
    ///
    /// Reserved for symmetry with [`EventKind::BlockBreak`]; the *decision*
    /// counterpart is delivered through
    /// [`Plugin::before_block_place`](crate::Plugin::before_block_place), not as a
    /// subscribable notification.
    BlockPlace,
    /// A block placement was accepted at the intent boundary and routed to the
    /// simulation (an after-the-fact notification; see
    /// [`PluginEvent::AfterBlockPlace`]).
    AfterBlockPlace,
    /// A block break was accepted at the intent boundary and routed to the
    /// simulation (an after-the-fact notification; see
    /// [`PluginEvent::AfterBlockBreak`]).
    AfterBlockBreak,
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
    /// A block placement was accepted at the intent boundary and routed to the
    /// simulation.
    ///
    /// Delivered *after* the corresponding
    /// [`Plugin::before_block_place`](crate::Plugin::before_block_place) returned
    /// [`PluginBlockDecision::Allow`] / [`PluginBlockDecision::Replace`] and the
    /// edit was sent on for the simulation to apply. "Accepted" means accepted at
    /// the boundary and routed — the simulation may still reject it at the tick
    /// boundary (out of reach, chunk not loaded); a tick-confirmed notification is
    /// future work.
    AfterBlockPlace {
        /// The player that placed the block.
        player: PlayerId,
        /// The position the block was placed at.
        pos: BlockPos,
        /// The block-state id that was placed (the resolved or replacement state).
        block_state_id: u32,
    },
    /// A block break was accepted at the intent boundary and routed to the
    /// simulation.
    ///
    /// The after-the-fact counterpart of
    /// [`Plugin::before_block_break`](crate::Plugin::before_block_break); see
    /// [`PluginEvent::AfterBlockPlace`] for the precise "accepted" semantics.
    AfterBlockBreak {
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
            PluginEvent::AfterBlockPlace { .. } => EventKind::AfterBlockPlace,
            PluginEvent::AfterBlockBreak { .. } => EventKind::AfterBlockBreak,
        }
    }
}

/// A player's attempt to place a block, handed to
/// [`Plugin::before_block_place`](crate::Plugin::before_block_place) so the plugin
/// can decide whether (and how) it proceeds.
///
/// UNSTABLE / dev-only: part of the in-development block-decision surface (see the
/// [module docs](self)). The host builds one of these from the inbound packet at
/// the intent boundary; plugins only read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPlaceAttempt {
    player: PlayerId,
    pos: BlockPos,
    block_state_id: u32,
}

impl BlockPlaceAttempt {
    /// Builds a place attempt. Called by the host at the intent boundary.
    pub const fn new(player: PlayerId, pos: BlockPos, block_state_id: u32) -> Self {
        Self {
            player,
            pos,
            block_state_id,
        }
    }

    /// Returns the player attempting the placement.
    pub const fn player(&self) -> PlayerId {
        self.player
    }

    /// Returns the position the block would be placed at.
    pub const fn pos(&self) -> BlockPos {
        self.pos
    }

    /// Returns the opaque registry block-state id the player is placing.
    pub const fn block_state_id(&self) -> u32 {
        self.block_state_id
    }
}

/// A player's attempt to break a block, handed to
/// [`Plugin::before_block_break`](crate::Plugin::before_block_break).
///
/// UNSTABLE / dev-only: part of the in-development block-decision surface (see the
/// [module docs](self)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockBreakAttempt {
    player: PlayerId,
    pos: BlockPos,
}

impl BlockBreakAttempt {
    /// Builds a break attempt. Called by the host at the intent boundary.
    pub const fn new(player: PlayerId, pos: BlockPos) -> Self {
        Self { player, pos }
    }

    /// Returns the player attempting the break.
    pub const fn player(&self) -> PlayerId {
        self.player
    }

    /// Returns the position of the block being broken.
    pub const fn pos(&self) -> BlockPos {
        self.pos
    }
}

/// A plugin's decision about a pending block edit, returned from a `before_*`
/// hook ([`Plugin::before_block_place`](crate::Plugin::before_block_place) /
/// [`Plugin::before_block_break`](crate::Plugin::before_block_break)).
///
/// UNSTABLE / dev-only: this enum and the hooks that return it are part of the
/// in-development block-decision surface (see the [module docs](self)) and may
/// change without notice. The enum is `#[non_exhaustive]`, so a `match` over it
/// must include a wildcard arm.
///
/// # How the host combines decisions
///
/// When several plugins are consulted, the host folds their decisions with a
/// fixed, fail-safe precedence (`PluginHost::dispatch_block_decision`):
/// the first [`Deny`](PluginBlockDecision::Deny) is absorbing and short-circuits
/// the rest (a security action beats a convenience rewrite); otherwise a
/// [`Replace`](PluginBlockDecision::Replace) is last-writer-wins and
/// [`EmitIntents`](PluginBlockDecision::EmitIntents) vectors are concatenated
/// (subject to [`MAX_EMITTED_INTENTS`]).
///
/// # Fail-safe
///
/// If a hook panics or otherwise fails, the host treats that plugin's
/// contribution as [`Deny`](PluginBlockDecision::Deny): a broken plugin must not
/// silently let a destructive or placement action through.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum PluginBlockDecision {
    /// Let the edit proceed unchanged.
    Allow,
    /// Reject the edit; nothing is mutated.
    ///
    /// An optional [`TextComponent`] is shown to the acting player to explain the
    /// rejection.
    Deny {
        /// The message shown to the player, if any.
        message: Option<TextComponent>,
    },
    /// Proceed, but with a different block-state than the one the player chose.
    ///
    /// The id is the opaque registry block-state value; the application converts
    /// it to its typed form at the boundary. (A raw `u32`, not a typed
    /// `BlockStateId`, because this crate must not depend on `ferrumc-world` —
    /// consistent with [`WorldIntent::SetBlock`] and
    /// [`WorldView::block_state_id`](crate::WorldView::block_state_id).)
    Replace {
        /// The replacement registry block-state id.
        block_state_id: u32,
    },
    /// Proceed with the original edit and additionally submit these world-mutation
    /// intents.
    ///
    /// The vector is capped at [`MAX_EMITTED_INTENTS`]; the host drops anything
    /// beyond it.
    EmitIntents(Vec<WorldIntent>),
}

impl PluginBlockDecision {
    /// Returns whether this decision denies the edit.
    pub const fn is_deny(&self) -> bool {
        matches!(self, PluginBlockDecision::Deny { .. })
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

        let placed = PluginEvent::AfterBlockPlace {
            player: PlayerId::offline("Steve"),
            pos: BlockPos::ORIGIN,
            block_state_id: 1,
        };
        assert_eq!(placed.kind(), EventKind::AfterBlockPlace);

        let after_break = PluginEvent::AfterBlockBreak {
            player: PlayerId::offline("Steve"),
            pos: BlockPos::ORIGIN,
        };
        assert_eq!(after_break.kind(), EventKind::AfterBlockBreak);
    }

    #[test]
    fn block_attempts_expose_their_fields() {
        let player = PlayerId::offline("Steve");
        let place = BlockPlaceAttempt::new(player, BlockPos::new(1, 2, 3), 85);
        assert_eq!(place.player(), player);
        assert_eq!(place.pos(), BlockPos::new(1, 2, 3));
        assert_eq!(place.block_state_id(), 85);

        let brk = BlockBreakAttempt::new(player, BlockPos::new(4, 5, 6));
        assert_eq!(brk.player(), player);
        assert_eq!(brk.pos(), BlockPos::new(4, 5, 6));
    }

    #[test]
    fn deny_is_recognised() {
        assert!(PluginBlockDecision::Deny { message: None }.is_deny());
        assert!(!PluginBlockDecision::Allow.is_deny());
        assert!(!PluginBlockDecision::Replace { block_state_id: 1 }.is_deny());
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
