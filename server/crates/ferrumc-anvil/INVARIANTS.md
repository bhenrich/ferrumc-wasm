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
- Supported block entities are inserted only through `Chunk::set_block_entity`.
  The raw list is rejected above `MAX_BLOCK_ENTITIES`; duplicate supported
  positions and duplicate schema-significant NBT fields are rejected.
- Unknown block-entity ids are skipped for version tolerance. A malformed
  supported sign/chest is a classified error, not a silently dropped entity.
- Known block-entity payloads are decoded only when the root `DataVersion`
  equals the pinned 1.21.8 value. Other or absent versions are rejected for
  known ids rather than interpreted under the wrong disk schema.
- Imported signs preserve only bounded 1.21.8 structured-NBT literal strings or
  single-`text` compounds that the public world model can represent.
  Non-default wax/color/glow, filtered messages, composite, and semantic
  text-component forms are rejected rather than normalized.
- Empty chest inventories are imported. A non-empty `Items` list is rejected
  rather than emptied because `ferrumc-anvil` may not add the out-of-map
  `ferrumc-items` dependency or duplicate the world storage codec. Unresolved
  loot tables, custom names, locks, and generic data components are likewise
  rejected rather than lost.
- The v2 `Chunk` derives heightmaps and has no biome storage; biomes and stored
  heightmaps are intentionally not read.
