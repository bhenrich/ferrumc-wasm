//! The chunk-section network encoder: turns a [`Chunk`] into the wire bytes a
//! 1.21.8 client expects inside `Chunk Data and Update Light`.
//!
//! This module owns the three pieces of that packet that are genuinely derived
//! from chunk contents:
//!
//! - [`encode_chunk_section_data`] — the opaque section-data blob: all
//!   [`SECTION_COUNT`] sections bottom-to-top, each a non-air count plus a
//!   block-states and a biomes [`PalettedContainer`].
//! - [`pack_motion_blocking_heightmap`] — the `MOTION_BLOCKING` heightmap packed
//!   into the 9-bits-per-entry long array the protocol carries.
//! - [`ChunkLightData`] — the light masks and full-bright sky-light arrays.
//!
//! The packet framing (length prefixes, NBT, block entities) belongs to the
//! protocol layer; this module produces only the chunk-derived payloads as plain
//! `Vec`s so the app can wrap them in the generated packet types.

use ferrumc_registry::dimension;

use crate::chunk::{Chunk, SECTION_COUNT};
use crate::error::WorldError;
use crate::heightmap::HeightmapKind;
use crate::packed_array::PackedArray;
use crate::paletted_container::write_single_value_container;

/// Bits-per-entry the *direct* block-states representation uses on the wire.
///
/// Vanilla 1.21.8 sizes the direct palette to `ceil(log2(global block-state
/// count))`, which is 15 bits. A flat world never reaches the direct
/// representation (its sections are single-valued air or small indirect
/// palettes), so this only matters for correctness if a future generator does.
const DIRECT_WIRE_BITS_BLOCK: u8 = 15;

/// Biome registry id written for every section's single-valued biome container.
///
/// The crate has no biome storage yet, so every section reports `plains`, which
/// is registry id `0` (assigned by registry send order in the configuration
/// phase: `worldgen/biome:[plains]` is the first and only biome).
const PLAINS_BIOME_ID: u32 = 0;

/// Lowest buildable world `y` (`dimension::MIN_Y`, `-64`).
const MIN_Y: i32 = dimension::MIN_Y;

/// Bits-per-entry for the `MOTION_BLOCKING` heightmap.
///
/// Each entry is `(highest_y + 1) - MIN_Y`, in `0..=HEIGHT` (`0..=384`).
/// `ceil(log2(385)) = 9` bits per entry.
const HEIGHTMAP_BITS: u8 = 9;

/// Number of `(x, z)` columns in a chunk (`16 * 16`).
const HEIGHTMAP_COLUMNS: usize = 16 * 16;

/// Number of light sections in an overworld column: the 24 world sections plus
/// one below the build floor and one above the build ceiling.
pub const LIGHT_SECTION_COUNT: usize = SECTION_COUNT + 2;

/// Bytes in one light section's data array: 4096 nibbles packed two-per-byte.
pub const LIGHT_SECTION_BYTES: usize = 2048;

/// A `BitSet` (as a single 64-bit word) with the low [`LIGHT_SECTION_COUNT`]
/// bits set, marking every light section present. Equals `0x03FF_FFFF`.
const ALL_LIGHT_SECTIONS_MASK: i64 = (1i64 << LIGHT_SECTION_COUNT) - 1;

/// Encodes a chunk's section-data blob: the opaque payload carried verbatim in
/// `Chunk Data and Update Light`.
///
/// The blob is all [`SECTION_COUNT`] (24) sections bottom-to-top, each laid out
/// as:
///
/// 1. `block_count`: a big-endian `i16`, the section's non-air block count.
/// 2. `block_states`: a [`PalettedContainer`](crate::PalettedContainer) of block
///    states (see [`PalettedContainer::encode_network`](crate::PalettedContainer::encode_network)).
/// 3. `biomes`: a [`PalettedContainer`](crate::PalettedContainer) of biomes,
///    here always a single-valued `plains` container (`bits_per_entry = 0`, one
///    `VarInt` value, then a `VarInt(0)` empty long-array count) because the
///    crate has no biome storage yet.
///
/// # Errors
///
/// Propagates [`WorldError::ValueTooWide`] from
/// [`PalettedContainer::encode_network`](crate::PalettedContainer::encode_network)
/// if a direct section holds a block id wider than the 15-bit wire width. Genuine
/// block-state ids fit, so this does not occur for real chunk data.
pub fn encode_chunk_section_data(chunk: &Chunk) -> Result<Vec<u8>, WorldError> {
    // A flat chunk is ~16 KiB (8 indirect sections of 256 longs); reserve up
    // front to avoid repeated growth while encoding.
    let mut out = Vec::with_capacity(SECTION_COUNT * 2_100);
    for section in chunk.sections() {
        // `non_air_count` is at most the section volume (4096), well within an
        // `i16`; clamp defensively to stay panic-free.
        let count = i16::try_from(section.non_air_count()).unwrap_or(i16::MAX);
        out.extend_from_slice(&count.to_be_bytes());

        section
            .block_states()
            .encode_network(&mut out, DIRECT_WIRE_BITS_BLOCK)?;

        // Biomes: a single-valued plains container for the whole section. Its
        // (empty) long array still carries the mandatory VarInt(0) count; the
        // shared single-value encoder writes bpe, the value, and that count so
        // this matches the block-states container's single-value form exactly.
        write_single_value_container(&mut out, PLAINS_BIOME_ID);
    }
    Ok(out)
}

