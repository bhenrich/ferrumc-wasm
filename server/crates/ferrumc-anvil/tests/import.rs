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
use ferrumc_registry::DATA_VERSION;
use ferrumc_world::{BlockEntity, BlockStateId, SignKind, MAX_BLOCK_ENTITIES, MAX_SIGN_LINE_BYTES};

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

    // This genuine fixture carries two brushable-block entities, which the
    // current world model does not represent. Unknown ids stay version-tolerant
    // and are skipped without suppressing the chunk's supported terrain.
    assert_eq!(chunk.block_entity_count(), 0);
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
    chunk_root_with_block_entities(sections, Vec::new())
}

/// Wraps section and block-entity tags into a chunk root compound.
fn chunk_root_with_block_entities(sections: Vec<NbtTag>, block_entities: Vec<NbtTag>) -> NbtTag {
    chunk_root_for_data_version(DATA_VERSION, sections, block_entities)
}

/// Wraps chunk tags with an explicit root world-data version.
fn chunk_root_for_data_version(
    data_version: i32,
    sections: Vec<NbtTag>,
    block_entities: Vec<NbtTag>,
) -> NbtTag {
    let mut root = NbtCompound::new();
    root.push("DataVersion", NbtTag::Int(data_version));
    root.push("sections", NbtTag::List(sections));
    root.push("block_entities", NbtTag::List(block_entities));
    NbtTag::Compound(root)
}

/// Builds the common id/position fields of a block-entity compound.
fn block_entity_header(id: &str, pos: BlockPos) -> NbtCompound {
    let mut entity = NbtCompound::new();
    entity.push("id", NbtTag::String(id.to_owned()));
    entity.push("x", NbtTag::Int(pos.x()));
    entity.push("y", NbtTag::Int(pos.y()));
    entity.push("z", NbtTag::Int(pos.z()));
    entity
}

/// Builds one modern sign-face compound.
fn sign_face(lines: [&str; 4]) -> NbtTag {
    sign_face_messages(
        lines
            .into_iter()
            .map(|line| NbtTag::String(line.to_owned()))
            .collect(),
    )
}

/// Builds one modern sign-face compound with explicit message tags.
fn sign_face_messages(messages: Vec<NbtTag>) -> NbtTag {
    let mut face = NbtCompound::new();
    face.push("color", NbtTag::String("black".to_owned()));
    face.push("has_glowing_text", NbtTag::Byte(0));
    face.push("messages", NbtTag::List(messages));
    NbtTag::Compound(face)
}

/// Builds a sign face whose four lines use structured compound components.
fn structured_sign_face(lines: [&str; 4]) -> NbtTag {
    sign_face_messages(
        lines
            .into_iter()
            .map(|line| {
                let mut component = NbtCompound::new();
                component.push("text", NbtTag::String(line.to_owned()));
                NbtTag::Compound(component)
            })
            .collect(),
    )
}

/// Builds a modern sign block entity at `pos`.
fn sign_block_entity(pos: BlockPos) -> NbtTag {
    sign_block_entity_with_front(pos, sign_face(["Imported", "", "Anvil sign", ""]))
}

/// Builds a modern sign block entity with an explicit front-face tag.
fn sign_block_entity_with_front(pos: BlockPos, front: NbtTag) -> NbtTag {
    sign_block_entity_of_kind("minecraft:sign", pos, front)
}

/// Builds a modern sign-family block entity with an explicit front face.
fn sign_block_entity_of_kind(id: &str, pos: BlockPos, front: NbtTag) -> NbtTag {
    let mut entity = block_entity_header(id, pos);
    entity.push("is_waxed", NbtTag::Byte(0));
    entity.push("front_text", front);
    entity.push("back_text", sign_face([""; 4]));
    NbtTag::Compound(entity)
}

