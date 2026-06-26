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
