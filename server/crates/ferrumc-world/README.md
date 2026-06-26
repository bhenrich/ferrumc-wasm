# ferrumc-world

Pure world model. No threads, no DB, no packets.

This crate owns the world's block-storage value types and a flat-world
generator:

- `BlockStateId` — a newtype over the protocol block-state id (default: air).
- `PackedArray` — a bit-packed array of fixed-width entries (Minecraft
  chunk-storage layout, non-spanning).
- `PalettedContainer` — block-state storage with an automatically promoted
  palette (single value → indirect palette → direct ids).
- `ChunkSection` — a 16x16x16 block container indexed by `LocalBlockPos`.
- `Chunk` — a full-height stack of `SECTION_COUNT` (24) sections spanning the
  overworld (`MIN_Y = -64`, height 384). Addressed by absolute `BlockPos`, with
  per-section dirty tracking (`DirtySections`), per-column `Heightmap`s, and
  documented placeholders for lighting (`ChunkLight`) and block entities
  (`BlockEntity`).
- `FlatWorldGenerator` — deterministically fills a `Chunk` with a flat overworld
  profile: bedrock floor, stone fill, dirt, grass surface, then air.

## Invariants

See `INVARIANTS.md` in this directory.