/// Builds an empty modern chest block entity at `pos`.
fn chest_block_entity(pos: BlockPos) -> NbtTag {
    let mut entity = block_entity_header("minecraft:chest", pos);
    entity.push("Items", NbtTag::List(Vec::new()));
    NbtTag::Compound(entity)
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

#[test]
fn imports_block_entities_from_non_flat_multi_section_chunk() {
    let chest_pos = BlockPos::new(1, -64, 0);
    let sign_pos = BlockPos::new(1, 64, 0);

    let lower_palette: &[(&str, &[(&str, &str)])] = &[
        ("minecraft:air", &[]),
        ("minecraft:stone", &[]),
        ("minecraft:chest", &[]),
    ];
    let mut lower_data = vec![0i64; 256];
    // At local y=z=0: x=0 is stone and x=1 is a chest.
    lower_data[0] = 1 | (2 << 4);

    let upper_palette: &[(&str, &[(&str, &str)])] = &[
        ("minecraft:air", &[]),
        ("minecraft:dirt", &[]),
        ("minecraft:oak_sign", &[]),
    ];
    let mut upper_data = vec![0i64; 256];
    // At local y=z=0: x=0 is dirt and x=1 is an oak sign.
    upper_data[0] = 1 | (2 << 4);

    let root = chunk_root_with_block_entities(
        vec![
            section(-4, lower_palette, &lower_data),
            section(4, upper_palette, &upper_data),
        ],
        vec![sign_block_entity(sign_pos), chest_block_entity(chest_pos)],
    );
    let region = region_for(&root);
    let chunk = region.read_chunk(0, 0).expect("reads").expect("present");

    // Both deliberately non-flat sections survive alongside their block entities.
    assert_eq!(
        chunk.get_block(BlockPos::new(0, -64, 0)),
        Some(BlockStateId::new(1))
    );
    assert_eq!(
        chunk.get_block(BlockPos::new(0, 64, 0)),
        Some(BlockStateId::new(10))
    );
    assert_eq!(
        chunk.block_entity_count(),
        2,
        "Anvil block entities must not be dropped"
    );

    let Some(BlockEntity::Sign(sign)) = chunk.block_entity(sign_pos) else {
        panic!("imported sign missing or has wrong kind");
    };
    assert_eq!(sign.kind(), SignKind::Sign);
    assert_eq!(sign.front().lines()[0], "Imported");
    assert_eq!(sign.front().lines()[2], "Anvil sign");
    assert!(sign.back().lines().iter().all(String::is_empty));

    let Some(BlockEntity::Chest(chest)) = chunk.block_entity(chest_pos) else {
        panic!("imported chest missing or has wrong kind");
    };
    assert_eq!(chest.slots().len(), 27);
    assert!(chest.slots().iter().all(|stack| stack.item().is_none()));
}

#[test]
fn imports_structured_hanging_sign_and_trapped_chest() {
    let sign_pos = BlockPos::new(0, 64, 0);
    let chest_pos = BlockPos::new(1, 64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[
        ("minecraft:air", &[]),
        ("minecraft:oak_hanging_sign", &[]),
        ("minecraft:trapped_chest", &[]),
    ];
    let mut data = vec![0i64; 256];
    data[0] = 1 | (2 << 4);
    let sign = sign_block_entity_of_kind(
        "minecraft:hanging_sign",
        sign_pos,
        structured_sign_face(["{literal", "structured", "", "backslash: \\"]),
    );
    let mut chest = block_entity_header("minecraft:trapped_chest", chest_pos);
    chest.push("Items", NbtTag::List(Vec::new()));
    let root = chunk_root_with_block_entities(
        vec![section(4, palette, &data)],
        vec![sign, NbtTag::Compound(chest)],
    );

    let chunk = region_for(&root)
        .read_chunk(0, 0)
        .expect("reads")
        .expect("present");
    let Some(BlockEntity::Sign(sign)) = chunk.block_entity(sign_pos) else {
        panic!("hanging sign missing");
    };
    assert_eq!(sign.kind(), SignKind::Hanging);
    assert_eq!(sign.front().lines()[0], "{literal");
    assert_eq!(sign.front().lines()[1], "structured");
    assert_eq!(sign.front().lines()[3], "backslash: \\");
    assert!(matches!(
        chunk.block_entity(chest_pos),
        Some(BlockEntity::Chest(_))
    ));
}

#[test]
fn chest_entity_id_must_match_the_chest_block_state() {
    let pos = BlockPos::new(0, 64, 0);
    for (state_name, entity_id) in [
        ("minecraft:chest", "minecraft:trapped_chest"),
        ("minecraft:trapped_chest", "minecraft:chest"),
    ] {
        let palette = [(state_name, &[][..])];
        let mut chest = block_entity_header(entity_id, pos);
        chest.push("Items", NbtTag::List(Vec::new()));
        let root = chunk_root_with_block_entities(
            vec![section(4, &palette, &[])],
            vec![NbtTag::Compound(chest)],
        );
        let err = region_for(&root).read_chunk(0, 0).unwrap_err();
        assert!(matches!(
            err,
            AnvilError::BlockEntityStateMismatch {
                id,
                pos: rejected,
                ..
            } if id == entity_id && rejected == pos
        ));
    }
}

// --- Malformed input ----------------------------------------------------------

#[test]
fn block_entities_field_with_wrong_type_is_rejected() {
    let mut root = NbtCompound::new();
    root.push("sections", NbtTag::List(Vec::new()));
    root.push("block_entities", NbtTag::Int(1));
    let region = region_for(&NbtTag::Compound(root));
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::WrongFieldType {
            field: "block_entities",
            ..
        }
    ));
}

