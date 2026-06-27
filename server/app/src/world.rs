//! World and simulation bring-up: load the spawn area through the sim
//! load-or-generate flow and pre-build the clientbound packets every joining
//! player receives.
//!
//! The spawn chunks are loaded once at startup into the single shard's
//! [`LoadedChunkMap`](ferrumc_sim::LoadedChunkMap) via
//! [`acquire_spawn`](ferrumc_sim::LoadedChunkMap::acquire_spawn) (try the
//! [`WorldStore`], else generate flat terrain). From the resident chunks we build
//! a shared [`JoinKit`]: the `JoinGame` packet, the spawn position, and one
//! `ChunkDataAndLight` per spawn chunk. Connection tasks clone the `Arc<JoinKit>`
//! and replay it the moment a client reaches play, so no per-connection work
//! touches the simulation shard.

use ferrumc_codec::{BoundedBytes, BoundedString};
use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_proto::generated::play::{ChunkDataAndLight, Heightmap, JoinGame, SpawnInfo};
use ferrumc_proto::types::BlockPosition;
use ferrumc_registry::dimension;
use ferrumc_sim::{SimShard, SpawnChunkTickets};
use ferrumc_storage::InMemoryStore;
use ferrumc_world::{
    encode_chunk_section_data, pack_motion_blocking_heightmap, Chunk, ChunkLightData,
    FlatWorldGenerator,
};

use crate::config::AppConfig;

/// Placeholder protocol entity id assigned to a joining player in `JoinGame`.
///
/// The simulation does not yet allocate entity ids; `1` stands in for the slice.
const PLAYER_ENTITY_ID: i32 = 1;

/// Maximum players advertised in `JoinGame`. Informational only this milestone.
const MAX_PLAYERS: i32 = 32;

/// Dimension-type registry index for the overworld in the minimal registry.
const OVERWORLD_DIMENSION_TYPE: i32 = 0;

/// Sea level reported in `SpawnInfo`, matching the flat profile's grass surface.
const SEA_LEVEL: i32 = 63;

/// Creative game mode id, sent so a barebones client has a defined mode.
const GAMEMODE_CREATIVE: u8 = 1;

/// "No previous game mode" sentinel (`-1` as an unsigned byte).
const NO_PREVIOUS_GAMEMODE: u8 = u8::MAX;

/// `MOTION_BLOCKING` heightmap type id, the one heightmap the slice transmits.
///
/// The 1.21.5+ array-form heightmap is keyed by numeric type; `4` selects
/// `MOTION_BLOCKING` (the highest block that blocks motion or contains a fluid).
const MOTION_BLOCKING_HEIGHTMAP: i32 = 4;

/// The clientbound packets replayed to every player the instant they reach play.
///
/// Built once at startup from the resident spawn chunks and shared behind an
/// [`Arc`](std::sync::Arc). It is exactly the keystone payload of the vertical
/// slice: the `JoinGame` that puts the client in play, the absolute spawn
/// position to synchronize to, and the spawn-area chunk column packets.
#[derive(Debug, Clone)]
pub(crate) struct JoinKit {
    /// The `JoinGame` packet announcing the (flat) overworld.
    join_game: JoinGame,
    /// The world-spawn position the player is placed at.
    spawn_position: ferrumc_math::Vec3,
    /// The chunk the spawn point falls in, sent as `Set Center Chunk` so the
    /// client centres its view square on the spawn area before the chunks arrive.
    spawn_chunk: ChunkPos,
    /// The block-aligned world spawn, sent as `Set Default Spawn Position`.
    spawn_block: BlockPosition,
    /// One `ChunkDataAndLight` packet per resident spawn chunk.
    chunks: Vec<ChunkDataAndLight>,
}

impl JoinKit {
    /// The `JoinGame` packet to send first.
    pub(crate) fn join_game(&self) -> &JoinGame {
        &self.join_game
    }

    /// The absolute world-spawn position to synchronize the client to.
    pub(crate) fn spawn_position(&self) -> ferrumc_math::Vec3 {
        self.spawn_position
    }

    /// The chunk the spawn point falls in (the `Set Center Chunk` target).
    pub(crate) fn spawn_chunk(&self) -> ChunkPos {
        self.spawn_chunk
    }

