//! Vanilla-style operator levels.

use crate::error::InvalidOperatorLevel;

/// A vanilla-style operator level in the range `0..=4`.
///
/// The levels mirror Minecraft's permission tiers (0 = no elevated access,
/// 4 = full operator). This type is a plain validated value: it records a
/// subject's tier but does *not* itself grant any [`PermissionNode`] — node
/// resolution is handled separately by
/// [`PermissionSet`](crate::PermissionSet). Callers decide how (or whether) a
/// level translates into node access.
///
/// [`PermissionNode`]: crate::PermissionNode
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct OperatorLevel(u8);

impl OperatorLevel {
    /// The highest valid operator level.
    pub const MAX: u8 = 4;

    /// Level 0: no elevated permissions (the [`Default`]).
    pub const NONE: Self = Self(0);

    /// Level 4: full operator access.
    pub const OWNER: Self = Self(Self::MAX);

    /// Creates a level from `level`, or `None` if it exceeds [`OperatorLevel::MAX`].
    pub const fn new(level: u8) -> Option<Self> {
        if level <= Self::MAX {
            Some(Self(level))
        } else {
            None
        }
    }

    /// Returns the numeric level (`0..=4`).
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Returns whether this level is at least `other`.
    pub const fn is_at_least(self, other: Self) -> bool {
        self.0 >= other.0
    }
}

impl TryFrom<u8> for OperatorLevel {
    type Error = InvalidOperatorLevel;

    fn try_from(level: u8) -> Result<Self, Self::Error> {
        Self::new(level).ok_or(InvalidOperatorLevel(level))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_levels() {
        for level in 0..=OperatorLevel::MAX {
            let op = OperatorLevel::new(level).expect("0..=4 is valid");
            assert_eq!(op.get(), level);
            assert_eq!(OperatorLevel::try_from(level), Ok(op));
        }
    }

    #[test]
    fn rejects_out_of_range() {
        assert_eq!(OperatorLevel::new(5), None);
        let err = OperatorLevel::try_from(200).expect_err("200 is out of range");
        assert_eq!(err.value(), 200);
    }

    #[test]
    fn default_and_constants() {
        assert_eq!(OperatorLevel::default(), OperatorLevel::NONE);
        assert_eq!(OperatorLevel::NONE.get(), 0);
        assert_eq!(OperatorLevel::OWNER.get(), 4);
    }

    #[test]
    fn ordering_and_is_at_least() {
        assert!(OperatorLevel::OWNER > OperatorLevel::NONE);
        assert!(OperatorLevel::OWNER.is_at_least(OperatorLevel::NONE));
        assert!(!OperatorLevel::NONE.is_at_least(OperatorLevel::OWNER));
        let two = OperatorLevel::new(2).expect("valid");
        assert!(two.is_at_least(two));
    }
}
