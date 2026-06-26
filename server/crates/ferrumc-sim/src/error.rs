//! Error types for the simulation layer.

use ferrumc_core::ServerError;

/// A classifying error returned by simulation operations.
///
/// Each variant names the *kind* of failure so callers can react
/// programmatically rather than parse message strings, per the project's
/// error-classification rule. Convert one into the server-wide [`ServerError`]
/// with the provided [`From`] impl.
///
/// The enum is `#[non_exhaustive]`: more variants will appear as the simulation
/// grows, so downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SimError {
    /// A [`SimShard`](crate::SimShard) inbox was at capacity and rejected an
    /// input.
    ///
    /// The inbox applies *reject* backpressure: it never blocks (it runs on a
    /// sim worker that must not stall) and never silently drops (that would
    /// desync clients). The caller — upstream, the session router — decides what
    /// to do on rejection (retry next tick, coalesce, or disconnect a flooding
    /// client).
    #[error("simulation shard inbox is full (capacity {capacity})")]
    InboxFull {
        /// The fixed inbox capacity that was reached.
        capacity: usize,
    },

    /// The tick counter could not advance because it would overflow [`u64`].
    ///
    /// At 20 ticks per second this cannot occur for roughly 29 billion years; it
    /// is surfaced as a classified error rather than a panic so the counter can
    /// never wrap silently and the no-panic rule still holds.
    #[error("simulation tick counter overflowed u64")]
    TickOverflow,
}

impl From<SimError> for ServerError {
    fn from(error: SimError) -> Self {
        match error {
            SimError::InboxFull { capacity } => {
                ServerError::capacity(format!("simulation shard inbox full (capacity {capacity})"))
            }
            SimError::TickOverflow => {
                ServerError::internal("simulation tick counter overflowed u64")
            }
        }
    }
}

/// Convenience result alias whose error type is [`SimError`].
///
/// Prefer this over spelling out `Result<T, SimError>` by hand.
pub type SimResult<T> = core::result::Result<T, SimError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_strings_are_classified() {
        assert_eq!(
            SimError::InboxFull { capacity: 8 }.to_string(),
            "simulation shard inbox is full (capacity 8)"
        );
        assert_eq!(
            SimError::TickOverflow.to_string(),
            "simulation tick counter overflowed u64"
        );
    }

    #[test]
    fn inbox_full_maps_to_capacity_server_error() {
        let server: ServerError = SimError::InboxFull { capacity: 4 }.into();
        assert!(matches!(server, ServerError::Capacity(_)));
    }

    #[test]
    fn tick_overflow_maps_to_internal_server_error() {
        let server: ServerError = SimError::TickOverflow.into();
        assert!(matches!(server, ServerError::Internal { .. }));
    }

    #[test]
    fn variants_compare_by_value() {
        assert_eq!(
            SimError::InboxFull { capacity: 1 },
            SimError::InboxFull { capacity: 1 }
        );
        assert_ne!(
            SimError::InboxFull { capacity: 1 },
            SimError::InboxFull { capacity: 2 }
        );
    }
}
