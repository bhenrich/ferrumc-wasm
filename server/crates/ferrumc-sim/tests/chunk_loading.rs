//! Integration tests for chunk tickets, the load-or-generate flow, and the
//! dirty-chunk handoff, exercised through the public API with an
//! [`InMemoryStore`] and a [`FlatWorldGenerator`] (no networking, no real DB).

use ferrumc_core::{DimensionId, WorldId};
use ferrumc_math::{BlockPos, ChunkPos, ShardPos};
use ferrumc_sim::{
    ChunkProvenance, ChunkTicket, LoadedChunkMap, SimShard, SpawnChunkTickets, TicketLevel,
    TicketReason,
};
use ferrumc_storage::{
    ChunkKey, ChunkRecord, InMemoryStore, SchemaVersion, WorldStore, MAX_SAVE_BATCH,
};
use ferrumc_world::{BlockStateId, Chunk, FlatWorldGenerator};

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
