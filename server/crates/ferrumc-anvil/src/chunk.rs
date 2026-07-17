//! Turning a decoded chunk NBT tree into a [`ferrumc_world::Chunk`].
//!
//! A modern (1.18+) Anvil chunk stores its blocks under `sections`, a list of
//! per-16-block-tall section compounds. Each section's `block_states` holds:
//!
//! - `palette` — a list of `{ Name, Properties? }` compounds naming the distinct
//!   block states the section uses; and
//! - `data` — a packed `long_array` of palette indices, one per block, in
//!   Minecraft's `YZX` order. The entry width is `max(4, ceil(log2(len)))` bits,
//!   and indices never straddle a 64-bit word (the high bits of each word are
//!   left unused). When the palette has a single entry, `data` is omitted and
//!   every block takes that one state.
//!
//! Each palette entry is resolved to a numeric block-state id via
//! [`ferrumc_registry::block_state::compute_state_id`], which maps a vanilla
//! block name plus its property map to the 1.21.8 protocol id. Blocks unknown to
//! the pinned registry fall back to air (the import stays version-tolerant
//! rather than failing the whole region).
//!
//! The root `block_entities` list is imported into the same [`Chunk`]. The
//! current world model represents regular/hanging signs and chests; unknown ids
//! are skipped, while malformed supported entries, duplicates, out-of-chunk
//! positions, and payloads that cannot be represented without loss are rejected
//! with classified errors.

use std::collections::{BTreeMap, BTreeSet};

use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_nbt::{NbtCompound, NbtTag};
use ferrumc_registry::block_state::{compute_state_id, state_id_to_block_name};
use ferrumc_registry::dimension;
use ferrumc_registry::DATA_VERSION;
use ferrumc_world::{
    sign_kind_for_state, BlockEntity, BlockStateId, ChestInventory, Chunk, Sign, SignKind,
    MAX_BLOCK_ENTITIES, MAX_SIGN_COLOR_BYTES, MAX_SIGN_LINE_BYTES, SECTION_VOLUME, SIGN_LINES,
};

use crate::error::{AnvilError, ChunkCoord};

/// Lowest buildable world `y`, inclusive (`-64`).
const MIN_Y: i32 = dimension::MIN_Y;

/// Edge length of a section in blocks.
const SECTION_EDGE: i32 = 16;

/// Minimum bits-per-entry for a section's packed block-state data.
const MIN_BITS_PER_BLOCK: u32 = 4;

/// Imports a decoded chunk NBT root into a world [`Chunk`] at `chunk_pos`.
///
/// `root` must be the chunk's named-root `TAG_Compound`. Sections whose vertical
/// span falls outside the buildable world height (e.g. the boundary
/// lighting-only sections vanilla sometimes writes) are skipped. A chunk with no
/// `sections` field yields an all-air chunk.
pub(crate) fn chunk_from_nbt(
    coord: ChunkCoord,
    chunk_pos: ChunkPos,
    root: &NbtTag,
) -> Result<Chunk, AnvilError> {
    let NbtTag::Compound(root) = root else {
        return Err(AnvilError::WrongFieldType {
            coord,
            field: "root",
        });
    };

    let mut chunk = Chunk::new(chunk_pos);
    let data_version = match unique_field(coord, root, "DataVersion", "DataVersion")? {
        Some(NbtTag::Int(version)) => Some(*version),
        Some(_) => {
            return Err(AnvilError::WrongFieldType {
                coord,
                field: "DataVersion",
            });
        }
        None => None,
    };

    // A proto-chunk (not yet fully generated) may legitimately carry no
    // sections; treat that as an empty column, but still inspect block entities
    // below so the optional field can never be bypassed by an early return.
    match root.get("sections") {
        None => {}
        Some(NbtTag::List(sections)) => {
            for section in sections {
                import_section(coord, chunk_pos, &mut chunk, section)?;
            }
        }
        Some(_) => {
            return Err(AnvilError::WrongFieldType {
                coord,
                field: "sections",
            });
        }
    }

    import_block_entities(coord, &mut chunk, root, data_version)?;
    Ok(chunk)
}

