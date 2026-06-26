//! The fixed-rate tick coordinator.

use core::num::NonZeroU32;

use ferrumc_core::Tick;

use crate::error::SimError;

/// Builds a [`NonZeroU32`] in const context, falling back to `1` for a zero
/// input.
///
/// Used only for compile-time constants whose literal is known non-zero; the
/// fallback exists solely so the expression is total without a panic.
const fn non_zero_u32(value: u32) -> NonZeroU32 {
    match NonZeroU32::new(value) {
        Some(v) => v,
        None => NonZeroU32::MIN,
    }
}

/// A fixed simulation tick rate, in ticks per second.
///
/// The rate is *informational*: the [`TickCoordinator`] step API never consults
/// a wall clock, so a `TickRate` only records the target cadence (and the
/// nominal per-tick duration) for downstream schedulers and metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TickRate {
    ticks_per_second: NonZeroU32,
}

impl TickRate {
    /// The vanilla Minecraft target of 20 ticks per second.
    pub const VANILLA: Self = Self {
        ticks_per_second: non_zero_u32(20),
    };

    /// Creates a tick rate of `ticks_per_second` ticks per second.
    ///
    /// The rate is a [`NonZeroU32`] so a zero-rate (which has no meaningful
    /// per-tick duration) is unrepresentable.
    pub const fn new(ticks_per_second: NonZeroU32) -> Self {
        Self { ticks_per_second }
    }

    /// Returns the configured ticks per second.
    pub const fn ticks_per_second(self) -> u32 {
        self.ticks_per_second.get()
    }

    /// Returns the nominal duration of one tick in nanoseconds.
    ///
    /// This is `1_000_000_000 / ticks_per_second` (50 ms at 20 TPS), provided
    /// for downstream schedulers; the coordinator itself never sleeps.
    pub fn nanos_per_tick(self) -> u64 {
        1_000_000_000u64 / u64::from(self.ticks_per_second.get())
    }
}

impl Default for TickRate {
    fn default() -> Self {
        Self::VANILLA
    }
}

/// Drives the simulation forward one tick at a time.
///
/// The coordinator owns the authoritative [`Tick`] counter and advances it by
/// exactly one on each [`advance`](TickCoordinator::advance) call. The step API
/// is fully deterministic and **never** consults a wall clock, so tests can
/// drive it directly.
///
/// # No catch-up
///
/// Each call advances exactly one tick. There is deliberately no mechanism to
/// run several ticks to "catch up" to a missed wall-clock deadline: catch-up
/// ticks turn transient lag into a death spiral (see the overload-handling rules
/// in `CLAUDE.md`). A real run loop that falls behind simply ticks late and
/// reports lag; it never calls `advance` extra times.
#[derive(Debug, Clone)]
pub struct TickCoordinator {
    current: Tick,
    rate: TickRate,
}

impl TickCoordinator {
    /// Creates a coordinator starting at [`Tick::ZERO`] with the given `rate`.
    pub const fn new(rate: TickRate) -> Self {
        Self {
            current: Tick::ZERO,
            rate,
        }
    }

    /// Creates a coordinator resuming at `start` (for example after loading a
    /// saved world) with the given `rate`.
    pub const fn resuming_at(rate: TickRate, start: Tick) -> Self {
        Self {
            current: start,
            rate,
        }
    }

    /// Returns the most recently completed tick (the current counter value).
    ///
    /// Before the first [`advance`](TickCoordinator::advance) this is the start
    /// tick ([`Tick::ZERO`] for [`new`](TickCoordinator::new)).
    pub const fn current(&self) -> Tick {
        self.current
    }

    /// Returns the configured [`TickRate`].
    pub const fn rate(&self) -> TickRate {
        self.rate
    }

    /// Advances the counter by exactly one tick and returns the new value.
    ///
    /// Advances by one and only one tick — there is no catch-up. Returns
    /// [`SimError::TickOverflow`] if the counter would overflow [`u64`], leaving
    /// the counter unchanged, so it can never wrap silently.
    pub fn advance(&mut self) -> Result<Tick, SimError> {
        let next = self.current.next().ok_or(SimError::TickOverflow)?;
        self.current = next;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_rate_is_vanilla() {
        assert_eq!(TickRate::default(), TickRate::VANILLA);
        assert_eq!(TickRate::VANILLA.ticks_per_second(), 20);
    }

    #[test]
    fn nanos_per_tick_matches_rate() {
        assert_eq!(TickRate::VANILLA.nanos_per_tick(), 50_000_000);
        let ten = TickRate::new(NonZeroU32::new(10).expect("nonzero"));
        assert_eq!(ten.nanos_per_tick(), 100_000_000);
    }

    #[test]
    fn new_starts_at_zero() {
        let coord = TickCoordinator::new(TickRate::VANILLA);
        assert_eq!(coord.current(), Tick::ZERO);
        assert_eq!(coord.rate(), TickRate::VANILLA);
    }

    #[test]
    fn advance_increments_by_one() {
        let mut coord = TickCoordinator::new(TickRate::VANILLA);
        assert_eq!(coord.advance(), Ok(Tick::new(1)));
        assert_eq!(coord.advance(), Ok(Tick::new(2)));
        assert_eq!(coord.current(), Tick::new(2));
    }

    #[test]
    fn resuming_at_continues_from_start() {
        let mut coord = TickCoordinator::resuming_at(TickRate::VANILLA, Tick::new(100));
        assert_eq!(coord.current(), Tick::new(100));
        assert_eq!(coord.advance(), Ok(Tick::new(101)));
    }

    #[test]
    fn advance_reports_overflow_without_wrapping() {
        let mut coord = TickCoordinator::resuming_at(TickRate::VANILLA, Tick::new(u64::MAX));
        assert_eq!(coord.advance(), Err(SimError::TickOverflow));
        // The counter is left untouched on overflow.
        assert_eq!(coord.current(), Tick::new(u64::MAX));
    }
}
