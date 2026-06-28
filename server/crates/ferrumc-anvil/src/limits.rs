//! [`AnvilLimits`]: the resource caps applied to every region read.

use ferrumc_nbt::NbtLimits;

/// Resource limits enforced while reading an untrusted region file.
///
/// Two distinct denial-of-service vectors are bounded: the on-disk file size
/// (capped before the bytes are read into memory) and the *decompressed* size of
/// any single chunk (capped to defeat a decompression bomb). Construct the
/// defaults with [`AnvilLimits::default`] and override individual caps with the
/// chained `with_*` builders.
///
/// ```
/// use ferrumc_anvil::AnvilLimits;
///
/// let limits = AnvilLimits::default().with_max_chunk_bytes(4 * 1024 * 1024);
/// assert_eq!(limits.max_chunk_bytes(), 4 * 1024 * 1024);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnvilLimits {
    max_file_bytes: u64,
    max_chunk_bytes: usize,
}

impl AnvilLimits {
    /// Default maximum on-disk region-file size: 256 MiB. Real region files run
    /// to a few tens of MiB; this leaves generous headroom while still refusing
    /// an absurdly large file before allocating for it.
    pub const DEFAULT_MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

    /// Default maximum *decompressed* size of a single chunk: 8 MiB. A typical
    /// chunk decompresses to tens of KiB; the cap stops a crafted chunk from
    /// inflating without bound.
    pub const DEFAULT_MAX_CHUNK_BYTES: usize = 8 * 1024 * 1024;

    /// Replaces the maximum on-disk file size in bytes.
    #[must_use]
    pub fn with_max_file_bytes(mut self, max_file_bytes: u64) -> Self {
        self.max_file_bytes = max_file_bytes;
        self
    }

    /// Replaces the maximum decompressed chunk size in bytes.
    #[must_use]
    pub fn with_max_chunk_bytes(mut self, max_chunk_bytes: usize) -> Self {
        self.max_chunk_bytes = max_chunk_bytes;
        self
    }

    /// The maximum on-disk file size in bytes.
    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_bytes
    }

    /// The maximum decompressed chunk size in bytes.
    pub fn max_chunk_bytes(&self) -> usize {
        self.max_chunk_bytes
    }

    /// The [`NbtLimits`] used to decode a chunk payload, derived so the NBT
    /// decoder may consume at most [`Self::max_chunk_bytes`] bytes.
    pub(crate) fn nbt_limits(&self) -> NbtLimits {
        NbtLimits::default().with_max_bytes(self.max_chunk_bytes)
    }
}

impl Default for AnvilLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
            max_chunk_bytes: Self::DEFAULT_MAX_CHUNK_BYTES,
        }
    }
}
