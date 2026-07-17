//! [`AnvilError`]: the classified failure modes of region-file import.

use std::path::PathBuf;

/// A region-local chunk coordinate `(x, z)` within the 32x32 grid, used to
/// pinpoint which chunk a failure came from.
pub type ChunkCoord = (u8, u8);

/// Everything that can go wrong while reading a vanilla Anvil region file.
///
/// Anvil files are untrusted input, so every variant *classifies* a distinct
/// failure mode (truncation, a bad length, an unsupported codec, malformed NBT,
/// …) rather than collapsing into one opaque string. The enum is
/// `#[non_exhaustive]`: new variants may be added, so downstream `match`es must
/// carry a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AnvilError {
    /// The region file path does not exist.
    #[error("region file not found: {0}")]
    NotFound(PathBuf),

    /// The region file could not be read from disk.
    #[error("failed to read region file {path}: {source}")]
    Io {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The file is larger than the configured cap, so it is refused before any
    /// allocation rather than risking memory exhaustion.
    #[error("region file is {len} bytes, exceeding the {max}-byte cap")]
    FileTooLarge {
        /// The file's length in bytes.
        len: u64,
        /// The configured maximum.
        max: u64,
    },

    /// The file is smaller than the mandatory 8 KiB header (two 4 KiB sector
    /// tables), so it cannot be a valid region file.
    #[error("region file is {len} bytes, smaller than the {min}-byte header")]
    HeaderTooSmall {
        /// The file's length in bytes.
        len: usize,
        /// The minimum header size (8192 bytes).
        min: usize,
    },

    /// A region-local chunk coordinate was outside the 32x32 grid.
    #[error("region-local chunk coordinate ({x}, {z}) is outside the 32x32 grid")]
    ChunkCoordOutOfRange {
        /// The offending local x (must be `0..32`).
        x: u8,
        /// The offending local z (must be `0..32`).
        z: u8,
    },

    /// A chunk's location entry points to a sector range that runs past the end
    /// of the file.
    #[error("chunk {coord:?} location points past the end of the region file")]
    ChunkOffsetOutOfRange {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
    },

    /// A chunk's 4-byte length prefix is zero or runs past the end of the file.
    #[error("chunk {coord:?} declares an invalid payload length of {len} bytes")]
    BadChunkLength {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The declared payload length.
        len: usize,
    },

    /// The chunk's compression scheme byte is not one this importer supports.
    ///
    /// Supported schemes are `1` (gzip), `2` (zlib), and `3` (uncompressed).
    /// `4` (LZ4) and any other value are rejected here.
    #[error("chunk {coord:?} uses unsupported compression scheme {scheme}")]
    UnsupportedCompression {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The unsupported scheme byte (low 7 bits).
        scheme: u8,
    },

    /// The chunk is stored in an external `c.x.z.mcc` file (the compression byte
    /// has its high bit set), which this importer does not read.
    #[error("chunk {coord:?} is stored in an external .mcc file, which is unsupported")]
    ExternalChunk {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
    },

    /// The chunk's compressed payload failed to decompress.
    #[error("failed to decompress chunk {coord:?}: {source}")]
    Decompress {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The decompression I/O error.
        source: std::io::Error,
    },

    /// The chunk's decompressed payload exceeded the configured cap (guards
    /// against decompression bombs).
    #[error("decompressed chunk {coord:?} exceeds the {max}-byte cap")]
    ChunkTooLarge {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The configured decompressed-size cap.
        max: usize,
    },

    /// The chunk's decompressed payload was not valid NBT.
    #[error("malformed NBT in chunk {coord:?}: {source}")]
    Nbt {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The underlying NBT decode error.
        source: ferrumc_nbt::NbtError,
    },

    /// A required NBT field was missing from the chunk payload.
    #[error("chunk {coord:?} is missing required NBT field '{field}'")]
    MissingField {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The name of the absent field.
        field: &'static str,
    },

    /// An NBT field was present but had an unexpected tag type.
    #[error("chunk {coord:?} NBT field '{field}' has the wrong type")]
    WrongFieldType {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The name of the mistyped field.
        field: &'static str,
    },

    /// A schema-significant compound field appeared more than once.
    ///
    /// `ferrumc-nbt` intentionally preserves duplicate compound names, so the
    /// importer rejects ambiguity instead of silently taking the first value.
    #[error("chunk {coord:?} NBT field '{field}' appears more than once")]
    DuplicateNbtField {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The duplicated field path.
        field: &'static str,
    },

    /// A known block entity came from a world-data schema this importer does
    /// not interpret.
    #[error(
        "chunk {coord:?} block entity at {pos:?} uses data version {found:?}, expected {expected}"
    )]
    UnsupportedBlockEntityDataVersion {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The sign or chest position.
        pos: ferrumc_math::BlockPos,
        /// The root `DataVersion`, or `None` when it was absent.
        found: Option<i32>,
        /// The pinned 1.21.8 data version.
        expected: i32,
    },

    /// The raw block-entity list exceeded the world model's per-chunk cap.
    #[error(
        "chunk {coord:?} carries {count} block entities, exceeding the per-chunk cap of {max}"
    )]
    TooManyBlockEntities {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The rejected raw list length.
        count: usize,
        /// The maximum accepted list length.
        max: usize,
    },

    /// Two supported block entities claimed the same absolute block position.
    #[error("chunk {coord:?} carries duplicate block entities at {pos:?}")]
    DuplicateBlockEntity {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The duplicated absolute position.
        pos: ferrumc_math::BlockPos,
    },

    /// A supported block entity named a position outside the target chunk.
    #[error("chunk {coord:?} block entity at {pos:?} is outside the target chunk")]
    BlockEntityOutsideChunk {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The offending absolute position.
        pos: ferrumc_math::BlockPos,
    },

    /// A supported block entity did not agree with the imported block state.
    #[error(
        "chunk {coord:?} block entity '{id}' at {pos:?} does not match block-state id {state}"
    )]
    BlockEntityStateMismatch {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The block-entity resource location.
        id: String,
        /// The entity's absolute position.
        pos: ferrumc_math::BlockPos,
        /// The imported numeric block-state id.
        state: u32,
    },

    /// A sign face did not carry exactly four message entries.
    #[error(
        "chunk {coord:?} sign at {pos:?} face '{face}' has {count} messages, expected {expected}"
    )]
    BadSignMessageCount {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The sign's absolute position.
        pos: ferrumc_math::BlockPos,
        /// The face field (`front_text` or `back_text`).
        face: &'static str,
        /// The rejected message count.
        count: usize,
        /// The required fixed message count.
        expected: usize,
    },

    /// One literal sign line exceeded the world model's byte ceiling.
    #[error(
        "chunk {coord:?} sign at {pos:?} face '{face}' line {line} is {len} bytes, exceeding the {max}-byte cap"
    )]
    SignTextTooLong {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The sign's absolute position.
        pos: ferrumc_math::BlockPos,
        /// The face field (`front_text` or `back_text`).
        face: &'static str,
        /// The zero-based line number.
        line: usize,
        /// The rejected literal's encoded byte length.
        len: usize,
        /// The maximum accepted encoded byte length.
        max: usize,
    },

    /// A supported payload contains data the current public world-model API
    /// cannot represent without loss.
    #[error("chunk {coord:?} block entity at {pos:?} has unsupported non-default field '{field}'")]
    UnsupportedBlockEntityData {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The entity's absolute position.
        pos: ferrumc_math::BlockPos,
        /// The unsupported field path.
        field: &'static str,
    },

    /// Inserting a decoded block entity into the bounded world chunk failed.
    #[error("chunk {coord:?} failed to place a block entity at {pos:?}: {source}")]
    BlockEntityPlacement {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The entity's absolute position.
        pos: ferrumc_math::BlockPos,
        /// The underlying world-model error.
        source: ferrumc_world::WorldError,
    },

    /// A section's block-state palette was empty (every section must name at
    /// least one block state).
    #[error("chunk {coord:?} section {section_y} has an empty block-state palette")]
    EmptyPalette {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The section's vanilla `Y` index.
        section_y: i32,
    },

    /// A section's packed block-state data had the wrong number of longs for its
    /// palette size.
    #[error(
        "chunk {coord:?} section {section_y} block-state data has {got} longs, expected {expected}"
    )]
    BadBlockStateData {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The section's vanilla `Y` index.
        section_y: i32,
        /// The number of longs actually present.
        got: usize,
        /// The number of longs required for the palette's bit width.
        expected: usize,
    },

    /// A packed block-state entry referenced a palette slot that does not exist.
    #[error(
        "chunk {coord:?} section {section_y} references palette index {index} of {len} entries"
    )]
    PaletteIndexOutOfRange {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The section's vanilla `Y` index.
        section_y: i32,
        /// The out-of-range palette index.
        index: usize,
        /// The palette length.
        len: usize,
    },

    /// Placing an imported block into the world model failed. The section range
    /// is validated up front, so this is a defensive guard that should not occur
    /// in practice.
    #[error("chunk {coord:?} failed to place a block: {source}")]
    Placement {
        /// The region-local chunk coordinate.
        coord: ChunkCoord,
        /// The underlying world-model error.
        source: ferrumc_world::WorldError,
    },

    /// A region file name did not match the required `r.<x>.<z>.mca` pattern, so
    /// its region coordinates could not be derived.
    #[error("invalid region file name {0:?}: expected 'r.<x>.<z>.mca'")]
    BadRegionFileName(String),
}
