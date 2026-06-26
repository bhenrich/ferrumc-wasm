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
use ferrumc_proto::generated::play::{ChunkDataAndLight, JoinGame, SpawnInfo};
use ferrumc_registry::dimension;
use ferrumc_sim::{SimShard, SpawnChunkTickets};
use ferrumc_storage::InMemoryStore;
use ferrumc_world::{Chunk, FlatWorldGenerator, HeightmapKind};

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
    Ok(JoinKit {
        join_game: build_join_game(config)?,
        spawn_position: config.spawn,
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
fn chunk_packet(pos: ChunkPos, chunk: &Chunk) -> anyhow::Result<ChunkDataAndLight> {
    let blob = BoundedBytes::<2_097_152>::new(chunk_data_blob(chunk))
        .map_err(|err| anyhow::anyhow!("chunk blob exceeds protocol bound: {err}"))?;
    Ok(ChunkDataAndLight::new(
        pos.x(),
        pos.z(),
        Vec::new(),
        blob,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    ))
}

/// Produces the opaque chunk-data blob carried in `ChunkDataAndLight`.
///
/// The vanilla paletted section + biome network format is a later milestone; the
/// slice ships the documented opaque-blob form. The blob is deterministic and
/// genuinely chunk-derived — the world-surface height of every column — so it
/// proves the flat terrain was generated, without claiming the wire layout a
/// vanilla client would render.
fn chunk_data_blob(chunk: &Chunk) -> Vec<u8> {
    let surface = chunk.heightmap(HeightmapKind::WorldSurface);
    let mut blob = Vec::with_capacity(16 * 16 * 4);
    for z in 0..16u8 {
        for x in 0..16u8 {
            let height = surface.height(x, z).unwrap_or(i32::MIN);
            blob.extend_from_slice(&height.to_be_bytes());
        }
    }
    blob
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
        // Every chunk blob is the per-column surface height grid (16x16 i32).
        for chunk in setup.join_kit.chunks() {
            assert_eq!(chunk.chunk_data().as_slice().len(), 16 * 16 * 4);
        }
    }
}
