//! Integration tests for chunk tickets, the load-or-generate flow, and the
//! dirty-chunk handoff, exercised through the public API with an
//! [`InMemoryStore`] and a [`FlatWorldGenerator`] (no networking, no real DB).

use ferrumc_core::{DimensionId, WorldId};
use ferrumc_math::{BlockPos, ChunkPos, ShardPos};
use ferrumc_sim::{
    ChunkProvenance, ChunkTicket, LoadedChunkMap, SimError, SimShard, SpawnChunkTickets,
    TicketLevel, TicketReason, OVERLAY_SCHEMA_VERSION,
};
use ferrumc_storage::{
    ChunkKey, ChunkOverlayRecord, ChunkRecord, InMemoryStore, SchemaVersion, StorageError,
    WorldStore, MAX_SAVE_BATCH,
};
use ferrumc_world::{
    BlockEntity, BlockStateId, ChestInventory, Chunk, FlatWorldGenerator, Sign, SignKind,
};

const WORLD: WorldId = WorldId::new(0);
const DIMENSION: DimensionId = DimensionId::new(0);

fn key(pos: ChunkPos) -> ChunkKey {
    ChunkKey::new(WORLD, DIMENSION, pos)
}

fn map() -> LoadedChunkMap {
    LoadedChunkMap::new(WORLD, DIMENSION)
}

fn spawn_ticket() -> ChunkTicket {
    ChunkTicket::of(TicketReason::Spawn)
}

/// A clean (already-persisted) chunk with one marker block, distinguishable
/// from generated flat terrain.
fn stored_marker(pos: ChunkPos, marker: BlockPos, state: BlockStateId) -> ChunkRecord {
    let mut chunk = Chunk::new(pos);
    chunk.set_block(marker, state).expect("marker in chunk");
    chunk.clear_dirty();
    ChunkRecord::new(SchemaVersion::new(1), chunk)
}

#[tokio::test]
async fn load_or_generate_miss_generates_hit_loads() {
    let store = InMemoryStore::new();
    let generator = FlatWorldGenerator::new();
    let pos = ChunkPos::new(0, 0);

    // Miss: an empty store generates flat terrain.
    let mut generate_map = map();
    let outcome = generate_map
        .acquire(&store, &generator, pos, spawn_ticket())
        .await
        .expect("acquire generates on miss");
    assert_eq!(outcome.provenance(), Some(ChunkProvenance::Generated));
    let grass = generate_map
        .get(pos)
        .and_then(|c| c.get_block(BlockPos::new(0, 63, 0)))
        .expect("surface");
    assert_ne!(grass, BlockStateId::AIR, "generated terrain has a surface");

    // Hit: a stored chunk is returned verbatim, not regenerated.
    let marker = BlockPos::new(1, 64, 1);
    store
        .save_chunk(key(pos), stored_marker(pos, marker, BlockStateId::new(7)))
        .await
        .expect("seed store");

    let mut load_map = map();
    let outcome = load_map
        .acquire(&store, &generator, pos, spawn_ticket())
        .await
        .expect("acquire loads on hit");
    assert_eq!(outcome.provenance(), Some(ChunkProvenance::Loaded));
    let loaded = load_map.get(pos).expect("resident");
    assert_eq!(loaded.get_block(marker), Some(BlockStateId::new(7)));
    assert_eq!(
        loaded.get_block(BlockPos::new(0, 63, 0)),
        Some(BlockStateId::AIR),
        "a loaded chunk must not be regenerated"
    );
}

struct ImportedChunkFixture {
    pos: ChunkPos,
    edited: BlockPos,
    imported_sibling: BlockPos,
    imported_other_section: BlockPos,
    later_edit: BlockPos,
    sign_pos: BlockPos,
    chest_pos: BlockPos,
    sign: BlockEntity,
    chest: BlockEntity,
}

