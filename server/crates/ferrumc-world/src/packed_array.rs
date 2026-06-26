//! A bit-packed array of fixed-width unsigned entries, Minecraft chunk-storage
//! style.

use crate::error::WorldError;

/// Number of bits in a backing word.
const WORD_BITS: usize = u64::BITS as usize;

/// A compact array of `len` unsigned entries, each `bits_per_entry` bits wide,
/// packed into `u64` words.
///
/// This follows the modern (1.16+) Minecraft chunk-storage layout: entries are
/// **non-spanning**. Each `u64` holds `floor(64 / bits_per_entry)` whole
/// entries, low bits first, and any leftover high bits in a word are unused
/// padding. An entry therefore never straddles a word boundary, which is what
/// the wire format and the client expect.
///
/// All indexing is bounds-checked: [`PackedArray::get`] returns `None` and
/// [`PackedArray::set`] returns [`WorldError`] for an out-of-range index, so no
/// valid call can panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedArray {
    /// Packed backing storage; `len.div_ceil(values_per_word)` words.
    words: Vec<u64>,
    /// Number of logical entries.
    len: usize,
    /// Width of each entry, in `1..=64`.
    bits_per_entry: u8,
    /// Whole entries stored per `u64` word (`64 / bits_per_entry`).
    values_per_word: usize,
    /// Low-`bits_per_entry` mask used to read and clamp a single entry.
    value_mask: u64,
}

impl PackedArray {
    /// Creates a zero-filled array of `len` entries, each `bits_per_entry` bits
    /// wide.
    ///
    /// Returns [`WorldError::InvalidBitsPerEntry`] unless `bits_per_entry` is in
    /// `1..=64`.
    pub fn new(bits_per_entry: u8, len: usize) -> Result<Self, WorldError> {
        if bits_per_entry == 0 || usize::from(bits_per_entry) > WORD_BITS {
            return Err(WorldError::InvalidBitsPerEntry {
                bits: bits_per_entry,
            });
        }
        let values_per_word = WORD_BITS / usize::from(bits_per_entry);
        // Shifting a `u64` by 64 is undefined behaviour in the abstract machine,
        // so the full-width case is handled without a shift.
        let value_mask = if usize::from(bits_per_entry) == WORD_BITS {
            u64::MAX
        } else {
            (1u64 << bits_per_entry) - 1
        };
        let word_count = len.div_ceil(values_per_word);
        Ok(Self {
            words: vec![0; word_count],
            len,
            bits_per_entry,
            values_per_word,
            value_mask,
        })
    }

    /// Returns the number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the array holds no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the width of each entry, in bits.
    #[must_use]
    pub const fn bits_per_entry(&self) -> u8 {
        self.bits_per_entry
    }