/// Packs a chunk's `MOTION_BLOCKING` heightmap into the protocol's long array.
///
/// Returns 37 longs: 256 columns at 9 bits per entry, packed non-spanning
/// (7 entries per long, `ceil(256 / 7) = 37`). Each column's value
/// is `(highest_y + 1) - MIN_Y` — the number of layers from the world floor up to
/// and including the highest motion-blocking block — or `0` for an all-air
/// column. A flat surface at `y = 63` packs to `(63 + 1) - (-64) = 128`.
///
/// The result is the bare long array; the caller pairs it with the heightmap
/// type id (`4` for `MOTION_BLOCKING`) and the protocol's length prefix.
///
/// # Errors
///
/// Propagates [`WorldError`] if the packed array cannot be built, which only
/// happens for a height outside the buildable range and so never for a chunk
/// produced by this crate.
pub fn pack_motion_blocking_heightmap(chunk: &Chunk) -> Result<Vec<i64>, WorldError> {
    let heightmap = chunk.heightmap(HeightmapKind::MotionBlocking);
    let mut packed = PackedArray::new(HEIGHTMAP_BITS, HEIGHTMAP_COLUMNS)?;
    for z in 0..16u8 {
        for x in 0..16u8 {
            // The protocol indexes heightmap columns as `z * 16 + x`.
            let index = usize::from(z) * 16 + usize::from(x);
            let value = match heightmap.height(x, z) {
                Some(y) => {
                    let layers = i64::from(y) - i64::from(MIN_Y) + 1;
                    // `layers` is positive for any in-range block; the fallback
                    // keeps this total without an `unwrap`.
                    u64::try_from(layers).unwrap_or(0)
                }
                None => 0,
            };
            packed.set(index, value)?;
        }
    }
    Ok(packed
        .words()
        .iter()
        .map(|&word| i64::from_ne_bytes(word.to_ne_bytes()))
        .collect())
}

/// The light masks and per-section light arrays for one chunk column.
///
/// These populate the `Update Light` portion of `Chunk Data and Update Light`.
/// Lighting is not computed yet, so [`ChunkLightData::full_bright`] is the only
/// constructor: it floods the column with full sky light and no block light,
/// which is what a flat overworld looks like to a client.
///
/// The masks are `BitSet`s over [`LIGHT_SECTION_COUNT`] (26) sections, indexed
/// from one section below the build floor up to one above the build ceiling. A
/// section's data is present iff its bit is set in the data mask and clear in the
/// matching empty mask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkLightData {
    /// Sections that have sky-light data (one `i64` `BitSet` word).
    sky_light_mask: Vec<i64>,
    /// Sections that have block-light data.
    block_light_mask: Vec<i64>,
    /// Sections whose sky light is all zero (none, here).
    empty_sky_light_mask: Vec<i64>,
    /// Sections whose block light is all zero (every section, here).
    empty_block_light_mask: Vec<i64>,
    /// One 2048-byte array per set [`Self::sky_light_mask`] bit.
    sky_light: Vec<Vec<u8>>,
    /// One 2048-byte array per set [`Self::block_light_mask`] bit (none, here).
    block_light: Vec<Vec<u8>>,
}

impl ChunkLightData {
    /// Builds full-bright light: every section gets maximum sky light and no
    /// block light.
    ///
    /// Concretely the sky-light mask marks all [`LIGHT_SECTION_COUNT`] sections
    /// present with one `0x03FF_FFFF` word and supplies that many `0xFF`-filled
    /// 2048-byte arrays; the empty-block-light mask marks all sections as
    /// zero-block-light; and the block-light data and the two remaining masks are
    /// empty.
    #[must_use]
    pub fn full_bright() -> Self {
        Self {
            sky_light_mask: vec![ALL_LIGHT_SECTIONS_MASK],
            block_light_mask: Vec::new(),
            empty_sky_light_mask: Vec::new(),
            empty_block_light_mask: vec![ALL_LIGHT_SECTIONS_MASK],
            sky_light: vec![vec![0xFF; LIGHT_SECTION_BYTES]; LIGHT_SECTION_COUNT],
            block_light: Vec::new(),
        }
    }