impl ImportedChunkFixture {
    fn new() -> Self {
        let pos = ChunkPos::new(6, -3);
        let origin = pos.origin_block(0);
        let mut sign = Sign::new(SignKind::Sign);
        sign.set_face_lines(
            true,
            [
                "imported".to_owned(),
                "anvil".to_owned(),
                String::new(),
                "survives".to_owned(),
            ],
        );
        Self {
            pos,
            edited: BlockPos::new(origin.x() + 1, 70, origin.z() + 1),
            imported_sibling: BlockPos::new(origin.x() + 2, 71, origin.z() + 2),
            imported_other_section: BlockPos::new(origin.x() + 3, 20, origin.z() + 3),
            later_edit: BlockPos::new(origin.x() + 4, 130, origin.z() + 4),
            sign_pos: BlockPos::new(origin.x() + 5, 80, origin.z() + 5),
            chest_pos: BlockPos::new(origin.x() + 6, 96, origin.z() + 6),
            sign: BlockEntity::Sign(sign),
            chest: BlockEntity::Chest(ChestInventory::new()),
        }
    }

    fn imported_chunk(&self) -> Chunk {
        let mut chunk = Chunk::new(self.pos);
        for (pos, state) in [
            (self.edited, 41),
            (self.imported_sibling, 42),
            (self.imported_other_section, 43),
            (self.later_edit, 44),
        ] {
            chunk
                .set_block(pos, BlockStateId::new(state))
                .expect("imported coordinate in chunk");
        }
        chunk
            .set_block_entity(self.sign_pos, self.sign.clone())
            .expect("set imported sign");
        chunk
            .set_block_entity(self.chest_pos, self.chest.clone())
            .expect("set imported chest");
        chunk.clear_dirty();
        chunk
    }

    fn assert_composed(&self, chunk: &Chunk, later_state: BlockStateId) {
        assert_eq!(chunk.get_block(self.edited), Some(BlockStateId::new(51)));
        assert_eq!(
            chunk.get_block(self.imported_sibling),
            Some(BlockStateId::new(42))
        );
        assert_eq!(
            chunk.get_block(self.imported_other_section),
            Some(BlockStateId::new(43)),
            "an untouched imported section must not be replaced by flat terrain"
        );
        assert_eq!(chunk.get_block(self.later_edit), Some(later_state));
        assert_eq!(chunk.block_entity(self.sign_pos), Some(&self.sign));
        assert_eq!(chunk.block_entity(self.chest_pos), Some(&self.chest));
    }
}

async fn seed_imported_base_and_overlay(store: &InMemoryStore, fixture: &ImportedChunkFixture) {
    // A full `ChunkRecord` is the shape handed to sim by the Anvil import
    // boundary. This synthetic fixture also carries block entities so composition
    // is covered independently of the importer's still-separate decoding scope.
    // Markers span the overlay section and untouched imported sections.
    let imported = fixture.imported_chunk();
    store
        .save_chunk(
            key(fixture.pos),
            ChunkRecord::new(SchemaVersion::new(1), imported.clone()),
        )
        .await
        .expect("seed imported full record");

    let mut overlay_source = imported;
    overlay_source
        .set_block(fixture.edited, BlockStateId::new(51))
        .expect("overlay coordinate in chunk");
    overlay_source.mark_persist_dirty(fixture.edited);
    let overlay = ChunkOverlayRecord::from_chunk(
        ferrumc_sim::OVERLAY_SCHEMA_VERSION,
        fixture.pos,
        &overlay_source,
        1,
    );
    assert_eq!(overlay.section_count(), 1);
    store
        .save_chunk_overlays(vec![(key(fixture.pos), overlay)])
        .await
        .expect("seed overlay");
}

async fn assert_empty_overlay_preserves_import(
    store: &InMemoryStore,
    generator: &FlatWorldGenerator,
) {
    let empty_pos = ChunkPos::new(-8, 5);
    let empty_key = key(empty_pos);
    let empty_marker = empty_pos.origin_block(140);
    let empty_sign_pos = empty_pos.origin_block(90);
    let empty_sign = BlockEntity::Sign(Sign::new(SignKind::Sign));
    let mut empty_import = Chunk::new(empty_pos);
    empty_import
        .set_block(empty_marker, BlockStateId::new(61))
        .expect("empty-overlay marker in chunk");
    empty_import
        .set_block_entity(empty_sign_pos, empty_sign.clone())
        .expect("set empty-overlay sign");
    empty_import.clear_dirty();
    store
        .save_chunk(
            empty_key,
            ChunkRecord::new(SchemaVersion::new(1), empty_import),
        )
        .await
        .expect("seed second imported record");
    let empty_overlay = ChunkOverlayRecord::from_chunk(
        ferrumc_sim::OVERLAY_SCHEMA_VERSION,
        empty_pos,
        &Chunk::new(empty_pos),
        3,
    );
    assert_eq!(empty_overlay.section_count(), 0);
    assert_eq!(empty_overlay.block_entity_count(), 0);
    store
        .save_chunk_overlays(vec![(empty_key, empty_overlay)])
        .await
        .expect("seed empty overlay");

    let mut empty_load = map();
    empty_load
        .acquire(store, generator, empty_pos, spawn_ticket())
        .await
        .expect("load import with empty overlay");
    let empty_reloaded = empty_load.get(empty_pos).expect("empty-overlay resident");
    assert_eq!(
        empty_reloaded.get_block(empty_marker),
        Some(BlockStateId::new(61))
    );
    assert_eq!(
        empty_reloaded.block_entity(empty_sign_pos),
        Some(&empty_sign)
    );
}

