//! A full-height chunk: a vertical stack of [`ChunkSection`]s.

use ferrumc_math::{BlockPos, ChunkPos, LocalBlockPos};
use ferrumc_registry::dimension;

use crate::block_state::BlockStateId;
use crate::chunk_section::ChunkSection;
use crate::dirty::DirtySections;
use crate::error::WorldError;
use crate::heightmap::{Heightmap, HeightmapKind};

/// Lowest buildable world `y`, inclusive (`dimension::MIN_Y`, `-64`).
const MIN_Y: i32 = dimension::MIN_Y;

/// Total stacked block layers in a column (`dimension::HEIGHT`, `384`).
const WORLD_HEIGHT: usize = dimension::HEIGHT as usize;

/// Edge length of a chunk section in blocks (16 on every axis).
pub(crate) const SECTION_EDGE: u8 = 16;

/// Number of 16-block sections stacked to span the overworld column.
///
/// `dimension::HEIGHT / 16 = 384 / 16 = 24`. The drift between this and the
/// registry geometry is guarded by a test.
pub const SECTION_COUNT: usize = WORLD_HEIGHT / 16;

/// Resolves a world `y` to its `(section index, local y)` within a chunk.
///
/// Returns `None` if `y` is outside the buildable range
/// (`MIN_Y ..= MIN_Y + HEIGHT - 1`). The section index is 0-based from the
/// bottom of the world; the local `y` is in `0..16`. This is the headline
/// negative-`y` mapping: `y = -64` is `(0, 0)` and `y = -48` is `(1, 0)`.
fn section_of(y: i32) -> Option<(usize, u8)> {
    // `checked_sub` rejects `y` so large the offset would overflow `i32`, and
    // `try_from` rejects `y < MIN_Y` (a negative offset), so both ends of the
    // range are handled without panicking.
    let offset = usize::try_from(y.checked_sub(MIN_Y)?).ok()?;
    if offset >= WORLD_HEIGHT {
        return None;
    }
    let edge = usize::from(SECTION_EDGE);
    let section_index = offset / edge;
    // `offset % 16 < 16`, so it always fits a `u8`.
    let local_y = u8::try_from(offset % edge).ok()?;
    Some((section_index, local_y))
}

/// Returns the world `y` of the bottom block of section `section_index`, or
/// `None` if the index is out of range.
pub(crate) fn section_base_y(section_index: usize) -> Option<i32> {
    if section_index >= SECTION_COUNT {
        return None;
    }
    // `section_index < 24`, so `* 16 < 384` fits an `i32` without overflow.
    let blocks_below = section_index.checked_mul(usize::from(SECTION_EDGE))?;
    let offset = i32::try_from(blocks_below).ok()?;
    Some(MIN_Y + offset)
}

/// Placeholder for a chunk's computed lighting (sky-light and block-light).
///
/// The lighting engine is out of scope for this milestone, so this type carries
/// no data yet and a chunk's [`Chunk::light`] is always `None`. It reserves a
/// stable type and field so lighting can be added later without reshaping the
/// chunk model. It cannot be constructed outside this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChunkLight {}

/// Placeholder for a chunk's block entities (chests, signs, spawners, ...).
///
/// Block-entity NBT modelling is a later milestone, so a chunk has none for now
/// and [`Chunk::block_entities`] always returns an empty slice. This type
/// reserves a stable element type for that collection and cannot be constructed
/// outside this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlockEntity {}

