//! The six axis-aligned block faces and their unit offsets.

use crate::BlockPos;

/// One of the six axis-aligned block faces.
///
/// The variants are declared in Minecraft's canonical face order
/// (`Down`, `Up`, `North`, `South`, `West`, `East`), which is the order the
/// protocol uses to encode a face index. Each direction has a unit
/// [`Direction::offset`] in block space and an [`Direction::opposite`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Direction {
    /// Negative Y (downward).
    Down,
    /// Positive Y (upward).
    Up,
    /// Negative Z (north).
    North,
    /// Positive Z (south).
    South,
    /// Negative X (west).
    West,
    /// Positive X (east).
    East,
}

impl Direction {
    /// All six directions in Minecraft's canonical face order.
    pub const ALL: [Self; 6] = [
        Self::Down,
        Self::Up,
        Self::North,
        Self::South,
        Self::West,
        Self::East,
    ];

    /// Returns the unit block offset that moves one block in this direction.
    ///
    /// For example [`Direction::Up`] is `(0, 1, 0)` and [`Direction::North`] is
    /// `(0, 0, -1)`.
    pub const fn offset(self) -> BlockPos {
        match self {
            Self::Down => BlockPos::new(0, -1, 0),
            Self::Up => BlockPos::new(0, 1, 0),
            Self::North => BlockPos::new(0, 0, -1),
            Self::South => BlockPos::new(0, 0, 1),
            Self::West => BlockPos::new(-1, 0, 0),
            Self::East => BlockPos::new(1, 0, 0),
        }
    }

    /// Returns the direction facing the opposite way along the same axis.
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Down => Self::Up,
            Self::Up => Self::Down,
            Self::North => Self::South,
            Self::South => Self::North,
            Self::West => Self::East,
            Self::East => Self::West,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_are_in_minecraft_order() {
        assert_eq!(
            Direction::ALL,
            [
                Direction::Down,
                Direction::Up,
                Direction::North,
                Direction::South,
                Direction::West,
                Direction::East,
            ]
        );
    }

    #[test]
    fn offsets_are_correct_unit_vectors() {
        assert_eq!(Direction::Down.offset(), BlockPos::new(0, -1, 0));
        assert_eq!(Direction::Up.offset(), BlockPos::new(0, 1, 0));
        assert_eq!(Direction::North.offset(), BlockPos::new(0, 0, -1));
        assert_eq!(Direction::South.offset(), BlockPos::new(0, 0, 1));
        assert_eq!(Direction::West.offset(), BlockPos::new(-1, 0, 0));
        assert_eq!(Direction::East.offset(), BlockPos::new(1, 0, 0));
    }

    #[test]
    fn opposite_pairs_all_six() {
        assert_eq!(Direction::Down.opposite(), Direction::Up);
        assert_eq!(Direction::Up.opposite(), Direction::Down);
        assert_eq!(Direction::North.opposite(), Direction::South);
        assert_eq!(Direction::South.opposite(), Direction::North);
        assert_eq!(Direction::West.opposite(), Direction::East);
        assert_eq!(Direction::East.opposite(), Direction::West);
    }

    #[test]
    fn opposite_is_an_involution_and_negates_offset() {
        for dir in Direction::ALL {
            assert_eq!(dir.opposite().opposite(), dir);
            let o = dir.offset();
            let back = dir.opposite().offset();
            assert_eq!(
                (o.x() + back.x(), o.y() + back.y(), o.z() + back.z()),
                (0, 0, 0)
            );
        }
    }
}
