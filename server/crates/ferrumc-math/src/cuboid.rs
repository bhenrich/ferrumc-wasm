//! An inclusive, axis-aligned cuboid of block positions.

use crate::BlockPos;

/// An inclusive axis-aligned cuboid region of blocks, defined by two opposite
/// corners.
///
/// Construction with [`Cuboid::new`] normalizes the corners, so [`Cuboid::min`]
/// always holds the component-wise minimum and [`Cuboid::max`] the
/// component-wise maximum regardless of the order the corners were supplied.
/// Both bounds are *inclusive*: a cuboid whose corners coincide contains exactly
/// one block.
///
/// This is the geometry behind the region build commands (`/fill`, `/replace`,
/// `/undo`). It is a pure value type — it owns no world state and performs no
/// mutation; callers iterate it ([`Cuboid::iter`]) and route each position
/// through the simulation's block funnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cuboid {
    min: BlockPos,
    max: BlockPos,
}

impl Cuboid {
    /// Builds the smallest cuboid containing both `a` and `b` (inclusive on every
    /// axis). The corners may be given in any order; the result is normalized so
    /// [`min`](Self::min) is the component-wise minimum.
    #[must_use]
    pub fn new(a: BlockPos, b: BlockPos) -> Self {
        let min = BlockPos::new(a.x().min(b.x()), a.y().min(b.y()), a.z().min(b.z()));
        let max = BlockPos::new(a.x().max(b.x()), a.y().max(b.y()), a.z().max(b.z()));
        Self { min, max }
    }

    /// Returns the component-wise minimum corner (inclusive).
    #[must_use]
    pub const fn min(self) -> BlockPos {
        self.min
    }

    /// Returns the component-wise maximum corner (inclusive).
    #[must_use]
    pub const fn max(self) -> BlockPos {
        self.max
    }

    /// Returns the number of blocks the cuboid contains (inclusive on every
    /// axis).
    ///
    /// The count saturates at [`u64::MAX`] for cuboids too large for a `u64`
    /// (only reachable with extreme coordinates spanning most of the `i32`
    /// range). A volume cap that rejects oversized regions therefore still
    /// rejects such a cuboid — the saturated value is far above any sane cap.
    #[must_use]
    pub fn volume(self) -> u64 {
        // Each span is `max - min + 1`. `max >= min` per construction and the
        // widest possible `i32` span (`i32::MAX - i32::MIN`) plus one is `2^32`,
        // which fits in `i64`/`u64`; the *product* of three spans can exceed
        // `u64`, so multiply with overflow checks and saturate.
        let span = |lo: i32, hi: i32| -> u64 { (i64::from(hi) - i64::from(lo) + 1) as u64 };
        let dx = span(self.min.x(), self.max.x());
        let dy = span(self.min.y(), self.max.y());
        let dz = span(self.min.z(), self.max.z());
        dx.checked_mul(dy)
            .and_then(|xy| xy.checked_mul(dz))
            .unwrap_or(u64::MAX)
    }

    /// Returns `true` if `pos` lies within the cuboid (inclusive bounds on every
    /// axis).
    #[must_use]
    pub fn contains(self, pos: BlockPos) -> bool {
        (self.min.x()..=self.max.x()).contains(&pos.x())
            && (self.min.y()..=self.max.y()).contains(&pos.y())
            && (self.min.z()..=self.max.z()).contains(&pos.z())
    }

