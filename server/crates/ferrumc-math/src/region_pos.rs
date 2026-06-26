//! Anvil region-file coordinates (a 32x32 grid of chunks).

use crate::ChunkPos;

/// Bit shift mapping a region axis back to the chunk coordinate of its corner.
///
/// An Anvil region holds a 32x32 grid of chunks (`32 == 1 << 5`), so the
/// region's minimum-corner chunk coordinate is `region << 5`.
const CHUNK_SHIFT: i32 = 5;

/// The position of an Anvil region file: a 32x32 grid of chunks.
///
/// Regions are the on-disk grouping used by the vanilla Anvil format. Convert
/// from a [`ChunkPos`] with [`ChunkPos::to_region_pos`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegionPos {
    x: i32,
    z: i32,
}

impl RegionPos {
    /// Creates a region position from its `x` and `z` coordinates.
    pub const fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }

    /// Returns the region x coordinate.
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Returns the region z coordinate.
    pub const fn z(self) -> i32 {
        self.z
    }

    /// Returns the chunk at this region's minimum (`-x`, `-z`) corner.
    ///
    /// This is the natural reverse of [`ChunkPos::to_region_pos`]: the region
    /// origin is `(x << 5, z << 5)` in chunk coordinates.
    pub const fn origin_chunk(self) -> ChunkPos {
        ChunkPos::new(self.x << CHUNK_SHIFT, self.z << CHUNK_SHIFT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_round_trip() {
        let r = RegionPos::new(-2, 5);
        assert_eq!((r.x(), r.z()), (-2, 5));
    }

    #[test]
    fn origin_chunk_is_reverse_of_to_region_pos() {
        let region = RegionPos::new(-1, 1);
        let origin = region.origin_chunk();
        assert_eq!(origin, ChunkPos::new(-32, 32));
        assert_eq!(origin.to_region_pos(), region);
    }
}