    /// The sky-light `BitSet`: sections that carry sky-light data.
    #[must_use]
    pub fn sky_light_mask(&self) -> &[i64] {
        &self.sky_light_mask
    }

    /// The block-light `BitSet`: sections that carry block-light data.
    #[must_use]
    pub fn block_light_mask(&self) -> &[i64] {
        &self.block_light_mask
    }

    /// The empty-sky-light `BitSet`: sections whose sky light is all zero.
    #[must_use]
    pub fn empty_sky_light_mask(&self) -> &[i64] {
        &self.empty_sky_light_mask
    }

    /// The empty-block-light `BitSet`: sections whose block light is all zero.
    #[must_use]
    pub fn empty_block_light_mask(&self) -> &[i64] {
        &self.empty_block_light_mask
    }

    /// The sky-light arrays, one 2048-byte array per set [`Self::sky_light_mask`]
    /// bit, in ascending section order.
    #[must_use]
    pub fn sky_light(&self) -> &[Vec<u8>] {
        &self.sky_light
    }

    /// The block-light arrays, one 2048-byte array per set
    /// [`Self::block_light_mask`] bit, in ascending section order.
    #[must_use]
    pub fn block_light(&self) -> &[Vec<u8>] {
        &self.block_light
    }
}

#[cfg(test)]
mod tests {
    use super::{
        encode_chunk_section_data, pack_motion_blocking_heightmap, ChunkLightData,
        LIGHT_SECTION_BYTES, LIGHT_SECTION_COUNT, MIN_Y,
    };
    use crate::block_state::BlockStateId;
    use crate::chunk::SECTION_COUNT;
    use crate::packed_array::PackedArray;
    use crate::{Chunk, FlatWorldGenerator};
    use ferrumc_math::{BlockPos, ChunkPos};

    /// Reads a `VarInt` from `bytes` at `pos`, advancing `pos`.
    fn read_var_u32(bytes: &[u8], pos: &mut usize) -> u32 {
        let mut value = 0u32;
        let mut shift = 0u32;
        loop {
            let byte = bytes[*pos];
            *pos += 1;
            value |= u32::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
            shift += 7;
        }
    }

    /// A decoded section header from the blob: its non-air count and the
    /// block-states bits-per-entry.
    struct SectionHeader {
        count: i16,
        block_bpe: u8,
    }

    /// Walks one `PalettedContainer` at `pos`, returning its bits-per-entry and
    /// advancing `pos` past the palette and long array.
    fn skip_container(bytes: &[u8], pos: &mut usize) -> u8 {
        let bpe = bytes[*pos];
        *pos += 1;
        if bpe == 0 {
            // Single value: one varint palette value, then the mandatory
            // VarInt(0) empty long-array count.
            let _ = read_var_u32(bytes, pos);
            let long_count = read_var_u32(bytes, pos);
            assert_eq!(long_count, 0, "single-valued container carries no longs");
            return bpe;
        }
        if bpe <= 8 {
            // Indirect: palette length, that many ids.
            let len = read_var_u32(bytes, pos);
            for _ in 0..len {
                let _ = read_var_u32(bytes, pos);
            }
        }
        // Indirect and direct both prefix the non-spanning long array with its
        // word count, which must equal ceil(4096 / (64 / bpe)).
        let long_count = read_var_u32(bytes, pos) as usize;
        let values_per_word = 64 / usize::from(bpe);
        let expected = 4096usize.div_ceil(values_per_word);
        assert_eq!(long_count, expected, "long count matches the packed layout");
        *pos += long_count * 8;
        bpe
    }

    /// Fully parses the section-data blob into per-section headers, asserting it
    /// is consumed exactly (no trailing bytes).
    fn parse_blob(bytes: &[u8]) -> Vec<SectionHeader> {
        let mut pos = 0usize;
        let mut headers = Vec::new();
        for _ in 0..SECTION_COUNT {
            let count = i16::from_be_bytes([bytes[pos], bytes[pos + 1]]);
            pos += 2;
            let block_bpe = skip_container(bytes, &mut pos);
            // Biome container: single-valued plains.
            let biome_bpe = skip_container(bytes, &mut pos);
            assert_eq!(biome_bpe, 0, "biome container must be single-valued");
            headers.push(SectionHeader { count, block_bpe });
        }
        assert_eq!(pos, bytes.len(), "blob has trailing bytes");
        headers
    }

    #[test]
    fn empty_chunk_blob_is_all_single_air() {
        let chunk = Chunk::new(ChunkPos::ORIGIN);
        let blob = encode_chunk_section_data(&chunk).unwrap();
        // 24 sections, each: i16 count (2) + block single-air container (bpe 0,
        // value 0, long-count 0 = 3) + biome single-plains container (3) = 8.
        assert_eq!(blob.len(), SECTION_COUNT * 8);
        for section in blob.chunks_exact(8) {
            assert_eq!(section, &[0, 0, 0, 0, 0, 0, 0, 0]);
        }
        let headers = parse_blob(&blob);
        assert_eq!(headers.len(), SECTION_COUNT);
        for header in &headers {
            assert_eq!(header.count, 0);
            assert_eq!(header.block_bpe, 0);
        }
    }

