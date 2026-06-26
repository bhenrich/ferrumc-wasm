//! Mutation intents and the sink a plugin submits them to.

use ferrumc_core::{PlayerId, TextComponent};
use ferrumc_math::{BlockPos, Vec3};

use crate::error::IntentError;

/// A requested world change a plugin asks the simulation to perform.
///
/// Plugins never mutate the world directly; they submit *intents* through a
/// [`CommandSink`]. The simulation validates and applies them at a tick
/// boundary (or rejects them), preserving the rule that simulation state is only
/// changed by the simulation itself. The enum is `#[non_exhaustive]`.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum WorldIntent {
    /// Set the block at `pos` to the given block-state id.
    SetBlock {
        /// Where to place the block.
        pos: BlockPos,
        /// The opaque registry block-state id to place.
        block_state_id: u32,
    },
    /// Teleport `player` to `position`.
    Teleport {
        /// The player to move.
        player: PlayerId,
        /// The destination.
        position: Vec3,
    },
    /// Send `message` to `player`.
    Message {
        /// The recipient.
        player: PlayerId,
        /// The chat message to send.
        message: TextComponent,
    },
}

/// Accepts mutation intents from a plugin during a single event.
///
/// Like [`WorldView`](crate::WorldView), a sink is valid only for the call in
/// which it is handed out, and is never a raw channel to the simulation. This
/// trait is a shell; the simulation layer provides the concrete implementation
/// that bounds and applies the queued intents.
pub trait CommandSink {
    /// Queues `intent` for the simulation to apply later.
    ///
    /// Returns [`IntentError::QueueFull`] if the bounded intent queue is full,
    /// or [`IntentError::Rejected`] if the intent is refused by policy.
    fn submit(&mut self, intent: WorldIntent) -> Result<(), IntentError>;
}
