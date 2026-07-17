# ferrumc-anvil

Vanilla **Anvil** region-file reader and import path. Loads a prebuilt vanilla
Minecraft world (the `region/r.<x>.<z>.mca` files) into `FerrumC`'s native world
model. Read-only import; this crate does not write Anvil files.

## What it does

- Parses the `.mca` region container: the 8 KiB header (1024 chunk location
  entries + 1024 timestamps), each chunk's 4-byte length + compression byte
  (`1` gzip, `2` zlib, `3` uncompressed), and decompresses the chunk body.
- Decodes the chunk NBT with the hardened [`ferrumc-nbt`] decoder.
- Unpacks each section's palette-indexed block grid and maps every palette entry
  (`Name` + `Properties`) to a numeric block-state id via
  [`ferrumc-registry`]'s `compute_state_id`.
- Imports supported block entities into the chunk's bounded block-entity map:
  regular/hanging signs with plain text on both faces, and empty
  chest/trapped-chest inventories.
- Produces [`ferrumc-world`] `Chunk`s keyed by typed `ChunkPos`.

## Scope & limitations

- **Blocks and representable block entities.** The v2 `Chunk` derives its
  heightmap from blocks and has no biome storage, so biomes and stored
  heightmaps are not imported. Unknown block-entity ids are skipped for version
  tolerance.
- **Loss is explicit.** The current public world-model API cannot reconstruct
  waxed/colored/glowing signs or non-empty chest inventories from this crate
  without violating the dependency map. Those non-default payloads are rejected
  with `UnsupportedBlockEntityData`, never silently normalized or emptied.
  Filtered sign text and unresolved chest loot tables, custom names, or locks
  are rejected for the same reason, as are generic block-entity data
  components.
- **Plain sign text.** 1.21.8 structured-NBT text components are retained only
  when they are a literal NBT string or a compound containing only `text`.
  Styled, composite, translated, selector, and other semantic component forms
  are rejected because the world sign model stores plain lines.
- **Modern format.** Targets the 1.18+ `sections[].block_states` layout. Blocks
  unknown to the pinned 1.21.8 registry map to air, so imports are tolerant of
  worlds saved by other versions. Known sign/chest payloads are decoded only
  when the chunk root carries the pinned 1.21.8 `DataVersion`; another or absent
  version is rejected rather than guessed.
- **Not supported:** LZ4 (compression `4`) and external `.mcc` chunks are
  rejected with a classified error; Anvil *writing* is out of scope.

## Hostile-input safety

Anvil files are untrusted. File size and per-chunk decompressed size are capped
([`AnvilLimits`]), every file-declared length is bounds-checked before use, and
NBT decoding runs under the `ferrumc-nbt` limits. The raw block-entity list is
also capped at the world model's 4,096-entry limit before semantic processing;
duplicate positions and schema-significant duplicate fields are rejected. Sign
text decoding has an independent byte cap and no recursive component path. No
`unwrap`/`expect` outside tests; `#![forbid(unsafe_code)]`.

## Invariants

See `INVARIANTS.md` in this directory.

[`ferrumc-nbt`]: ../ferrumc-nbt
[`ferrumc-world`]: ../ferrumc-world
[`ferrumc-registry`]: ../ferrumc-registry
