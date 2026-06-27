//! World-mutation intents shared across the simulation, session, and plugin
//! layers.

use ferrumc_core::{PlayerId, TextComponent};

use crate::{BlockPos, Vec3};

/// A requested world change submitted to the simulation to perform.
///
/// Callers never mutate the world directly; they submit *intents* that the
/// simulation validates and applies at a tick boundary (or rejects), preserving
/// the rule that simulation state is only changed by the simulation itself.
///
/// This type lives in `ferrumc-math` (the lowest crate that already owns
/// [`BlockPos`]/[`Vec3`] and can see `ferrumc-core`'s [`PlayerId`] /
/// [`TextComponent`]) so the simulation and session layers can route intents
/// without depending on the plugin API. The plugin API re-exports it for
/// backward compatibility.
///
/// The enum is `#[non_exhaustive]`: new intent kinds will be added as the
/// simulation grows, so downstream `match`es must include a wildcard arm.
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
