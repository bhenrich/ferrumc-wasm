//! Biome resource-location constants for the MVP flat world.
//!
//! Biome ids are *not* fixed protocol constants: they are server-assigned
//! indices into the biome registry sent during the configuration phase, so only
//! internal consistency matters. This module therefore exposes the stable
//! resource-location string (which clients key on) rather than a numeric id.

/// Resource location of the plains biome — the single biome the MVP flat world
/// places everywhere.
pub const PLAINS: &str = "minecraft:plains";
