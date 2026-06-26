//! Simulation-shard coordinates (an 8x8 grid of chunks).

use crate::ChunkPos;

/// Bit shift mapping a shard axis back to the chunk coordinate of its corner.
///
/// A simulation shard owns an 8x8 grid of chunks (`8 == 1 << 3`), so the
/// shard's minimum-corner chunk coordinate is `shard << 3`.
const CHUNK_SHIFT: i32 = 3;

/// The position of a simulation shard: an 8x8 grid of chunks exclusively owned
/// by one shard worker.
///
/// Sharding is the unit of parallelism in the simulation layer; a shard owns
/// its chunks and entities outright (no locks). Convert from a [`ChunkPos`]
/// with [`ChunkPos::to_shard_pos`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardPos {
    x: i32,
    z: i32,
}

impl ShardPos {
    /// Creates a shard position from its `x` and `z` coordinates.
    pub const fn new(x: i32, z: i32) -> Self {
        Self { x, z }
    }

    /// Returns the shard x coordinate.
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Returns the shard z coordinate.
    pub const fn z(self) -> i32 {
        self.z
    }

    /// Returns the chunk at this shard's minimum (`-x`, `-z`) corner.
    ///
    /// This is the natural reverse of [`ChunkPos::to_shard_pos`]: the shard
    /// origin is `(x << 3, z << 3)` in chunk coordinates.
    pub const fn origin_chunk(self) -> ChunkPos {
        ChunkPos::new(self.x << CHUNK_SHIFT, self.z << CHUNK_SHIFT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_round_trip() {
        let s = ShardPos::new(7, -3);
        assert_eq!((s.x(), s.z()), (7, -3));
    }

    #[test]
    fn origin_chunk_is_reverse_of_to_shard_pos() {
        let shard = ShardPos::new(-1, 2);
        let origin = shard.origin_chunk();
        assert_eq!(origin, ChunkPos::new(-8, 16));
        assert_eq!(origin.to_shard_pos(), shard);
    }
}