#[test]
fn data_version_with_wrong_type_is_rejected() {
    let mut root = NbtCompound::new();
    root.push("DataVersion", NbtTag::String("1.21.8".to_owned()));
    root.push("sections", NbtTag::List(Vec::new()));
    let err = region_for(&NbtTag::Compound(root))
        .read_chunk(0, 0)
        .unwrap_err();
    assert!(matches!(
        err,
        AnvilError::WrongFieldType {
            field: "DataVersion",
            ..
        }
    ));
}

#[test]
fn non_compound_block_entity_is_rejected() {
    let root = chunk_root_with_block_entities(
        Vec::new(),
        vec![NbtTag::String("not a compound".to_owned())],
    );
    let region = region_for(&root);
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::WrongFieldType {
            field: "block_entities[]",
            ..
        }
    ));
}

#[test]
fn missing_and_wrong_type_entity_ids_are_rejected() {
    for entity in [NbtCompound::new(), {
        let mut entity = NbtCompound::new();
        entity.push("id", NbtTag::Int(7));
        entity
    }] {
        let root = chunk_root_with_block_entities(Vec::new(), vec![NbtTag::Compound(entity)]);
        let err = region_for(&root).read_chunk(0, 0).unwrap_err();
        assert!(matches!(
            err,
            AnvilError::MissingField {
                field: "block_entities[].id",
                ..
            } | AnvilError::WrongFieldType {
                field: "block_entities[].id",
                ..
            }
        ));
    }
}

#[test]
fn malformed_supported_entity_is_checked_even_without_sections() {
    let mut entity = NbtCompound::new();
    entity.push("id", NbtTag::String("minecraft:sign".to_owned()));
    entity.push("x", NbtTag::Int(0));
    entity.push("y", NbtTag::Int(64));
    // Deliberately no z: this also proves the old no-sections early return is gone.
    let mut root = NbtCompound::new();
    root.push(
        "block_entities",
        NbtTag::List(vec![NbtTag::Compound(entity)]),
    );
    let region = region_for(&NbtTag::Compound(root));
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::MissingField {
            field: "block_entities[].z",
            ..
        }
    ));
}

#[test]
fn supported_entity_coordinate_with_wrong_type_is_rejected() {
    let mut entity = NbtCompound::new();
    entity.push("id", NbtTag::String("minecraft:sign".to_owned()));
    entity.push("x", NbtTag::String("zero".to_owned()));
    entity.push("y", NbtTag::Int(64));
    entity.push("z", NbtTag::Int(0));
    let root = chunk_root_with_block_entities(Vec::new(), vec![NbtTag::Compound(entity)]);
    let err = region_for(&root).read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::WrongFieldType {
            field: "block_entities[].x",
            ..
        }
    ));
}

