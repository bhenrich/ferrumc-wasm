//! Integration tests for the Anvil importer.
//!
//! Two complementary fixtures are exercised:
//!
//! - `tests/fixtures/r.0.0.mca` — a real single vanilla chunk (chunk `(0, 0)`
//!   extracted from a genuine Minecraft world), proving the importer reads what
//!   Minecraft actually wrote.
//! - in-memory synthetic regions built here with known blocks at known
//!   positions, proving the palette/section unpack and registry mapping land
//!   each block exactly where it belongs.
//!
//! Malformed inputs (truncated header, bad compression byte, external chunk,
//! corrupt NBT, wrong-length section data, out-of-range palette index, oversized
//! decompression) are all asserted to produce a classified error and never
//! panic.

use std::io::Write;

use ferrumc_anvil::{import_region_file, AnvilError, AnvilLimits, Region};
use ferrumc_math::{BlockPos, ChunkPos, RegionPos};
use ferrumc_nbt::{NbtCompound, NbtLimits, NbtTag};
use ferrumc_world::BlockStateId;

/// Path to a committed fixture under `tests/fixtures/`.
fn fixture(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// --- Real vanilla data --------------------------------------------------------

#[test]
fn imports_real_vanilla_chunk_with_bedrock_floor() {
    let region = Region::open(fixture("r.0.0.mca")).expect("region opens");
    assert_eq!(region.region_pos(), RegionPos::new(0, 0));
    assert!(region.contains_chunk(0, 0));

    let chunk = region
        .read_chunk(0, 0)
        .expect("chunk reads")
        .expect("chunk is present");
    assert_eq!(chunk.pos(), ChunkPos::new(0, 0));

    // In a 1.18+ overworld the entire y == -64 layer is bedrock.
    let bedrock = BlockStateId::from_name("bedrock").expect("bedrock is a known block");
    for z in 0..16 {
        for x in 0..16 {
            assert_eq!(
                chunk.get_block(BlockPos::new(x, -64, z)),
                Some(bedrock),
                "expected bedrock at ({x}, -64, {z})"
            );
        }
    }

    // And there is substantial non-air terrain in the lower world.
    let mut non_air = 0usize;
    for y in -64..0 {
        for z in 0..16 {
            for x in 0..16 {
                if chunk
                    .get_block(BlockPos::new(x, y, z))
                    .is_some_and(|b| !b.is_air())
                {
                    non_air += 1;
                }
            }
        }
    }
    assert!(
        non_air > 1000,
        "expected real terrain, found {non_air} blocks"
    );
}

#[test]
fn import_region_file_collects_present_chunks() {
    let chunks = import_region_file(fixture("r.0.0.mca")).expect("import succeeds");
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].0, ChunkPos::new(0, 0));
}

// --- Synthetic regions with known blocks --------------------------------------

/// Builds a single section compound at vanilla `Y` with the given palette and
/// packed data. Each palette entry is `(name, properties)`.
fn section(y: i8, palette: &[(&str, &[(&str, &str)])], data: &[i64]) -> NbtTag {
    let mut palette_list = Vec::new();
    for (name, props) in palette {
        let mut entry = NbtCompound::new();
        entry.push("Name", NbtTag::String((*name).to_owned()));
        if !props.is_empty() {
            let mut p = NbtCompound::new();
            for (k, v) in *props {
                p.push(*k, NbtTag::String((*v).to_owned()));
            }
            entry.push("Properties", NbtTag::Compound(p));
        }
        palette_list.push(NbtTag::Compound(entry));
    }

    let mut block_states = NbtCompound::new();
    block_states.push("palette", NbtTag::List(palette_list));
    if !data.is_empty() {
        block_states.push("data", NbtTag::LongArray(data.to_vec()));
    }

    let mut sec = NbtCompound::new();
    sec.push("Y", NbtTag::Byte(y));
    sec.push("block_states", NbtTag::Compound(block_states));
    NbtTag::Compound(sec)
}

/// Wraps section tags into a chunk root compound.
fn chunk_root(sections: Vec<NbtTag>) -> NbtTag {
    let mut root = NbtCompound::new();
    root.push("sections", NbtTag::List(sections));
    NbtTag::Compound(root)
}

/// Serializes a chunk root to file-form NBT bytes.
fn nbt_bytes(root: &NbtTag) -> Vec<u8> {
    ferrumc_nbt::write_named_root("", root, &NbtLimits::default()).expect("nbt writes")
}

