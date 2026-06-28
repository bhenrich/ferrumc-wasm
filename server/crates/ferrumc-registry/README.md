# ferrumc-registry

Pinned Minecraft 1.21.8 registry data for the server, exposed as deterministic,
hardcoded runtime constants.

The runtime is dependency-free and does no I/O or JSON parsing at startup: every
value is either a `const` baked at compile time (block/biome/dimension) or a
`static` array generated at build time from a vendored snapshot (items). The
vendored upstream snapshots (`fixtures/protocol/1_21_8/` and this crate's
`data/`) are parsed only by `build.rs` (item tables) and by `#[cfg(test)]` drift
guards that re-parse the real data and assert the generated tables still match,
so a re-pin to a newer data version cannot silently desync them.

## What it provides

- Version constants: `MINECRAFT_VERSION`, `PROTOCOL_VERSION`, `DATA_VERSION`.
- `block_state` — fixed block-*state* (palette) ids for the flat-world block set.
- `default_block_state_id(name)` — resource-location → default state id lookup.
- `dimension` — overworld geometry constants (`OVERWORLD`, `MIN_Y`, `HEIGHT`).
- `biome` — biome resource-location constants (`PLAINS`).
- `item` — item lookups: `lookup_item_protocol_id`, `lookup_item_name`,
  `item_max_stack`, `item_to_block_state` (generated from `data/items.json` +
  `data/item_to_block_mapping.json`).

## Data provenance

Block-state ids are fixed protocol constants and must match the vanilla 1.21.8
client jar. They are derived from PrismarineJS/minecraft-data; see
`fixtures/protocol/1_21_8/manifest.toml` for the exact pinned commit and
checksums, and `fixtures/protocol/1_21_8/NOTICE` for attribution.

Biome and dimension-type ids are *not* fixed constants — they are server-assigned
indices into the dynamic registries sent during the configuration phase — so this
crate exposes their stable resource-location strings rather than numeric ids.

Within the crate map this crate may depend only on `ferrumc-core` (the runtime
currently needs nothing from it). It must never depend on the simulation,
networking, or storage lanes.
