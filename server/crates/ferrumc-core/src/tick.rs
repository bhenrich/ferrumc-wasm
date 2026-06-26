//! The server tick counter.

use core::fmt;

/// A monotonically increasing server tick counter.
///
/// The simulation advances at a fixed rate (20 ticks per second is the target),
/// and `Tick` counts how many ticks have elapsed since startup. It is a `u64`,
/// so at 20 TPS it will not overflow for roughly 29 billion years — but advance
/// helpers are still overflow-aware so the counter can never wrap silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Tick(u64);

impl Tick {
    /// The first tick of the server (tick zero).
    pub const ZERO: Self = Self(0);

    /// Wraps a raw tick count.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw tick count.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next tick, or `None` if advancing would overflow `u64`.
    ///
    /// This is the checked single-step advance: it never wraps silently.
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Adds `ticks`, returning `None` if the result would overflow `u64`.
    pub const fn checked_add(self, ticks: u64) -> Option<Self> {
        match self.0.checked_add(ticks) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Adds `ticks`, saturating at [`u64::MAX`] instead of overflowing.
    #[must_use]
    pub const fn saturating_add(self, ticks: u64) -> Self {
        Self(self.0.saturating_add(ticks))
    }
}

impl fmt::Display for Tick {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_zero_agree() {
        assert_eq!(Tick::default(), Tick::ZERO);
        assert_eq!(Tick::ZERO.get(), 0);
    }

    #[test]
    fn next_advances_by_one() {
        let t = Tick::new(41);
        assert_eq!(t.next(), Some(Tick::new(42)));
    }

    #[test]
    fn next_is_checked_at_max() {
        assert_eq!(Tick::new(u64::MAX).next(), None);
    }

    #[test]
    fn checked_add_detects_overflow() {
        assert_eq!(Tick::new(10).checked_add(5), Some(Tick::new(15)));
        assert_eq!(Tick::new(u64::MAX).checked_add(1), None);
        assert_eq!(Tick::new(u64::MAX - 2).checked_add(5), None);
    }

    #[test]
    fn saturating_add_clamps_at_max() {
        assert_eq!(Tick::new(10).saturating_add(5), Tick::new(15));
        assert_eq!(Tick::new(u64::MAX).saturating_add(1), Tick::new(u64::MAX));
        assert_eq!(
            Tick::new(u64::MAX - 2).saturating_add(100),
            Tick::new(u64::MAX)
        );
    }

    #[test]
    fn display_renders_count() {
        assert_eq!(Tick::new(1234).to_string(), "1234");
    }
}
