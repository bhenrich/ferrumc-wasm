//! World and simulation bring-up: load the spawn area through the sim
//! load-or-generate flow and pre-build the clientbound packets every joining
//! player receives.
//!
//! The spawn chunks are loaded once at startup into the single shard's
//! [`LoadedChunkMap`](ferrumc_sim::LoadedChunkMap) via
//! [`acquire_spawn`](ferrumc_sim::LoadedChunkMap::acquire_spawn) (try the
//! [`WorldStore`], else generate flat terrain) and kept resident by a `Spawn`
//! ticket. From them we build a shared [`JoinKit`]: the `JoinGame` packet, the
//! spawn position, and the *list of spawn chunk positions*. Connection tasks clone
//! the `Arc<JoinKit>` and replay the framing the moment a client reaches play.
//!
//! The spawn-area chunk blobs themselves are **not** cached in the kit: they are
//! fetched live from the resident shard chunks at join time (via a
//! [`StreamChunks`](crate::driver::SimCommand::StreamChunks) round-trip), so a
//! block another player placed in a spawn chunk is reflected on every (re)join —
//! a cached snapshot would replay the stale pre-edit terrain instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ferrumc_anvil::{region_pos_from_path, Region};
use ferrumc_codec::{BoundedBytes, BoundedString};
use ferrumc_core::{DimensionId, WorldId};
use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_proto::generated::play::{ChunkDataAndLight, Heightmap, JoinGame, SpawnInfo};
use ferrumc_proto::types::BlockPosition;
use ferrumc_registry::dimension;

use ferrumc_sim::{RegionLimits, SimShard, SpawnChunkTickets};
use ferrumc_storage::{
    ChunkKey, ChunkRecord, InMemoryStore, PlayerStore, RedbStore, SchemaVersion, WorldStore,
};
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

/// Maximum number of imported chunks buffered before a flush to the store during
/// an Anvil import.
///
/// The import streams region files one at a time and writes the decoded chunks to
/// the store in batches of this size. It bounds the peak save-batch allocation —
/// comfortably under [`ferrumc_storage::MAX_SAVE_BATCH`] (4096) — so a large
/// multi-region map never forces a single giant write nor materialises the whole
/// world in one buffer.
const ANVIL_IMPORT_BATCH: usize = 256;

/// The shared join payload replayed to every player the instant they reach play.
///
/// Built once at startup and shared behind an [`Arc`](std::sync::Arc). It is the
/// keystone framing of the vertical slice: the `JoinGame` that puts the client in
/// play, the absolute spawn position to synchronize to, and the *positions* of the
/// spawn-area chunk columns.
///
/// The chunk blobs are deliberately **not** stored here. A joining connection
/// fetches them live from the resident shard chunks at join time (see
/// [`send_join_kit`](crate::connection)), so an edit placed in a spawn chunk by a
/// previous session is reflected on the next join; a cached snapshot would replay
/// the stale pre-edit terrain.
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
    /// The positions of the spawn-area chunk columns, in deterministic order. The
    /// connection fetches each column's live blob from the shard at join time.
    spawn_positions: Vec<ChunkPos>,
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

    /// The chunk positions covered by the spawn batch sent at join.
    ///
    /// A connection both fetches the live blob for each of these from the shard at
    /// join time and seeds its per-player loaded-chunk set with them, so chunk
    /// streaming never re-sends a chunk the client already received at join.
    pub(crate) fn chunk_positions(&self) -> impl Iterator<Item = ChunkPos> + '_ {
        self.spawn_positions.iter().copied()
    }
}

/// The simulation pieces produced by [`build_world`], ready to drive.
///
/// The shard already owns the resident spawn chunks; `join_kit` is the shared
/// clientbound payload derived from them. The `store` and `generator` are handed
/// to the driver so it can run the load-or-generate flow for chunks streamed in
/// around a moving player (the same store the spawn area was loaded from, so
/// chunk persistence stays consistent).
pub(crate) struct WorldSetup {
    /// The single simulation shard owning the spawn area.
    pub(crate) shard: SimShard,
    /// The shared join payload replayed to connecting players.
    pub(crate) join_kit: std::sync::Arc<JoinKit>,
    /// The chunk store the driver and storage worker share, behind the
    /// [`WorldStore`] trait so the backend (durable redb or in-memory) is chosen
    /// once at startup and the rest of the app stays backend-agnostic.
    pub(crate) store: Arc<dyn WorldStore>,
    /// The per-player record store, the *same* backend object as [`store`] viewed
    /// through the [`PlayerStore`] trait. Connection tasks load a player's saved
    /// state on join and save it on leave/shutdown through this handle.
    pub(crate) player_store: Arc<dyn PlayerStore>,
    /// The terrain generator the driver uses on a store miss.
    pub(crate) generator: FlatWorldGenerator,
}