/// zlib-compresses bytes the way a vanilla writer would for scheme 2.
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).expect("zlib write");
    enc.finish().expect("zlib finish")
}

/// Assembles a one-chunk region file placing chunk `(0, 0)` with the given
/// compression `scheme` and already-encoded `body`.
fn assemble_region(scheme: u8, body: &[u8]) -> Vec<u8> {
    const SECTOR: usize = 4096;
    let payload_len = 1 + body.len(); // scheme byte + body
    let total = 4 + payload_len; // length prefix + payload
    let sectors = total.div_ceil(SECTOR);
    assert!(sectors <= 0xFF, "fixture too large for one location entry");

    let mut bytes = vec![0u8; 2 * SECTOR]; // location + timestamp tables
    let entry = (2u32 << 8) | sectors as u32; // chunk starts at sector 2
    bytes[0..4].copy_from_slice(&entry.to_be_bytes());

    let mut block = Vec::new();
    block.extend_from_slice(&(payload_len as u32).to_be_bytes());
    block.push(scheme);
    block.extend_from_slice(body);
    block.resize(sectors * SECTOR, 0);
    bytes.extend_from_slice(&block);
    bytes
}

/// Convenience: a zlib-compressed, scheme-2 region for the given chunk root.
fn region_for(root: &NbtTag) -> Region {
    let bytes = assemble_region(2, &zlib(&nbt_bytes(root)));
    Region::from_bytes(RegionPos::new(0, 0), bytes).expect("region builds")
}

#[test]
fn maps_palette_blocks_and_properties_to_exact_positions() {
    // One section at the bottom of the world (Y = -4 -> base y = -64).
    // Palette: air, stone, dirt, oak_log(axis=x). Bits-per-entry = 4, so 16
    // entries pack into each of the 256 longs; only the first long is non-zero.
    let palette: &[(&str, &[(&str, &str)])] = &[
        ("minecraft:air", &[]),
        ("minecraft:stone", &[]),
        ("minecraft:dirt", &[]),
        ("minecraft:oak_log", &[("axis", "x")]),
    ];
    // Flat index i = (y<<8)|(z<<4)|x. For y=z=0, i == x, packed 4 bits each:
    //   x=0 -> palette 1 (stone), x=1 -> 2 (dirt), x=2 -> 3 (oak_log).
    let mut data = vec![0i64; 256];
    data[0] = 1 | (2 << 4) | (3 << 8);

    let region = region_for(&chunk_root(vec![section(-4, palette, &data)]));
    let chunk = region.read_chunk(0, 0).expect("reads").expect("present");

    assert_eq!(
        chunk.get_block(BlockPos::new(0, -64, 0)),
        Some(BlockStateId::new(1)),
        "stone"
    );
    assert_eq!(
        chunk.get_block(BlockPos::new(1, -64, 0)),
        Some(BlockStateId::new(10)),
        "dirt"
    );
    assert_eq!(
        chunk.get_block(BlockPos::new(2, -64, 0)),
        Some(BlockStateId::new(136)),
        "oak_log axis=x"
    );
    assert_eq!(
        chunk.get_block(BlockPos::new(3, -64, 0)),
        Some(BlockStateId::AIR),
        "untouched block stays air"
    );
}

#[test]
fn single_entry_palette_without_data_fills_section() {
    // Palette of one (stone) and no `data` -> the whole section is stone.
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:stone", &[])];
    let region = region_for(&chunk_root(vec![section(-4, palette, &[])]));
    let chunk = region.read_chunk(0, 0).expect("reads").expect("present");

    for (x, y, z) in [(0, -64, 0), (15, -49, 15), (7, -55, 9)] {
        assert_eq!(
            chunk.get_block(BlockPos::new(x, y, z)),
            Some(BlockStateId::new(1)),
            "stone fill at ({x}, {y}, {z})"
        );
    }
}

#[test]
fn unknown_block_maps_to_air() {
    // A block the pinned registry does not know must degrade to air, not fail.
    let palette: &[(&str, &[(&str, &str)])] = &[
        ("minecraft:air", &[]),
        ("minecraft:totally_made_up_block", &[]),
    ];
    let mut data = vec![0i64; 256];
    data[0] = 1; // index 0 -> palette slot 1 (the unknown block)

    let region = region_for(&chunk_root(vec![section(-4, palette, &data)]));
    let chunk = region.read_chunk(0, 0).expect("reads").expect("present");
    assert_eq!(
        chunk.get_block(BlockPos::new(0, -64, 0)),
        Some(BlockStateId::AIR)
    );
}

