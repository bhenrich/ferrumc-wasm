//! [`Region`]: a loaded Anvil region file and the low-level chunk-locating and
//! decompression logic that sits beneath the world-model import.
//!
//! An Anvil region file (`r.<x>.<z>.mca`) stores a 32x32 grid of chunks. Its
//! layout is:
//!
//! - **bytes `0..4096`** — 1024 location entries, one per chunk. Each is a
//!   big-endian `u32`: the high 24 bits are the chunk's start offset in 4 KiB
//!   sectors, the low 8 bits its length in sectors. An all-zero entry means the
//!   chunk is absent.
//! - **bytes `4096..8192`** — 1024 timestamp entries (ignored here).
//! - **the rest** — chunk payloads, each a 4-byte big-endian length, a 1-byte
//!   compression scheme, then the compressed chunk NBT, padded up to a sector
//!   boundary.

use std::io::Read;
use std::path::Path;

use ferrumc_math::{ChunkPos, RegionPos};
use ferrumc_world::Chunk;

use crate::chunk::chunk_from_nbt;
use crate::error::{AnvilError, ChunkCoord};
use crate::limits::AnvilLimits;

/// Size of one Anvil sector in bytes.
const SECTOR_BYTES: usize = 4096;

/// Size of the two-table header (locations + timestamps) in bytes.
const HEADER_BYTES: usize = 2 * SECTOR_BYTES;

/// Edge length of a region in chunks (a region is `32 x 32` chunks).
const REGION_EDGE: u8 = 32;

/// Supported per-chunk compression schemes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
    /// Scheme `1`: gzip.
    Gzip,
    /// Scheme `2`: zlib (the vanilla default).
    Zlib,
    /// Scheme `3`: uncompressed.
    None,
}

impl Compression {
    /// Maps a compression scheme byte to a [`Compression`], rejecting the
    /// external-file flag (high bit) and unsupported schemes.
    fn from_scheme(scheme: u8, coord: ChunkCoord) -> Result<Self, AnvilError> {
        // The high bit signals the chunk lives in an external `c.x.z.mcc` file.
        if scheme & 0x80 != 0 {
            return Err(AnvilError::ExternalChunk { coord });
        }
        match scheme {
            1 => Ok(Self::Gzip),
            2 => Ok(Self::Zlib),
            3 => Ok(Self::None),
            other => Err(AnvilError::UnsupportedCompression {
                coord,
                scheme: other,
            }),
        }
    }
}

/// A loaded Anvil region file: its raw bytes, the region's coordinates, and the
/// limits applied while decoding it.
///
/// Construct one with [`Region::open`] (which derives the region coordinates
/// from the `r.<x>.<z>.mca` file name) or [`Region::from_bytes`] (which takes
/// the coordinates explicitly). The whole file is held in memory; its size is
/// bounded by [`AnvilLimits::max_file_bytes`] before it is read.
#[derive(Debug, Clone)]
pub struct Region {
    pos: RegionPos,
    bytes: Vec<u8>,
    limits: AnvilLimits,
}