/// Returns the chunk position the spawn point falls in.
fn spawn_center_chunk(config: &AppConfig) -> ChunkPos {
    let spawn = config.spawn();
    BlockPos::new(
        spawn.x.floor() as i32,
        spawn.y.floor() as i32,
        spawn.z.floor() as i32,
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
    let OpenedStore {
        store,
        player_store,
    } = open_store(config)?;
    let generator = FlatWorldGenerator::new();
    let spawn = SpawnChunkTickets::new(spawn_center_chunk(config), config.spawn_chunk_radius());

    let mut shard = SimShard::new(shard_pos);
    // Bound the region build commands (/fill, /replace, /undo) with the operator's
    // configured caps; the shard re-checks these defensively on every region edit.
    shard.set_region_limits(RegionLimits {
        max_volume: config.max_region_fill_volume(),
        max_undo_entries: config.region_undo_history(),
        // The per-tick application budget keeps its built-in default; only the
        // operator-facing volume + undo caps are configurable.
        ..RegionLimits::default()
    });

    // If a prebuilt Anvil map is configured, import its region files into the
    // store BEFORE the spawn area is acquired, so the spawn chunks (and any later
    // streamed chunks) come from the loaded map instead of flat terrain. The
    // import keys chunks for the shard's own world/dimension and stamps them with
    // its chunk schema version, so the load-or-generate flow finds them. A
    // missing/empty/malformed map aborts startup here (before connections), rather
    // than silently serving flat terrain.
    if let Some(region_dir) = config.world().anvil_import_dir() {
        let loaded = shard.loaded_chunks();
        let imported = import_anvil_world(
            &*store,
            region_dir,
            loaded.world(),
            loaded.dimension(),
            loaded.schema_version(),
        )
        .await?;
        tracing::info!(
            chunks = imported,
            dir = %region_dir.display(),
            "imported Anvil world into the store",
        );
    }

    shard
        .loaded_chunks_mut()
        .acquire_spawn(&*store, &generator, &spawn)
        .await?;

    let join_kit = std::sync::Arc::new(build_join_kit(config, &spawn)?);
    Ok(WorldSetup {
        shard,
        join_kit,
        store,
        player_store,
        generator,
    })
}

/// The two trait-object views of the one store backend opened at startup.
///
/// Both `Arc`s point at the *same* concrete backend object, so chunk and player
/// records share one redb file (or one in-memory map). They are separate handles
/// only because the driver/storage-worker reach the backend through [`WorldStore`]
/// while connection tasks reach it through [`PlayerStore`].
struct OpenedStore {
    store: Arc<dyn WorldStore>,
    player_store: Arc<dyn PlayerStore>,
}

/// Opens the world store the runtime will use, selected by
/// [`AppConfig::world_dir`].
///
/// `Some(dir)` opens the durable redb-backed [`RedbStore`] at `dir/world.redb`
/// (creating the directory if needed) — the runtime default. `None` constructs an
/// [`InMemoryStore`], used by tests and any ephemeral run. Either way the concrete
/// backend is erased to the [`WorldStore`] and [`PlayerStore`] trait objects so
/// the driver, storage worker, and connection tasks stay backend-agnostic; the two
/// handles share the single backend object (one redb file / one in-memory map).
///
/// # Errors
///
/// Returns an error if the world directory cannot be created or the redb file
/// cannot be opened.
fn open_store(config: &AppConfig) -> anyhow::Result<OpenedStore> {
    if let Some(dir) = config.world_dir() {
        std::fs::create_dir_all(dir)
            .map_err(|err| anyhow::anyhow!("creating world directory {}: {err}", dir.display()))?;
        let path = dir.join("world.redb");
        let store = RedbStore::open(&path)
            .map_err(|err| anyhow::anyhow!("opening world store {}: {err}", path.display()))?;
        Ok(opened_store(Arc::new(store)))
    } else {
        Ok(opened_store(Arc::new(InMemoryStore::new())))
    }
}

/// Erases one concrete backend `Arc` into the [`WorldStore`] and [`PlayerStore`]
/// trait-object handles, both sharing the single backend object.
fn opened_store<T: WorldStore + PlayerStore + 'static>(store: Arc<T>) -> OpenedStore {
    OpenedStore {
        store: Arc::clone(&store) as Arc<dyn WorldStore>,
        player_store: store as Arc<dyn PlayerStore>,
    }
}

