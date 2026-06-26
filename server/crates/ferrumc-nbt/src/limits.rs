//! [`NbtLimits`]: the resource caps every decode is checked against.

/// Resource limits enforced while decoding NBT from untrusted input.
///
/// Every limit defends against a distinct denial-of-service vector. Construct
/// the defaults with [`NbtLimits::default`] and override individual caps with
/// the chained `with_*` builder methods:
///
/// ```
/// use ferrumc_nbt::NbtLimits;
///
/// let limits = NbtLimits::default().with_max_depth(64).with_max_list_len(1024);
/// assert_eq!(limits.max_depth(), 64);
/// ```
///
/// Fields are private so the invariants stay the crate's responsibility, not
/// the caller's; read them back through the getters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NbtLimits {
    depth: usize,
    bytes: usize,
    list_len: usize,
    string_bytes: usize,
}

impl NbtLimits {
    /// Default maximum nesting depth, matching the vanilla NBT limit.
    pub const DEFAULT_MAX_DEPTH: usize = 512;

    /// Default maximum total input size: 2 MiB, matching the network layer's
    /// decompressed-output cap.
    pub const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024;

    /// Default maximum element count for any list or array (about one million).
    pub const DEFAULT_MAX_LIST_LEN: usize = 1 << 20;

    /// Default maximum string length in bytes. NBT strings carry a `u16` length
    /// prefix, so no string can ever exceed this regardless of the limit.
    pub const DEFAULT_MAX_STRING_BYTES: usize = 65_535;

    /// Replaces the maximum nesting depth.
    ///
    /// Each descent into a `TAG_Compound` or `TAG_List` counts as one level;
    /// the root compound sits at depth 1, so a value of 0 rejects every input.
    #[must_use]
    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.depth = max_depth;
        self
    }

    /// Replaces the maximum total input size in bytes.
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.bytes = max_bytes;
        self
    }

    /// Replaces the maximum element count for lists and byte/int/long arrays.
    #[must_use]
    pub fn with_max_list_len(mut self, max_list_len: usize) -> Self {
        self.list_len = max_list_len;
        self
    }

    /// Replaces the maximum string length in bytes.
    #[must_use]
    pub fn with_max_string_bytes(mut self, max_string_bytes: usize) -> Self {
        self.string_bytes = max_string_bytes;
        self
    }

    /// The maximum nesting depth.
    pub fn max_depth(&self) -> usize {
        self.depth
    }

    /// The maximum total input size in bytes.
    pub fn max_bytes(&self) -> usize {
        self.bytes
    }

    /// The maximum element count for lists and byte/int/long arrays.
    pub fn max_list_len(&self) -> usize {
        self.list_len
    }

    /// The maximum string length in bytes.
    pub fn max_string_bytes(&self) -> usize {
        self.string_bytes
    }
}

impl Default for NbtLimits {
    fn default() -> Self {
        Self {
            depth: Self::DEFAULT_MAX_DEPTH,
            bytes: Self::DEFAULT_MAX_BYTES,
            list_len: Self::DEFAULT_MAX_LIST_LEN,
            string_bytes: Self::DEFAULT_MAX_STRING_BYTES,
        }
    }
}
