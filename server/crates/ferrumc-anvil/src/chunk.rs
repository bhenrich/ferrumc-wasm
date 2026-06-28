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

use std::collections::BTreeMap;

use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_nbt::{NbtCompound, NbtTag};
use ferrumc_registry::block_state::compute_state_id;
use ferrumc_registry::dimension;
use ferrumc_world::{BlockStateId, Chunk, SECTION_VOLUME};

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

    // A proto-chunk (not yet fully generated) may legitimately carry no
    // sections; treat that as an empty column rather than an error.
    let sections = match root.get("sections") {
        None => return Ok(chunk),
        Some(NbtTag::List(sections)) => sections,
        Some(_) => {
            return Err(AnvilError::WrongFieldType {
                coord,
                field: "sections",
            })
        }
    };

    for section in sections {
        import_section(coord, chunk_pos, &mut chunk, section)?;
    }

    Ok(chunk)
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