    /// The block-aligned world spawn (the `Set Default Spawn Position` target).
    pub(crate) fn spawn_block(&self) -> BlockPosition {
        self.spawn_block
    }

    /// The spawn-area chunk packets, in deterministic order.
    pub(crate) fn chunks(&self) -> &[ChunkDataAndLight] {
        &self.chunks
    }
}

/// The simulation pieces produced by [`build_world`], ready to drive.
///
/// The shard already owns the resident spawn chunks; `join_kit` is the shared
/// clientbound payload derived from them.
pub(crate) struct WorldSetup {
    /// The single simulation shard owning the spawn area.
    pub(crate) shard: SimShard,
    /// The shared join payload replayed to connecting players.
    pub(crate) join_kit: std::sync::Arc<JoinKit>,
}

/// Returns the chunk position the spawn point falls in.
fn spawn_center_chunk(config: &AppConfig) -> ChunkPos {
    BlockPos::new(
        config.spawn.x.floor() as i32,
        config.spawn.y.floor() as i32,
        config.spawn.z.floor() as i32,
    )
    .to_chunk_pos()
}

/// Builds the single shard, loads the spawn area through the load-or-generate
/// flow, and pre-builds the shared [`JoinKit`].
///
/// # Errors
///
/// Returns an error if a spawn chunk fails to load from the store, if a spawn
/// chunk is unexpectedly absent after acquisition, or if the dimension name or a
/// chunk blob exceeds its protocol bound.
pub(crate) async fn build_world(
    config: &AppConfig,
    shard_pos: ferrumc_math::ShardPos,
) -> anyhow::Result<WorldSetup> {
    let store = InMemoryStore::new();
    let generator = FlatWorldGenerator::new();
    let spawn = SpawnChunkTickets::new(spawn_center_chunk(config), config.spawn_chunk_radius);

    let mut shard = SimShard::new(shard_pos);
    shard
        .loaded_chunks_mut()
        .acquire_spawn(&store, &generator, &spawn)
        .await?;

    let join_kit = std::sync::Arc::new(build_join_kit(config, &shard, &spawn)?);
    Ok(WorldSetup { shard, join_kit })
}

/// Assembles the [`JoinKit`] from the shard's resident spawn chunks.
fn build_join_kit(
    config: &AppConfig,
    shard: &SimShard,
    spawn: &SpawnChunkTickets,
) -> anyhow::Result<JoinKit> {
    let mut chunks = Vec::with_capacity(spawn.chunk_count());
    for pos in spawn.positions() {
        let chunk = shard.loaded_chunks().get(pos).ok_or_else(|| {
            anyhow::anyhow!("spawn chunk ({}, {}) not resident", pos.x(), pos.z())
        })?;
        chunks.push(chunk_packet(pos, chunk)?);
    }
    let spawn_block = BlockPosition::new(
        config.spawn.x.floor() as i32,
        config.spawn.y.floor() as i32,
        config.spawn.z.floor() as i32,
    );
    Ok(JoinKit {
        join_game: build_join_game(config)?,
        spawn_position: config.spawn,
        spawn_chunk: spawn_center_chunk(config),
        spawn_block,
        chunks,
    })
}

/// Builds the `JoinGame` packet for the flat overworld.
fn build_join_game(config: &AppConfig) -> anyhow::Result<JoinGame> {
    let overworld = BoundedString::<32_767>::new(dimension::OVERWORLD.to_string())
        .map_err(|err| anyhow::anyhow!("overworld dimension name invalid: {err}"))?;
    let spawn_info = SpawnInfo::new(
        OVERWORLD_DIMENSION_TYPE,
        overworld.clone(),
        0,
        GAMEMODE_CREATIVE,
        NO_PREVIOUS_GAMEMODE,
        false,
        true,
        None,
        0,
        SEA_LEVEL,
    );
    Ok(JoinGame::new(
        PLAYER_ENTITY_ID,
        false,
        vec![overworld],
        MAX_PLAYERS,
        config.view_distance,
        config.simulation_distance,
        false,
        true,
        false,
        spawn_info,
        false,
    ))
}