/// Imports every supported block entity from the optional root list.
///
/// The raw list is capped before iteration, including entries whose ids are
/// unknown and skipped. This bounds both work and the duplicate-position set
/// even when a hostile file repeats one position many times.
fn import_block_entities(
    coord: ChunkCoord,
    chunk: &mut Chunk,
    root: &NbtCompound,
    data_version: Option<i32>,
) -> Result<(), AnvilError> {
    let Some(tag) = unique_field(coord, root, "block_entities", "block_entities")? else {
        return Ok(());
    };
    let NbtTag::List(entities) = tag else {
        return Err(AnvilError::WrongFieldType {
            coord,
            field: "block_entities",
        });
    };
    if entities.len() > MAX_BLOCK_ENTITIES {
        return Err(AnvilError::TooManyBlockEntities {
            coord,
            count: entities.len(),
            max: MAX_BLOCK_ENTITIES,
        });
    }

    let mut positions = BTreeSet::new();
    for tag in entities {
        let Some((pos, entity)) = import_block_entity(coord, chunk, tag, data_version)? else {
            continue;
        };
        if !positions.insert(pos) {
            return Err(AnvilError::DuplicateBlockEntity { coord, pos });
        }
        chunk
            .set_block_entity(pos, entity)
            .map_err(|source| AnvilError::BlockEntityPlacement { coord, pos, source })?;
    }
    Ok(())
}

/// Converts one supported block-entity compound, skipping unknown ids for
/// version tolerance.
fn import_block_entity(
    coord: ChunkCoord,
    chunk: &Chunk,
    tag: &NbtTag,
    data_version: Option<i32>,
) -> Result<Option<(BlockPos, BlockEntity)>, AnvilError> {
    let NbtTag::Compound(compound) = tag else {
        return Err(AnvilError::WrongFieldType {
            coord,
            field: "block_entities[]",
        });
    };
    let id = required_string(coord, compound, "id", "block_entities[].id")?;

    match id {
        "minecraft:sign" => {
            let pos = block_entity_position(coord, compound)?;
            require_supported_data_version(coord, pos, data_version)?;
            let entity = import_sign(coord, chunk, compound, pos, id, SignKind::Sign)?;
            Ok(Some((pos, entity)))
        }
        "minecraft:hanging_sign" => {
            let pos = block_entity_position(coord, compound)?;
            require_supported_data_version(coord, pos, data_version)?;
            let entity = import_sign(coord, chunk, compound, pos, id, SignKind::Hanging)?;
            Ok(Some((pos, entity)))
        }
        "minecraft:chest" | "minecraft:trapped_chest" => {
            let pos = block_entity_position(coord, compound)?;
            require_supported_data_version(coord, pos, data_version)?;
            let entity = import_chest(coord, chunk, compound, pos, id)?;
            Ok(Some((pos, entity)))
        }
        _ => Ok(None),
    }
}

/// Requires the pinned 1.21.8 disk schema before interpreting a known payload.
fn require_supported_data_version(
    coord: ChunkCoord,
    pos: BlockPos,
    data_version: Option<i32>,
) -> Result<(), AnvilError> {
    if data_version != Some(DATA_VERSION) {
        return Err(AnvilError::UnsupportedBlockEntityDataVersion {
            coord,
            pos,
            found: data_version,
            expected: DATA_VERSION,
        });
    }
    Ok(())
}

/// Imports a sign whose block state and currently representable payload agree
/// with the world model.
fn import_sign(
    coord: ChunkCoord,
    chunk: &Chunk,
    compound: &NbtCompound,
    pos: BlockPos,
    id: &str,
    expected_kind: SignKind,
) -> Result<BlockEntity, AnvilError> {
    let state = block_entity_state(coord, chunk, pos)?;
    match sign_kind_for_state(state.as_u32()) {
        Some(actual_kind) if actual_kind == expected_kind => {}
        _ => return Err(state_mismatch(coord, pos, id, state)),
    }
    reject_present_field(
        coord,
        compound,
        pos,
        "components",
        "block_entities[].components",
    )?;

    let is_waxed = required_byte(coord, compound, "is_waxed", "block_entities[].is_waxed")?;
    if is_waxed != 0 {
        return Err(AnvilError::UnsupportedBlockEntityData {
            coord,
            pos,
            field: "block_entities[].is_waxed",
        });
    }

    let front = import_sign_face(coord, compound, pos, "front_text")?;
    let back = import_sign_face(coord, compound, pos, "back_text")?;
    let mut sign = Sign::new(expected_kind);
    sign.set_face_lines(true, front);
    sign.set_face_lines(false, back);
    Ok(BlockEntity::Sign(sign))
}

