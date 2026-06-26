//! Dimension-type constants for the MVP overworld flat world.
//!
//! These describe the vertical extent of the single overworld dimension the
//! current milestone serves. The full 1.21.8 `dimension_type` blob (ambient
//! light, infiniburn tag, monster spawn light levels, ...) is sent as part of
//! the dynamic registry data in a later milestone; M04 only pins the geometry
//! constants other crates need to size chunk columns and clamp coordinates.

/// Resource location of the overworld dimension type.
pub const OVERWORLD: &str = "minecraft:overworld";

/// Lowest buildable Y coordinate in the overworld, inclusive (`-64`).
pub const MIN_Y: i32 = -64;

/// Total world height in blocks: the number of stacked Y layers (`384`).
///
/// Combined with [`MIN_Y`], the buildable range is `MIN_Y ..= MIN_Y + HEIGHT - 1`
/// (`-64 ..= 319`).
pub const HEIGHT: u32 = 384;