/// A full-height chunk column: [`SECTION_COUNT`] (24) stacked [`ChunkSection`]s
/// spanning the overworld from `MIN_Y` (`-64`) to `MIN_Y + HEIGHT - 1` (`319`).
///
/// Blocks are addressed by an absolute [`BlockPos`]: [`Chunk::get_block`] and
/// [`Chunk::set_block`] map the position to the owning section and a
/// [`LocalBlockPos`] within it. A position whose column is a different chunk, or
/// whose `y` is outside the buildable range, is rejected without panicking
/// (`None` from `get`, [`WorldError::BlockOutsideChunk`] from `set`).
///
/// A chunk tracks which sections have been modified (see
/// [`Chunk::dirty_sections`]) so the persistence and network layers can flush
/// only what changed. Lighting and block entities are present as documented
/// placeholders (see [`ChunkLight`] and [`BlockEntity`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The column this chunk occupies.
    pos: ChunkPos,
    /// Sections stacked bottom-to-top; `sections[0]` starts at `MIN_Y`.
    sections: [ChunkSection; SECTION_COUNT],
    /// Sections modified since the last [`Chunk::clear_dirty`].
    dirty: DirtySections,
    /// Sections changed by a *gameplay* mutation (not generation) since the last
    /// [`Chunk::clear_persist_dirty`]. This is the persistence signal, kept
    /// separate from [`Chunk::dirty`]: the flat generator marks `dirty` for every
    /// section it fills, so `dirty` cannot distinguish a generated baseline from a
    /// player edit. `persist_dirty` is marked **only** by the simulation layer
    /// (via [`Chunk::mark_persist_dirty`]) on an accepted block edit, never by
    /// [`Chunk::set_block`] or the generator, so a freshly generated and otherwise
    /// untouched chunk has an empty `persist_dirty` set and therefore produces no
    /// overlay record.
    persist_dirty: DirtySections,
    /// Placeholder lighting; always `None` until the lighting milestone.
    light: Option<ChunkLight>,
    /// Placeholder block entities; always empty until the block-entity
    /// milestone.
    block_entities: Vec<BlockEntity>,
}

impl Chunk {
    /// Creates an empty chunk at `pos`: every block is [`BlockStateId::AIR`], no
    /// section is dirty, and there is no light or block-entity data.
    #[must_use]
    pub fn new(pos: ChunkPos) -> Self {
        Self {
            pos,
            sections: std::array::from_fn(|_| ChunkSection::new()),
            dirty: DirtySections::new(),
            persist_dirty: DirtySections::new(),
            light: None,
            block_entities: Vec::new(),
        }
    }

    /// Returns the column this chunk occupies.
    #[must_use]
    pub const fn pos(&self) -> ChunkPos {
        self.pos
    }

    /// Returns the chunk's sections, stacked bottom-to-top (`[0]` at `MIN_Y`).
    #[must_use]
    pub fn sections(&self) -> &[ChunkSection] {
        &self.sections
    }

    /// Returns the section at `index` (0-based from the bottom), or `None` if
    /// `index >= SECTION_COUNT`.
    #[must_use]
    pub fn section(&self, index: usize) -> Option<&ChunkSection> {
        self.sections.get(index)
    }

    /// Returns the block-state id at `pos`, or `None` if `pos` is not in this
    /// chunk's column or its `y` is outside the buildable range.
    #[must_use]
    pub fn get_block(&self, pos: BlockPos) -> Option<BlockStateId> {
        let (section_index, local) = self.resolve(pos)?;
        let section = self.sections.get(section_index)?;
        Some(section.get(local))
    }

    /// Sets the block-state id at `pos`, marking the owning section dirty if the
    /// value actually changes.
    ///
    /// Returns [`WorldError::BlockOutsideChunk`] if `pos` is not in this chunk's
    /// column or its `y` is outside the buildable range.
    pub fn set_block(&mut self, pos: BlockPos, state: BlockStateId) -> Result<(), WorldError> {
        let (section_index, local) = self
            .resolve(pos)
            .ok_or(WorldError::BlockOutsideChunk { pos })?;
        self.set_local(section_index, local, state);
        Ok(())
    }

    /// Computes a surface heightmap of the requested [`HeightmapKind`].
    ///
    /// Each column's entry is the world `y` of its highest non-air block (or
    /// `None` for an all-air column). The result is a snapshot and does not track
    /// later edits.
    #[must_use]
    pub fn heightmap(&self, kind: HeightmapKind) -> Heightmap {
        Heightmap::compute(self, kind)
    }

    /// Returns the dirty-section set: the sections modified since construction or
    /// the last [`Chunk::clear_dirty`].
    #[must_use]
    pub const fn dirty_sections(&self) -> &DirtySections {
        &self.dirty
    }

    /// Returns `true` if section `index` has been modified since the last clear.
    #[must_use]
    pub fn is_section_dirty(&self, index: usize) -> bool {
        self.dirty.is_dirty(index)
    }

    /// Clears every dirty section, marking the whole chunk clean. Called after
    /// the chunk's changes have been persisted or sent.
    pub fn clear_dirty(&mut self) {
        self.dirty.clear();
    }