/// Imports one sign face, retaining its four plain-text lines.
///
/// Non-default styling is rejected explicitly because this crate cannot call
/// the world crate's storage-only reconstruction seam. This prevents
/// colored/glowing data from being silently normalized away.
fn import_sign_face(
    coord: ChunkCoord,
    sign: &NbtCompound,
    pos: BlockPos,
    face_name: &'static str,
) -> Result<[String; SIGN_LINES], AnvilError> {
    let face_path = match face_name {
        "front_text" => "block_entities[].front_text",
        _ => "block_entities[].back_text",
    };
    let face = required_compound(coord, sign, face_name, face_path)?;
    reject_present_field(
        coord,
        face,
        pos,
        "filtered_messages",
        match face_name {
            "front_text" => "block_entities[].front_text.filtered_messages",
            _ => "block_entities[].back_text.filtered_messages",
        },
    )?;

    let color_path = match face_name {
        "front_text" => "block_entities[].front_text.color",
        _ => "block_entities[].back_text.color",
    };
    let color = required_string(coord, face, "color", color_path)?;
    if color != "black" || color.len() > MAX_SIGN_COLOR_BYTES {
        return Err(AnvilError::UnsupportedBlockEntityData {
            coord,
            pos,
            field: color_path,
        });
    }

    let glow_path = match face_name {
        "front_text" => "block_entities[].front_text.has_glowing_text",
        _ => "block_entities[].back_text.has_glowing_text",
    };
    let glowing = required_byte(coord, face, "has_glowing_text", glow_path)?;
    if glowing != 0 {
        return Err(AnvilError::UnsupportedBlockEntityData {
            coord,
            pos,
            field: glow_path,
        });
    }

    let messages_path = match face_name {
        "front_text" => "block_entities[].front_text.messages",
        _ => "block_entities[].back_text.messages",
    };
    let messages = required_list(coord, face, "messages", messages_path)?;
    if messages.len() != SIGN_LINES {
        return Err(AnvilError::BadSignMessageCount {
            coord,
            pos,
            face: face_name,
            count: messages.len(),
            expected: SIGN_LINES,
        });
    }

    let mut lines: [String; SIGN_LINES] = std::array::from_fn(|_| String::new());
    for (index, (line, message)) in lines.iter_mut().zip(messages).enumerate() {
        *line = decode_sign_line(coord, pos, face_name, index, message)?;
    }
    Ok(lines)
}

/// Imports an empty chest shell.
///
/// A non-empty `Items` list is rejected instead of discarded: constructing
/// canonical `ItemStack`s would require a forbidden `ferrumc-anvil ->
/// ferrumc-items` dependency that is absent from the crate map.
fn import_chest(
    coord: ChunkCoord,
    chunk: &Chunk,
    compound: &NbtCompound,
    pos: BlockPos,
    id: &str,
) -> Result<BlockEntity, AnvilError> {
    let state = block_entity_state(coord, chunk, pos)?;
    let expected_state_name = match id {
        "minecraft:chest" => "chest",
        "minecraft:trapped_chest" => "trapped_chest",
        _ => return Err(state_mismatch(coord, pos, id, state)),
    };
    if state_id_to_block_name(state.as_u32()) != Some(expected_state_name) {
        return Err(state_mismatch(coord, pos, id, state));
    }

    for (name, path) in [
        ("components", "block_entities[].components"),
        ("LootTable", "block_entities[].LootTable"),
        ("LootTableSeed", "block_entities[].LootTableSeed"),
        ("CustomName", "block_entities[].CustomName"),
        ("Lock", "block_entities[].Lock"),
    ] {
        reject_present_field(coord, compound, pos, name, path)?;
    }

    if let Some(items) = unique_field(coord, compound, "Items", "block_entities[].Items")? {
        let NbtTag::List(items) = items else {
            return Err(AnvilError::WrongFieldType {
                coord,
                field: "block_entities[].Items",
            });
        };
        if !items.is_empty() {
            return Err(AnvilError::UnsupportedBlockEntityData {
                coord,
                pos,
                field: "block_entities[].Items",
            });
        }
    }
    Ok(BlockEntity::Chest(ChestInventory::new()))
}