    /// Returns the raw packed backing words.
    ///
    /// Entries are stored low bits first within each word, non-spanning; this is
    /// the same layout the chunk-section wire format uses, so the slice can be
    /// handed straight to a serializer.
    #[must_use]
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    /// Returns the entry at `index`, or `None` if `index >= len`.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<u64> {
        if index >= self.len {
            return None;
        }
        let word_index = index / self.values_per_word;
        let bit_offset = (index % self.values_per_word) * usize::from(self.bits_per_entry);
        // `word_index < words.len()` holds because `index < len` and there are
        // `len.div_ceil(values_per_word)` words, but `get` keeps this panic-free.
        let word = self.words.get(word_index)?;
        Some((word >> bit_offset) & self.value_mask)
    }

    /// Stores `value` at `index`.
    ///
    /// Returns [`WorldError::PackedIndexOutOfRange`] if `index >= len`, or
    /// [`WorldError::ValueTooWide`] if `value` does not fit in `bits_per_entry`
    /// bits.
    pub fn set(&mut self, index: usize, value: u64) -> Result<(), WorldError> {
        if index >= self.len {
            return Err(WorldError::PackedIndexOutOfRange {
                index,
                len: self.len,
            });
        }
        if value & !self.value_mask != 0 {
            return Err(WorldError::ValueTooWide {
                value,
                bits: self.bits_per_entry,
            });
        }
        let word_index = index / self.values_per_word;
        let bit_offset = (index % self.values_per_word) * usize::from(self.bits_per_entry);
        let word = self
            .words
            .get_mut(word_index)
            .ok_or(WorldError::PackedIndexOutOfRange {
                index,
                len: self.len,
            })?;
        *word = (*word & !(self.value_mask << bit_offset)) | (value << bit_offset);
        Ok(())
    }

    /// Returns a copy of this array repacked to `new_bits` bits per entry,
    /// preserving every entry value and the length.
    ///
    /// Returns [`WorldError::InvalidBitsPerEntry`] if `new_bits` is out of range,
    /// or [`WorldError::ValueTooWide`] if shrinking the width would truncate a
    /// stored value. Growing the width (the only case the palette promotion path
    /// uses) never truncates.
    pub fn resized(&self, new_bits: u8) -> Result<Self, WorldError> {
        let mut out = Self::new(new_bits, self.len)?;
        for index in 0..self.len {
            // `index < len`, so both calls are in range.
            let value = self.get(index).unwrap_or(0);
            out.set(index, value)?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::PackedArray;
    use crate::error::WorldError;

    #[test]
    fn rejects_invalid_bits_per_entry() {
        assert!(matches!(
            PackedArray::new(0, 16),
            Err(WorldError::InvalidBitsPerEntry { bits: 0 })
        ));
        assert!(matches!(
            PackedArray::new(65, 16),
            Err(WorldError::InvalidBitsPerEntry { bits: 65 })
        ));
        assert!(PackedArray::new(1, 16).is_ok());
        assert!(PackedArray::new(64, 16).is_ok());
    }

    #[test]
    fn empty_array_has_no_words_and_no_entries() {
        let arr = PackedArray::new(5, 0).unwrap();
        assert!(arr.is_empty());
        assert_eq!(arr.len(), 0);
        assert!(arr.words().is_empty());
        assert_eq!(arr.get(0), None);
    }

    #[test]
    fn word_count_matches_non_spanning_layout() {
        // 5 bits -> 12 entries per 64-bit word.
        let arr = PackedArray::new(5, 24).unwrap();
        assert_eq!(arr.words().len(), 2);
        // 13 entries needs a third word (24 fit in two, 25 spill into a third).
        let arr = PackedArray::new(5, 25).unwrap();
        assert_eq!(arr.words().len(), 3);
        // 64 bits -> 1 entry per word.
        let arr = PackedArray::new(64, 3).unwrap();
        assert_eq!(arr.words().len(), 3);
    }

    #[test]
    fn round_trips_across_bit_widths() {
        for bits in [1u8, 2, 3, 4, 5, 8, 13, 15, 32, 64] {
            let len = 200usize;
            let mut arr = PackedArray::new(bits, len).unwrap();
            let max = if bits == 64 {
                u64::MAX
            } else {
                (1u64 << bits) - 1
            };
            // Deterministic pseudo-values bounded by the entry width.
            let values: Vec<u64> = (0..len).map(|i| (i as u64 * 2_654_435_761) & max).collect();
            for (i, &v) in values.iter().enumerate() {
                arr.set(i, v).unwrap();
            }
            for (i, &v) in values.iter().enumerate() {
                assert_eq!(arr.get(i), Some(v), "bits={bits} index={i}");
            }
        }
    }

    #[test]
    fn entries_do_not_corrupt_neighbours_across_word_boundary() {
        // 5 bits -> 12 entries per word. Index 11 is the last entry in word 0,
        // index 12 is the first entry in word 1. Writing one must not disturb
        // the other, nor the entries packed beside them in the same word.
        let mut arr = PackedArray::new(5, 24).unwrap();
        for i in 0..24 {
            arr.set(i, 0b1_0101).unwrap();
        }
        arr.set(11, 0b1_1111).unwrap();
        arr.set(12, 0b0_0001).unwrap();
        assert_eq!(arr.get(10), Some(0b1_0101));
        assert_eq!(arr.get(11), Some(0b1_1111));
        assert_eq!(arr.get(12), Some(0b0_0001));
        assert_eq!(arr.get(13), Some(0b1_0101));
    }

    #[test]
    fn rejects_too_wide_value() {
        let mut arr = PackedArray::new(4, 8).unwrap();
        assert!(arr.set(0, 15).is_ok());
        assert!(matches!(
            arr.set(0, 16),
            Err(WorldError::ValueTooWide { value: 16, bits: 4 })
        ));
    }

    #[test]
    fn out_of_range_index_is_reported() {
        let mut arr = PackedArray::new(4, 8).unwrap();
        assert_eq!(arr.get(8), None);
        assert!(matches!(
            arr.set(8, 0),
            Err(WorldError::PackedIndexOutOfRange { index: 8, len: 8 })
        ));
    }

    #[test]
    fn resized_grows_and_preserves_values() {
        let mut arr = PackedArray::new(4, 32).unwrap();
        for i in 0..32 {
            arr.set(i, (i as u64) & 0xF).unwrap();
        }
        let wider = arr.resized(8).unwrap();
        assert_eq!(wider.bits_per_entry(), 8);
        assert_eq!(wider.len(), 32);
        for i in 0..32 {
            assert_eq!(wider.get(i), Some((i as u64) & 0xF));
        }
    }

    #[test]
    fn resized_smaller_rejects_values_that_no_longer_fit() {
        let mut arr = PackedArray::new(8, 4).unwrap();
        arr.set(0, 200).unwrap();
        assert!(matches!(
            arr.resized(4),
            Err(WorldError::ValueTooWide { .. })
        ));
    }
}
