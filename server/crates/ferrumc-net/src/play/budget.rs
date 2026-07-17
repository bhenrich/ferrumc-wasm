//! [`PacketBudget`]: a per-connection token-bucket rate limiter that classifies
//! serverbound play frames as within or over budget.

use std::time::Instant;

use super::disconnect::DisconnectReason;

/// Default sustained serverbound play-frame rate, in frames per second.
///
/// Mirrors the networking model's 300 frames/sec budget: a well-behaved client
/// sending movement, actions, and keep-alives stays comfortably under it, while
/// a flooding client is flagged.
pub const DEFAULT_PLAY_FRAME_RATE: f64 = 300.0;

/// Default token-bucket burst capacity, in frames.
///
/// Set to twice the sustained rate so a brief spike (a flurry of actions on a
/// laggy tick) is absorbed without flagging, but a sustained flood drains the
/// bucket and is classified over budget.
pub const DEFAULT_PLAY_FRAME_BURST: f64 = 600.0;

/// Whether a charged frame stayed within the rate budget.
///
/// Returned by [`PacketBudget::charge`] and surfaced per packet by the
/// [`PlayReader`](crate::PlayReader). It is a *classification*, not an action:
/// the caller decides whether to throttle, ignore, or escalate an over-budget
/// frame to a [`DisconnectReason::BudgetExceeded`](crate::DisconnectReason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetStatus {
    /// The frame was charged against available tokens; the peer is within its
    /// rate budget.
    WithinBudget,
    /// The bucket was empty: the peer exceeded its sustained rate. The frame is
    /// still decoded and returned, but flagged.
    OverBudget,
}

impl BudgetStatus {
    /// `true` when the frame exceeded the rate budget.
    pub fn is_over_budget(self) -> bool {
        matches!(self, Self::OverBudget)
    }

    /// `true` when the frame was within the rate budget.
    pub fn is_within_budget(self) -> bool {
        matches!(self, Self::WithinBudget)
    }
}

/// A byte-denominated token bucket for pre-decompression wire admission.
///
/// This is deliberately distinct from [`PacketBudget`]: packet-budget tokens
/// represent complete frames and overage is advisory, while this bucket charges
/// the exact outer wire span and a [`PlayIngress`](crate::PlayIngress) treats
/// overage as fatal before attempting decompression.
#[derive(Debug, Clone)]
pub struct WireByteBudget {
    bucket: PacketBudget,
    admitted_bytes: u64,
}

impl WireByteBudget {
    /// Creates a byte budget starting with `burst_bytes` available.
    ///
    /// `bytes_per_sec` is the sustained byte refill rate and `burst_bytes` the
    /// maximum accumulated allowance. Invalid numeric inputs fail closed through
    /// the same sanitization as [`PacketBudget::new`].
    pub fn new(now: Instant, bytes_per_sec: f64, burst_bytes: f64) -> Self {
        Self {
            bucket: PacketBudget::new(now, bytes_per_sec, burst_bytes),
            admitted_bytes: 0,
        }
    }

    /// The sustained refill rate in wire bytes per second.
    pub fn bytes_per_sec(&self) -> f64 {
        self.bucket.rate_per_sec()
    }

    /// The maximum accumulated wire-byte allowance.
    pub fn burst_bytes(&self) -> f64 {
        self.bucket.burst()
    }

    /// The currently available wire-byte allowance.
    pub fn available_bytes(&self) -> f64 {
        self.bucket.available_tokens()
    }

    /// The total successfully admitted wire bytes, saturating at `u64::MAX`.
    pub fn admitted_bytes(&self) -> u64 {
        self.admitted_bytes
    }

    /// Refills the byte allowance for elapsed caller-supplied time.
    pub fn refill(&mut self, now: Instant) {
        self.bucket.refill(now);
    }

    /// Charges one complete outer frame span.
    ///
    /// A span too large for the underlying integer cost fails closed. Failed
    /// admission does not deduct tokens or increment the admitted-byte counter.
    pub(crate) fn admit(&mut self, now: Instant, wire_bytes: usize) -> BudgetStatus {
        let Ok(cost) = u32::try_from(wire_bytes) else {
            self.bucket.refill(now);
            return BudgetStatus::OverBudget;
        };
        let status = self.bucket.charge(now, cost);
        if status.is_within_budget() {
            let wire_bytes = u64::try_from(wire_bytes).unwrap_or(u64::MAX);
            self.admitted_bytes = self.admitted_bytes.saturating_add(wire_bytes);
        }
        status
    }
}

