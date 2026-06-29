//! Deterministic world time: the day-night clock the simulation advances.
//!
//! [`WorldTime`] is a small value type the driver owns and advances exactly once
//! per tick (alongside the authoritative [`Tick`](ferrumc_core::Tick) counter), so
//! the clock is a pure function of the tick count and any `/time` adjustments — no
//! wall clock, fully replayable.
//!
//! It carries two quantities, mirroring the wire fields of the clientbound Update
//! Time packet:
//!
//! * `world_age` — the monotonic total number of ticks the world has run. Only
//!   ever increases (saturating at [`i64::MAX`]); never wraps or rewinds, even
//!   when `/time` rewinds the day-night phase.
//! * `time_of_day` — the day-night phase in ticks, kept in `0..`[`DAY_LENGTH_TICKS`]
//!   so it wraps once per Minecraft day. The canonical phases are [`TIME_DAY`],
//!   [`TIME_NOON`], [`TIME_NIGHT`], and [`TIME_MIDNIGHT`].
//!
//! Whether the phase advances automatically is on by default (the vanilla
//! `doDaylightCycle` gamerule equivalent); a configurable gamerule toggle is
//! deferred. The driver always advances the clock and tells clients the phase is
//! increasing.

/// Number of ticks in a full Minecraft day-night cycle.
///
/// [`WorldTime::time_of_day`] is kept in `0..DAY_LENGTH_TICKS` and wraps here.
pub const DAY_LENGTH_TICKS: i64 = 24_000;

/// `time_of_day` for `/time set day` — sunrise, the start of the working day.
pub const TIME_DAY: i64 = 1_000;
/// `time_of_day` for `/time set noon` — the sun at its zenith.
pub const TIME_NOON: i64 = 6_000;
/// `time_of_day` for `/time set night` — dusk, when hostile mobs would spawn.
pub const TIME_NIGHT: i64 = 13_000;
/// `time_of_day` for `/time set midnight` — the moon at its zenith.
pub const TIME_MIDNIGHT: i64 = 18_000;

/// The deterministic day-night clock for the world.
///
/// See the [module docs](self) for the invariants on `world_age` (monotonic) and
/// `time_of_day` (wrapped to `0..`[`DAY_LENGTH_TICKS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorldTime {
    /// Monotonic total world age in ticks (never wraps or rewinds).
    world_age: i64,
    /// Day-night phase in ticks, always kept in `0..DAY_LENGTH_TICKS`.
    time_of_day: i64,
}

impl WorldTime {
    /// A fresh clock at world age `0` and time-of-day `0` (the very start of a
    /// day, just before [`TIME_DAY`]).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            world_age: 0,
            time_of_day: 0,
        }
    }

    /// The monotonic total world age in ticks.
    #[must_use]
    pub const fn world_age(&self) -> i64 {
        self.world_age
    }

    /// The current day-night phase in ticks, always in `0..`[`DAY_LENGTH_TICKS`].
    #[must_use]
    pub const fn time_of_day(&self) -> i64 {
        self.time_of_day
    }

    /// Advances the clock by one tick: `world_age` increments (saturating at
    /// [`i64::MAX`] so it can never wrap negative) and `time_of_day` advances by
    /// one, wrapping back to `0` after [`DAY_LENGTH_TICKS`].
    pub fn advance(&mut self) {
        self.world_age = self.world_age.saturating_add(1);
        self.time_of_day = wrap_day(self.time_of_day + 1);
    }

    /// Sets the absolute day-night phase — the `/time set <ticks>` effect.
    ///
    /// `ticks` is wrapped into `0..`[`DAY_LENGTH_TICKS`], so a value past a day (or
    /// a negative one) maps onto the equivalent in-range phase. `world_age` is left
    /// untouched: setting the time of day never rewinds the world's age.
    pub fn set_time_of_day(&mut self, ticks: i64) {
        self.time_of_day = wrap_day(ticks);
    }

    /// Adds `ticks` to the day-night phase — the `/time add <ticks>` effect.
    ///
    /// The result wraps within `0..`[`DAY_LENGTH_TICKS`]; `ticks` may be negative
    /// to wind the phase backwards. `world_age` is left untouched.
    pub fn add_time(&mut self, ticks: i64) {
        // Wrap the addend first so a near-`i64::MAX` argument cannot overflow the
        // intermediate sum (both operands are then in `0..DAY_LENGTH_TICKS`).
        self.time_of_day = wrap_day(self.time_of_day + wrap_day(ticks));
    }
}

/// Reduces any tick value into the canonical `0..`[`DAY_LENGTH_TICKS`] range.
///
/// Uses Euclidean remainder so negative inputs map to a positive phase (e.g. `-1`
/// becomes `DAY_LENGTH_TICKS - 1`), matching how Minecraft folds out-of-range
/// `/time` arguments.
fn wrap_day(ticks: i64) -> i64 {
    ticks.rem_euclid(DAY_LENGTH_TICKS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_at_zero() {
        let time = WorldTime::new();
        assert_eq!(time.world_age(), 0);
        assert_eq!(time.time_of_day(), 0);
        assert_eq!(time, WorldTime::default());
    }

    #[test]
    fn advance_increments_both_counters() {
        let mut time = WorldTime::new();
        time.advance();
        assert_eq!(time.world_age(), 1);
        assert_eq!(time.time_of_day(), 1);
    }

    #[test]
    fn time_of_day_wraps_at_a_full_day_but_age_does_not() {
        let mut time = WorldTime::new();
        // Run exactly one full day plus one tick.
        for _ in 0..=DAY_LENGTH_TICKS {
            time.advance();
        }
        // Age is monotonic across the wrap; phase folded back to 1.
        assert_eq!(time.world_age(), DAY_LENGTH_TICKS + 1);
        assert_eq!(time.time_of_day(), 1);
    }

    #[test]
    fn set_time_of_day_wraps_and_leaves_age_untouched() {
        let mut time = WorldTime::new();
        time.advance();
        let age = time.world_age();

        time.set_time_of_day(TIME_DAY);
        assert_eq!(time.time_of_day(), 1_000);

        // A value past a full day folds back into range; age never moves.
        time.set_time_of_day(DAY_LENGTH_TICKS + TIME_NOON);
        assert_eq!(time.time_of_day(), TIME_NOON);

        // A negative set folds to the equivalent positive phase.
        time.set_time_of_day(-1);
        assert_eq!(time.time_of_day(), DAY_LENGTH_TICKS - 1);

        assert_eq!(time.world_age(), age);
    }

    #[test]
    fn add_time_wraps_in_both_directions() {
        let mut time = WorldTime::new();
        time.set_time_of_day(TIME_NIGHT);

        time.add_time(1_000);
        assert_eq!(time.time_of_day(), 14_000);

        // Adding past the end of the day wraps around.
        time.add_time(DAY_LENGTH_TICKS);
        assert_eq!(time.time_of_day(), 14_000);

        // Subtracting below zero wraps to the top of the day.
        time.set_time_of_day(TIME_DAY);
        time.add_time(-2_000);
        assert_eq!(time.time_of_day(), DAY_LENGTH_TICKS - 1_000);
    }

    #[test]
    fn add_time_handles_extreme_arguments_without_overflow() {
        let mut time = WorldTime::new();
        time.set_time_of_day(TIME_NOON);
        // Must not panic on overflow; the addend is wrapped before the sum.
        time.add_time(i64::MAX);
        assert!((0..DAY_LENGTH_TICKS).contains(&time.time_of_day()));
        time.add_time(i64::MIN);
        assert!((0..DAY_LENGTH_TICKS).contains(&time.time_of_day()));
    }
}
