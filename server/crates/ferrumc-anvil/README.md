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
- Produces [`ferrumc-world`] `Chunk`s keyed by typed `ChunkPos`.

## Scope & limitations

- **Blocks only.** The v2 `Chunk` derives its heightmap from blocks and has no
  biome storage, so biomes and stored heightmaps are not imported.
- **Modern format.** Targets the 1.18+ `sections[].block_states` layout. Blocks
  unknown to the pinned 1.21.8 registry map to air, so imports are tolerant of
  worlds saved by other versions.
- **Not supported:** LZ4 (compression `4`) and external `.mcc` chunks are
  rejected with a classified error; Anvil *writing* is out of scope.

## Hostile-input safety

Anvil files are untrusted. File size and per-chunk decompressed size are capped
([`AnvilLimits`]), every file-declared length is bounds-checked before use, and
NBT decoding runs under the `ferrumc-nbt` limits. No `unwrap`/`expect` outside
tests; `#![forbid(unsafe_code)]`.

## Invariants

See `INVARIANTS.md` in this directory.

[`ferrumc-nbt`]: ../ferrumc-nbt
[`ferrumc-world`]: ../ferrumc-world
[`ferrumc-registry`]: ../ferrumc-registry