impl Region {
    /// Opens a region file from disk with the default [`AnvilLimits`].
    ///
    /// The region coordinates are derived from the file name, which must match
    /// `r.<x>.<z>.mca`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AnvilError> {
        Self::open_with_limits(path, AnvilLimits::default())
    }

    /// Opens a region file from disk with explicit [`AnvilLimits`].
    pub fn open_with_limits(
        path: impl AsRef<Path>,
        limits: AnvilLimits,
    ) -> Result<Self, AnvilError> {
        let path = path.as_ref();
        let region_pos = region_pos_from_path(path)?;

        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(AnvilError::NotFound(path.to_path_buf()))
            }
            Err(source) => {
                return Err(AnvilError::Io {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };

        // Reject an oversized file before reading it into memory.
        if metadata.len() > limits.max_file_bytes() {
            return Err(AnvilError::FileTooLarge {
                len: metadata.len(),
                max: limits.max_file_bytes(),
            });
        }

        let bytes = std::fs::read(path).map_err(|source| AnvilError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        Self::from_bytes_with_limits(region_pos, bytes, limits)
    }

    /// Builds a region from in-memory bytes with the default [`AnvilLimits`].
    ///
    /// Use this when the bytes did not come from a path with a parseable name
    /// (for example a test fixture or an in-memory buffer); the region
    /// coordinates are supplied explicitly.
    pub fn from_bytes(region_pos: RegionPos, bytes: Vec<u8>) -> Result<Self, AnvilError> {
        Self::from_bytes_with_limits(region_pos, bytes, AnvilLimits::default())
    }

    /// Builds a region from in-memory bytes with explicit [`AnvilLimits`].
    pub fn from_bytes_with_limits(
        region_pos: RegionPos,
        bytes: Vec<u8>,
        limits: AnvilLimits,
    ) -> Result<Self, AnvilError> {
        if bytes.len() < HEADER_BYTES {
            return Err(AnvilError::HeaderTooSmall {
                len: bytes.len(),
                min: HEADER_BYTES,
            });
        }
        Ok(Self {
            pos: region_pos,
            bytes,
            limits,
        })
    }

    /// Returns the region's coordinates.
    #[must_use]
    pub fn region_pos(&self) -> RegionPos {
        self.pos
    }

    /// Returns the absolute [`ChunkPos`] of the chunk at region-local `(x, z)`.
    ///
    /// Returns [`AnvilError::ChunkCoordOutOfRange`] if either coordinate is not
    /// in `0..32`.
    pub fn chunk_pos(&self, local_x: u8, local_z: u8) -> Result<ChunkPos, AnvilError> {
        check_local(local_x, local_z)?;
        let origin = self.pos.origin_chunk();
        Ok(ChunkPos::new(
            origin.x() + i32::from(local_x),
            origin.z() + i32::from(local_z),
        ))
    }

    /// Returns `true` if the chunk at region-local `(x, z)` is present in the
    /// file (its location entry is non-zero). Returns `false` for both an absent
    /// chunk and an out-of-range coordinate.
    #[must_use]
    pub fn contains_chunk(&self, local_x: u8, local_z: u8) -> bool {
        matches!(self.locate(local_x, local_z), Ok(Some(_)))
    }

    /// Returns the decompressed NBT bytes for the chunk at region-local
    /// `(x, z)`, or `None` if the chunk is absent.
    ///
    /// The returned buffer is the raw chunk NBT (a named-root `TAG_Compound`);
    /// its size is bounded by [`AnvilLimits::max_chunk_bytes`].
    pub fn chunk_payload(&self, local_x: u8, local_z: u8) -> Result<Option<Vec<u8>>, AnvilError> {
        let coord = (local_x, local_z);
        let Some((offset, payload_len)) = self.locate(local_x, local_z)? else {
            return Ok(None);
        };

        // Layout at `offset`: [u32 payload_len][u8 scheme][compressed...]. The
        // length already covers the scheme byte, so the compressed body is
        // `payload_len - 1` bytes immediately after it.
        // Pass the raw scheme byte: `from_scheme` inspects the high (external)
        // bit itself before matching the low bits.
        let scheme = self.bytes[offset + 4];
        let compression = Compression::from_scheme(scheme, coord)?;
        let body_start = offset + 5;
        let body_end = offset + 4 + payload_len;
        let compressed = &self.bytes[body_start..body_end];

        let payload = decompress(
            compression,
            compressed,
            coord,
            self.limits.max_chunk_bytes(),
        )?;
        Ok(Some(payload))
    }

    /// Reads and imports the chunk at region-local `(x, z)` into the world
    /// model, or `None` if the chunk is absent.
    pub fn read_chunk(&self, local_x: u8, local_z: u8) -> Result<Option<Chunk>, AnvilError> {
        let coord = (local_x, local_z);
        let chunk_pos = self.chunk_pos(local_x, local_z)?;
        let Some(payload) = self.chunk_payload(local_x, local_z)? else {
            return Ok(None);
        };

        // The chunk root is the file-form named compound. Tolerate trailing
        // bytes (some writers pad), taking only the root tag.
        let (root, _consumed) =
            ferrumc_nbt::read_named_root_with_consumed(&payload, &self.limits.nbt_limits())
                .map(|(_name, tag, consumed)| (tag, consumed))
                .map_err(|source| AnvilError::Nbt { coord, source })?;

        let chunk = chunk_from_nbt(coord, chunk_pos, &root)?;
        Ok(Some(chunk))
    }

    /// Iterates every present chunk in the region, importing each into the world
    /// model and yielding `(ChunkPos, Chunk)`.
    ///
    /// The iterator is lazy: each chunk is decompressed and parsed only when the
    /// iterator reaches it. A malformed chunk yields an `Err`; the caller
    /// decides whether to stop or skip it.
    #[must_use]
    pub fn iter_chunks(&self) -> RegionChunkIter<'_> {
        RegionChunkIter {
            region: self,
            next: 0,
        }
    }

    /// Resolves the chunk at region-local `(x, z)` to its `(byte offset, payload
    /// length)` within the file, or `None` if the chunk is absent.
    fn locate(&self, local_x: u8, local_z: u8) -> Result<Option<(usize, usize)>, AnvilError> {
        check_local(local_x, local_z)?;
        let coord = (local_x, local_z);
        let entry_index =
            (usize::from(local_x) + usize::from(local_z) * usize::from(REGION_EDGE)) * 4;

        // The location table is the first 4096 bytes; the header-size check in
        // the constructor guarantees this read is in bounds.
        let entry = u32::from_be_bytes([
            self.bytes[entry_index],
            self.bytes[entry_index + 1],
            self.bytes[entry_index + 2],
            self.bytes[entry_index + 3],
        ]);
        if entry == 0 {
            return Ok(None);
        }

        let offset_sectors = (entry >> 8) as usize;
        let size_sectors = (entry & 0xFF) as usize;
        // A chunk can never start inside the header (sectors 0 and 1).
        if offset_sectors < 2 || size_sectors == 0 {
            return Err(AnvilError::ChunkOffsetOutOfRange { coord });
        }

        let offset = offset_sectors
            .checked_mul(SECTOR_BYTES)
            .ok_or(AnvilError::ChunkOffsetOutOfRange { coord })?;
        let sector_span = size_sectors
            .checked_mul(SECTOR_BYTES)
            .ok_or(AnvilError::ChunkOffsetOutOfRange { coord })?;
        let sector_end = offset
            .checked_add(sector_span)
            .ok_or(AnvilError::ChunkOffsetOutOfRange { coord })?;
        // Need at least the 4-byte length prefix, and the declared sectors must
        // fit inside the file.
        if sector_end > self.bytes.len() || offset + 4 > self.bytes.len() {
            return Err(AnvilError::ChunkOffsetOutOfRange { coord });
        }

        let payload_len = u32::from_be_bytes([
            self.bytes[offset],
            self.bytes[offset + 1],
            self.bytes[offset + 2],
            self.bytes[offset + 3],
        ]) as usize;
        // `payload_len` covers the scheme byte plus the compressed body, so it
        // must be at least 1 and fit within the file after the length prefix.
        let body_end = offset
            .checked_add(4)
            .and_then(|v| v.checked_add(payload_len))
            .ok_or(AnvilError::BadChunkLength {
                coord,
                len: payload_len,
            })?;
        if payload_len < 1 || body_end > self.bytes.len() {
            return Err(AnvilError::BadChunkLength {
                coord,
                len: payload_len,
            });
        }

        Ok(Some((offset, payload_len)))
    }
}

