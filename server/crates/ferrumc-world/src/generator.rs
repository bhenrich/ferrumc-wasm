//! A deterministic flat-world chunk generator.

use ferrumc_math::{ChunkPos, LocalBlockPos};
use ferrumc_registry::{block_state, dimension};

use crate::block_state::BlockStateId;
use crate::chunk::{self, Chunk, SECTION_COUNT, SECTION_EDGE};

/// World `y` of the bedrock floor: the very bottom of the buildable range
/// (`dimension::MIN_Y`, `-64`).
const BEDROCK_Y: i32 = dimension::MIN_Y;

/// World `y` of the grass surface layer in the default flat profile.
const DEFAULT_SURFACE_Y: i32 = 63;

/// Number of dirt layers directly beneath the grass surface in the default flat
/// profile.
const DEFAULT_DIRT_LAYERS: i32 = 3;

/// A deterministic generator that fills a [`Chunk`] with a flat overworld
/// profile, identical for every column and every chunk position.
///
/// # Layer layout
///
/// With `MIN_Y = -64` and the default profile (grass surface at `y = 63`, three
/// dirt layers), each column is generated bottom-to-top as:
///
/// | World `y`      | Block         |
/// |----------------|---------------|
/// | `-64`          | `bedrock`     |
/// | `-63 ..= 59`   | `stone`       |
/// | `60 ..= 62`    | `dirt`        |
/// | `63`           | `grass_block` |
/// | `64 ..= 319`   | `air`         |
///
/// The block ids come from [`ferrumc_registry::block_state`]. The resulting
/// chunk has every section that received a non-air block marked dirty (a freshly
/// generated chunk has not yet been persisted or sent), so the surface and lower
/// sections are dirty while the all-air sections above stay clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlatWorldGenerator {
    /// World `y` of the grass surface layer.
    surface_y: i32,
    /// Number of dirt layers directly beneath the surface.
    dirt_layers: i32,
}

impl FlatWorldGenerator {
    /// Creates a generator with the default flat profile documented on
    /// [`FlatWorldGenerator`] (grass surface at `y = 63`, three dirt layers).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            surface_y: DEFAULT_SURFACE_Y,
            dirt_layers: DEFAULT_DIRT_LAYERS,
        }
    }

    /// Generates the chunk at `pos`, filling it with the flat profile.
    #[must_use]
    pub fn generate(&self, pos: ChunkPos) -> Chunk {
        let mut chunk = Chunk::new(pos);
        for section_index in 0..SECTION_COUNT {
            let Some(base_y) = chunk::section_base_y(section_index) else {
                continue;
            };
            for local_y in 0..SECTION_EDGE {
                let world_y = base_y + i32::from(local_y);
                let state = self.block_at(world_y);
                if state.is_air() {
                    continue;
                }
                // The profile is uniform across the column, so every (x, z) in
                // this layer gets the same block.
                for z in 0..SECTION_EDGE {
                    for x in 0..SECTION_EDGE {
                        if let Some(local) = LocalBlockPos::new(x, local_y, z) {
                            chunk.set_local(section_index, local, state);
                        }
                    }
                }
            }
        }
        chunk
    }

    /// Returns the block-state id this profile places at world height `y`.
    fn block_at(self, y: i32) -> BlockStateId {
        // Lowest dirt layer; stone fills everything below it (above bedrock).
        let dirt_floor = self.surface_y - self.dirt_layers;
        if y == BEDROCK_Y {
            BlockStateId::new(block_state::BEDROCK)
        } else if y > self.surface_y {
            BlockStateId::AIR
        } else if y == self.surface_y {
            BlockStateId::new(block_state::GRASS_BLOCK)
        } else if y >= dirt_floor {
            BlockStateId::new(block_state::DIRT)
        } else {
            BlockStateId::new(block_state::STONE)
        }
    }
}

