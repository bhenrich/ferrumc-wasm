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

use ferrumc_core::{EntityId, PlayerId, TextComponent};
use ferrumc_math::{BlockPos, Direction, WorldIntent};

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
    /// A player crossed into a new block position.
    ///
    /// Delivered as a read-only [`PluginEvent::PlayerMove`] notification (the host
    /// throttles it to block granularity — sub-block movement does not refire it).
    PlayerMove,
    /// A player sent a chat message.
    ///
    /// Reserved for symmetry: the *decision* counterpart is delivered through
    /// [`Plugin::before_chat`](crate::Plugin::before_chat) (so a plugin can drop
    /// the line before broadcast), not as a subscribable notification.
    Chat,
    /// A player interacted (right-clicked a block, the air, or an entity).
    ///
    /// Reserved for symmetry: the *decision* counterpart is delivered through
    /// [`Plugin::before_interact`](crate::Plugin::before_interact), not as a
    /// subscribable notification.
    Interact,
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
    /// A player crossed into a new block position.
    ///
    /// A read-only, observe-only notification: the host fires it only when the
    /// player's integer [`BlockPos`] changes (sub-block movement is throttled out),
    /// so a plugin sees one event per block entered, not one per movement packet.
    /// There is no veto — movement cannot be cancelled through this surface.
    PlayerMove {
        /// The player that moved.
        player: PlayerId,
        /// The block position the player moved out of.
        from: BlockPos,
        /// The block position the player moved into.
        to: BlockPos,
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
            PluginEvent::PlayerMove { .. } => EventKind::PlayerMove,
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

/// The hand a player used for an interaction.
///
/// Mirrors the protocol's two-hand model. The host maps the inbound packet's hand
/// index onto this so plugins never read a raw wire integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InteractHand {
    /// The main hand (the held hotbar item).
    Main,
    /// The off-hand (the shield slot).
    Off,
}

/// What a player interacted with, handed to
/// [`Plugin::before_interact`](crate::Plugin::before_interact).
///
/// A clean DTO: a plugin sees a typed target, never a raw packet. The enum is
/// `#[non_exhaustive]`, so a `match` over it must include a wildcard arm.
///
/// Only [`Block`](InteractTarget::Block) is produced by the current protocol slice
/// (a use-item-on-block right-click); [`Air`](InteractTarget::Air) (use-item) and
/// [`Entity`](InteractTarget::Entity) (use-on-entity) are modelled now so the
/// surface is stable once those serverbound packets are wired (a future milestone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InteractTarget {
    /// The player right-clicked while aiming at nothing (use-item in the air).
    Air,
    /// The player right-clicked a block face.
    Block {
        /// The position of the clicked block.
        pos: BlockPos,
        /// The clicked face of the block.
        face: Direction,
    },
    /// The player right-clicked an entity.
    Entity {
        /// The protocol id of the clicked entity.
        entity: EntityId,
    },
}

/// A player's chat message, handed to
/// [`Plugin::before_chat`](crate::Plugin::before_chat) *before* the line is
/// broadcast so a plugin can drop it.
///
/// A read-only DTO: the host builds it from the inbound packet at the intent
/// boundary; plugins only read it. Carries an owned message so it is not tied to
/// the connection's buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatAttempt {
    player: PlayerId,
    message: String,
}

impl ChatAttempt {
    /// Builds a chat attempt. Called by the host at the intent boundary.
    pub fn new(player: PlayerId, message: impl Into<String>) -> Self {
        Self {
            player,
            message: message.into(),
        }
    }

    /// Returns the player who sent the message.
    pub const fn player(&self) -> PlayerId {
        self.player
    }

    /// Returns the message text the player sent.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// A player's interaction (right-click), handed to
/// [`Plugin::before_interact`](crate::Plugin::before_interact) so the plugin can
/// observe it or veto it.
///
/// A read-only DTO built by the host from the inbound packet at the intent
/// boundary; plugins only read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractAttempt {
    player: PlayerId,
    hand: InteractHand,
    target: InteractTarget,
}

impl InteractAttempt {
    /// Builds an interaction attempt. Called by the host at the intent boundary.
    pub const fn new(player: PlayerId, hand: InteractHand, target: InteractTarget) -> Self {
        Self {
            player,
            hand,
            target,
        }
    }

    /// Returns the player who interacted.
    pub const fn player(&self) -> PlayerId {
        self.player
    }

    /// Returns the hand the player used.
    pub const fn hand(&self) -> InteractHand {
        self.hand
    }

    /// Returns what the player interacted with.
    pub const fn target(&self) -> InteractTarget {
        self.target
    }
}

/// A plugin's decision about a pending, vetoable player event (a chat message or
/// an interaction), returned from [`Plugin::before_chat`](crate::Plugin::before_chat)
/// / [`Plugin::before_interact`](crate::Plugin::before_interact).
///
/// The event-side counterpart of [`PluginBlockDecision`] — simpler, because a chat
/// or interaction is only ever allowed or dropped, never rewritten. The enum is
/// `#[non_exhaustive]`, so a `match` over it must include a wildcard arm.
///
/// # How the host combines decisions
///
/// When several plugins are consulted, the first
/// [`Deny`](PluginEventDecision::Deny) is absorbing and short-circuits the rest
/// (the same fail-safe precedence the block-decision fold uses).
///
/// # Fail-safe
///
/// If a hook panics, the host treats that plugin's contribution as
/// [`Deny`](PluginEventDecision::Deny): a broken plugin must not silently let a
/// chat line or interaction through.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginEventDecision {
    /// Let the event proceed unchanged.
    Allow,
    /// Drop the event; the chat line is not broadcast / the interaction is not
    /// acted on.
    ///
    /// An optional [`TextComponent`] is shown to the acting player to explain it.
    Deny {
        /// The message shown to the player, if any.
        message: Option<TextComponent>,
    },
}

impl PluginEventDecision {
    /// Returns whether this decision denies the event.
    pub const fn is_deny(&self) -> bool {
        matches!(self, PluginEventDecision::Deny { .. })
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

        let moved = PluginEvent::PlayerMove {
            player: PlayerId::offline("Steve"),
            from: BlockPos::ORIGIN,
            to: BlockPos::new(1, 0, 0),
        };
        assert_eq!(moved.kind(), EventKind::PlayerMove);
    }

    #[test]
    fn chat_and_interact_attempts_expose_their_fields() {
        let player = PlayerId::offline("Steve");

        let chat = ChatAttempt::new(player, "hello world");
        assert_eq!(chat.player(), player);
        assert_eq!(chat.message(), "hello world");

        let interact = InteractAttempt::new(
            player,
            InteractHand::Main,
            InteractTarget::Block {
                pos: BlockPos::new(1, 2, 3),
                face: Direction::Up,
            },
        );
        assert_eq!(interact.player(), player);
        assert_eq!(interact.hand(), InteractHand::Main);
        assert_eq!(
            interact.target(),
            InteractTarget::Block {
                pos: BlockPos::new(1, 2, 3),
                face: Direction::Up,
            }
        );
    }

    #[test]
    fn event_decision_recognises_deny() {
        assert!(PluginEventDecision::Deny { message: None }.is_deny());
        assert!(!PluginEventDecision::Allow.is_deny());
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