#[tokio::test]
async fn imported_anvil_chunk_is_base_then_overlay_applies() {
    let store = InMemoryStore::new();
    let generator = FlatWorldGenerator::new();
    let fixture = ImportedChunkFixture::new();
    seed_imported_base_and_overlay(&store, &fixture).await;

    let mut first_load = map();
    let outcome = first_load
        .acquire(&store, &generator, fixture.pos, spawn_ticket())
        .await
        .expect("compose imported base and overlay");
    assert_eq!(outcome.provenance(), Some(ChunkProvenance::Loaded));
    let first_resident = first_load.get(fixture.pos).expect("first resident");
    fixture.assert_composed(first_resident, BlockStateId::new(44));
    assert!(!first_resident.dirty_sections().any());
    assert!(!first_resident.persist_dirty_sections().any());

    // A later edit in another section must persist cumulatively over the same
    // imported base, retaining the first overlay and all imported data.
    {
        let chunk = first_load
            .get_mut(fixture.pos)
            .expect("resident for later edit");
        chunk
            .set_block(fixture.later_edit, BlockStateId::new(52))
            .expect("later coordinate in chunk");
        chunk.mark_persist_dirty(fixture.later_edit);
    }
    store
        .save_chunk_overlays(first_load.take_persist_dirty(2))
        .await
        .expect("persist cumulative overlay");

    let mut second_load = map();
    second_load
        .acquire(&store, &generator, fixture.pos, spawn_ticket())
        .await
        .expect("reload composed chunk");
    fixture.assert_composed(
        second_load.get(fixture.pos).expect("second resident"),
        BlockStateId::new(52),
    );
    assert_empty_overlay_preserves_import(&store, &generator).await;
}

#[tokio::test]
async fn deleted_imported_block_entity_does_not_resurrect() {
    let store = InMemoryStore::new();
    let generator = FlatWorldGenerator::new();
    let fixture = ImportedChunkFixture::new();
    let mut imported = fixture.imported_chunk();
    imported
        .set_block(fixture.sign_pos, BlockStateId::new(63))
        .expect("imported sign block in chunk");
    imported.clear_dirty();
    store
        .save_chunk(
            key(fixture.pos),
            ChunkRecord::new(SchemaVersion::new(1), imported.clone()),
        )
        .await
        .expect("seed imported base");

    let mut overlay_source = imported;
    overlay_source
        .set_block(fixture.sign_pos, BlockStateId::AIR)
        .expect("break imported sign");
    assert!(overlay_source
        .remove_block_entity(fixture.sign_pos)
        .is_some());
    overlay_source.mark_persist_dirty(fixture.sign_pos);
    let overlay = ChunkOverlayRecord::from_chunk(
        ferrumc_sim::OVERLAY_SCHEMA_VERSION,
        fixture.pos,
        &overlay_source,
        1,
    );
    assert_eq!(overlay.section_count(), 1);
    assert_eq!(overlay.block_entity_count(), 1, "the chest remains");
    store
        .save_chunk_overlays(vec![(key(fixture.pos), overlay)])
        .await
        .expect("persist sign deletion");

    let mut loaded = map();
    loaded
        .acquire(&store, &generator, fixture.pos, spawn_ticket())
        .await
        .expect("reload imported base plus deletion overlay");
    let chunk = loaded.get(fixture.pos).expect("resident chunk");
    assert_eq!(chunk.get_block(fixture.sign_pos), Some(BlockStateId::AIR));
    assert!(
        chunk.block_entity(fixture.sign_pos).is_none(),
        "a complete v3 overlay must preserve deletion of an imported block entity"
    );
    assert_eq!(chunk.block_entity(fixture.chest_pos), Some(&fixture.chest));

    // A later overlay in another section must carry the deletion forward when
    // the cumulative edit set is captured and composed over the same import.
    {
        let chunk = loaded
            .get_mut(fixture.pos)
            .expect("resident for later edit");
        chunk
            .set_block(fixture.later_edit, BlockStateId::new(72))
            .expect("later edit in chunk");
        chunk.mark_persist_dirty(fixture.later_edit);
    }
    store
        .save_chunk_overlays(loaded.take_persist_dirty(2))
        .await
        .expect("persist deletion plus later edit");
    let mut reloaded = map();
    reloaded
        .acquire(&store, &generator, fixture.pos, spawn_ticket())
        .await
        .expect("reload cumulative deletion overlay");
    let chunk = reloaded.get(fixture.pos).expect("reloaded chunk");
    assert_eq!(
        chunk.get_block(fixture.later_edit),
        Some(BlockStateId::new(72))
    );
    assert!(chunk.block_entity(fixture.sign_pos).is_none());
    assert_eq!(chunk.block_entity(fixture.chest_pos), Some(&fixture.chest));
}