/// Rejects a semantic field the current world model cannot retain.
fn reject_present_field(
    coord: ChunkCoord,
    compound: &NbtCompound,
    pos: BlockPos,
    name: &str,
    path: &'static str,
) -> Result<(), AnvilError> {
    if unique_field(coord, compound, name, path)?.is_some() {
        return Err(AnvilError::UnsupportedBlockEntityData {
            coord,
            pos,
            field: path,
        });
    }
    Ok(())
}

/// Reads the unique absolute position fields of a supported block entity.
fn block_entity_position(
    coord: ChunkCoord,
    compound: &NbtCompound,
) -> Result<BlockPos, AnvilError> {
    let x = required_int(coord, compound, "x", "block_entities[].x")?;
    let y = required_int(coord, compound, "y", "block_entities[].y")?;
    let z = required_int(coord, compound, "z", "block_entities[].z")?;
    Ok(BlockPos::new(x, y, z))
}

/// Returns the imported state under a supported entity, classifying a foreign
/// column or out-of-height position separately from a type mismatch.
fn block_entity_state(
    coord: ChunkCoord,
    chunk: &Chunk,
    pos: BlockPos,
) -> Result<BlockStateId, AnvilError> {
    chunk
        .get_block(pos)
        .ok_or(AnvilError::BlockEntityOutsideChunk { coord, pos })
}

/// Builds the common known-entity/state mismatch error.
fn state_mismatch(coord: ChunkCoord, pos: BlockPos, id: &str, state: BlockStateId) -> AnvilError {
    AnvilError::BlockEntityStateMismatch {
        coord,
        id: id.to_owned(),
        pos,
        state: state.as_u32(),
    }
}

/// Looks up one schema-significant compound field and rejects duplicate names.
fn unique_field<'a>(
    coord: ChunkCoord,
    compound: &'a NbtCompound,
    name: &str,
    path: &'static str,
) -> Result<Option<&'a NbtTag>, AnvilError> {
    let mut matches = compound
        .iter()
        .filter_map(|(candidate, value)| (candidate == name).then_some(value));
    let value = matches.next();
    if matches.next().is_some() {
        return Err(AnvilError::DuplicateNbtField { coord, field: path });
    }
    Ok(value)
}

/// Reads a required schema-significant tag after duplicate-name validation.
fn required_field<'a>(
    coord: ChunkCoord,
    compound: &'a NbtCompound,
    name: &str,
    path: &'static str,
) -> Result<&'a NbtTag, AnvilError> {
    unique_field(coord, compound, name, path)?
        .ok_or(AnvilError::MissingField { coord, field: path })
}

/// Reads a required string field.
fn required_string<'a>(
    coord: ChunkCoord,
    compound: &'a NbtCompound,
    name: &str,
    path: &'static str,
) -> Result<&'a str, AnvilError> {
    match required_field(coord, compound, name, path)? {
        NbtTag::String(value) => Ok(value),
        _ => Err(AnvilError::WrongFieldType { coord, field: path }),
    }
}

/// Reads a required compound field.
fn required_compound<'a>(
    coord: ChunkCoord,
    compound: &'a NbtCompound,
    name: &str,
    path: &'static str,
) -> Result<&'a NbtCompound, AnvilError> {
    match required_field(coord, compound, name, path)? {
        NbtTag::Compound(value) => Ok(value),
        _ => Err(AnvilError::WrongFieldType { coord, field: path }),
    }
}