#[test]
fn duplicate_block_entity_field_is_rejected() {
    let mut entity = block_entity_header("minecraft:future_entity", BlockPos::new(0, 64, 0));
    entity.push("id", NbtTag::String("minecraft:another_entity".to_owned()));
    let root = chunk_root_with_block_entities(Vec::new(), vec![NbtTag::Compound(entity)]);
    let region = region_for(&root);
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::DuplicateNbtField {
            field: "block_entities[].id",
            ..
        }
    ));
}

#[test]
fn oversized_block_entity_list_is_rejected_before_iteration() {
    let entities = (0..=MAX_BLOCK_ENTITIES)
        .map(|_| {
            let mut entity = NbtCompound::new();
            entity.push("id", NbtTag::String("minecraft:future_entity".to_owned()));
            NbtTag::Compound(entity)
        })
        .collect();
    let root = chunk_root_with_block_entities(Vec::new(), entities);
    let region = region_for(&root);
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::TooManyBlockEntities {
            count,
            max,
            ..
        } if count == MAX_BLOCK_ENTITIES + 1 && max == MAX_BLOCK_ENTITIES
    ));
}

#[test]
fn exact_block_entity_list_limit_is_accepted() {
    let entities = (0..MAX_BLOCK_ENTITIES)
        .map(|_| {
            let mut entity = NbtCompound::new();
            entity.push("id", NbtTag::String("minecraft:future_entity".to_owned()));
            NbtTag::Compound(entity)
        })
        .collect();
    let root = chunk_root_with_block_entities(Vec::new(), entities);
    let chunk = region_for(&root)
        .read_chunk(0, 0)
        .expect("limit is accepted")
        .expect("present");
    assert_eq!(chunk.block_entity_count(), 0);
}

#[test]
fn duplicate_supported_block_entity_position_is_rejected() {
    let pos = BlockPos::new(0, 64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:oak_sign", &[])];
    let root = chunk_root_with_block_entities(
        vec![section(4, palette, &[])],
        vec![sign_block_entity(pos), sign_block_entity(pos)],
    );
    let region = region_for(&root);
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::DuplicateBlockEntity {
            pos: duplicate,
            ..
        } if duplicate == pos
    ));
}

#[test]
fn block_entity_position_outside_chunk_is_rejected() {
    let pos = BlockPos::new(16, 64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:oak_sign", &[])];
    let root = chunk_root_with_block_entities(
        vec![section(4, palette, &[])],
        vec![sign_block_entity(pos)],
    );
    let region = region_for(&root);
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::BlockEntityOutsideChunk {
            pos: rejected,
            ..
        } if rejected == pos
    ));
}

#[test]
fn supported_entity_on_incompatible_block_is_rejected() {
    let pos = BlockPos::new(0, 64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:stone", &[])];
    let root = chunk_root_with_block_entities(
        vec![section(4, palette, &[])],
        vec![sign_block_entity(pos)],
    );
    let region = region_for(&root);
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::BlockEntityStateMismatch {
            pos: rejected,
            state: 1,
            ..
        } if rejected == pos
    ));
}

#[test]
fn supported_entity_on_air_is_rejected() {
    let pos = BlockPos::new(0, 64, 0);
    let root = chunk_root_with_block_entities(Vec::new(), vec![sign_block_entity(pos)]);
    let err = region_for(&root).read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::BlockEntityStateMismatch {
            pos: rejected,
            state: 0,
            ..
        } if rejected == pos
    ));
}