/// Builds the `ChunkDataAndLight` packet for one resident chunk.
///
/// The payload is the real 1.21.8 wire form: the paletted section blob from
/// [`encode_chunk_section_data`], the `MOTION_BLOCKING` heightmap packed by
/// [`pack_motion_blocking_heightmap`], and the full-bright sky light from
/// [`ChunkLightData::full_bright`] (no block light). Block entities are empty.
///
/// # Errors
///
/// Returns an error if the section blob or a packed heightmap cannot be produced
/// (only for out-of-range block data, which the flat generator never emits), or
/// if an encoded buffer exceeds its protocol length bound.
fn chunk_packet(pos: ChunkPos, chunk: &Chunk) -> anyhow::Result<ChunkDataAndLight> {
    let blob = BoundedBytes::<2_097_152>::new(encode_chunk_section_data(chunk)?)
        .map_err(|err| anyhow::anyhow!("chunk blob exceeds protocol bound: {err}"))?;

    let heightmaps = vec![Heightmap::new(
        MOTION_BLOCKING_HEIGHTMAP,
        pack_motion_blocking_heightmap(chunk)?,
    )];

    // Lighting is not computed yet: flood the column with full sky light and no
    // block light, which is exactly how a flat overworld renders.
    let light = ChunkLightData::full_bright();
    let sky_light = light
        .sky_light()
        .iter()
        .map(|section| BoundedBytes::<2048>::new(section.clone()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| anyhow::anyhow!("sky-light section exceeds protocol bound: {err}"))?;

    Ok(ChunkDataAndLight::new(
        pos.x(),
        pos.z(),
        heightmaps,
        blob,
        Vec::new(),
        light.sky_light_mask().to_vec(),
        light.block_light_mask().to_vec(),
        light.empty_sky_light_mask().to_vec(),
        light.empty_block_light_mask().to_vec(),
        sky_light,
        Vec::new(),
    ))
}

#[cfg(test)]
mod tests {
    use ferrumc_math::ShardPos;

    use super::*;
    use crate::config::AppConfig;

    #[tokio::test]
    async fn build_world_loads_spawn_and_builds_kit() {
        let config = AppConfig::default();
        let setup = build_world(&config, ShardPos::new(0, 0))
            .await
            .expect("world builds");

        // Radius 2 -> a 5x5 spawn square, all resident, all with a chunk packet.
        let expected = (2 * usize::from(config.spawn_chunk_radius) + 1).pow(2);
        assert_eq!(setup.shard.loaded_chunks().loaded_count(), expected);
        assert_eq!(setup.join_kit.chunks().len(), expected);
        assert_eq!(setup.join_kit.spawn_position(), config.spawn);
        assert_eq!(
            setup.join_kit.join_game().view_distance(),
            config.view_distance
        );
        // The spawn point (8, 64, 8) falls in chunk (0, 0) and block (8, 64, 8).
        assert_eq!(
            setup.join_kit.spawn_chunk(),
            ferrumc_math::ChunkPos::new(0, 0)
        );
        let block = setup.join_kit.spawn_block();
        assert_eq!((block.x(), block.y(), block.z()), (8, 64, 8));

        // Every chunk packet carries the real wire payload: a non-empty paletted
        // section blob, the 37-long MOTION_BLOCKING heightmap, and full-bright sky
        // light over all 26 light sections.
        for chunk in setup.join_kit.chunks() {
            assert!(!chunk.chunk_data().as_slice().is_empty());

            let heightmaps = chunk.heightmaps();
            assert_eq!(heightmaps.len(), 1);
            assert_eq!(heightmaps[0].kind(), 4);
            assert_eq!(heightmaps[0].data().len(), 37);

            assert_eq!(chunk.sky_light_mask(), &[0x03FF_FFFF]);
            assert_eq!(chunk.empty_block_light_mask(), &[0x03FF_FFFF]);
            assert!(chunk.block_light_mask().is_empty());
            assert_eq!(chunk.sky_light().len(), 26);
            assert!(chunk.block_light().is_empty());
            for section in chunk.sky_light() {
                assert_eq!(section.as_slice().len(), 2048);
            }
        }
    }
}
