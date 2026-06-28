# Invariants: ferrumc-anvil

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- All public items have rustdoc.
- Error types classify the failure mode.

## Crate-Specific

- Anvil files are untrusted. Every length read from the file (sector offsets,
  chunk payload length, NBT lengths) is bounds-checked before it is used to index
  or allocate.
- The on-disk file size is capped (`AnvilLimits::max_file_bytes`) before the file
  is read into memory; the decompressed size of each chunk is capped
  (`AnvilLimits::max_chunk_bytes`) to defeat decompression bombs.
- Chunk NBT is decoded only through `ferrumc-nbt` (hardened), never a hand-rolled
  parser.
- The importer never panics on malformed input — it returns a classified
  `AnvilError`.
- Import is read-only and version-tolerant: a block name the pinned 1.21.8
  registry does not know maps to air rather than failing the region.
- Only block states are imported (the v2 `Chunk` derives heightmaps and has no
  biome storage); biomes and stored heightmaps are intentionally not read.
