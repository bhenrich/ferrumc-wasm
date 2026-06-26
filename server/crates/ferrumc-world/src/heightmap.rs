//! Per-column surface heightmaps for a chunk.

use ferrumc_math::LocalBlockPos;

use crate::block_state::BlockStateId;
use crate::chunk::{self, Chunk, SECTION_COUNT, SECTION_EDGE};

/// Number of `(x, z)` columns in a chunk's 16x16 heightmap grid.
const COLUMNS: usize = 16 * 16;

/// Which heightmap to compute.
///
/// Minecraft maintains several heightmaps per chunk; this crate models the two
/// the network protocol cares about for the flat overworld:
///
/// - [`HeightmapKind::MotionBlocking`] — the highest block that blocks motion.
/// - [`HeightmapKind::WorldSurface`] — the highest non-air block.
///
/// For the block set this crate generates (bedrock, stone, dirt, grass) every
/// non-air block both occludes the sky and blocks motion, so the two heightmaps
/// coincide and are computed identically as "highest non-air block per column".
/// They are kept as distinct kinds so callers request the right one and the
/// distinction can be refined when non-solid blocks (water, plants, ...) arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeightmapKind {
    /// Highest block in each column that blocks motion.
    MotionBlocking,
    /// Highest non-air block in each column (the visible world surface).
    WorldSurface,
}

/// Returns `true` if `state` counts as a surface block for `kind`.
///
/// Both kinds currently reduce to "not air"; see [`HeightmapKind`].
pub(crate) const fn counts_as_surface(kind: HeightmapKind, state: BlockStateId) -> bool {
    match kind {
        // One arm for both kinds: every modelled block is solid, so "blocks
        // motion" and "occludes the sky" are the same predicate today. Adding a
        // kind later will force this match to be revisited.
        HeightmapKind::MotionBlocking | HeightmapKind::WorldSurface => !state.is_air(),
    }
}

/// A computed surface heightmap for a single chunk.
///
/// Each entry is the world `y` of the highest non-air block in that column, or
/// `None` if the column is entirely air. The heightmap is a snapshot computed by
/// [`crate::Chunk::heightmap`]; it is not kept in sync with later block edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heightmap {
    /// Highest non-air block `y` per column, indexed by `z * 16 + x`.
    heights: [Option<i32>; COLUMNS],
}

impl Heightmap {
    /// Computes the heightmap of `chunk` for the requested [`HeightmapKind`].
    ///
    /// Each column is scanned from the top of the world down; the first block
    /// that [`counts_as_surface`] for `kind` gives the column's height.
    pub(crate) fn compute(chunk: &Chunk, kind: HeightmapKind) -> Self {
        let mut heights = [None; COLUMNS];
        for (column, slot) in heights.iter_mut().enumerate() {
            // The array is laid out `z * 16 + x`, so recover `(x, z)` from the
            // flat index. Both are `< 16`, so the conversions never fail.
            let edge = usize::from(SECTION_EDGE);
            if let (Ok(x), Ok(z)) = (u8::try_from(column % edge), u8::try_from(column / edge)) {
                *slot = column_surface(chunk, x, z, kind);
            }
        }
        Self { heights }
    }

    /// Returns the world `y` of the highest non-air block in column `(x, z)`, or
    /// `None` if the column is entirely air.
    ///
    /// `x` and `z` are local column coordinates in `0..16`; an out-of-range
    /// coordinate returns `None` rather than panicking.
    #[must_use]
    pub fn height(&self, x: u8, z: u8) -> Option<i32> {
        if x >= 16 || z >= 16 {
            return None;
        }
        let index = usize::from(z) * 16 + usize::from(x);
        // `index < 16 * 16 == COLUMNS`, so `get` always hits; `flatten` collapses
        // the `Option<&Option<i32>>` to the stored `Option<i32>`.
        self.heights.get(index).copied().flatten()
    }
}

/// Returns the world `y` of the highest block in column `(x, z)` of `chunk` that
/// counts as a surface block for `kind`, or `None` if the column has none.
fn column_surface(chunk: &Chunk, x: u8, z: u8, kind: HeightmapKind) -> Option<i32> {
    for section_index in (0..SECTION_COUNT).rev() {
        let Some(section) = chunk.section(section_index) else {
            continue;
        };
        if section.is_empty() {
            continue;
        }
        let Some(base_y) = chunk::section_base_y(section_index) else {
            continue;
        };
        for local_y in (0..SECTION_EDGE).rev() {
            let Some(local) = LocalBlockPos::new(x, local_y, z) else {
                continue;
            };
            if counts_as_surface(kind, section.get(local)) {
                return Some(base_y + i32::from(local_y));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{counts_as_surface, HeightmapKind, COLUMNS};
    use crate::block_state::BlockStateId;

    #[test]
    fn columns_is_256() {
        assert_eq!(COLUMNS, 256);
    }

    #[test]
    fn surface_predicate_treats_non_air_as_surface_for_both_kinds() {
        for kind in [HeightmapKind::MotionBlocking, HeightmapKind::WorldSurface] {
            assert!(!counts_as_surface(kind, BlockStateId::AIR));
            assert!(counts_as_surface(kind, BlockStateId::new(1)));
        }
    }
}