/// Reads a required list field.
fn required_list<'a>(
    coord: ChunkCoord,
    compound: &'a NbtCompound,
    name: &str,
    path: &'static str,
) -> Result<&'a [NbtTag], AnvilError> {
    match required_field(coord, compound, name, path)? {
        NbtTag::List(value) => Ok(value),
        _ => Err(AnvilError::WrongFieldType { coord, field: path }),
    }
}

/// Reads a required byte field.
fn required_byte(
    coord: ChunkCoord,
    compound: &NbtCompound,
    name: &str,
    path: &'static str,
) -> Result<i8, AnvilError> {
    match required_field(coord, compound, name, path)? {
        NbtTag::Byte(value) => Ok(*value),
        _ => Err(AnvilError::WrongFieldType { coord, field: path }),
    }
}

/// Reads a required 32-bit integer field.
fn required_int(
    coord: ChunkCoord,
    compound: &NbtCompound,
    name: &str,
    path: &'static str,
) -> Result<i32, AnvilError> {
    match required_field(coord, compound, name, path)? {
        NbtTag::Int(value) => Ok(*value),
        _ => Err(AnvilError::WrongFieldType { coord, field: path }),
    }
}

/// Imports a single section compound into `chunk`.
fn import_section(
    coord: ChunkCoord,
    chunk_pos: ChunkPos,
    chunk: &mut Chunk,
    section: &NbtTag,
) -> Result<(), AnvilError> {
    let NbtTag::Compound(section) = section else {
        return Err(AnvilError::WrongFieldType {
            coord,
            field: "sections[]",
        });
    };

    let section_y = section_y(coord, section)?;
    let base_y = section_y * SECTION_EDGE;
    // Skip sections that do not lie entirely within the buildable column (the
    // boundary lighting sections vanilla writes just below/above the world).
    // The check is done in the unsigned offset-from-bottom domain so it needs no
    // signed-to-signed cast of the registry height.
    if base_y < MIN_Y {
        return Ok(());
    }
    let offset_from_bottom = (base_y - MIN_Y) as u32;
    if offset_from_bottom + SECTION_EDGE as u32 > dimension::HEIGHT {
        return Ok(());
    }

    // A section without block_states is all air; nothing to place.
    let Some(block_states) = section.get("block_states") else {
        return Ok(());
    };
    let NbtTag::Compound(block_states) = block_states else {
        return Err(AnvilError::WrongFieldType {
            coord,
            field: "block_states",
        });
    };

    let palette = match block_states.get("palette") {
        Some(NbtTag::List(palette)) => palette,
        Some(_) => {
            return Err(AnvilError::WrongFieldType {
                coord,
                field: "palette",
            })
        }
        None => {
            return Err(AnvilError::MissingField {
                coord,
                field: "palette",
            })
        }
    };
    if palette.is_empty() {
        return Err(AnvilError::EmptyPalette { coord, section_y });
    }

    let mapped = map_palette(coord, palette)?;

    match block_states.get("data") {
        // Single-state section: every block is palette[0]. (Vanilla omits `data`
        // in this case.) An all-air section is then a no-op.
        None => {
            let state = mapped[0];
            if !state.is_air() {
                fill_section(coord, chunk_pos, chunk, base_y, state)?;
            }
            Ok(())
        }
        Some(NbtTag::LongArray(data)) => {
            unpack_section(coord, chunk_pos, chunk, section_y, base_y, &mapped, data)
        }
        Some(_) => Err(AnvilError::WrongFieldType {
            coord,
            field: "data",
        }),
    }
}

/// Reads a section's vanilla `Y` index, accepting the conventional `TAG_Byte`
/// (and tolerating `TAG_Int`).
fn section_y(coord: ChunkCoord, section: &NbtCompound) -> Result<i32, AnvilError> {
    match section.get("Y") {
        Some(NbtTag::Byte(y)) => Ok(i32::from(*y)),
        Some(NbtTag::Int(y)) => Ok(*y),
        Some(_) => Err(AnvilError::WrongFieldType { coord, field: "Y" }),
        None => Err(AnvilError::MissingField { coord, field: "Y" }),
    }
}

