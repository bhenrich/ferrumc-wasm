//! Per-connection chat rate limiting (a tick-driven token bucket).

use ferrumc_core::Tick;

/// Maximum number of relayed chat lines a connection may burst before the rate
/// limiter throttles it.
///
/// One client must not fan spam into every recipient's bounded outbound channel;
/// a burst of `8` covers normal conversation while capping a flood.
const CHAT_BURST: u32 = 8;

/// Server ticks that must elapse to refill one chat token.
///
/// At the 20 TPS target, `10` ticks is ~0.5 s, so the sustained relay rate is ~2
/// lines/second once the burst is spent.
const CHAT_TICKS_PER_TOKEN: u64 = 10;

/// A per-connection token bucket that paces relayed chat so one client cannot
/// saturate every recipient's bounded outbound channel.
///
/// Driven by the monotonic server tick ([`ServerClock`](ferrumc_observability::ServerClock),
/// no syscall): tokens refill at [`CHAT_TICKS_PER_TOKEN`] and are capped at
/// [`CHAT_BURST`]. This lives in the connection task — which is allowed a
/// non-deterministic time source — and is never consulted from a deterministic
/// sim/session tick path. It is a small, pure struct so the policy is
/// unit-testable without a live socket.
pub(super) struct ChatRateLimiter {
    /// Tokens currently available; one is spent per relayed line.
    tokens: u32,
    /// The tick the token count was last reconciled against.
    last_tick: Tick,
}

impl ChatRateLimiter {
    /// Builds a full bucket as of `now`.
    pub(super) fn new(now: Tick) -> Self {
        Self {
            tokens: CHAT_BURST,
            last_tick: now,
        }
    }

    /// Refills any tokens earned since the last call, then spends one.
    ///
    /// Returns `true` if a token was available (the line may be relayed) or `false`
    /// if the sender is over budget (drop the line). `last_tick` advances only by
    /// whole refill intervals, so partial progress toward the next token is not
    /// lost.
    pub(super) fn try_consume(&mut self, now: Tick) -> bool {
        let elapsed = now.get().saturating_sub(self.last_tick.get());
        if elapsed >= CHAT_TICKS_PER_TOKEN {
            let intervals = elapsed / CHAT_TICKS_PER_TOKEN;
            let refill = u32::try_from(intervals.min(u64::from(CHAT_BURST))).unwrap_or(CHAT_BURST);
            self.tokens = self.tokens.saturating_add(refill).min(CHAT_BURST);
            self.last_tick = Tick::new(self.last_tick.get() + intervals * CHAT_TICKS_PER_TOKEN);
        }
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use ferrumc_core::Tick;

    use super::{ChatRateLimiter, CHAT_BURST, CHAT_TICKS_PER_TOKEN};

    #[test]
    fn chat_rate_limiter_allows_a_burst_then_throttles_until_refill() {
        let mut limiter = ChatRateLimiter::new(Tick::new(0));
        // The full burst is allowed within a single tick.
        for _ in 0..CHAT_BURST {
            assert!(
                limiter.try_consume(Tick::new(0)),
                "burst line within budget"
            );
        }
        // The next line at the same tick is over budget and dropped.
        assert!(
            !limiter.try_consume(Tick::new(0)),
            "over-budget line throttled"
        );
        // One refill interval later, exactly one more line is allowed.
        assert!(limiter.try_consume(Tick::new(CHAT_TICKS_PER_TOKEN)));
        assert!(!limiter.try_consume(Tick::new(CHAT_TICKS_PER_TOKEN)));
    }

    #[test]
    fn chat_rate_limiter_refill_is_capped_at_the_burst() {
        let mut limiter = ChatRateLimiter::new(Tick::new(0));
        // Spend the whole burst.
        for _ in 0..CHAT_BURST {
            assert!(limiter.try_consume(Tick::new(0)));
        }
        // A long idle gap refills at most CHAT_BURST tokens, never more.
        let far = Tick::new(CHAT_TICKS_PER_TOKEN * u64::from(CHAT_BURST) * 100);
        for _ in 0..CHAT_BURST {
            assert!(limiter.try_consume(far), "refilled up to the burst cap");
        }
        assert!(!limiter.try_consume(far), "cannot exceed the burst cap");
    }
}