impl Default for FlatWorldGenerator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{FlatWorldGenerator, BEDROCK_Y, DEFAULT_SURFACE_Y};
    use crate::block_state::BlockStateId;
    use crate::heightmap::HeightmapKind;
    use ferrumc_math::{BlockPos, ChunkPos};
    use ferrumc_registry::block_state;

    fn state(name: u32) -> BlockStateId {
        BlockStateId::new(name)
    }

    fn block(chunk: &crate::Chunk, x: i32, y: i32, z: i32) -> BlockStateId {
        chunk
            .get_block(BlockPos::new(x, y, z))
            .expect("position within the generated chunk")
    }

    #[test]
    fn flat_chunk_has_expected_blocks_at_representative_heights() {
        let chunk = FlatWorldGenerator::new().generate(ChunkPos::ORIGIN);

        // Bedrock floor at the very bottom.
        assert_eq!(block(&chunk, 0, BEDROCK_Y, 0), state(block_state::BEDROCK));
        // Stone fill just above bedrock and well below the surface.
        assert_eq!(
            block(&chunk, 8, BEDROCK_Y + 1, 8),
            state(block_state::STONE)
        );
        assert_eq!(block(&chunk, 8, 0, 8), state(block_state::STONE));
        assert_eq!(block(&chunk, 8, 59, 8), state(block_state::STONE));
        // Three dirt layers below the surface.
        assert_eq!(block(&chunk, 1, 60, 1), state(block_state::DIRT));
        assert_eq!(block(&chunk, 1, 62, 1), state(block_state::DIRT));
        // Grass surface.
        assert_eq!(
            block(&chunk, 15, DEFAULT_SURFACE_Y, 15),
            state(block_state::GRASS_BLOCK)
        );
        // Air above the surface, including the very top of the world.
        assert_eq!(
            block(&chunk, 0, DEFAULT_SURFACE_Y + 1, 0),
            BlockStateId::AIR
        );
        assert_eq!(block(&chunk, 0, 200, 0), BlockStateId::AIR);
        assert_eq!(block(&chunk, 0, 319, 0), BlockStateId::AIR);
    }

    #[test]
    fn every_column_is_identical() {
        let chunk = FlatWorldGenerator::new().generate(ChunkPos::ORIGIN);
        for z in 0..16 {
            for x in 0..16 {
                assert_eq!(block(&chunk, x, BEDROCK_Y, z), state(block_state::BEDROCK));
                assert_eq!(
                    block(&chunk, x, DEFAULT_SURFACE_Y, z),
                    state(block_state::GRASS_BLOCK)
                );
                assert_eq!(
                    block(&chunk, x, DEFAULT_SURFACE_Y + 1, z),
                    BlockStateId::AIR
                );
            }
        }
    }

    #[test]
    fn generation_is_deterministic_across_positions() {
        let generator = FlatWorldGenerator::new();
        let a = generator.generate(ChunkPos::new(0, 0));
        let b = generator.generate(ChunkPos::new(0, 0));
        assert_eq!(a, b);
        // A different position differs only by its `pos`, not its blocks.
        let c = generator.generate(ChunkPos::new(-5, 7));
        for z in 0..16 {
            for x in 0..16 {
                // Compare same local column at the surface; absolute coords differ.
                let surface_a = block(&a, x, DEFAULT_SURFACE_Y, z);
                let surface_c = c
                    .get_block(BlockPos::new(-5 * 16 + x, DEFAULT_SURFACE_Y, 7 * 16 + z))
                    .expect("within chunk c");
                assert_eq!(surface_a, surface_c);
            }
        }
    }

    #[test]
    fn heightmap_matches_the_grass_surface() {
        let chunk = FlatWorldGenerator::new().generate(ChunkPos::ORIGIN);
        let surface = chunk.heightmap(HeightmapKind::WorldSurface);
        let motion = chunk.heightmap(HeightmapKind::MotionBlocking);
        for z in 0..16u8 {
            for x in 0..16u8 {
                assert_eq!(surface.height(x, z), Some(DEFAULT_SURFACE_Y));
                assert_eq!(motion.height(x, z), Some(DEFAULT_SURFACE_Y));
            }
        }
    }

    #[test]
    fn generated_chunk_marks_filled_sections_dirty() {
        let chunk = FlatWorldGenerator::new().generate(ChunkPos::ORIGIN);
        assert!(chunk.dirty_sections().any());
        // Section 0 (bedrock + stone) is dirty.
        assert!(chunk.is_section_dirty(0));
        // The surface sits in section ((63 - (-64)) / 16) == 7, which is dirty.
        assert!(chunk.is_section_dirty(7));
        // The topmost section (y 304..=319) is pure air and stays clean.
        assert!(!chunk.is_section_dirty(crate::SECTION_COUNT - 1));
    }
}