/// Imports every chunk from every `r.<x>.<z>.mca` region file in `region_dir`
/// into `store`, keyed for `world`/`dimension` and stamped with `schema_version`.
///
/// Returns the number of chunks imported.
///
/// # Bounded memory
///
/// The import is bounded so a large map cannot exhaust memory: region files are
/// processed one at a time, each read, decompressed, and NBT-parsed off the async
/// runtime via [`tokio::task::spawn_blocking`] (the work is blocking file I/O plus
/// CPU-bound parsing, which must never run on a Tokio worker), and the decoded
/// chunks are written to `store` in [`ANVIL_IMPORT_BATCH`]-sized batches. Peak
/// memory is therefore roughly one region's decoded chunks plus one save batch,
/// never the whole world. The Anvil reader itself caps each region file's on-disk
/// size and each chunk's decompressed size (see `ferrumc_anvil::AnvilLimits`).
///
/// # Errors
///
/// Returns an error if `region_dir` cannot be read, if it holds no region files,
/// if a region file is malformed (truncated, bad compression, corrupt NBT, …), or
/// if a store write fails. A malformed map aborts startup rather than silently
/// serving partial or flat terrain.
async fn import_anvil_world(
    store: &dyn WorldStore,
    region_dir: &Path,
    world: WorldId,
    dimension: DimensionId,
    schema_version: SchemaVersion,
) -> anyhow::Result<usize> {
    let region_paths = collect_region_paths(region_dir)?;
    if region_paths.is_empty() {
        anyhow::bail!(
            "anvil import directory {} contains no r.<x>.<z>.mca region files",
            region_dir.display()
        );
    }

    let mut imported = 0usize;
    let mut batch: Vec<(ChunkKey, ChunkRecord)> = Vec::with_capacity(ANVIL_IMPORT_BATCH);
    for path in region_paths {
        // Read + decompress + NBT-parse the whole region off the async runtime:
        // blocking file I/O and CPU-bound work, never run on a Tokio worker.
        let chunks =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(ChunkPos, Chunk)>> {
                let region = Region::open(&path)
                    .map_err(|err| anyhow::anyhow!("opening region {}: {err}", path.display()))?;
                region
                    .iter_chunks()
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|err| anyhow::anyhow!("decoding region {}: {err}", path.display()))
            })
            .await
            .map_err(|err| anyhow::anyhow!("anvil import task panicked: {err}"))??;

        for (pos, chunk) in chunks {
            batch.push((
                ChunkKey::new(world, dimension, pos),
                ChunkRecord::new(schema_version, chunk),
            ));
            imported += 1;
            // Flush in bounded batches so one large map never forces a giant write.
            if batch.len() >= ANVIL_IMPORT_BATCH {
                let full = std::mem::replace(&mut batch, Vec::with_capacity(ANVIL_IMPORT_BATCH));
                store
                    .save_chunks(full)
                    .await
                    .map_err(|err| anyhow::anyhow!("saving imported chunks: {err}"))?;
            }
        }
    }
    if !batch.is_empty() {
        store
            .save_chunks(batch)
            .await
            .map_err(|err| anyhow::anyhow!("saving imported chunks: {err}"))?;
    }
    Ok(imported)
}

