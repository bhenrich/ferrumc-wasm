//! Absolute block coordinates and their conversions to coarser spaces.

use crate::{ChunkPos, Direction, LocalBlockPos, SectionPos};

/// Bit shift mapping a block axis to its chunk/section axis.
///
/// A chunk and a section are both 16 blocks wide on the relevant axis, and
/// `16 == 1 << 4`, so a floor division by 16 is the arithmetic shift `>> 4`.
const CHUNK_SHIFT: i32 = 4;

/// An absolute block position in the world, in block units.
///
/// This is the finest-grained integer coordinate: one unit is one block. The
/// `y` axis spans the full world height. Coarser coordinate spaces are reached
/// by flooring conversions ([`BlockPos::to_chunk_pos`],
/// [`BlockPos::to_section_pos`]); the position *within* the enclosing section
/// is given by [`BlockPos::to_local`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockPos {
    x: i32,
    y: i32,
    z: i32,
}

impl BlockPos {
    /// The block at the world origin, `(0, 0, 0)`.
    pub const ORIGIN: Self = Self::new(0, 0, 0);

    /// Creates a block position from its three axis coordinates.
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// Returns the x (east/west) coordinate.
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Returns the y (vertical) coordinate.
    pub const fn y(self) -> i32 {
        self.y
    }

    /// Returns the z (north/south) coordinate.
    pub const fn z(self) -> i32 {
        self.z
    }

    /// Returns the [`ChunkPos`] of the chunk that contains this block.
    ///
    /// Uses arithmetic shift (floor division by 16), so negative blocks map to
    /// the correct chunk: block `x = -1` is in chunk `x = -1`.
    pub const fn to_chunk_pos(self) -> ChunkPos {
        ChunkPos::new(self.x >> CHUNK_SHIFT, self.z >> CHUNK_SHIFT)
    }

    /// Returns the [`SectionPos`] of the 16x16x16 section that contains this
    /// block.
    ///
    /// All three axes are floored by 16, including the vertical one, so block
    /// `y = -1` is in section `y = -1`.
    pub const fn to_section_pos(self) -> SectionPos {
        SectionPos::new(
            self.x >> CHUNK_SHIFT,
            self.y >> CHUNK_SHIFT,
            self.z >> CHUNK_SHIFT,
        )
    }

    /// Returns this block's [`LocalBlockPos`] within its enclosing section
    /// (each axis in `0..16`).
    ///
    /// The masking wraps negatives the way floor division demands: block
    /// `x = -1` has local `x = 15`.
    pub const fn to_local(self) -> LocalBlockPos {
        LocalBlockPos::from_block(self)
    }

    /// Returns the block one step away in `direction`.
    #[must_use]
    pub const fn offset(self, direction: Direction) -> Self {
        let delta = direction.offset();
        Self::new(self.x + delta.x, self.y + delta.y, self.z + delta.z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_round_trip() {
        let p = BlockPos::new(3, -7, 11);
        assert_eq!((p.x(), p.y(), p.z()), (3, -7, 11));
        assert_eq!(BlockPos::ORIGIN, BlockPos::new(0, 0, 0));
    }

    #[test]
    fn block_to_chunk_floors_negative_x() {
        // The exact boundary cases called out in the milestone.
        assert_eq!(BlockPos::new(-1, 0, 0).to_chunk_pos(), ChunkPos::new(-1, 0));
        assert_eq!(
            BlockPos::new(-16, 0, 0).to_chunk_pos(),
            ChunkPos::new(-1, 0)
        );
        assert_eq!(
            BlockPos::new(-17, 0, 0).to_chunk_pos(),
            ChunkPos::new(-2, 0)
        );
    }

    #[test]
    fn block_to_chunk_positive_and_boundaries() {
        assert_eq!(BlockPos::new(0, 0, 0).to_chunk_pos(), ChunkPos::new(0, 0));
        assert_eq!(BlockPos::new(15, 0, 15).to_chunk_pos(), ChunkPos::new(0, 0));
        assert_eq!(BlockPos::new(16, 0, 16).to_chunk_pos(), ChunkPos::new(1, 1));
        assert_eq!(BlockPos::new(31, 0, 31).to_chunk_pos(), ChunkPos::new(1, 1));
    }

    #[test]
    fn block_to_chunk_floors_negative_z() {
        assert_eq!(BlockPos::new(0, 0, -1).to_chunk_pos(), ChunkPos::new(0, -1));
        assert_eq!(
            BlockPos::new(0, 0, -16).to_chunk_pos(),
            ChunkPos::new(0, -1)
        );
        assert_eq!(
            BlockPos::new(0, 0, -17).to_chunk_pos(),
            ChunkPos::new(0, -2)
        );
    }

    #[test]
    fn block_to_section_floors_negative_y() {
        assert_eq!(BlockPos::new(0, -1, 0).to_section_pos().y(), -1);
        assert_eq!(BlockPos::new(0, -16, 0).to_section_pos().y(), -1);
        assert_eq!(BlockPos::new(0, -17, 0).to_section_pos().y(), -2);
        assert_eq!(BlockPos::new(0, 0, 0).to_section_pos().y(), 0);
        assert_eq!(BlockPos::new(0, 16, 0).to_section_pos().y(), 1);
        assert_eq!(
            BlockPos::new(-1, -1, -1).to_section_pos(),
            SectionPos::new(-1, -1, -1)
        );
    }

    #[test]
    fn block_to_local_wraps_negatives() {
        let p = BlockPos::new(-1, 5, -1).to_local();
        assert_eq!((p.x(), p.y(), p.z()), (15, 5, 15));
        let q = BlockPos::new(-16, -16, -16).to_local();
        assert_eq!((q.x(), q.y(), q.z()), (0, 0, 0));
        let r = BlockPos::new(17, 0, 33).to_local();
        assert_eq!((r.x(), r.y(), r.z()), (1, 0, 1));
    }

    #[test]
    fn offset_steps_in_each_direction() {
        let base = BlockPos::new(10, 20, 30);
        assert_eq!(base.offset(Direction::Up), BlockPos::new(10, 21, 30));
        assert_eq!(base.offset(Direction::Down), BlockPos::new(10, 19, 30));
        assert_eq!(base.offset(Direction::North), BlockPos::new(10, 20, 29));
        assert_eq!(base.offset(Direction::South), BlockPos::new(10, 20, 31));
        assert_eq!(base.offset(Direction::West), BlockPos::new(9, 20, 30));
        assert_eq!(base.offset(Direction::East), BlockPos::new(11, 20, 30));
    }
}