    /// Returns the persist-dirty section set: the sections changed by a gameplay
    /// mutation since construction or the last [`Chunk::clear_persist_dirty`].
    ///
    /// This is the persistence signal (see the `persist_dirty` field): unlike
    /// [`Chunk::dirty_sections`], it is empty for a freshly generated chunk, so
    /// only player-modified chunks ever produce an overlay record.
    #[must_use]
    pub const fn persist_dirty_sections(&self) -> &DirtySections {
        &self.persist_dirty
    }

    /// Returns `true` if section `index` was changed by a gameplay mutation since
    /// the last [`Chunk::clear_persist_dirty`].
    #[must_use]
    pub fn is_section_persist_dirty(&self, index: usize) -> bool {
        self.persist_dirty.is_dirty(index)
    }

    /// Marks the section owning `pos` persist-dirty: a gameplay mutation changed a
    /// block there and the chunk must be written to the overlay store.
    ///
    /// Called **only** by the simulation layer after an accepted block edit, never
    /// by [`Chunk::set_block`] or the generator, so the world model stays
    /// cause-agnostic and the generated baseline never marks itself for
    /// persistence. A `pos` outside this chunk's column or buildable range is a
    /// no-op (it resolves to no section), so the call can never panic.
    pub fn mark_persist_dirty(&mut self, pos: BlockPos) {
        if let Some((section_index, _)) = self.resolve(pos) {
            self.persist_dirty.mark(section_index);
        }
    }

    /// Clears every persist-dirty section. Called after the chunk's gameplay
    /// edits have been captured into an overlay record for the storage layer.
    pub fn clear_persist_dirty(&mut self) {
        self.persist_dirty.clear();
    }

    /// Returns the chunk's lighting, or `None` while lighting is unimplemented
    /// (see [`ChunkLight`]).
    #[must_use]
    pub const fn light(&self) -> Option<&ChunkLight> {
        self.light.as_ref()
    }

    /// Returns the chunk's block entities, currently always empty (see
    /// [`BlockEntity`]).
    #[must_use]
    pub fn block_entities(&self) -> &[BlockEntity] {
        &self.block_entities
    }

    /// Sets a block by pre-resolved `(section index, local position)`, marking
    /// the section dirty only when the value changes. Internal helper shared by
    /// [`Self::set_block`] and the flat-world generator; the caller guarantees
    /// the index and local position are in range.
    pub(crate) fn set_local(
        &mut self,
        section_index: usize,
        local: LocalBlockPos,
        state: BlockStateId,
    ) {
        let Some(section) = self.sections.get_mut(section_index) else {
            return;
        };
        if section.get(local) == state {
            return;
        }
        section.set(local, state);
        self.dirty.mark(section_index);
    }

    /// Resolves an absolute [`BlockPos`] to `(section index, local position)`
    /// within this chunk, or `None` if it is not in this chunk's column or its
    /// `y` is outside the buildable range.
    fn resolve(&self, pos: BlockPos) -> Option<(usize, LocalBlockPos)> {
        if pos.to_chunk_pos() != self.pos {
            return None;
        }
        let (section_index, local_y) = section_of(pos.y())?;
        let column = pos.to_local();
        let local = LocalBlockPos::new(column.x(), local_y, column.z())?;
        Some((section_index, local))
    }
}

#[cfg(test)]
mod tests {
    use super::{section_base_y, section_of, Chunk, MIN_Y, SECTION_COUNT, WORLD_HEIGHT};
    use crate::block_state::BlockStateId;
    use crate::heightmap::HeightmapKind;
    use ferrumc_math::{BlockPos, ChunkPos};
    use ferrumc_registry::dimension;

    /// Highest buildable world `y` (`319`).
    fn max_y() -> i32 {
        MIN_Y + i32::try_from(WORLD_HEIGHT).expect("world height fits i32") - 1
    }

    #[test]
    fn section_count_spans_the_world_height() {
        assert_eq!(SECTION_COUNT, 24);
        assert_eq!(
            SECTION_COUNT * 16,
            usize::try_from(dimension::HEIGHT).expect("height fits usize")
        );
        assert_eq!(MIN_Y, -64);
        assert_eq!(max_y(), 319);
    }