/// A token bucket that rate-limits serverbound play frames.
///
/// Tokens refill continuously at [`rate_per_sec`](Self::rate_per_sec) and are
/// capped at [`burst`](Self::burst). Each frame [`charge`](Self::charge)s one or
/// more tokens; when the bucket cannot cover the cost the frame is classified
/// [`BudgetStatus::OverBudget`] and **no** tokens are deducted (the count never
/// goes negative), so the peer stays flagged until the bucket refills.
///
/// Time is supplied by the caller as an [`Instant`] rather than read from the
/// clock internally, so the bucket is fully deterministic and unit-testable
/// without sleeping.
#[derive(Debug, Clone)]
pub struct PacketBudget {
    rate_per_sec: f64,
    burst: f64,
    tokens: f64,
    last_refill: Instant,
}

impl PacketBudget {
    /// Creates a bucket starting full (`burst` tokens) as of `now`.
    ///
    /// `rate_per_sec` is the sustained refill rate and `burst` the maximum
    /// accumulated tokens. Non-finite or negative inputs are clamped to `0.0`, so
    /// a misconfiguration degrades to "always over budget" rather than producing
    /// `NaN` arithmetic.
    pub fn new(now: Instant, rate_per_sec: f64, burst: f64) -> Self {
        let rate_per_sec = sanitize(rate_per_sec);
        let burst = sanitize(burst);
        Self {
            rate_per_sec,
            burst,
            tokens: burst,
            last_refill: now,
        }
    }

    /// Creates a bucket with the [`DEFAULT_PLAY_FRAME_RATE`] and
    /// [`DEFAULT_PLAY_FRAME_BURST`], starting full as of `now`.
    pub fn with_defaults(now: Instant) -> Self {
        Self::new(now, DEFAULT_PLAY_FRAME_RATE, DEFAULT_PLAY_FRAME_BURST)
    }

    /// The sustained refill rate, in tokens per second.
    pub fn rate_per_sec(&self) -> f64 {
        self.rate_per_sec
    }

    /// The maximum number of tokens the bucket holds.
    pub fn burst(&self) -> f64 {
        self.burst
    }

    /// The number of tokens currently available, as of the last
    /// [`charge`](Self::charge) or [`refill`](Self::refill).
    pub fn available_tokens(&self) -> f64 {
        self.tokens
    }

    /// Refills the bucket for the time elapsed since the last update, capping at
    /// [`burst`](Self::burst).
    ///
    /// Called automatically by [`charge`](Self::charge); exposed so a caller can
    /// advance the bucket on an idle tick. A `now` earlier than the last update
    /// (a non-monotonic read) contributes no tokens rather than removing any.
    pub fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate_per_sec).min(self.burst);
        self.last_refill = now;
    }

    /// Refills for the elapsed time, then charges `cost` tokens for a frame.
    ///
    /// Returns [`BudgetStatus::WithinBudget`] and deducts the cost when the
    /// bucket can cover it, otherwise [`BudgetStatus::OverBudget`] with the token
    /// count left untouched.
    pub fn charge(&mut self, now: Instant, cost: u32) -> BudgetStatus {
        self.refill(now);
        let cost = f64::from(cost);
        if self.tokens >= cost {
            self.tokens -= cost;
            BudgetStatus::WithinBudget
        } else {
            BudgetStatus::OverBudget
        }
    }

    /// Charges one serverbound play frame and reports whether the connection must
    /// be torn down for sustained over-budget abuse.
    ///
    /// Refills for the elapsed time, then charges a single token: the v1 uniform
    /// per-frame cost. Every serverbound frame is weighed equally — there is no
    /// per-packet criticality discount, so the policy is trivial to reason about.
    /// Returns `Ok(())` while the peer stays within its [`burst`](Self::burst) and
    /// sustained budget, or `Err(`[`DisconnectReason::BudgetExceeded`]`)` the moment
    /// the bucket cannot cover the frame, letting the caller drop a flooding client
    /// deterministically.
    ///
    /// The default `600`-token burst (twice the sustained rate) leaves ample
    /// headroom for the mandatory frames of normal play — keep-alive echoes and
    /// teleport confirms — so a well-behaved client never trips this; only a
    /// sustained flood beyond the rate drains the bucket.
    pub fn admit_frame(&mut self, now: Instant) -> Result<(), DisconnectReason> {
        match self.charge(now, 1) {
            BudgetStatus::WithinBudget => Ok(()),
            BudgetStatus::OverBudget => Err(DisconnectReason::BudgetExceeded),
        }
    }
}

