# ferrumc-world

Pure world model. No threads, no DB, no packets.

This crate owns the block-storage value types for a chunk section:

- `BlockStateId` — a newtype over the protocol block-state id (default: air).
- `PackedArray` — a bit-packed array of fixed-width entries (Minecraft
  chunk-storage layout, non-spanning).
- `PalettedContainer` — block-state storage with an automatically promoted
  palette (single value → indirect palette → direct ids).
- `ChunkSection` — a 16x16x16 block container indexed by `LocalBlockPos`.

## Invariants

See `INVARIANTS.md` in this directory.
