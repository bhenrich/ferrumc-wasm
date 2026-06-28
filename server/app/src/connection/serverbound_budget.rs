//! Per-connection serverbound packet budget: a token bucket that gates how fast a
//! client may send play frames, dropping a sustained flood.
//!
//! Wraps the networking [`PacketBudget`] with the per-connection over-budget tally
//! the play loop publishes into telemetry. Lives in the connection task (which is
//! allowed a non-deterministic [`Instant`] clock) and is never consulted from the
//! deterministic sim/session tick path — it is a small, pure struct so the policy
//! is unit-testable without a live socket.

use std::time::Instant;

use ferrumc_net::{DisconnectReason, PacketBudget};

/// A per-connection serverbound frame budget plus its over-budget tally.
///
/// Each play connection owns one private bucket, so one peer's flood never eats
/// into another's headroom. Every serverbound frame is [`admit`](Self::admit)ted
/// before it is handled; once the burst is drained and the sustained rate is
/// exceeded, the connection is dropped with [`DisconnectReason::BudgetExceeded`].
pub(super) struct ServerboundBudget {
    /// The token bucket charged one token per serverbound frame.
    budget: PacketBudget,
    /// Frames classified over budget for this connection (published into the
    /// per-player network telemetry). Saturates rather than wraps.
    over_budget: u64,
}

impl ServerboundBudget {
    /// Builds a full bucket as of `now` with the configured sustained
    /// `rate_per_sec` and `burst`.
    pub(super) fn new(now: Instant, rate_per_sec: f64, burst: f64) -> Self {
        Self {
            budget: PacketBudget::new(now, rate_per_sec, burst),
            over_budget: 0,
        }
    }

    /// Charges one serverbound frame as of `now`.
    ///
    /// Returns `Ok(())` while the peer stays within budget. On the first
    /// over-budget frame it tallies the event and returns the
    /// [`DisconnectReason::BudgetExceeded`] the caller drops the connection with —
    /// a deterministic, documented response to a flood rather than an unbounded
    /// backlog of unhandled frames.
    pub(super) fn admit(&mut self, now: Instant) -> Result<(), DisconnectReason> {
        self.budget.admit_frame(now).inspect_err(|_reason| {
            self.over_budget = self.over_budget.saturating_add(1);
        })
    }

    /// The number of serverbound frames classified over budget for this
    /// connection, for the per-player network telemetry snapshot.
    pub(super) fn over_budget(&self) -> u64 {
        self.over_budget
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ferrumc_net::DisconnectReason;

    use super::ServerboundBudget;

    #[test]
    fn normal_traffic_stays_within_budget() {
        let now = Instant::now();
        let mut budget = ServerboundBudget::new(now, 300.0, 600.0);
        // A realistic burst of movement/actions well under the 600 burst passes
        // without ever tripping the budget.
        for _ in 0..200 {
            assert_eq!(budget.admit(now), Ok(()));
        }
        assert_eq!(budget.over_budget(), 0);
    }

    #[test]
    fn a_flood_past_the_burst_is_disconnected_and_counted() {
        let now = Instant::now();
        // A small bucket charged at a single instant: the burst passes, then the
        // flood is dropped deterministically and every over-budget frame is tallied.
        let mut budget = ServerboundBudget::new(now, 300.0, 5.0);
        for _ in 0..5 {
            assert_eq!(budget.admit(now), Ok(()));
        }
        assert_eq!(budget.admit(now), Err(DisconnectReason::BudgetExceeded));
        assert_eq!(budget.admit(now), Err(DisconnectReason::BudgetExceeded));
        assert_eq!(budget.over_budget(), 2);
    }

    #[test]
    fn the_bucket_refills_so_a_paced_client_is_never_dropped() {
        let t0 = Instant::now();
        let mut budget = ServerboundBudget::new(t0, 300.0, 1.0);
        assert_eq!(budget.admit(t0), Ok(()));
        assert_eq!(budget.admit(t0), Err(DisconnectReason::BudgetExceeded));
        // ~4 ms later (> 1/300 s) a token has refilled, so a client that respected
        // the rate is admitted again rather than dropped.
        let t1 = t0 + Duration::from_millis(4);
        assert_eq!(budget.admit(t1), Ok(()));
    }
}
