//! Read-only event values and decisions returned by plugins.

use crate::{BlockPos, CommandInvocation, Direction, EntityId, FacadeError, PlayerId, TimerId};

/// Maximum accepted byte length of a chat-attempt payload.
pub const MAX_CHAT_BYTES: usize = 256;

/// Maximum accepted byte length of plain-text decision feedback.
pub const MAX_FEEDBACK_BYTES: usize = 4_096;

/// Stable event discriminants shared by both packaging modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum EventKind {
    /// A player finished joining.
    PlayerJoin,
    /// A player left.
    PlayerLeave,
    /// A block-break notification.
    BlockBreak,
    /// A block placement accepted at the intent boundary and routed.
    AfterBlockPlace,
    /// A block break accepted at the intent boundary and routed.
    AfterBlockBreak,
    /// A player crossed a block boundary.
    PlayerMove,
    /// A vetoable block-placement attempt.
    BlockPlaceAttempt,
    /// A vetoable block-break attempt.
    BlockBreakAttempt,
    /// A vetoable chat attempt.
    ChatAttempt,
    /// A vetoable interaction attempt.
    InteractAttempt,
    /// A registered plugin command was invoked.
    Command,
    /// A deterministic plugin timer became due.
    Timer,
}

/// An event carrying one player identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerEvent {
    player: PlayerId,
}

impl PlayerEvent {
    /// Creates a player event.
    pub const fn new(player: PlayerId) -> Self {
        Self { player }
    }

    /// Returns the affected player.
    pub const fn player(self) -> PlayerId {
        self.player
    }
}

/// An event carrying a player and block position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockEvent {
    player: PlayerId,
    pos: BlockPos,
}

impl BlockEvent {
    /// Creates a block event.
    pub const fn new(player: PlayerId, pos: BlockPos) -> Self {
        Self { player, pos }
    }

    /// Returns the affected player.
    pub const fn player(self) -> PlayerId {
        self.player
    }

    /// Returns the block position.
    pub const fn pos(self) -> BlockPos {
        self.pos
    }
}

/// A block-placement notification accepted at the intent boundary and routed.
///
/// The simulation may still reject the edit, so this is not a tick-confirmed
/// mutation event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockPlaceEvent {
    player: PlayerId,
    pos: BlockPos,
    block_state_id: u32,
}

impl BlockPlaceEvent {
    /// Creates a block-placement notification.
    pub const fn new(player: PlayerId, pos: BlockPos, block_state_id: u32) -> Self {
        Self {
            player,
            pos,
            block_state_id,
        }
    }

    /// Returns the player who placed the block.
    pub const fn player(self) -> PlayerId {
        self.player
    }

    /// Returns the placed block position.
    pub const fn pos(self) -> BlockPos {
        self.pos
    }

    /// Returns the opaque registry block-state identifier.
    pub const fn block_state_id(self) -> u32 {
        self.block_state_id
    }
}

/// A player crossing from one block position to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveEvent {
    player: PlayerId,
    from: BlockPos,
    to: BlockPos,
}

impl MoveEvent {
    /// Creates a block-boundary movement event.
    pub const fn new(player: PlayerId, from: BlockPos, to: BlockPos) -> Self {
        Self { player, from, to }
    }

    /// Returns the moving player.
    pub const fn player(self) -> PlayerId {
        self.player
    }

    /// Returns the previous block position.
    pub const fn from(self) -> BlockPos {
        self.from
    }

    /// Returns the new block position.
    pub const fn to(self) -> BlockPos {
        self.to
    }
}

/// A vetoable block-placement attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceAttempt {
    player: PlayerId,
    pos: BlockPos,
    block_state_id: u32,
}

impl PlaceAttempt {
    /// Creates a block-placement attempt.
    pub const fn new(player: PlayerId, pos: BlockPos, block_state_id: u32) -> Self {
        Self {
            player,
            pos,
            block_state_id,
        }
    }

    /// Returns the acting player.
    pub const fn player(self) -> PlayerId {
        self.player
    }