    #[test]
    fn section_of_maps_negative_y_correctly() {
        // Bottom of the world.
        assert_eq!(section_of(MIN_Y), Some((0, 0)));
        // Top of the first section, then the bottom of the second.
        assert_eq!(section_of(MIN_Y + 15), Some((0, 15)));
        assert_eq!(section_of(MIN_Y + 16), Some((1, 0)));
        // y = -48 is the first block of section 1.
        assert_eq!(section_of(-48), Some((1, 0)));
        // y = 0 sits in section 4 ((0 - (-64)) / 16 == 4).
        assert_eq!(section_of(0), Some((4, 0)));
        // Top of the world.
        assert_eq!(section_of(max_y()), Some((SECTION_COUNT - 1, 15)));
    }

    #[test]
    fn section_of_rejects_out_of_range_y() {
        assert_eq!(section_of(MIN_Y - 1), None);
        assert_eq!(section_of(max_y() + 1), None);
        assert_eq!(section_of(i32::MIN), None);
        assert_eq!(section_of(i32::MAX), None);
    }

    #[test]
    fn section_base_y_is_the_inverse_of_section_of() {
        for section in 0..SECTION_COUNT {
            let base = section_base_y(section).expect("section in range");
            assert_eq!(section_of(base), Some((section, 0)));
        }
        assert_eq!(section_base_y(0), Some(MIN_Y));
        assert_eq!(section_base_y(SECTION_COUNT), None);
    }

    #[test]
    fn new_chunk_is_all_air_and_clean() {
        let chunk = Chunk::new(ChunkPos::new(2, -3));
        assert_eq!(chunk.pos(), ChunkPos::new(2, -3));
        assert_eq!(chunk.sections().len(), SECTION_COUNT);
        assert!(!chunk.dirty_sections().any());
        assert!(chunk.light().is_none());
        assert!(chunk.block_entities().is_empty());
        // Representative reads across the height range are all air.
        let base = chunk.pos().origin_block(0);
        for y in [MIN_Y, MIN_Y + 100, 0, max_y()] {
            let pos = BlockPos::new(base.x(), y, base.z());
            assert_eq!(chunk.get_block(pos), Some(BlockStateId::AIR));
        }
    }

    #[test]
    fn set_block_round_trips_and_marks_section_dirty() {
        let mut chunk = Chunk::new(ChunkPos::ORIGIN);
        let stone = BlockStateId::new(1);
        let pos = BlockPos::new(3, 5, 9);
        // y = 5 lives in section ((5 - (-64)) / 16) == 4.
        assert!(chunk.set_block(pos, stone).is_ok());
        assert_eq!(chunk.get_block(pos), Some(stone));
        assert!(chunk.is_section_dirty(4));
        assert!(chunk.dirty_sections().any());
        assert_eq!(chunk.dirty_sections().count(), 1);
        // A neighbour column is untouched.
        assert_eq!(
            chunk.get_block(BlockPos::new(4, 5, 9)),
            Some(BlockStateId::AIR)
        );
    }

    #[test]
    fn setting_the_same_value_does_not_dirty() {
        let mut chunk = Chunk::new(ChunkPos::ORIGIN);
        // Writing air over air is a no-op and must not dirty the section.
        assert!(chunk
            .set_block(BlockPos::new(0, 0, 0), BlockStateId::AIR)
            .is_ok());
        assert!(!chunk.dirty_sections().any());
    }

    #[test]
    fn set_block_does_not_mark_persist_dirty() {
        // The persist-dirty signal must be driven only by the simulation layer,
        // never by an ordinary write or the generator, so a generated/edited chunk
        // is not persisted unless gameplay explicitly marks it.
        let mut chunk = Chunk::new(ChunkPos::ORIGIN);
        let pos = BlockPos::new(3, 5, 9);
        chunk
            .set_block(pos, BlockStateId::new(1))
            .expect("in range");
        assert!(chunk.dirty_sections().any());
        assert!(
            !chunk.persist_dirty_sections().any(),
            "set_block must not touch the persist-dirty mask"
        );
    }

    #[test]
    fn mark_persist_dirty_tracks_owning_section() {
        let mut chunk = Chunk::new(ChunkPos::ORIGIN);
        let pos = BlockPos::new(3, 5, 9); // section ((5 - (-64)) / 16) == 4
        chunk.mark_persist_dirty(pos);
        assert!(chunk.persist_dirty_sections().any());
        assert!(chunk.is_section_persist_dirty(4));
        assert_eq!(chunk.persist_dirty_sections().count(), 1);
        // The network dirty mask is independent and untouched by this call.
        assert!(!chunk.dirty_sections().any());

        chunk.clear_persist_dirty();
        assert!(!chunk.persist_dirty_sections().any());
    }