/// A lazy iterator over every present chunk in a [`Region`].
///
/// Yields `Result<(ChunkPos, Chunk), AnvilError>` in row-major (`z` outer, `x`
/// inner) order, skipping absent chunks.
pub struct RegionChunkIter<'a> {
    region: &'a Region,
    next: usize,
}

impl Iterator for RegionChunkIter<'_> {
    type Item = Result<(ChunkPos, Chunk), AnvilError>;

    fn next(&mut self) -> Option<Self::Item> {
        let total = usize::from(REGION_EDGE) * usize::from(REGION_EDGE);
        while self.next < total {
            let index = self.next;
            self.next += 1;
            let local_x = (index % usize::from(REGION_EDGE)) as u8;
            let local_z = (index / usize::from(REGION_EDGE)) as u8;
            match self.region.read_chunk(local_x, local_z) {
                Ok(Some(chunk)) => {
                    // `read_chunk` already validated the coordinate, so the pos
                    // lookup cannot fail here.
                    match self.region.chunk_pos(local_x, local_z) {
                        Ok(pos) => return Some(Ok((pos, chunk))),
                        Err(err) => return Some(Err(err)),
                    }
                }
                Ok(None) => {}
                Err(err) => return Some(Err(err)),
            }
        }
        None
    }
}

/// Validates that a region-local chunk coordinate lies in the `0..32` grid.
fn check_local(local_x: u8, local_z: u8) -> Result<(), AnvilError> {
    if local_x >= REGION_EDGE || local_z >= REGION_EDGE {
        return Err(AnvilError::ChunkCoordOutOfRange {
            x: local_x,
            z: local_z,
        });
    }
    Ok(())
}