    /// Returns the requested block position.
    pub const fn pos(self) -> BlockPos {
        self.pos
    }

    /// Returns the requested opaque block-state identifier.
    pub const fn block_state_id(self) -> u32 {
        self.block_state_id
    }
}

/// A vetoable chat message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatAttempt {
    player: PlayerId,
    message: String,
}

impl ChatAttempt {
    /// Creates a bounded chat attempt.
    pub fn new(player: PlayerId, message: impl Into<String>) -> Result<Self, FacadeError> {
        let message = message.into();
        if message.len() > MAX_CHAT_BYTES {
            return Err(FacadeError::LimitExceeded {
                resource: "chat message",
                len: message.len(),
                max: MAX_CHAT_BYTES,
            });
        }
        Ok(Self { player, message })
    }

    /// Returns the sender.
    pub const fn player(&self) -> PlayerId {
        self.player
    }

    /// Returns the plain `UTF-8` chat text.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Which hand a player used for an interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InteractHand {
    /// The main hand.
    Main,
    /// The off hand.
    Off,
}

/// The typed target of a player interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InteractTarget {
    /// The player used an item while targeting no resource.
    Air,
    /// The player targeted one block face.
    Block {
        /// Target block position.
        pos: BlockPos,
        /// Target block face.
        face: Direction,
    },
    /// The player targeted one entity.
    Entity {
        /// Target entity identifier.
        entity: EntityId,
    },
}

/// A vetoable player interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionAttempt {
    player: PlayerId,
    hand: InteractHand,
    target: InteractTarget,
}

impl InteractionAttempt {
    /// Creates an interaction attempt.
    pub const fn new(player: PlayerId, hand: InteractHand, target: InteractTarget) -> Self {
        Self {
            player,
            hand,
            target,
        }
    }

    /// Returns the acting player.
    pub const fn player(self) -> PlayerId {
        self.player
    }

    /// Returns the used hand.
    pub const fn hand(self) -> InteractHand {
        self.hand
    }

    /// Returns the interaction target.
    pub const fn target(self) -> InteractTarget {
        self.target
    }
}

/// A read-only event delivered to a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A player finished joining.
    PlayerJoin(PlayerEvent),
    /// A player left.
    PlayerLeave(PlayerEvent),
    /// A block-break notification.
    BlockBreak(BlockEvent),
    /// A block placement accepted at the intent boundary and routed.
    ///
    /// The simulation may still reject the edit, so this is not a
    /// tick-confirmed mutation event.
    AfterBlockPlace(BlockPlaceEvent),
    /// A block break accepted at the intent boundary and routed.
    ///
    /// The simulation may still reject the edit, so this is not a
    /// tick-confirmed mutation event.
    AfterBlockBreak(BlockEvent),
    /// A player crossed a block boundary.
    PlayerMove(MoveEvent),
    /// A vetoable block-placement attempt.
    BlockPlaceAttempt(PlaceAttempt),
    /// A vetoable block-break attempt.
    BlockBreakAttempt(BlockEvent),
    /// A vetoable chat attempt.
    ChatAttempt(ChatAttempt),
    /// A vetoable interaction attempt.
    InteractAttempt(InteractionAttempt),
    /// A registered command was invoked.
    Command(CommandInvocation),
    /// A deterministic timer became due.
    Timer(TimerId),
}

impl Event {
    /// Returns the event discriminant.
    pub const fn kind(&self) -> EventKind {
        match self {
            Event::PlayerJoin(_) => EventKind::PlayerJoin,
            Event::PlayerLeave(_) => EventKind::PlayerLeave,
            Event::BlockBreak(_) => EventKind::BlockBreak,
            Event::AfterBlockPlace(_) => EventKind::AfterBlockPlace,
            Event::AfterBlockBreak(_) => EventKind::AfterBlockBreak,
            Event::PlayerMove(_) => EventKind::PlayerMove,
            Event::BlockPlaceAttempt(_) => EventKind::BlockPlaceAttempt,
            Event::BlockBreakAttempt(_) => EventKind::BlockBreakAttempt,
            Event::ChatAttempt(_) => EventKind::ChatAttempt,
            Event::InteractAttempt(_) => EventKind::InteractAttempt,
            Event::Command(_) => EventKind::Command,
            Event::Timer(_) => EventKind::Timer,
        }
    }
}