async fn assert_incompatible_overlay_schema_is_refused(schema_version: SchemaVersion) {
    let store = InMemoryStore::new();
    let generator = FlatWorldGenerator::new();
    let fixture = ImportedChunkFixture::new();
    let imported = fixture.imported_chunk();
    store
        .save_chunk(
            key(fixture.pos),
            ChunkRecord::new(SchemaVersion::new(1), imported.clone()),
        )
        .await
        .expect("seed imported base");

    let mut overlay_source = imported;
    overlay_source
        .set_block(fixture.edited, BlockStateId::new(71))
        .expect("overlay edit in chunk");
    overlay_source.mark_persist_dirty(fixture.edited);
    let overlay = ChunkOverlayRecord::from_chunk(schema_version, fixture.pos, &overlay_source, 1);
    assert_eq!(overlay.section_count(), 1);
    store
        .save_chunk_overlays(vec![(key(fixture.pos), overlay)])
        .await
        .expect("persist incompatible overlay fixture");

    let mut loaded = map();
    let error = loaded
        .acquire(&store, &generator, fixture.pos, spawn_ticket())
        .await
        .expect_err("incompatible overlay schema must be refused");
    assert_eq!(
        error,
        SimError::ChunkLoad {
            pos: fixture.pos,
            source: StorageError::IncompatiblePreAlphaData.into(),
        }
    );
    assert!(
        !loaded.is_loaded(fixture.pos),
        "a refused overlay must not make the chunk resident"
    );
}

#[tokio::test]
async fn old_overlay_schema_is_refused_as_incompatible_pre_alpha_data() {
    assert_eq!(OVERLAY_SCHEMA_VERSION, SchemaVersion::new(3));
    assert_incompatible_overlay_schema_is_refused(SchemaVersion::new(2)).await;
}

#[tokio::test]
async fn future_overlay_schema_is_refused_as_incompatible_pre_alpha_data() {
    assert_eq!(OVERLAY_SCHEMA_VERSION, SchemaVersion::new(3));
    assert_incompatible_overlay_schema_is_refused(SchemaVersion::new(4)).await;
}

#[tokio::test]
async fn tickets_add_and_remove_govern_the_loaded_set() {
    let store = InMemoryStore::new();
    let generator = FlatWorldGenerator::new();
    let pos = ChunkPos::new(4, -2);
    let mut m = map();

    let spawn = spawn_ticket();
    let player = ChunkTicket::of(TicketReason::Player);

    assert!(!m.is_loaded(pos));

    // Two distinct tickets bring it in and keep it in.
    m.acquire(&store, &generator, pos, spawn)
        .await
        .expect("spawn ticket");
    m.acquire(&store, &generator, pos, player)
        .await
        .expect("player ticket");
    assert!(m.is_loaded(pos));
    assert_eq!(m.ticket_count(pos), 2);
    // Effective level is the stronger (lower) of the two.
    assert_eq!(m.effective_level(pos), Some(TicketLevel::ENTITY_TICKING));

    // Releasing one leaves the other holding the chunk.
    assert!(m.release(pos, player).is_still_loaded());
    assert!(m.is_loaded(pos));
    assert_eq!(m.effective_level(pos), Some(TicketLevel::TICKING));

    // Releasing the last unloads it.
    assert!(m.release(pos, spawn).is_unloaded());
    assert!(!m.is_loaded(pos));
}

