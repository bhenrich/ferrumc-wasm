//! The [`NbtError`] taxonomy and the crate [`Result`] alias.

use ferrumc_codec::CodecError;

/// A convenience alias for results produced by this crate.
pub type Result<T> = core::result::Result<T, NbtError>;

/// Every way NBT decoding or encoding can fail.
///
/// Each variant classifies a *distinct* failure so callers can react precisely
/// instead of matching on an opaque string. Underlying byte-level failures
/// (short reads, trailing bytes) arrive wrapped in [`NbtError::Codec`].
///
/// The enum is `#[non_exhaustive]`: new failure modes may be added without a
/// breaking change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NbtError {
    /// Nesting (compounds and lists) descended past the configured `max_depth`.
    #[error("NBT nesting exceeded the maximum depth of {max}")]
    DepthExceeded {
        /// The configured maximum depth.
        max: usize,
    },

    /// The input was larger than the configured `max_bytes`.
    #[error("NBT input of {len} bytes exceeds the maximum of {max}")]
    MaxBytesExceeded {
        /// The actual input length in bytes.
        len: usize,
        /// The configured maximum byte count.
        max: usize,
    },

    /// A list or array declared more elements than `max_list_len` permits.
    #[error("NBT sequence length {len} exceeds the maximum of {max}")]
    ListTooLong {
        /// The declared element count.
        len: usize,
        /// The configured maximum element count.
        max: usize,
    },

    /// A string was longer than `max_string_bytes`, or longer than a `u16`
    /// length prefix can express (the latter only on the write path).
    #[error("NBT string of {len} bytes exceeds the maximum of {max}")]
    StringTooLong {
        /// The declared string length in bytes.
        len: usize,
        /// The configured maximum string length in bytes.
        max: usize,
    },

    /// A tag type id outside the valid range `0..=12` was encountered.
    #[error("unknown NBT tag type id {id}")]
    UnknownTagType {
        /// The offending type id byte.
        id: u8,
    },

    /// A string's bytes were not valid Java Modified `UTF-8`.
    ///
    /// `TAG_String` is encoded with Java's Modified `UTF-8` (the encoding of
    /// `DataInput.readUTF`); see the crate documentation. Byte sequences that do
    /// not form valid Modified `UTF-8` — an invalid lead byte, a missing or
    /// malformed continuation byte, or an unpaired surrogate — are rejected with
    /// this error.
    #[error("NBT string was not valid Modified UTF-8")]
    InvalidUtf8,

    /// A list or array declared a negative length, which is never valid.
    #[error("NBT sequence declared a negative length of {len}")]
    NegativeLength {
        /// The offending negative length.
        len: i32,
    },

    /// A non-empty list declared its element type as `TAG_End`.
    #[error("non-empty NBT list declared element type TAG_End")]
    MalformedList,

    /// A list being encoded held elements of more than one tag type, which the
    /// format cannot represent (every element shares the list's single declared
    /// element type). Only reachable on the write path.
    #[error("NBT list is heterogeneous: expected element type id {expected} but found {found}")]
    HeterogeneousList {
        /// The element type id declared by the first element.
        expected: u8,
        /// The differing element type id that was found.
        found: u8,
    },

    /// A root tag was not the required `TAG_Compound`.
    #[error("NBT root must be a TAG_Compound but found tag type id {id}")]
    UnexpectedRootTag {
        /// The type id that was found in the root position.
        id: u8,
    },

    /// An error surfaced from the underlying bounded reader (short read,
    /// trailing bytes, and similar low-level conditions).
    #[error(transparent)]
    Codec(#[from] CodecError),
}
