//! Chunk-column coordinates and conversions to shards, regions, and blocks.

use crate::{BlockPos, RegionPos, ShardPos};

/// Bit shift mapping a chunk axis to its shard axis.
///
/// A simulation shard is 8 chunks wide (`8 == 1 << 3`), so flooring a chunk
/// coordinate to its shard is the arithmetic shift `>> 3`.
const SHARD_SHIFT: i32 = 3;

/// Bit shift mapping a chunk axis to its Anvil-region axis.
///
/// An Anvil region file holds a 32x32 grid of chunks (`32 == 1 << 5`), so
/// flooring a chunk coordinate to its region is the arithmetic shift `>> 5`.
const REGION_SHIFT: i32 = 5;

/// Bit shift mapping a chunk axis back to the block coordinate of its corner.
///
/// A chunk is 16 blocks wide (`16 == 1 << 4`), so the chunk's minimum-corner
/// block coordinate is `chunk << 4`.
const BLOCK_SHIFT: i32 = 4;

/// The position of a chunk: a full-height 16x16 column of blocks.
///
/// Chunks have no `y` axis; a chunk is the entire vertical column at its
/// `(x, z)`. Convert from a [`BlockPos`] with [`BlockPos::to_chunk_pos`], and
/// coarsen to the simulation shard or Anvil region with
/// [`ChunkPos::to_shard_pos`] / [`ChunkPos::to_region_pos`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkPos {
    x: i32,
    z: i32,
}

impl ChunkPos {
    /// The chunk at the world origin, `(0, 0)`.
    pub const ORIGIN: Self = Self::new(0, 0);

    /// Creates a chunk position from its `x` and `z` coordinates.
    pub const fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }

    /// Returns the chunk x coordinate.
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Returns the chunk z coordinate.
    pub const fn z(self) -> i32 {
        self.z
    }

    /// Returns the [`ShardPos`] of the simulation shard that owns this chunk.
    ///
    /// Uses arithmetic shift (floor division by 8), so chunk `x = -1` is in
    /// shard `x = -1`.
    pub const fn to_shard_pos(self) -> ShardPos {
        ShardPos::new(self.x >> SHARD_SHIFT, self.z >> SHARD_SHIFT)
    }

    /// Returns the [`RegionPos`] of the Anvil region file that stores this
    /// chunk.
    ///
    /// Uses arithmetic shift (floor division by 32), so chunk `x = -1` is in
    /// region `x = -1`.
    pub const fn to_region_pos(self) -> RegionPos {
        RegionPos::new(self.x >> REGION_SHIFT, self.z >> REGION_SHIFT)
    }

    /// Returns the block at this chunk's minimum (`-x`, `-z`) corner at world
    /// height `y`.
    ///
    /// This is the natural reverse of [`BlockPos::to_chunk_pos`]: the chunk
    /// origin is `(x << 4, y, z << 4)`.
    pub const fn origin_block(self, y: i32) -> BlockPos {
        BlockPos::new(self.x << BLOCK_SHIFT, y, self.z << BLOCK_SHIFT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_round_trip() {
        let c = ChunkPos::new(-4, 9);
        assert_eq!((c.x(), c.z()), (-4, 9));
        assert_eq!(ChunkPos::ORIGIN, ChunkPos::new(0, 0));
    }

    #[test]
    fn chunk_to_shard_floors_negative() {
        // Same boundary shape as block->chunk, but with the 8-chunk (>>3) edge.
        assert_eq!(ChunkPos::new(-1, 0).to_shard_pos(), ShardPos::new(-1, 0));
        assert_eq!(ChunkPos::new(-8, 0).to_shard_pos(), ShardPos::new(-1, 0));
        assert_eq!(ChunkPos::new(-9, 0).to_shard_pos(), ShardPos::new(-2, 0));
        assert_eq!(ChunkPos::new(0, -1).to_shard_pos(), ShardPos::new(0, -1));
        assert_eq!(ChunkPos::new(0, -9).to_shard_pos(), ShardPos::new(0, -2));
    }

    #[test]
    fn chunk_to_shard_positive_boundaries() {
        assert_eq!(ChunkPos::new(0, 7).to_shard_pos(), ShardPos::new(0, 0));
        assert_eq!(ChunkPos::new(8, 8).to_shard_pos(), ShardPos::new(1, 1));
    }

    #[test]
    fn chunk_to_region_floors_negative() {
        // Same boundary shape, with the 32-chunk (>>5) Anvil region edge.
        assert_eq!(ChunkPos::new(-1, 0).to_region_pos(), RegionPos::new(-1, 0));
        assert_eq!(ChunkPos::new(-32, 0).to_region_pos(), RegionPos::new(-1, 0));
        assert_eq!(ChunkPos::new(-33, 0).to_region_pos(), RegionPos::new(-2, 0));
        assert_eq!(ChunkPos::new(0, -33).to_region_pos(), RegionPos::new(0, -2));
    }

    #[test]
    fn chunk_to_region_positive_boundaries() {
        assert_eq!(ChunkPos::new(31, 31).to_region_pos(), RegionPos::new(0, 0));
        assert_eq!(ChunkPos::new(32, 32).to_region_pos(), RegionPos::new(1, 1));
    }

    #[test]
    fn origin_block_is_reverse_of_to_chunk_pos() {
        let chunk = ChunkPos::new(-2, 3);
        let origin = chunk.origin_block(64);
        assert_eq!(origin, BlockPos::new(-32, 64, 48));
        // Round trip: the origin block resolves back to the same chunk.
        assert_eq!(origin.to_chunk_pos(), chunk);
    }
}
