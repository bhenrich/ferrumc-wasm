#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! Vanilla Anvil world import into `FerrumC`'s world model.
//!
//! The entry points form two layers:
//!
//! - [`Region`] is the loaded region file. [`Region::read_chunk`] imports one
//!   chunk by its region-local `(x, z)`, [`Region::iter_chunks`] walks every
//!   present chunk lazily, and [`Region::chunk_payload`] exposes the raw
//!   decompressed chunk NBT bytes for callers that want to inspect them.
//! - [`import_region_file`] and [`import_world_dir`] are convenience wrappers
//!   that eagerly collect every chunk from one file, or from every
//!   `r.<x>.<z>.mca` in a `region/` directory, respectively.
//!
//! Every public function returns a [`Result`] over [`AnvilError`], which
//! classifies each failure mode (truncation, bad length, unsupported codec,
//! malformed NBT, …) so callers can react programmatically.

mod chunk;
mod error;
mod limits;
mod region;

use std::path::Path;

use ferrumc_math::ChunkPos;
use ferrumc_world::Chunk;

pub use error::{AnvilError, ChunkCoord};
pub use limits::AnvilLimits;
pub use region::{region_pos_from_path, Region, RegionChunkIter};

/// Imports every present chunk from a single region file.
///
/// The region coordinates are derived from the file name (`r.<x>.<z>.mca`), so
/// the returned [`ChunkPos`] values are absolute. Fails on the first malformed
/// chunk.
///
/// This buffers the whole region's chunks in memory; for incremental processing
/// use [`Region::iter_chunks`] directly.
pub fn import_region_file(path: impl AsRef<Path>) -> Result<Vec<(ChunkPos, Chunk)>, AnvilError> {
    let region = Region::open(path)?;
    region.iter_chunks().collect()
}

/// Imports every present chunk from every `r.<x>.<z>.mca` file in a directory.
///
/// Entries whose names do not match the region pattern are skipped. Fails on the
/// first I/O error or malformed chunk.
///
/// Like [`import_region_file`], this buffers every chunk in memory; a large
/// multi-region world can be sizeable, so prefer iterating region-by-region for
/// bulk loads.
pub fn import_world_dir(
    region_dir: impl AsRef<Path>,
) -> Result<Vec<(ChunkPos, Chunk)>, AnvilError> {
    let region_dir = region_dir.as_ref();
    let entries = std::fs::read_dir(region_dir).map_err(|source| AnvilError::Io {
        path: region_dir.to_path_buf(),
        source,
    })?;

    let mut chunks = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| AnvilError::Io {
            path: region_dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        // Only region files; anything else in the directory is ignored.
        if region_pos_from_path(&path).is_err() {
            continue;
        }
        let region = Region::open(&path)?;
        for result in region.iter_chunks() {
            chunks.push(result?);
        }
    }
    Ok(chunks)
}