/// Clamps a configured rate/burst to a finite, non-negative value.
fn sanitize(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    // Token counts in these tests are exact, representable values (integers and
    // halves), so exact float comparison is intentional and correct.
    #![allow(clippy::float_cmp)]

    use std::time::Duration;

    use super::*;

    #[test]
    fn starts_full_and_within_budget() {
        let now = Instant::now();
        let mut budget = PacketBudget::new(now, 300.0, 600.0);
        assert_eq!(budget.available_tokens(), 600.0);
        assert_eq!(budget.charge(now, 1), BudgetStatus::WithinBudget);
        assert_eq!(budget.available_tokens(), 599.0);
    }

    #[test]
    fn draining_the_bucket_flags_over_budget() {
        let now = Instant::now();
        // A tiny bucket with no refill window: 3 tokens, then flagged.
        let mut budget = PacketBudget::new(now, 300.0, 3.0);
        assert_eq!(budget.charge(now, 1), BudgetStatus::WithinBudget);
        assert_eq!(budget.charge(now, 1), BudgetStatus::WithinBudget);
        assert_eq!(budget.charge(now, 1), BudgetStatus::WithinBudget);
        assert_eq!(budget.charge(now, 1), BudgetStatus::OverBudget);
        // No tokens were deducted on the over-budget charge.
        assert_eq!(budget.available_tokens(), 0.0);
    }

    #[test]
    fn refill_restores_tokens_over_time() {
        let t0 = Instant::now();
        let mut budget = PacketBudget::new(t0, 300.0, 300.0);
        // Drain it.
        for _ in 0..300 {
            assert_eq!(budget.charge(t0, 1), BudgetStatus::WithinBudget);
        }
        assert_eq!(budget.charge(t0, 1), BudgetStatus::OverBudget);
        // 100 ms at 300/s refills ~30 tokens.
        let t1 = t0 + Duration::from_millis(100);
        budget.refill(t1);
        assert!((budget.available_tokens() - 30.0).abs() < 1e-6);
        assert_eq!(budget.charge(t1, 1), BudgetStatus::WithinBudget);
    }

    #[test]
    fn refill_is_capped_at_burst() {
        let t0 = Instant::now();
        let mut budget = PacketBudget::new(t0, 300.0, 600.0);
        budget.charge(t0, 600); // empty it
        assert_eq!(budget.available_tokens(), 0.0);
        // A long idle period must not overflow past the burst cap.
        let t1 = t0 + Duration::from_mins(1);
        budget.refill(t1);
        assert_eq!(budget.available_tokens(), 600.0);
    }

    #[test]
    fn non_monotonic_time_adds_no_tokens() {
        let t0 = Instant::now();
        let mut budget = PacketBudget::new(t0, 300.0, 600.0);
        budget.charge(t0, 100);
        let before = budget.available_tokens();
        // A `now` earlier than the last update must not remove or add tokens.
        let earlier = t0.checked_sub(Duration::from_secs(1)).unwrap_or(t0);
        budget.refill(earlier);
        assert_eq!(budget.available_tokens(), before);
    }

    #[test]
    fn a_multi_token_charge_larger_than_the_bucket_is_over_budget() {
        let now = Instant::now();
        let mut budget = PacketBudget::new(now, 300.0, 5.0);
        assert_eq!(budget.charge(now, 10), BudgetStatus::OverBudget);
        assert_eq!(budget.available_tokens(), 5.0);
    }

    #[test]
    fn misconfigured_rate_degrades_to_always_over_budget() {
        let now = Instant::now();
        let mut budget = PacketBudget::new(now, f64::NAN, -5.0);
        assert_eq!(budget.rate_per_sec(), 0.0);
        assert_eq!(budget.burst(), 0.0);
        assert_eq!(budget.charge(now, 1), BudgetStatus::OverBudget);
    }

    #[test]
    fn admit_frame_passes_a_full_burst_within_budget() {
        let now = Instant::now();
        let mut budget = PacketBudget::new(now, 300.0, 600.0);
        // The whole burst, charged at one instant, is admitted without a disconnect.
        for _ in 0..600 {
            assert_eq!(budget.admit_frame(now), Ok(()));
        }
    }

    #[test]
    fn admit_frame_disconnects_once_the_burst_is_drained() {
        let now = Instant::now();
        // A 5-token burst with no refill window: a flood past it trips the budget.
        let mut budget = PacketBudget::new(now, 300.0, 5.0);
        for _ in 0..5 {
            assert_eq!(budget.admit_frame(now), Ok(()));
        }
        assert_eq!(
            budget.admit_frame(now),
            Err(DisconnectReason::BudgetExceeded)
        );
        // Still over budget while the bucket stays empty.
        assert_eq!(
            budget.admit_frame(now),
            Err(DisconnectReason::BudgetExceeded)
        );
    }

    #[test]
    fn admit_frame_recovers_after_refill() {
        let t0 = Instant::now();
        let mut budget = PacketBudget::new(t0, 300.0, 1.0);
        assert_eq!(budget.admit_frame(t0), Ok(()));
        assert_eq!(
            budget.admit_frame(t0),
            Err(DisconnectReason::BudgetExceeded)
        );
        // ~4 ms later (> 1/300 s) a single token has refilled, so a paced client
        // that respected the rate is admitted again rather than dropped.
        let t1 = t0 + Duration::from_millis(4);
        assert_eq!(budget.admit_frame(t1), Ok(()));
    }
}