#[test]
fn known_entity_from_another_or_missing_data_version_is_rejected() {
    let pos = BlockPos::new(0, 64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:oak_sign", &[])];
    let old = chunk_root_for_data_version(
        3465,
        vec![section(4, palette, &[])],
        vec![sign_block_entity(pos)],
    );
    let mut missing = NbtCompound::new();
    missing.push("sections", NbtTag::List(vec![section(4, palette, &[])]));
    missing.push("block_entities", NbtTag::List(vec![sign_block_entity(pos)]));

    for (root, found) in [(old, Some(3465)), (NbtTag::Compound(missing), None)] {
        let err = region_for(&root).read_chunk(0, 0).unwrap_err();
        assert!(matches!(
            err,
            AnvilError::UnsupportedBlockEntityDataVersion {
                found: actual,
                expected,
                ..
            } if actual == found && expected == DATA_VERSION
        ));
    }
}

#[test]
fn sign_with_wrong_message_count_is_rejected() {
    let pos = BlockPos::new(0, 64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:oak_sign", &[])];
    let front = sign_face_messages(vec![
        NbtTag::String("one".to_owned()),
        NbtTag::String("two".to_owned()),
        NbtTag::String("three".to_owned()),
    ]);
    let root = chunk_root_with_block_entities(
        vec![section(4, palette, &[])],
        vec![sign_block_entity_with_front(pos, front)],
    );
    let region = region_for(&root);
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::BadSignMessageCount {
            count: 3,
            expected: 4,
            ..
        }
    ));
}

#[test]
fn sign_face_and_message_tag_types_are_rejected() {
    let pos = BlockPos::new(0, 64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:oak_sign", &[])];
    let mut wrong_color = NbtCompound::new();
    wrong_color.push("color", NbtTag::Int(0));
    wrong_color.push("has_glowing_text", NbtTag::Byte(0));
    wrong_color.push(
        "messages",
        NbtTag::List(vec![NbtTag::String(String::new()); 4]),
    );
    let cases = [
        (NbtTag::Int(1), "block_entities[].front_text"),
        (
            NbtTag::Compound(wrong_color),
            "block_entities[].front_text.color",
        ),
        (
            sign_face_messages(vec![NbtTag::Int(0); 4]),
            "block_entities[].text_component",
        ),
    ];

    for (front, expected_field) in cases {
        let root = chunk_root_with_block_entities(
            vec![section(4, palette, &[])],
            vec![sign_block_entity_with_front(pos, front)],
        );
        let err = region_for(&root).read_chunk(0, 0).unwrap_err();
        assert!(
            matches!(
                err,
                AnvilError::WrongFieldType { field, .. } if field == expected_field
            ),
            "got {err:?}"
        );
    }
}

#[test]
fn semantic_sign_component_is_rejected_instead_of_flattened() {
    let pos = BlockPos::new(0, 64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:oak_sign", &[])];
    let mut semantic = NbtCompound::new();
    semantic.push("text", NbtTag::String("styled".to_owned()));
    semantic.push("bold", NbtTag::Byte(1));
    let front = sign_face_messages(
        std::iter::once(NbtTag::Compound(semantic))
            .chain((0..3).map(|_| {
                let mut empty = NbtCompound::new();
                empty.push("text", NbtTag::String(String::new()));
                NbtTag::Compound(empty)
            }))
            .collect(),
    );
    let root = chunk_root_with_block_entities(
        vec![section(4, palette, &[])],
        vec![sign_block_entity_with_front(pos, front)],
    );
    let region = region_for(&root);
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::UnsupportedBlockEntityData {
            field: "block_entities[].text_component",
            ..
        }
    ));
}

#[test]
fn filtered_sign_messages_are_rejected_instead_of_discarded() {
    let pos = BlockPos::new(0, 64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:oak_sign", &[])];
    let NbtTag::Compound(mut front) = sign_face([""; 4]) else {
        panic!("helper produces a compound");
    };
    front.push(
        "filtered_messages",
        NbtTag::List(vec![NbtTag::String(String::new()); 4]),
    );
    let root = chunk_root_with_block_entities(
        vec![section(4, palette, &[])],
        vec![sign_block_entity_with_front(pos, NbtTag::Compound(front))],
    );
    let err = region_for(&root).read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::UnsupportedBlockEntityData {
            field: "block_entities[].front_text.filtered_messages",
            ..
        }
    ));
}

