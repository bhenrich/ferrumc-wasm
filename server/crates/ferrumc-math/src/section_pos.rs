//! Coordinates of a 16x16x16 cubic chunk section.

use crate::{BlockPos, ChunkPos};

/// Bit shift mapping a section axis back to the block coordinate of its corner.
///
/// A section is 16 blocks wide on every axis (`16 == 1 << 4`), so the section's
/// minimum-corner block coordinate is `section << 4`.
const BLOCK_SHIFT: i32 = 4;

/// The position of a 16x16x16 cubic section of blocks.
///
/// Unlike a [`ChunkPos`], a section has a `y` axis: it is one vertical slice of
/// a chunk column. Convert from a [`BlockPos`] with
/// [`BlockPos::to_section_pos`]; drop the `y` axis to get the enclosing column
/// with [`SectionPos::to_chunk_pos`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SectionPos {
    x: i32,
    y: i32,
    z: i32,
}

impl SectionPos {
    /// Creates a section position from its three axis coordinates.
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// Returns the section x coordinate.
    pub const fn x(self) -> i32 {
        self.x
    }

    /// Returns the section y coordinate (the vertical slice index).
    pub const fn y(self) -> i32 {
        self.y
    }

    /// Returns the section z coordinate.
    pub const fn z(self) -> i32 {
        self.z
    }

    /// Returns the [`ChunkPos`] of the column this section belongs to (its
    /// `(x, z)`, ignoring `y`).
    pub const fn to_chunk_pos(self) -> ChunkPos {
        ChunkPos::new(self.x, self.z)
    }

    /// Returns the block at this section's minimum (`-x`, `-y`, `-z`) corner.
    ///
    /// This is the natural reverse of [`BlockPos::to_section_pos`]: the section
    /// origin is `(x << 4, y << 4, z << 4)`.
    pub const fn origin_block(self) -> BlockPos {
        BlockPos::new(
            self.x << BLOCK_SHIFT,
            self.y << BLOCK_SHIFT,
            self.z << BLOCK_SHIFT,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_round_trip() {
        let s = SectionPos::new(-3, -1, 4);
        assert_eq!((s.x(), s.y(), s.z()), (-3, -1, 4));
    }

    #[test]
    fn to_chunk_pos_drops_y() {
        assert_eq!(
            SectionPos::new(5, -2, 7).to_chunk_pos(),
            ChunkPos::new(5, 7)
        );
    }

    #[test]
    fn origin_block_is_reverse_of_to_section_pos() {
        let section = SectionPos::new(-1, -1, 2);
        let origin = section.origin_block();
        assert_eq!(origin, BlockPos::new(-16, -16, 32));
        assert_eq!(origin.to_section_pos(), section);
    }
}
