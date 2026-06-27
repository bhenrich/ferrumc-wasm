//! A 16x16x16 chunk section's block storage.

use ferrumc_math::LocalBlockPos;

use crate::block_state::BlockStateId;
use crate::paletted_container::PalettedContainer;

/// Number of blocks in a chunk section (`16 * 16 * 16`).
pub const SECTION_VOLUME: usize = 16 * 16 * 16;

/// The block storage of a single 16x16x16 chunk section.
///
/// Blocks are addressed by [`LocalBlockPos`], whose [`LocalBlockPos::index`] is
/// always in `0..SECTION_VOLUME`, so every access is in range by construction
/// and neither [`ChunkSection::get`] nor [`ChunkSection::set`] can fail or
/// panic. Storage is a [`PalettedContainer`] that promotes its representation as
/// the section gains distinct block states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSection {
    blocks: PalettedContainer<SECTION_VOLUME>,
}

impl ChunkSection {
    /// Creates an empty section with every block set to [`BlockStateId::AIR`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: PalettedContainer::new(),
        }
    }

    /// Creates a section with every block set to `state`.
    #[must_use]
    pub fn filled(state: BlockStateId) -> Self {
        Self {
            blocks: PalettedContainer::filled(state),
        }
    }

    /// Returns the block-state id at `pos`.
    #[must_use]
    pub fn get(&self, pos: LocalBlockPos) -> BlockStateId {
        // `pos.index()` is always in `0..SECTION_VOLUME`, so the lookup is in
        // range; the default is a panic-free fallback that is never reached.
        self.blocks.get(pos.index()).unwrap_or_default()
    }

    /// Sets the block-state id at `pos`.
    pub fn set(&mut self, pos: LocalBlockPos, state: BlockStateId) {
        // `pos.index()` is always in `0..SECTION_VOLUME == CAPACITY` and any
        // `BlockStateId` fits the storage, so `set` cannot return an error here.
        let _ = self.blocks.set(pos.index(), state);
    }

    /// Returns the number of blocks that are not [`BlockStateId::AIR`].
    #[must_use]
    pub const fn non_air_count(&self) -> usize {
        self.blocks.non_air_count()
    }

    /// Returns the section's block-state storage for network encoding.
    ///
    /// Used by [`crate::network`] to write the section's block-states
    /// `PalettedContainer` into the chunk-data blob.
    pub(crate) const fn block_states(&self) -> &PalettedContainer<SECTION_VOLUME> {
        &self.blocks
    }

    /// Returns `true` if the section contains no non-air blocks.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.blocks.non_air_count() == 0
    }
}

impl Default for ChunkSection {
    /// A default section is entirely [`BlockStateId::AIR`].
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{ChunkSection, SECTION_VOLUME};
    use crate::block_state::BlockStateId;
    use ferrumc_math::LocalBlockPos;

    fn pos(x: u8, y: u8, z: u8) -> LocalBlockPos {
        LocalBlockPos::new(x, y, z).expect("coordinates within 0..16")
    }

    #[test]
    fn section_volume_is_4096() {
        assert_eq!(SECTION_VOLUME, 4096);
    }

    #[test]
    fn new_section_is_empty_air() {
        let s = ChunkSection::new();
        assert!(s.is_empty());
        assert_eq!(s.non_air_count(), 0);
        assert_eq!(s.get(pos(0, 0, 0)), BlockStateId::AIR);
        assert_eq!(s.get(pos(15, 15, 15)), BlockStateId::AIR);
    }

    #[test]
    fn get_set_round_trips_by_local_pos() {
        let mut s = ChunkSection::new();
        let stone = BlockStateId::new(1);
        s.set(pos(1, 2, 3), stone);
        assert_eq!(s.get(pos(1, 2, 3)), stone);
        // A neighbour sharing two axes must be unaffected.
        assert_eq!(s.get(pos(2, 2, 3)), BlockStateId::AIR);
        assert_eq!(s.non_air_count(), 1);
        assert!(!s.is_empty());
    }

    #[test]
    fn distinct_corners_round_trip() {
        let mut s = ChunkSection::new();
        // Set every corner to a distinct state and read them all back.
        let corners = [
            (pos(0, 0, 0), BlockStateId::new(1)),
            (pos(15, 0, 0), BlockStateId::new(2)),
            (pos(0, 15, 0), BlockStateId::new(3)),
            (pos(0, 0, 15), BlockStateId::new(4)),
            (pos(15, 15, 15), BlockStateId::new(5)),
        ];
        for (p, state) in corners {
            s.set(p, state);
        }
        for (p, state) in corners {
            assert_eq!(s.get(p), state);
        }
        assert_eq!(s.non_air_count(), 5);
    }

    #[test]
    fn clearing_a_block_restores_air_count() {
        let mut s = ChunkSection::filled(BlockStateId::new(1));
        assert_eq!(s.non_air_count(), SECTION_VOLUME);
        assert!(!s.is_empty());
        s.set(pos(8, 8, 8), BlockStateId::AIR);
        assert_eq!(s.non_air_count(), SECTION_VOLUME - 1);
        assert_eq!(s.get(pos(8, 8, 8)), BlockStateId::AIR);
    }

    #[test]
    fn every_index_is_addressable() {
        // Touch all 4096 positions to confirm none are out of range or aliased.
        let mut s = ChunkSection::new();
        for y in 0..16u8 {
            for z in 0..16u8 {
                for x in 0..16u8 {
                    s.set(pos(x, y, z), BlockStateId::new(1));
                }
            }
        }
        assert_eq!(s.non_air_count(), SECTION_VOLUME);
    }
}