    #[test]
    fn flat_chunk_blob_has_solid_and_air_sections() {
        let chunk = FlatWorldGenerator::new().generate(ChunkPos::ORIGIN);
        let blob = encode_chunk_section_data(&chunk).unwrap();
        let headers = parse_blob(&blob);
        assert_eq!(headers.len(), SECTION_COUNT);

        for (index, header) in headers.iter().enumerate() {
            if index < 8 {
                // Sections 0..=7 (y -64..=63) are fully solid: every block is
                // non-air and the small palette clamps to 4 bits per entry.
                assert_eq!(header.count, 4096, "section {index} count");
                assert_eq!(header.block_bpe, 4, "section {index} bpe");
            } else {
                // Sections 8..=23 are entirely air: single-valued, no longs.
                assert_eq!(header.count, 0, "section {index} count");
                assert_eq!(header.block_bpe, 0, "section {index} bpe");
            }
        }
    }

    #[test]
    fn flat_heightmap_is_37_longs_of_128() {
        let chunk = FlatWorldGenerator::new().generate(ChunkPos::ORIGIN);
        let longs = pack_motion_blocking_heightmap(&chunk).unwrap();
        // 256 columns, 9 bits each, 7 per long -> 37 longs.
        assert_eq!(longs.len(), 37);

        // Every column is grass at y = 63 -> (63 + 1) - (-64) = 128.
        let mut expected = PackedArray::new(9, 256).unwrap();
        for index in 0..256 {
            expected.set(index, 128).unwrap();
        }
        let expected_longs: Vec<i64> = expected
            .words()
            .iter()
            .map(|&word| i64::from_ne_bytes(word.to_ne_bytes()))
            .collect();
        assert_eq!(longs, expected_longs);
    }

    #[test]
    fn empty_chunk_heightmap_is_all_zero() {
        let chunk = Chunk::new(ChunkPos::ORIGIN);
        let longs = pack_motion_blocking_heightmap(&chunk).unwrap();
        assert_eq!(longs.len(), 37);
        assert!(longs.iter().all(|&l| l == 0));
    }

    #[test]
    fn heightmap_value_follows_the_formula() {
        let mut chunk = Chunk::new(ChunkPos::ORIGIN);
        // A single block at y = 100 in column (0, 0).
        chunk
            .set_block(BlockPos::new(0, 100, 0), BlockStateId::new(1))
            .unwrap();
        let longs = pack_motion_blocking_heightmap(&chunk).unwrap();

        // Reconstruct the packed entries to read column (0, 0) at index 0.
        let mut packed = PackedArray::new(9, 256).unwrap();
        for (index, &long) in longs.iter().enumerate() {
            // Re-pack the words; index by the same non-spanning layout.
            let word = u64::from_ne_bytes(long.to_ne_bytes());
            // Write the word back directly by decoding its 7 entries.
            for slot in 0..7 {
                let entry = (word >> (slot * 9)) & 0x1FF;
                let column = index * 7 + slot;
                if column < 256 {
                    packed.set(column, entry).unwrap();
                }
            }
        }
        // value = (100 + 1) - (-64) = 165.
        assert_eq!(packed.get(0), Some(165));
        let expected = u64::try_from(100 - i64::from(MIN_Y) + 1).unwrap();
        assert_eq!(expected, 165);
        // Every other column is air -> 0.
        assert_eq!(packed.get(1), Some(0));
    }

    #[test]
    fn full_bright_light_masks_and_arrays() {
        let light = ChunkLightData::full_bright();

        assert_eq!(light.sky_light_mask(), &[0x03FF_FFFF_i64]);
        assert!(light.block_light_mask().is_empty());
        assert!(light.empty_sky_light_mask().is_empty());
        assert_eq!(light.empty_block_light_mask(), &[0x03FF_FFFF_i64]);

        // One 2048-byte 0xFF array per light section.
        assert_eq!(light.sky_light().len(), LIGHT_SECTION_COUNT);
        assert_eq!(LIGHT_SECTION_COUNT, 26);
        for array in light.sky_light() {
            assert_eq!(array.len(), LIGHT_SECTION_BYTES);
            assert!(array.iter().all(|&byte| byte == 0xFF));
        }
        assert!(light.block_light().is_empty());

        // The mask word has exactly the low 26 bits set.
        assert_eq!(0x03FF_FFFF_i64, (1i64 << 26) - 1);
    }
}
