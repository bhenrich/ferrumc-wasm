//! Error types for the world block model.

/// Failures produced by the block-storage primitives in this crate.
///
/// Each variant *classifies* the failure so callers can react programmatically
/// rather than parsing message strings. The enum is `#[non_exhaustive]`, so new
/// variants may be added later and downstream `match`es must include a wildcard
/// arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WorldError {
    /// A [`crate::PalettedContainer`] index was outside `0..CAPACITY`.
    #[error("container index {index} out of range for capacity {capacity}")]
    IndexOutOfRange {
        /// The offending index.
        index: usize,
        /// The container's fixed capacity (the exclusive upper bound).
        capacity: usize,
    },

    /// A [`crate::PackedArray`] index was outside `0..len`.
    #[error("packed-array index {index} out of range for length {len}")]
    PackedIndexOutOfRange {
        /// The offending index.
        index: usize,
        /// The array's length (the exclusive upper bound).
        len: usize,
    },

    /// A value did not fit in the array's bits-per-entry width.
    #[error("value {value} does not fit in {bits} bit(s) per entry")]
    ValueTooWide {
        /// The value that overflowed the entry width.
        value: u64,
        /// The configured bits-per-entry.
        bits: u8,
    },

    /// A [`crate::PackedArray`] was constructed with an unusable entry width.
    ///
    /// Bits-per-entry must be in `1..=64`: zero stores no information and more
    /// than 64 bits cannot fit in a single `u64` word.
    #[error("bits per entry must be in 1..=64, got {bits}")]
    InvalidBitsPerEntry {
        /// The rejected bits-per-entry value.
        bits: u8,
    },
}
