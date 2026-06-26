//! Fixed block-*state* ids for the minimal flat-world block set.
//!
//! A block-state id is the integer written into a chunk section's block palette
//! on the wire. Unlike biome and dimension-type ids — which are server-assigned
//! indices into the dynamic registries sent during the configuration phase —
//! block-state ids are **fixed protocol constants**: they must match the values
//! baked into the vanilla 1.21.8 client jar, or the client renders the wrong
//! blocks.
//!
//! The values here are the *default* states of each block (the state a freshly
//! placed block takes), taken from the vendored `blocks.json` snapshot. A
//! `#[cfg(test)]` drift guard in the crate root re-parses that snapshot and
//! asserts these constants still match, so a re-pin to a newer data version
//! cannot silently desync them.
//!
//! The flat-world generator (in `ferrumc-world`) layers these as:
//! bedrock floor, [`STONE`] fill, [`DIRT`] subsurface, [`GRASS_BLOCK`] surface,
//! and [`AIR`] above.

/// Block-state id for `minecraft:air`.
pub const AIR: u32 = 0;

/// Block-state id for `minecraft:stone`.
pub const STONE: u32 = 1;

/// Block-state id for the default state of `minecraft:grass_block`.
///
/// `grass_block` has a single `snowy` boolean property spanning state ids 8
/// (`snowy=true`) and 9 (`snowy=false`); the default is `snowy=false` → `9`.
pub const GRASS_BLOCK: u32 = 9;

/// Block-state id for `minecraft:dirt` (block id 9, single state).
pub const DIRT: u32 = 10;

/// Block-state id for `minecraft:bedrock` (block id 34, single state).
pub const BEDROCK: u32 = 85;