    #[test]
    fn mark_persist_dirty_out_of_range_is_noop() {
        let mut chunk = Chunk::new(ChunkPos::ORIGIN);
        chunk.mark_persist_dirty(BlockPos::new(0, MIN_Y - 1, 0));
        chunk.mark_persist_dirty(BlockPos::new(16, 0, 0)); // a different column
        assert!(!chunk.persist_dirty_sections().any());
    }

    #[test]
    fn clear_dirty_resets_tracking() {
        let mut chunk = Chunk::new(ChunkPos::ORIGIN);
        let _ = chunk.set_block(BlockPos::new(0, 0, 0), BlockStateId::new(1));
        assert!(chunk.dirty_sections().any());
        chunk.clear_dirty();
        assert!(!chunk.dirty_sections().any());
    }

    #[test]
    fn out_of_range_y_is_safe() {
        let mut chunk = Chunk::new(ChunkPos::ORIGIN);
        let below = BlockPos::new(0, MIN_Y - 1, 0);
        let above = BlockPos::new(0, max_y() + 1, 0);
        assert_eq!(chunk.get_block(below), None);
        assert_eq!(chunk.get_block(above), None);
        assert!(matches!(
            chunk.set_block(below, BlockStateId::new(1)),
            Err(crate::WorldError::BlockOutsideChunk { pos }) if pos == below
        ));
        assert!(matches!(
            chunk.set_block(above, BlockStateId::new(1)),
            Err(crate::WorldError::BlockOutsideChunk { pos }) if pos == above
        ));
        // Nothing was written, so the chunk stays clean.
        assert!(!chunk.dirty_sections().any());
    }

    #[test]
    fn block_in_another_column_is_rejected() {
        let chunk = Chunk::new(ChunkPos::ORIGIN);
        // A block in the neighbouring chunk (x = 16 -> chunk x = 1).
        let foreign = BlockPos::new(16, 0, 0);
        assert_eq!(chunk.get_block(foreign), None);
        let mut chunk = chunk;
        assert!(matches!(
            chunk.set_block(foreign, BlockStateId::new(1)),
            Err(crate::WorldError::BlockOutsideChunk { .. })
        ));
    }

    #[test]
    fn section_out_of_range_is_none() {
        let chunk = Chunk::new(ChunkPos::ORIGIN);
        assert!(chunk.section(0).is_some());
        assert!(chunk.section(SECTION_COUNT - 1).is_some());
        assert!(chunk.section(SECTION_COUNT).is_none());
    }

    #[test]
    fn heightmap_of_empty_chunk_is_all_none() {
        let chunk = Chunk::new(ChunkPos::ORIGIN);
        let hm = chunk.heightmap(HeightmapKind::WorldSurface);
        for z in 0..16u8 {
            for x in 0..16u8 {
                assert_eq!(hm.height(x, z), None);
            }
        }
    }

    #[test]
    fn heightmap_reports_highest_non_air_block() {
        let mut chunk = Chunk::new(ChunkPos::ORIGIN);
        // Two blocks in the same column at different heights: the higher wins.
        let _ = chunk.set_block(BlockPos::new(2, 10, 4), BlockStateId::new(1));
        let _ = chunk.set_block(BlockPos::new(2, 70, 4), BlockStateId::new(1));
        // A lone block in a different column.
        let _ = chunk.set_block(BlockPos::new(5, -30, 6), BlockStateId::new(1));

        let surface = chunk.heightmap(HeightmapKind::WorldSurface);
        let motion = chunk.heightmap(HeightmapKind::MotionBlocking);
        assert_eq!(surface.height(2, 4), Some(70));
        assert_eq!(surface.height(5, 6), Some(-30));
        assert_eq!(surface.height(0, 0), None);
        // Every modelled block is solid, so the two kinds agree.
        assert_eq!(motion.height(2, 4), Some(70));

        // Out-of-range column coordinates are safe.
        assert_eq!(surface.height(16, 0), None);
        assert_eq!(surface.height(0, 200), None);
    }
}