/// Bounded plain-text feedback attached to a denied decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feedback {
    message: String,
}

impl Feedback {
    /// Creates bounded plain-text feedback.
    pub fn new(message: impl Into<String>) -> Result<Self, FacadeError> {
        let message = message.into();
        if message.len() > MAX_FEEDBACK_BYTES {
            return Err(FacadeError::LimitExceeded {
                resource: "decision feedback",
                len: message.len(),
                max: MAX_FEEDBACK_BYTES,
            });
        }
        Ok(Self { message })
    }

    /// Returns the plain `UTF-8` feedback.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// A plugin's decision about a pending block placement.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockDecision {
    /// Allow the edit unchanged.
    Allow,
    /// Deny the edit, optionally with player feedback.
    Deny(Option<Feedback>),
    /// Allow a placement with a replacement block-state identifier.
    Replace(u32),
}

/// A plugin's allow-or-deny decision for an action with no replacement state.
///
/// This covers block breaking, chat, and interaction attempts. Block
/// replacement is valid only for placement and uses [`BlockDecision`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventDecision {
    /// Allow the event.
    Allow,
    /// Deny the event, optionally with player feedback.
    Deny(Option<Feedback>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_maps_to_its_stable_kind() {
        let player = PlayerId::offline("EventUser");
        let events = [
            Event::PlayerJoin(PlayerEvent::new(player)),
            Event::PlayerLeave(PlayerEvent::new(player)),
            Event::BlockBreak(BlockEvent::new(player, BlockPos::ORIGIN)),
            Event::AfterBlockPlace(BlockPlaceEvent::new(player, BlockPos::ORIGIN, 1)),
            Event::AfterBlockBreak(BlockEvent::new(player, BlockPos::ORIGIN)),
            Event::PlayerMove(MoveEvent::new(
                player,
                BlockPos::ORIGIN,
                BlockPos::new(1, 0, 0),
            )),
            Event::BlockPlaceAttempt(PlaceAttempt::new(player, BlockPos::ORIGIN, 1)),
            Event::BlockBreakAttempt(BlockEvent::new(player, BlockPos::ORIGIN)),
            Event::ChatAttempt(ChatAttempt::new(player, "hello").expect("bounded chat")),
            Event::InteractAttempt(InteractionAttempt::new(
                player,
                InteractHand::Main,
                InteractTarget::Air,
            )),
            Event::Command(
                CommandInvocation::new(
                    crate::HandlerId::new(1).expect("nonzero"),
                    player,
                    Vec::new(),
                )
                .expect("bounded invocation"),
            ),
            Event::Timer(TimerId::new(1).expect("nonzero")),
        ];
        let kinds: Vec<EventKind> = events.iter().map(Event::kind).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::PlayerJoin,
                EventKind::PlayerLeave,
                EventKind::BlockBreak,
                EventKind::AfterBlockPlace,
                EventKind::AfterBlockBreak,
                EventKind::PlayerMove,
                EventKind::BlockPlaceAttempt,
                EventKind::BlockBreakAttempt,
                EventKind::ChatAttempt,
                EventKind::InteractAttempt,
                EventKind::Command,
                EventKind::Timer,
            ]
        );
    }

    #[test]
    fn text_payloads_are_plain_and_bounded() {
        let player = PlayerId::offline("TextUser");
        assert!(ChatAttempt::new(player, "x".repeat(MAX_CHAT_BYTES + 1)).is_err());
        assert!(Feedback::new("x".repeat(MAX_FEEDBACK_BYTES + 1)).is_err());
        assert_eq!(Feedback::new("plain").expect("bounded").message(), "plain");
    }
}