/// Resolves each palette entry to a block-state id, once per section.
///
/// An entry naming a block the pinned registry does not know maps to air, so an
/// import never fails on an unrecognised (e.g. newer- or older-version) block.
fn map_palette(coord: ChunkCoord, palette: &[NbtTag]) -> Result<Vec<BlockStateId>, AnvilError> {
    let mut mapped = Vec::with_capacity(palette.len());
    for entry in palette {
        mapped.push(map_palette_entry(coord, entry)?);
    }
    Ok(mapped)
}

/// Resolves one `{ Name, Properties? }` palette entry to a block-state id.
fn map_palette_entry(coord: ChunkCoord, entry: &NbtTag) -> Result<BlockStateId, AnvilError> {
    let NbtTag::Compound(entry) = entry else {
        return Err(AnvilError::WrongFieldType {
            coord,
            field: "palette[]",
        });
    };

    let name = match entry.get("Name") {
        Some(NbtTag::String(name)) => name.as_str(),
        Some(_) => {
            return Err(AnvilError::WrongFieldType {
                coord,
                field: "palette[].Name",
            })
        }
        None => {
            return Err(AnvilError::MissingField {
                coord,
                field: "palette[].Name",
            })
        }
    };

    // Properties is an optional compound of string-valued block properties.
    // Non-string values are ignored (compute_state_id inherits the default for
    // any property left unspecified).
    let mut properties: BTreeMap<&str, &str> = BTreeMap::new();
    if let Some(NbtTag::Compound(props)) = entry.get("Properties") {
        for (key, value) in props.iter() {
            if let NbtTag::String(value) = value {
                properties.insert(key, value.as_str());
            }
        }
    }

    // Unknown block / property / value -> air, keeping the import resilient.
    Ok(compute_state_id(name, &properties).map_or(BlockStateId::AIR, BlockStateId::new))
}

/// Fills an in-range section (all 4096 blocks) with a single non-air state.
fn fill_section(
    coord: ChunkCoord,
    chunk_pos: ChunkPos,
    chunk: &mut Chunk,
    base_y: i32,
    state: BlockStateId,
) -> Result<(), AnvilError> {
    for index in 0..SECTION_VOLUME {
        place(coord, chunk_pos, chunk, base_y, index, state)?;
    }
    Ok(())
}

/// Unpacks a section's packed palette indices and writes every non-air block.
fn unpack_section(
    coord: ChunkCoord,
    chunk_pos: ChunkPos,
    chunk: &mut Chunk,
    section_y: i32,
    base_y: i32,
    mapped: &[BlockStateId],
    data: &[i64],
) -> Result<(), AnvilError> {
    let bits = bits_per_block(mapped.len());
    // A single-entry palette has bits == 0 but should have come through the
    // no-`data` path; guard so the shift/divisor below are always well-defined.
    if bits == 0 {
        let state = mapped[0];
        if !state.is_air() {
            fill_section(coord, chunk_pos, chunk, base_y, state)?;
        }
        return Ok(());
    }

    let entries_per_long = 64 / bits as usize;
    let expected = SECTION_VOLUME.div_ceil(entries_per_long);
    if data.len() != expected {
        return Err(AnvilError::BadBlockStateData {
            coord,
            section_y,
            got: data.len(),
            expected,
        });
    }

    let mask: u64 = (1u64 << bits) - 1;
    for index in 0..SECTION_VOLUME {
        let long_index = index / entries_per_long;
        let within = (index % entries_per_long) as u32;
        // Reinterpret the signed long's bit pattern as unsigned for shifting.
        let word = data[long_index] as u64;
        let palette_index = ((word >> (within * bits)) & mask) as usize;

        let state = *mapped
            .get(palette_index)
            .ok_or(AnvilError::PaletteIndexOutOfRange {
                coord,
                section_y,
                index: palette_index,
                len: mapped.len(),
            })?;
        if state.is_air() {
            continue;
        }
        place(coord, chunk_pos, chunk, base_y, index, state)?;
    }
    Ok(())
}