/// Collects the paths of every `r.<x>.<z>.mca` region file in `region_dir`, in a
/// deterministic (sorted) order.
///
/// Non-region entries (`level.dat`, a `data/` subdirectory, …) are skipped.
///
/// # Errors
///
/// Returns an error if `region_dir` cannot be read (for example, it does not
/// exist) — the configured-but-missing case that must fail startup loudly.
fn collect_region_paths(region_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let read_err = |err: std::io::Error| {
        anyhow::anyhow!(
            "reading anvil import directory {}: {err}",
            region_dir.display()
        )
    };
    let entries = std::fs::read_dir(region_dir).map_err(read_err)?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry.map_err(read_err)?.path();
        // Only r.<x>.<z>.mca region files; everything else in the directory is
        // ignored so a real world folder (level.dat, data/, …) imports cleanly.
        if region_pos_from_path(&path).is_ok() {
            paths.push(path);
        }
    }
    // Deterministic import order regardless of directory iteration order.
    paths.sort();
    Ok(paths)
}

/// Assembles the shared [`JoinKit`] framing.
///
/// Records only the spawn chunk *positions*; the connection fetches each column's
/// live blob from the resident shard chunks at join time, so the kit never holds a
/// stale snapshot of a spawn chunk.
fn build_join_kit(config: &AppConfig, spawn: &SpawnChunkTickets) -> anyhow::Result<JoinKit> {
    let spawn_position = config.spawn();
    let spawn_block = BlockPosition::new(
        spawn_position.x.floor() as i32,
        spawn_position.y.floor() as i32,
        spawn_position.z.floor() as i32,
    );
    Ok(JoinKit {
        join_game: build_join_game(config)?,
        spawn_position,
        spawn_chunk: spawn_center_chunk(config),
        spawn_block,
        spawn_positions: spawn.positions().collect(),
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
        config.view_distance(),
        config.simulation_distance(),
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
/// Shared by the join-kit builder (for the spawn batch) and the driver (for
/// chunks streamed in around a moving player), so both emit byte-identical chunk
/// packets.
///
/// # Errors
///
/// Returns an error if the section blob or a packed heightmap cannot be produced
/// (only for out-of-range block data, which the flat generator never emits), or
/// if an encoded buffer exceeds its protocol length bound.
pub(crate) fn chunk_packet(pos: ChunkPos, chunk: &Chunk) -> anyhow::Result<ChunkDataAndLight> {
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
    use ferrumc_world::BlockStateId;

    use super::*;
    use crate::config::AppConfig;

    /// The committed Anvil fixture directory (one `r.0.0.mca` region holding the
    /// single real vanilla chunk `(0, 0)`), reused from the `ferrumc-anvil` crate.
    fn anvil_fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../crates/ferrumc-anvil/tests/fixtures")
    }

    /// Reads chunk `(0, 0)` straight from the fixture via the Anvil reader: the
    /// source-of-truth terrain to compare the imported/loaded chunk against.
    fn fixture_chunk() -> Chunk {
        Region::open(anvil_fixtures_dir().join("r.0.0.mca"))
            .expect("region opens")
            .read_chunk(0, 0)
            .expect("chunk reads")
            .expect("chunk (0, 0) is present")
    }

    /// Asserts two chunks hold identical blocks across the full buildable column
    /// range. Compares block data only (not dirty masks), so it is robust to the
    /// store round-trip clearing dirty state.
    fn assert_same_blocks(actual: &Chunk, expected: &Chunk) {
        for y in -64..320 {
            for z in 0..16 {
                for x in 0..16 {
                    let pos = BlockPos::new(x, y, z);
                    assert_eq!(
                        actual.get_block(pos),
                        expected.get_block(pos),
                        "block mismatch at {pos:?}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn import_anvil_world_persists_fixture_chunks() {
        let store = InMemoryStore::new();
        let world = WorldId::new(0);
        let dimension = DimensionId::new(0);

        let imported = import_anvil_world(
            &store,
            &anvil_fixtures_dir(),
            world,
            dimension,
            ferrumc_sim::CHUNK_SCHEMA_VERSION,
        )
        .await
        .expect("fixture imports");
        // The fixture region holds exactly one present chunk, at (0, 0).
        assert_eq!(imported, 1);

        let loaded = store
            .load_chunk(ChunkKey::new(world, dimension, ChunkPos::new(0, 0)))
            .await
            .expect("load succeeds")
            .expect("chunk (0, 0) was imported")
            .into_chunk();

        // The store-loaded chunk matches the chunk the Anvil reader produces
        // directly from the same file: the startup path persisted the real
        // imported terrain, not flat generation.
        assert_same_blocks(&loaded, &fixture_chunk());

        // Explicit known-block spot check: a 1.18+ overworld has a full bedrock
        // floor at y == -64.
        let bedrock = BlockStateId::from_name("bedrock").expect("bedrock is known");
        for z in 0..16 {
            for x in 0..16 {
                assert_eq!(loaded.get_block(BlockPos::new(x, -64, z)), Some(bedrock));
            }
        }
    }

    #[tokio::test]
    async fn import_anvil_world_rejects_a_missing_directory() {
        let store = InMemoryStore::new();
        let missing = anvil_fixtures_dir().join("does-not-exist");
        let err = import_anvil_world(
            &store,
            &missing,
            WorldId::new(0),
            DimensionId::new(0),
            ferrumc_sim::CHUNK_SCHEMA_VERSION,
        )
        .await
        .expect_err("a missing import directory must abort startup");
        assert!(
            err.to_string().contains("anvil import directory"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn build_world_imports_configured_anvil_map() {
        let config = AppConfig::from_toml_str(&format!(
            "[world]\nanvil_import_dir = {:?}\n",
            anvil_fixtures_dir().to_string_lossy()
        ))
        .expect("fixture import config is valid");
        let setup = build_world(&config, ShardPos::new(0, 0))
            .await
            .expect("world builds with anvil import");

        // Spawn radius 2 -> a 5x5 resident square around chunk (0, 0).
        let expected_resident = (2 * usize::from(config.spawn_chunk_radius()) + 1).pow(2);
        assert_eq!(
            setup.shard.loaded_chunks().loaded_count(),
            expected_resident
        );

        // Chunk (0, 0) is present in the fixture, so the resident spawn chunk is
        // the imported vanilla terrain rather than flat generation.
        let resident = setup
            .shard
            .loaded_chunks()
            .get(ChunkPos::new(0, 0))
            .expect("spawn chunk (0, 0) resident");
        assert_same_blocks(resident, &fixture_chunk());

        // A neighbouring spawn chunk absent from the fixture falls back to flat
        // generation: chunk (2, 2) keeps the flat grass surface at y = 63.
        let flat = setup
            .shard
            .loaded_chunks()
            .get(ChunkPos::new(2, 2))
            .expect("neighbour spawn chunk resident");
        let grass = BlockStateId::from_name("grass_block").expect("grass_block is known");
        assert_eq!(
            flat.get_block(ChunkPos::new(2, 2).origin_block(63)),
            Some(grass)
        );
    }

    #[tokio::test]
    async fn build_world_loads_spawn_and_builds_kit() {
        let config = AppConfig::default();
        let setup = build_world(&config, ShardPos::new(0, 0))
            .await
            .expect("world builds");

        // Radius 2 -> a 5x5 spawn square, all resident, all covered by the kit's
        // position list.
        let expected = (2 * usize::from(config.spawn_chunk_radius()) + 1).pow(2);
        assert_eq!(setup.shard.loaded_chunks().loaded_count(), expected);
        assert_eq!(setup.join_kit.chunk_positions().count(), expected);
        assert_eq!(setup.join_kit.spawn_position(), config.spawn());
        assert_eq!(
            setup.join_kit.join_game().view_distance(),
            config.view_distance()
        );
        // The spawn point (8, 64, 8) falls in chunk (0, 0) and block (8, 64, 8).
        assert_eq!(
            setup.join_kit.spawn_chunk(),
            ferrumc_math::ChunkPos::new(0, 0)
        );
        let block = setup.join_kit.spawn_block();
        assert_eq!((block.x(), block.y(), block.z()), (8, 64, 8));

        // Every spawn column builds the real wire payload from its live resident
        // chunk (the same `chunk_packet` the join-time fetch uses): a non-empty
        // paletted section blob, the 37-long MOTION_BLOCKING heightmap, and
        // full-bright sky light over all 26 light sections.
        for pos in setup.join_kit.chunk_positions() {
            let resident = setup
                .shard
                .loaded_chunks()
                .get(pos)
                .expect("spawn chunk resident");
            let chunk = chunk_packet(pos, resident).expect("chunk packet builds");
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
