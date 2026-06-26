//! Block coordinates local to a single 16x16x16 section.

use crate::BlockPos;

/// Edge length of a section in blocks. Local coordinates are in `0..16`.
const SECTION_SIZE: u8 = 16;

/// Low bits of an absolute block coordinate that index its position within a
/// section. `0xF == 16 - 1`.
const LOCAL_MASK: i32 = 0xF;

/// A block position local to a single 16x16x16 section.
///
/// Each axis is in `0..16`, so the position fits a `u8`. This is the in-section
/// offset of a block, produced by masking an absolute [`BlockPos`]; it is the
/// natural key for indexing a section's flat block array via
/// [`LocalBlockPos::index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalBlockPos {
    x: u8,
    y: u8,
    z: u8,
}

impl LocalBlockPos {
    /// Creates a local position, returning `None` if any axis is not in
    /// `0..16`.
    pub const fn new(x: u8, y: u8, z: u8) -> Option<Self> {
        if x < SECTION_SIZE && y < SECTION_SIZE && z < SECTION_SIZE {
            Some(Self { x, y, z })
        } else {
            None
        }
    }

    /// Derives the in-section local position of an absolute [`BlockPos`].
    ///
    /// Each axis is masked with `& 15`, which wraps negatives the way floor
    /// division demands: block `x = -1` has local `x = 15`. The result is
    /// always valid, so this conversion is infallible.
    pub const fn from_block(block: BlockPos) -> Self {
        Self {
            x: (block.x() & LOCAL_MASK) as u8,
            y: (block.y() & LOCAL_MASK) as u8,
            z: (block.z() & LOCAL_MASK) as u8,
        }
    }

    /// Returns the local x coordinate (`0..16`).
    pub const fn x(self) -> u8 {
        self.x
    }

    /// Returns the local y coordinate (`0..16`).
    pub const fn y(self) -> u8 {
        self.y
    }

    /// Returns the local z coordinate (`0..16`).
    pub const fn z(self) -> u8 {
        self.z
    }

    /// Returns the flat index of this position into a section's 4096-element
    /// block array.
    ///
    /// Uses Minecraft's `YZX` ordering (`y` most significant): the index is
    /// `(y << 8) | (z << 4) | x`, which is in `0..4096`.
    // `as usize` rather than `usize::from`: the `From` trait is not yet usable in
    // const fns (rust-lang/rust#143874), and every axis is a `u8`, so the widening
    // cast is provably lossless.
    pub const fn index(self) -> usize {
        ((self.y as usize) << 8) | ((self.z as usize) << 4) | self.x as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_out_of_range() {
        assert!(LocalBlockPos::new(0, 0, 0).is_some());
        assert!(LocalBlockPos::new(15, 15, 15).is_some());
        assert!(LocalBlockPos::new(16, 0, 0).is_none());
        assert!(LocalBlockPos::new(0, 16, 0).is_none());
        assert!(LocalBlockPos::new(0, 0, 255).is_none());
    }

    #[test]
    fn from_block_masks_to_section() {
        // -1 & 15 == 15 is the headline negative-wrap case.
        let p = LocalBlockPos::from_block(BlockPos::new(-1, -1, -1));
        assert_eq!((p.x(), p.y(), p.z()), (15, 15, 15));
        let q = LocalBlockPos::from_block(BlockPos::new(16, 32, 48));
        assert_eq!((q.x(), q.y(), q.z()), (0, 0, 0));
        let r = LocalBlockPos::from_block(BlockPos::new(7, 8, 9));
        assert_eq!((r.x(), r.y(), r.z()), (7, 8, 9));
    }

    #[test]
    fn index_uses_yzx_ordering() {
        assert_eq!(
            LocalBlockPos::new(0, 0, 0).map(LocalBlockPos::index),
            Some(0)
        );
        assert_eq!(
            LocalBlockPos::new(1, 0, 0).map(LocalBlockPos::index),
            Some(1)
        );
        assert_eq!(
            LocalBlockPos::new(0, 0, 1).map(LocalBlockPos::index),
            Some(16)
        );
        assert_eq!(
            LocalBlockPos::new(0, 1, 0).map(LocalBlockPos::index),
            Some(256)
        );
        assert_eq!(
            LocalBlockPos::new(15, 15, 15).map(LocalBlockPos::index),
            Some(4095)
        );
    }
}