/// Writes a single block, mapping a section-local flat index to an absolute
/// [`BlockPos`] and setting it in the chunk.
fn place(
    coord: ChunkCoord,
    chunk_pos: ChunkPos,
    chunk: &mut Chunk,
    base_y: i32,
    index: usize,
    state: BlockStateId,
) -> Result<(), AnvilError> {
    // Flat index uses YZX order: y = i>>8, z = (i>>4)&15, x = i&15. Each masked
    // value is in 0..16, so the narrowing to u8 is lossless and the widen to i32
    // cannot wrap.
    let local_x = i32::from((index & 0xF) as u8);
    let local_z = i32::from(((index >> 4) & 0xF) as u8);
    let local_y = i32::from(((index >> 8) & 0xF) as u8);

    let origin = chunk_pos.origin_block(0);
    let pos = BlockPos::new(origin.x() + local_x, base_y + local_y, origin.z() + local_z);

    // The section range is validated up front, so this set is always in bounds;
    // surface any unexpected failure rather than dropping the block silently.
    chunk
        .set_block(pos, state)
        .map_err(|source| AnvilError::Placement { coord, source })
}

/// Decodes one 1.21.8 structured-NBT text component into bounded plain text.
///
/// A direct NBT string is the literal-text shorthand. A compound is retained
/// only when it consists solely of one string-valued `text` field; styling,
/// translated text, siblings, and list components are rejected rather than
/// flattened with semantic loss.
fn decode_sign_line(
    coord: ChunkCoord,
    pos: BlockPos,
    face: &'static str,
    line: usize,
    component: &NbtTag,
) -> Result<String, AnvilError> {
    let text = match component {
        NbtTag::String(text) => text,
        NbtTag::Compound(fields) => {
            let Some(value) = unique_field(
                coord,
                fields,
                "text",
                "block_entities[].text_component.text",
            )?
            else {
                return Err(AnvilError::UnsupportedBlockEntityData {
                    coord,
                    pos,
                    field: "block_entities[].text_component",
                });
            };
            if fields.len() != 1 {
                return Err(AnvilError::UnsupportedBlockEntityData {
                    coord,
                    pos,
                    field: "block_entities[].text_component",
                });
            }
            let NbtTag::String(text) = value else {
                return Err(AnvilError::WrongFieldType {
                    coord,
                    field: "block_entities[].text_component.text",
                });
            };
            text
        }
        _ => {
            return Err(AnvilError::WrongFieldType {
                coord,
                field: "block_entities[].text_component",
            });
        }
    };
    if text.len() > MAX_SIGN_LINE_BYTES {
        return Err(AnvilError::SignTextTooLong {
            coord,
            pos,
            face,
            line,
            len: text.len(),
            max: MAX_SIGN_LINE_BYTES,
        });
    }
    Ok(text.to_owned())
}

/// Bits-per-entry for a section's packed block-state data given its palette
/// length: `max(4, ceil(log2(len)))`, or `0` for a single-entry palette.
fn bits_per_block(palette_len: usize) -> u32 {
    if palette_len <= 1 {
        return 0;
    }
    // `ceil(log2(len))` == bit width of `len - 1`.
    let needed = usize::BITS - (palette_len - 1).leading_zeros();
    needed.max(MIN_BITS_PER_BLOCK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_per_block_matches_vanilla_thresholds() {
        assert_eq!(bits_per_block(1), 0);
        assert_eq!(bits_per_block(2), 4);
        assert_eq!(bits_per_block(16), 4);
        assert_eq!(bits_per_block(17), 5);
        assert_eq!(bits_per_block(32), 5);
        assert_eq!(bits_per_block(33), 6);
        assert_eq!(bits_per_block(SECTION_VOLUME), 12);
    }

    #[test]
    fn world_bottom_matches_dimension() {
        assert_eq!(MIN_Y, -64);
        assert_eq!(dimension::HEIGHT, 384);
    }
}
