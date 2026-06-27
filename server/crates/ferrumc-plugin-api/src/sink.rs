//! The sink a plugin submits mutation intents to.

use ferrumc_math::WorldIntent;

use crate::error::IntentError;

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
