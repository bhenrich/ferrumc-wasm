//! Per-section dirty tracking for a chunk.

use crate::chunk::SECTION_COUNT;

/// A set of "dirty" section indices: the sections of a [`crate::Chunk`] that
/// have been modified since the last [`DirtySections::clear`] and therefore need
/// to be re-persisted to storage or re-sent to clients.
///
/// Section indices are 0-based from the bottom of the world: section `0` is the
/// lowest 16-block slice (at `MIN_Y`) and section `SECTION_COUNT - 1` is the
/// topmost. The set is a fixed-width bitmask — there are only [`SECTION_COUNT`]
/// (24) sections, so a single `u32` holds one bit per section with bits to
/// spare. Out-of-range indices are ignored by [`DirtySections::mark`] and
/// reported clean by [`DirtySections::is_dirty`], so no call can panic.
///
/// [`SECTION_COUNT`]: crate::SECTION_COUNT
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DirtySections {
    /// Bit `i` set means section `i` is dirty. Only the low `SECTION_COUNT`
    /// bits are ever set; the rest stay zero.
    bits: u32,
}

impl DirtySections {
    /// Creates an empty set in which no section is dirty.
    #[must_use]
    pub const fn new() -> Self {
        Self { bits: 0 }
    }

    /// Marks section `section_index` dirty.
    ///
    /// Indices `>= SECTION_COUNT` are out of range and silently ignored, so a
    /// bad index can never panic or corrupt the bitmask.
    pub fn mark(&mut self, section_index: usize) {
        if section_index < SECTION_COUNT {
            // `section_index < SECTION_COUNT == 24 < 32`, so the shift width is
            // always valid for a `u32`.
            self.bits |= 1u32 << section_index;
        }
    }

    /// Returns `true` if section `section_index` is dirty.
    ///
    /// An out-of-range index is reported as clean (`false`).
    #[must_use]
    pub fn is_dirty(&self, section_index: usize) -> bool {
        section_index < SECTION_COUNT && ((self.bits >> section_index) & 1) == 1
    }

    /// Returns `true` if any section is dirty.
    #[must_use]
    pub const fn any(&self) -> bool {
        self.bits != 0
    }

    /// Returns the number of dirty sections.
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.bits.count_ones()
    }

    /// Clears every dirty bit, marking all sections clean.
    ///
    /// Called by the persistence/network layer once the dirty sections have been
    /// flushed.
    pub fn clear(&mut self) {
        self.bits = 0;
    }

    /// Returns an iterator over the indices of the dirty sections, ascending
    /// from the bottom of the world.
    pub fn dirty_indices(&self) -> impl Iterator<Item = usize> + '_ {
        (0..SECTION_COUNT).filter(move |&index| self.is_dirty(index))
    }
}

#[cfg(test)]
mod tests {
    use super::DirtySections;
    use crate::chunk::SECTION_COUNT;

    #[test]
    fn new_set_is_empty() {
        let d = DirtySections::new();
        assert!(!d.any());
        assert_eq!(d.count(), 0);
        assert_eq!(d.dirty_indices().count(), 0);
        for i in 0..SECTION_COUNT {
            assert!(!d.is_dirty(i));
        }
    }

    #[test]
    fn mark_sets_and_reports_dirty() {
        let mut d = DirtySections::new();
        d.mark(0);
        d.mark(7);
        d.mark(SECTION_COUNT - 1);
        assert!(d.any());
        assert_eq!(d.count(), 3);
        assert!(d.is_dirty(0));
        assert!(d.is_dirty(7));
        assert!(d.is_dirty(SECTION_COUNT - 1));
        assert!(!d.is_dirty(1));
        let collected: Vec<usize> = d.dirty_indices().collect();
        assert_eq!(collected, vec![0, 7, SECTION_COUNT - 1]);
    }

    #[test]
    fn out_of_range_mark_is_ignored() {
        let mut d = DirtySections::new();
        d.mark(SECTION_COUNT);
        d.mark(SECTION_COUNT + 1000);
        assert!(!d.any());
        assert!(!d.is_dirty(SECTION_COUNT));
        assert!(!d.is_dirty(SECTION_COUNT + 1000));
    }

    #[test]
    fn clear_resets_every_bit() {
        let mut d = DirtySections::new();
        d.mark(3);
        d.mark(11);
        assert!(d.any());
        d.clear();
        assert!(!d.any());
        assert_eq!(d.count(), 0);
    }

    #[test]
    fn marking_twice_is_idempotent() {
        let mut d = DirtySections::new();
        d.mark(5);
        d.mark(5);
        assert_eq!(d.count(), 1);
        assert!(d.is_dirty(5));
    }
}