    /// Iterates every block position in the cuboid in a deterministic order:
    /// ascending `y`, then `z`, then `x` (so a horizontal layer is visited before
    /// the one above it).
    ///
    /// The iterator yields exactly [`volume`](Self::volume) positions (when that
    /// fits in memory — a capped region is small). The order is stable, which the
    /// undo path relies on to pair captured prior states back to their positions.
    pub fn iter(self) -> impl Iterator<Item = BlockPos> {
        let (min, max) = (self.min, self.max);
        (min.y()..=max.y()).flat_map(move |y| {
            (min.z()..=max.z())
                .flat_map(move |z| (min.x()..=max.x()).map(move |x| BlockPos::new(x, y, z)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_normalizes_swapped_corners() {
        let a = BlockPos::new(5, 10, 15);
        let b = BlockPos::new(1, 2, 3);
        let from_ab = Cuboid::new(a, b);
        let from_ba = Cuboid::new(b, a);
        assert_eq!(from_ab, from_ba, "corner order must not matter");
        assert_eq!(from_ab.min(), BlockPos::new(1, 2, 3));
        assert_eq!(from_ab.max(), BlockPos::new(5, 10, 15));
    }

    #[test]
    fn single_block_cuboid_has_volume_one() {
        let p = BlockPos::new(-4, 70, 9);
        let cuboid = Cuboid::new(p, p);
        assert_eq!(cuboid.volume(), 1);
        assert_eq!(cuboid.iter().collect::<Vec<_>>(), vec![p]);
    }

    #[test]
    fn volume_counts_inclusive_spans() {
        // 2 x 3 x 4 = 24 blocks.
        let cuboid = Cuboid::new(BlockPos::new(0, 0, 0), BlockPos::new(1, 2, 3));
        assert_eq!(cuboid.volume(), 24);
        assert_eq!(cuboid.iter().count() as u64, cuboid.volume());
    }

    #[test]
    fn volume_handles_negative_coordinates() {
        // x: -2..=2 = 5, y: -1..=1 = 3, z: 0..=0 = 1 -> 15.
        let cuboid = Cuboid::new(BlockPos::new(-2, -1, 0), BlockPos::new(2, 1, 0));
        assert_eq!(cuboid.volume(), 15);
        assert_eq!(cuboid.iter().count() as u64, 15);
    }

    #[test]
    fn volume_saturates_for_an_enormous_cuboid() {
        // Spanning nearly the whole i32 range on every axis overflows u64; the
        // volume saturates rather than wrapping, so a cap check still rejects it.
        let cuboid = Cuboid::new(
            BlockPos::new(i32::MIN, i32::MIN, i32::MIN),
            BlockPos::new(i32::MAX, i32::MAX, i32::MAX),
        );
        assert_eq!(cuboid.volume(), u64::MAX);
    }

    #[test]
    fn contains_is_inclusive_on_bounds() {
        let cuboid = Cuboid::new(BlockPos::new(0, 0, 0), BlockPos::new(2, 2, 2));
        // Both corners and an interior block are inside.
        assert!(cuboid.contains(BlockPos::new(0, 0, 0)));
        assert!(cuboid.contains(BlockPos::new(2, 2, 2)));
        assert!(cuboid.contains(BlockPos::new(1, 1, 1)));
        // One step past any face is outside.
        assert!(!cuboid.contains(BlockPos::new(-1, 0, 0)));
        assert!(!cuboid.contains(BlockPos::new(0, 3, 0)));
        assert!(!cuboid.contains(BlockPos::new(0, 0, 3)));
    }

    #[test]
    fn iter_visits_every_block_exactly_once_in_order() {
        let cuboid = Cuboid::new(BlockPos::new(0, 0, 0), BlockPos::new(1, 1, 1));
        let visited: Vec<BlockPos> = cuboid.iter().collect();
        // 2x2x2 = 8 distinct blocks, ascending y then z then x.
        assert_eq!(
            visited,
            vec![
                BlockPos::new(0, 0, 0),
                BlockPos::new(1, 0, 0),
                BlockPos::new(0, 0, 1),
                BlockPos::new(1, 0, 1),
                BlockPos::new(0, 1, 0),
                BlockPos::new(1, 1, 0),
                BlockPos::new(0, 1, 1),
                BlockPos::new(1, 1, 1),
            ]
        );
        // Every visited block is contained, and there are no duplicates.
        assert!(visited.iter().all(|&p| cuboid.contains(p)));
        let mut sorted = visited.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), visited.len(), "no block is visited twice");
    }
}