#[tokio::test]
async fn dirty_chunks_are_collected_on_mutation_and_persistable() {
    let store = InMemoryStore::new();
    let generator = FlatWorldGenerator::new();
    let pos = ChunkPos::new(0, 0);

    // Seed a clean chunk so dirtiness comes only from our edit.
    store
        .save_chunk(
            key(pos),
            stored_marker(pos, BlockPos::new(0, 0, 0), BlockStateId::new(1)),
        )
        .await
        .expect("seed store");

    let mut m = map();
    m.acquire(&store, &generator, pos, spawn_ticket())
        .await
        .expect("acquire");
    assert!(m.take_dirty().is_empty(), "freshly loaded chunk is clean");

    // Mutate, then collect.
    m.get_mut(pos)
        .expect("resident")
        .set_block(BlockPos::new(2, 70, 2), BlockStateId::new(5))
        .expect("in range");

    let dirty = m.take_dirty();
    assert_eq!(dirty.len(), 1);
    assert_eq!(dirty[0].0, key(pos));
    assert!(dirty.len() <= MAX_SAVE_BATCH);

    // The handoff is a real save batch the store accepts.
    store.save_chunks(dirty).await.expect("persist dirty batch");
    // Draining cleared dirtiness.
    assert!(m.take_dirty().is_empty());

    // Reloading reflects the mutation.
    let reloaded = store
        .load_chunk(key(pos))
        .await
        .expect("load")
        .expect("present");
    assert_eq!(
        reloaded.chunk().get_block(BlockPos::new(2, 70, 2)),
        Some(BlockStateId::new(5))
    );
}

#[tokio::test]
async fn spawn_ticket_set_keeps_its_chunks_resident() {
    let store = InMemoryStore::new();
    let generator = FlatWorldGenerator::new();
    let spawn = SpawnChunkTickets::around(ChunkPos::ORIGIN);
    let mut m = map();

    let newly = m
        .acquire_spawn(&store, &generator, &spawn)
        .await
        .expect("acquire spawn set");
    assert_eq!(newly, spawn.chunk_count());
    assert_eq!(m.loaded_count(), spawn.chunk_count());
    for pos in spawn.positions() {
        assert!(m.is_loaded(pos));
    }

    // Re-acquiring the spawn set loads nothing new but stacks tickets.
    let newly_again = m
        .acquire_spawn(&store, &generator, &spawn)
        .await
        .expect("acquire spawn set again");
    assert_eq!(newly_again, 0);
    for pos in spawn.positions() {
        assert_eq!(m.ticket_count(pos), 2);
    }
}

#[tokio::test]
async fn shard_owns_its_loaded_chunks() {
    let store = InMemoryStore::new();
    let generator = FlatWorldGenerator::new();
    let mut shard = SimShard::in_dimension(ShardPos::new(0, 0), WORLD, DIMENSION);
    let pos = ChunkPos::new(0, 0);

    assert_eq!(shard.loaded_chunks().loaded_count(), 0);
    shard
        .loaded_chunks_mut()
        .acquire(&store, &generator, pos, spawn_ticket())
        .await
        .expect("acquire through shard");
    assert!(shard.loaded_chunks().is_loaded(pos));
    assert_eq!(shard.loaded_chunks().loaded_count(), 1);
}

#[tokio::test]
async fn identical_acquire_sequences_are_deterministic() {
    async fn run() -> Vec<ChunkPos> {
        let store = InMemoryStore::new();
        let generator = FlatWorldGenerator::new();
        let mut m = map();
        for pos in [
            ChunkPos::new(3, 1),
            ChunkPos::new(-2, 0),
            ChunkPos::new(0, 0),
            ChunkPos::new(3, 1), // duplicate: only stacks a ticket
        ] {
            m.acquire(&store, &generator, pos, spawn_ticket())
                .await
                .expect("acquire");
        }
        m.loaded_positions().collect()
    }

    let first = run().await;
    let second = run().await;
    assert_eq!(first, second, "same inputs, same resident set and order");
    assert_eq!(
        first,
        vec![
            ChunkPos::new(-2, 0),
            ChunkPos::new(0, 0),
            ChunkPos::new(3, 1),
        ]
    );
}