#[test]
fn oversized_sign_line_is_rejected_at_world_boundary() {
    let pos = BlockPos::new(0, 64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:oak_sign", &[])];
    let boundary = "x".repeat(MAX_SIGN_LINE_BYTES);
    let accepted_front = sign_face_messages(vec![
        NbtTag::String(boundary.clone()),
        NbtTag::String(String::new()),
        NbtTag::String(String::new()),
        NbtTag::String(String::new()),
    ]);
    let accepted_root = chunk_root_with_block_entities(
        vec![section(4, palette, &[])],
        vec![sign_block_entity_with_front(pos, accepted_front)],
    );
    let accepted = region_for(&accepted_root)
        .read_chunk(0, 0)
        .expect("exact line limit is accepted")
        .expect("present");
    let Some(BlockEntity::Sign(sign)) = accepted.block_entity(pos) else {
        panic!("accepted boundary sign missing");
    };
    assert_eq!(sign.front().lines()[0].as_str(), boundary.as_str());

    let front = sign_face_messages(vec![
        NbtTag::String("x".repeat(MAX_SIGN_LINE_BYTES + 1)),
        NbtTag::String(String::new()),
        NbtTag::String(String::new()),
        NbtTag::String(String::new()),
    ]);
    let root = chunk_root_with_block_entities(
        vec![section(4, palette, &[])],
        vec![sign_block_entity_with_front(pos, front)],
    );
    let region = region_for(&root);
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::SignTextTooLong {
            len,
            max,
            ..
        } if len == MAX_SIGN_LINE_BYTES + 1 && max == MAX_SIGN_LINE_BYTES
    ));
}

#[test]
fn non_empty_chest_is_rejected_instead_of_losing_items() {
    let pos = BlockPos::new(0, -64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:chest", &[])];
    let mut chest = block_entity_header("minecraft:chest", pos);
    chest.push(
        "Items",
        NbtTag::List(vec![NbtTag::Compound(NbtCompound::new())]),
    );
    let root = chunk_root_with_block_entities(
        vec![section(-4, palette, &[])],
        vec![NbtTag::Compound(chest)],
    );
    let region = region_for(&root);
    let err = region.read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::UnsupportedBlockEntityData {
            field: "block_entities[].Items",
            ..
        }
    ));
}

#[test]
fn chest_items_with_wrong_type_are_rejected() {
    let pos = BlockPos::new(0, -64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:chest", &[])];
    let mut chest = block_entity_header("minecraft:chest", pos);
    chest.push("Items", NbtTag::Compound(NbtCompound::new()));
    let root = chunk_root_with_block_entities(
        vec![section(-4, palette, &[])],
        vec![NbtTag::Compound(chest)],
    );
    let err = region_for(&root).read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::WrongFieldType {
            field: "block_entities[].Items",
            ..
        }
    ));
}

#[test]
fn unresolved_loot_chest_is_rejected_instead_of_emptied() {
    let pos = BlockPos::new(0, -64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:chest", &[])];
    let mut chest = block_entity_header("minecraft:chest", pos);
    chest.push(
        "LootTable",
        NbtTag::String("minecraft:chests/simple_dungeon".to_owned()),
    );
    chest.push("LootTableSeed", NbtTag::Long(42));
    let root = chunk_root_with_block_entities(
        vec![section(-4, palette, &[])],
        vec![NbtTag::Compound(chest)],
    );
    let err = region_for(&root).read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::UnsupportedBlockEntityData {
            field: "block_entities[].LootTable",
            ..
        }
    ));
}

#[test]
fn generic_block_entity_components_are_rejected_instead_of_lost() {
    let pos = BlockPos::new(0, -64, 0);
    let palette: &[(&str, &[(&str, &str)])] = &[("minecraft:chest", &[])];
    let mut chest = block_entity_header("minecraft:chest", pos);
    chest.push("Items", NbtTag::List(Vec::new()));
    chest.push("components", NbtTag::Compound(NbtCompound::new()));
    let root = chunk_root_with_block_entities(
        vec![section(-4, palette, &[])],
        vec![NbtTag::Compound(chest)],
    );
    let err = region_for(&root).read_chunk(0, 0).unwrap_err();
    assert!(matches!(
        err,
        AnvilError::UnsupportedBlockEntityData {
            field: "block_entities[].components",
            ..
        }
    ));
}

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