#[test]
fn out_of_range_sections_are_skipped() {
    // Y = 20 (base y = 320) sits above the buildable top (319) and must be
    // skipped without error, leaving an all-air chunk.
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:stone", &[])];
    let region = region_for(&chunk_root(vec![section(20, palette, &[])]));
    let chunk = region.read_chunk(0, 0).expect("reads").expect("present");
    assert_eq!(
        chunk.get_block(BlockPos::new(0, 319, 0)),
        Some(BlockStateId::AIR)
    );
}

// --- Malformed input ----------------------------------------------------------

#[test]
fn truncated_header_is_rejected() {
    let err = Region::from_bytes(RegionPos::new(0, 0), vec![0u8; 100]).unwrap_err();
    assert!(matches!(err, AnvilError::HeaderTooSmall { len: 100, .. }));
}

#[test]
fn unsupported_compression_is_rejected() {
    // Scheme 4 (LZ4) is not supported.
    let bytes = assemble_region(4, &[0u8; 16]);
    let region = Region::from_bytes(RegionPos::new(0, 0), bytes).expect("builds");
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::UnsupportedCompression { scheme: 4, .. }
    ));
}

#[test]
fn external_chunk_flag_is_rejected() {
    // High bit set on the compression byte signals an external .mcc chunk.
    let bytes = assemble_region(0x80 | 2, &[0u8; 16]);
    let region = Region::from_bytes(RegionPos::new(0, 0), bytes).expect("builds");
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(err, AnvilError::ExternalChunk { .. }));
}

#[test]
fn corrupt_nbt_is_rejected() {
    // Scheme 3 (uncompressed) with a body that is not valid NBT.
    let bytes = assemble_region(3, b"this is definitely not nbt");
    let region = Region::from_bytes(RegionPos::new(0, 0), bytes).expect("builds");
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(err, AnvilError::Nbt { .. }), "got {err:?}");
}

#[test]
fn wrong_length_block_state_data_is_rejected() {
    // palette len 2 -> 4 bits -> 256 longs expected, but only 10 supplied.
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:air", &[]), ("minecraft:stone", &[])];
    let region = region_for(&chunk_root(vec![section(-4, palette, &[0i64; 10])]));
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(
        matches!(
            err,
            AnvilError::BadBlockStateData {
                got: 10,
                expected: 256,
                ..
            }
        ),
        "got {err:?}"
    );
}

#[test]
fn out_of_range_palette_index_is_rejected() {
    // palette has 2 entries (valid indices 0, 1) but data references index 5.
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:air", &[]), ("minecraft:stone", &[])];
    let mut data = vec![0i64; 256];
    data[0] = 5;
    let region = region_for(&chunk_root(vec![section(-4, palette, &data)]));
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(
        matches!(
            err,
            AnvilError::PaletteIndexOutOfRange {
                index: 5,
                len: 2,
                ..
            }
        ),
        "got {err:?}"
    );
}

#[test]
fn decompression_bomb_is_capped() {
    // A valid chunk, but opened with a tiny decompressed-size cap.
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:stone", &[])];
    let bytes = assemble_region(
        2,
        &zlib(&nbt_bytes(&chunk_root(vec![section(-4, palette, &[])]))),
    );
    let limits = AnvilLimits::default().with_max_chunk_bytes(8);
    let region =
        Region::from_bytes_with_limits(RegionPos::new(0, 0), bytes, limits).expect("builds");
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(
        matches!(err, AnvilError::ChunkTooLarge { max: 8, .. }),
        "got {err:?}"
    );
}

#[test]
fn missing_chunk_is_none() {
    // An empty (but header-valid) region has no chunks.
    let region = Region::from_bytes(RegionPos::new(0, 0), vec![0u8; 8192]).expect("builds");
    assert!(!region.contains_chunk(0, 0));
    assert_eq!(region.read_chunk(0, 0).expect("reads"), None);
    assert_eq!(region.iter_chunks().count(), 0);
}

#[test]
fn out_of_range_local_coordinate_is_rejected() {
    let region = Region::from_bytes(RegionPos::new(0, 0), vec![0u8; 8192]).expect("builds");
    let err = region.read_chunk(32, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::ChunkCoordOutOfRange { x: 32, z: 0 }
    ));
}