/// Decompresses a chunk body under the given scheme, capping the output at
/// `max_bytes` to defeat a decompression bomb.
fn decompress(
    compression: Compression,
    compressed: &[u8],
    coord: ChunkCoord,
    max_bytes: usize,
) -> Result<Vec<u8>, AnvilError> {
    match compression {
        // Already plain bytes; still enforce the cap.
        Compression::None => {
            if compressed.len() > max_bytes {
                return Err(AnvilError::ChunkTooLarge {
                    coord,
                    max: max_bytes,
                });
            }
            Ok(compressed.to_vec())
        }
        Compression::Zlib => {
            read_capped(flate2::read::ZlibDecoder::new(compressed), coord, max_bytes)
        }
        Compression::Gzip => {
            // MultiGzDecoder handles the (rare) concatenated-member case too.
            read_capped(
                flate2::read::MultiGzDecoder::new(compressed),
                coord,
                max_bytes,
            )
        }
    }
}

/// Reads a decoder to end, refusing to buffer more than `max_bytes`.
///
/// Reads at most `max_bytes + 1` bytes so an output of exactly `max_bytes` is
/// accepted while anything larger is rejected without ever allocating the full
/// (potentially unbounded) decompressed stream.
fn read_capped(
    reader: impl Read,
    coord: ChunkCoord,
    max_bytes: usize,
) -> Result<Vec<u8>, AnvilError> {
    let mut out = Vec::new();
    // `take` bounds the total bytes pulled from the decoder, so `read_to_end`
    // can never grow `out` past `max_bytes + 1`.
    let limit = (max_bytes as u64).saturating_add(1);
    reader
        .take(limit)
        .read_to_end(&mut out)
        .map_err(|source| AnvilError::Decompress { coord, source })?;
    if out.len() > max_bytes {
        return Err(AnvilError::ChunkTooLarge {
            coord,
            max: max_bytes,
        });
    }
    Ok(out)
}

/// Derives a [`RegionPos`] from a region file path named `r.<x>.<z>.mca`.
///
/// The coordinates are signed and may be negative (e.g. `r.-1.0.mca`).
pub fn region_pos_from_path(path: &Path) -> Result<RegionPos, AnvilError> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| AnvilError::BadRegionFileName(path.to_string_lossy().into_owned()))?;
    region_pos_from_file_name(name).ok_or_else(|| AnvilError::BadRegionFileName(name.to_owned()))
}

/// Parses the `r.<x>.<z>.mca` file-name form into a [`RegionPos`], or `None` if
/// the name does not match.
fn region_pos_from_file_name(name: &str) -> Option<RegionPos> {
    let stem = name.strip_suffix(".mca")?;
    let rest = stem.strip_prefix("r.")?;
    let (x, z) = rest.split_once('.')?;
    Some(RegionPos::new(x.parse().ok()?, z.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_file_name_parses_signed_coords() {
        assert_eq!(
            region_pos_from_file_name("r.0.0.mca"),
            Some(RegionPos::new(0, 0))
        );
        assert_eq!(
            region_pos_from_file_name("r.-1.2.mca"),
            Some(RegionPos::new(-1, 2))
        );
        assert_eq!(
            region_pos_from_file_name("r.3.-4.mca"),
            Some(RegionPos::new(3, -4))
        );
    }

    #[test]
    fn region_file_name_rejects_malformed() {
        assert_eq!(region_pos_from_file_name("r.0.mca"), None);
        assert_eq!(region_pos_from_file_name("x.0.0.mca"), None);
        assert_eq!(region_pos_from_file_name("r.a.0.mca"), None);
        assert_eq!(region_pos_from_file_name("r.0.0.dat"), None);
        assert_eq!(region_pos_from_file_name(""), None);
    }

    #[test]
    fn compression_scheme_mapping() {
        let coord = (0, 0);
        assert_eq!(
            Compression::from_scheme(1, coord).unwrap(),
            Compression::Gzip
        );
        assert_eq!(
            Compression::from_scheme(2, coord).unwrap(),
            Compression::Zlib
        );
        assert_eq!(
            Compression::from_scheme(3, coord).unwrap(),
            Compression::None
        );
        assert!(matches!(
            Compression::from_scheme(4, coord),
            Err(AnvilError::UnsupportedCompression { scheme: 4, .. })
        ));
        assert!(matches!(
            Compression::from_scheme(0x82, coord),
            Err(AnvilError::ExternalChunk { .. })
        ));
    }
}
