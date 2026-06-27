# Invariants: ferrumc-world

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- All public items have rustdoc.
- Error types classify the failure mode.

## Crate-Specific

- `BlockStateId::AIR` is raw `0` and is the `Default`; "empty" everywhere means air.
- `PackedArray` is non-spanning: each `u64` holds `floor(64 / bits_per_entry)`
  whole entries, low bits first; an entry never crosses a word boundary.
- `PackedArray::new` accepts only `bits_per_entry` in `1..=64`.
- `PalettedContainer` indices and ids are validated before storage: an indirect
  palette index is always `< palette.len()`, and the packed width is always wide
  enough to hold every live index.
- `PalettedContainer::non_air_count` is maintained incrementally and always
  equals the true number of non-air entries (so `air_count == CAPACITY - non_air`).
- Representation only ever promotes (`Single` → `Indirect` → `Direct`); it never
  demotes, so reads stay cheap and counts stay consistent.
- A `LocalBlockPos` index is always in `0..4096`, so `ChunkSection` get/set are
  total functions that never error for any valid position.
- A `Chunk` has exactly `SECTION_COUNT` (`HEIGHT / 16 = 24`) sections, stacked
  bottom-to-top with `sections[0]` at `MIN_Y`. World `y` maps to section
  `(y - MIN_Y) / 16` and local `y` `(y - MIN_Y) % 16`; this floors correctly for
  negative `y` (`y = -64` → section 0, `y = -48` → section 1).
- `Chunk::get_block`/`set_block` take an absolute `BlockPos` and reject any
  position outside the chunk's own column or the buildable height range without
  panicking (`None` / `WorldError::BlockOutsideChunk`).
- `set_block` marks a section dirty only when the stored value actually changes;
  `DirtySections` ignores out-of-range indices and never panics.
- A `Heightmap` entry is the world `y` of the highest non-air block in a column,
  or `None` for an all-air column. For the current (all-solid) block set the
  motion-blocking and world-surface heightmaps are identical.
- `FlatWorldGenerator` is deterministic: the same `ChunkPos` always yields an
  equal `Chunk`, and the per-column profile is identical for every column and
  every chunk. Layer layout: `y = -64` bedrock, `-63..=59` stone, `60..=62`
  dirt, `63` grass, `64..=319` air.

## Network encoding (`network` module)

- `PalettedContainer::encode_network` writes the 1.21.5+ wire layout:
  `bits_per_entry` byte, palette, then the packed long array with **no length
  prefix** (the client recomputes the long count from `bits_per_entry` and
  `CAPACITY`). Single: `bpe = 0`, one `VarInt` value, no longs. Indirect:
  `VarInt` palette length + ids, then `ceil(CAPACITY / (64 / bpe))` longs. Direct:
  no palette, longs repacked to the `direct_wire_bits` argument.
- In-memory direct storage is 32 bits per entry for lossless ids, but the wire
  uses the vanilla width (15 for block states), applied only at encode time. A
  flat world never reaches the direct representation.
- `encode_chunk_section_data` emits exactly `SECTION_COUNT` (24) sections
  bottom-to-top, each `i16` non-air count + block-states container + a
  single-valued `plains` (id 0) biomes container.
- `pack_motion_blocking_heightmap` returns 37 longs (256 columns, 9 bits each,
  7 per long). Each entry is `(highest_y + 1) - MIN_Y`, or `0` for an all-air
  column.
- `ChunkLightData::full_bright` marks all `LIGHT_SECTION_COUNT` (26) sections in
  the sky-light and empty-block-light masks (`0x03FF_FFFF`), supplies 26 `0xFF`
  2048-byte sky arrays, and leaves block light and the other two masks empty.
