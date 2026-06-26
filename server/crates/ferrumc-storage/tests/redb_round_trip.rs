//! Integration tests for the redb-backed store: chunk/entity/player round-trips
//! through a temp database, batched saves, plugin-namespace isolation,
//! persistence across reopen, and missing-key semantics.

use ferrumc_core::{DimensionId, EntityId, GameMode, PlayerId, PluginId, ServerError, WorldId};
use ferrumc_math::{BlockPos, ChunkPos, LocalBlockPos};
use ferrumc_storage::{
    ChunkKey, ChunkRecord, EntityKey, EntityRecord, PlayerRecord, PlayerStore, PluginStore,
    RedbStore, SchemaVersion, StorageKey, WorldStore, MAX_PLUGIN_VALUE_LEN, MAX_SAVE_BATCH,
};
use ferrumc_world::{BlockStateId, Chunk};
use tempfile::TempDir;

fn world() -> WorldId {
    WorldId::new(0)
}

fn overworld() -> DimensionId {
    DimensionId::new(0)
}

/// Creates a fresh store under a temp dir, returning the dir guard so the file
/// outlives the store for the duration of a test.
fn fresh_store() -> (TempDir, RedbStore) {
    let dir = TempDir::new().expect("temp dir");
    let store = RedbStore::open(dir.path().join("world.redb")).expect("open store");
    (dir, store)
}

/// Asserts two chunks hold logically identical blocks at the same column.
///
/// Compares block content rather than `Chunk` equality so a difference in
/// internal palette representation (which the round trip may legitimately
/// reshape) does not produce a spurious failure.
fn assert_chunk_blocks_eq(left: &Chunk, right: &Chunk) {
    assert_eq!(left.pos(), right.pos(), "chunk positions differ");
    for index in 0..left.sections().len() {
        let a = left.section(index).expect("section in range");
        let b = right.section(index).expect("section in range");
        for y in 0..16u8 {
            for z in 0..16u8 {
                for x in 0..16u8 {
                    let pos = LocalBlockPos::new(x, y, z).expect("local pos");
                    assert_eq!(a.get(pos), b.get(pos), "block differs in section {index}");
                }
            }
        }
    }
}

#[tokio::test]
async fn chunk_round_trips_and_preserves_schema() {
    let (_dir, store) = fresh_store();
    let key = ChunkKey::new(world(), overworld(), ChunkPos::new(2, -5));

    let mut chunk = Chunk::new(ChunkPos::new(2, -5));
    // Block coordinates must fall in chunk (2, -5): x in 32..=47, z in -80..=-65.
    let block = BlockPos::new(33, 7, -77);
    chunk
        .set_block(block, BlockStateId::new(1))
        .expect("block is inside the chunk");

    store
        .save_chunk(key, ChunkRecord::new(SchemaVersion::new(11), chunk.clone()))
        .await
        .expect("save succeeds");

    let loaded = store
        .load_chunk(key)
        .await
        .expect("load succeeds")
        .expect("chunk is present");

    assert_eq!(loaded.schema_version(), SchemaVersion::new(11));
    assert_chunk_blocks_eq(loaded.chunk(), &chunk);
    assert_eq!(loaded.chunk().get_block(block), Some(BlockStateId::new(1)));
}

#[tokio::test]
async fn full_height_chunk_round_trips() {
    // Exercises the bottom (y = -64) and top (y = 319) sections so a wrong world
    // floor constant in the codec would fail rather than silently corrupt.
    let (_dir, store) = fresh_store();
    let key = ChunkKey::new(world(), overworld(), ChunkPos::ORIGIN);

    let mut chunk = Chunk::new(ChunkPos::ORIGIN);
    let bottom = BlockPos::new(0, -64, 0);
    let top = BlockPos::new(15, 319, 15);
    chunk
        .set_block(bottom, BlockStateId::new(1))
        .expect("bottom");
    chunk.set_block(top, BlockStateId::new(2)).expect("top");

    store
        .save_chunk(key, ChunkRecord::new(SchemaVersion::new(1), chunk.clone()))
        .await
        .expect("save");

    let loaded = store.load_chunk(key).await.expect("load").expect("present");
    assert_chunk_blocks_eq(loaded.chunk(), &chunk);
    assert_eq!(loaded.chunk().get_block(bottom), Some(BlockStateId::new(1)));
    assert_eq!(loaded.chunk().get_block(top), Some(BlockStateId::new(2)));
}

#[tokio::test]
async fn entity_round_trips_and_preserves_schema() {
    let (_dir, store) = fresh_store();
    let key = EntityKey::new(world(), overworld(), EntityId::new(7));
    let record = EntityRecord::new(SchemaVersion::new(4), vec![0xDE, 0xAD, 0xBE, 0xEF])
        .expect("within bound");

    store.save_entity(key, record).await.expect("save succeeds");

    let loaded = store
        .load_entity(key)
        .await
        .expect("load succeeds")
        .expect("entity is present");
    assert_eq!(loaded.schema_version(), SchemaVersion::new(4));
    assert_eq!(loaded.data(), &[0xDE, 0xAD, 0xBE, 0xEF]);
}

#[tokio::test]
async fn player_round_trips_and_preserves_schema_and_game_mode() {
    let (_dir, store) = fresh_store();
    let id = PlayerId::offline("Saad");
    let record = PlayerRecord::new(SchemaVersion::new(2), GameMode::Creative, vec![1, 2, 3])
        .expect("within bound");

    store.save_player(id, record).await.expect("save succeeds");

    let loaded = store
        .load_player(id)
        .await
        .expect("load succeeds")
        .expect("player is present");
    assert_eq!(loaded.schema_version(), SchemaVersion::new(2));
    assert_eq!(loaded.game_mode(), GameMode::Creative);
    assert_eq!(loaded.data(), &[1, 2, 3]);
}

#[tokio::test]
async fn batched_chunk_save_round_trips_and_rejects_oversized() {
    let (_dir, store) = fresh_store();

    let batch: Vec<_> = (0..4)
        .map(|i| {
            let key = ChunkKey::new(world(), overworld(), ChunkPos::new(i, 0));
            let mut chunk = Chunk::new(ChunkPos::new(i, 0));
            // One block per chunk, somewhere inside its column.
            let bx = i * 16 + 1;
            chunk
                .set_block(BlockPos::new(bx, 0, 0), BlockStateId::new((i as u32) + 1))
                .expect("inside chunk");
            (key, ChunkRecord::new(SchemaVersion::new(1), chunk))
        })
        .collect();

    store.save_chunks(batch).await.expect("batched save");

    for i in 0..4 {
        let key = ChunkKey::new(world(), overworld(), ChunkPos::new(i, 0));
        let loaded = store.load_chunk(key).await.expect("load").expect("present");
        let bx = i * 16 + 1;
        assert_eq!(
            loaded.chunk().get_block(BlockPos::new(bx, 0, 0)),
            Some(BlockStateId::new((i as u32) + 1))
        );
    }

    // A batch larger than the cap is rejected before anything is written.
    let oversized: Vec<_> = (0..=MAX_SAVE_BATCH)
        .map(|_| {
            let key = ChunkKey::new(world(), overworld(), ChunkPos::ORIGIN);
            (
                key,
                ChunkRecord::new(SchemaVersion::new(1), Chunk::new(ChunkPos::ORIGIN)),
            )
        })
        .collect();
    let err = store
        .save_chunks(oversized)
        .await
        .expect_err("oversized batch rejected");
    assert!(matches!(err, ServerError::Capacity(_)));
}

#[tokio::test]
async fn plugin_namespaces_are_isolated() {
    let (_dir, store) = fresh_store();
    let alice = PluginId::new("alice");
    let bob = PluginId::new("bob");
    let shared_key = StorageKey::new("secret").expect("valid key");

    store
        .put(&alice, shared_key.clone(), b"alice-data".to_vec())
        .await
        .expect("alice put");
    store
        .put(&bob, shared_key.clone(), b"bob-data".to_vec())
        .await
        .expect("bob put");

    // Same key string, different namespaces -> different values.
    assert_eq!(
        store.get(&alice, &shared_key).await.expect("ok"),
        Some(b"alice-data".to_vec())
    );
    assert_eq!(
        store.get(&bob, &shared_key).await.expect("ok"),
        Some(b"bob-data".to_vec())
    );

    // A key only alice set is invisible to bob.
    let alice_only = StorageKey::new("alice-only").expect("valid key");
    store
        .put(&alice, alice_only.clone(), b"x".to_vec())
        .await
        .expect("alice put");
    assert_eq!(store.get(&bob, &alice_only).await.expect("ok"), None);

    // Key enumeration never leaks across namespaces.
    let bob_keys = store.keys(&bob).await.expect("ok");
    assert_eq!(bob_keys, vec![shared_key.clone()]);

    let mut alice_keys = store.keys(&alice).await.expect("ok");
    alice_keys.sort();
    assert_eq!(alice_keys, vec![alice_only, shared_key.clone()]);

    // Bob deleting "his" key cannot touch alice's value under the same string.
    assert!(store.delete(&bob, &shared_key).await.expect("ok"));
    assert_eq!(store.get(&bob, &shared_key).await.expect("ok"), None);
    assert_eq!(
        store.get(&alice, &shared_key).await.expect("ok"),
        Some(b"alice-data".to_vec())
    );
}

#[tokio::test]
async fn plugin_with_shared_prefix_does_not_leak() {
    // Two plugin ids where one is a prefix of the other must stay isolated; the
    // length-prefixed namespacing prevents the range scan from bleeding over.
    let (_dir, store) = fresh_store();
    let short = PluginId::new("plug");
    let long = PluginId::new("plugin");
    let key = StorageKey::new("k").expect("key");

    store
        .put(&short, key.clone(), b"short".to_vec())
        .await
        .expect("short put");
    store
        .put(&long, key.clone(), b"long".to_vec())
        .await
        .expect("long put");

    assert_eq!(store.keys(&short).await.expect("ok"), vec![key.clone()]);
    assert_eq!(store.keys(&long).await.expect("ok"), vec![key.clone()]);
    assert_eq!(
        store.get(&short, &key).await.expect("ok"),
        Some(b"short".to_vec())
    );
    assert_eq!(
        store.get(&long, &key).await.expect("ok"),
        Some(b"long".to_vec())
    );
}

#[tokio::test]
async fn data_persists_across_reopen() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("persist.redb");

    let chunk_key = ChunkKey::new(world(), overworld(), ChunkPos::new(1, 1));
    let player = PlayerId::offline("persistent");
    let plugin = PluginId::new("keeper");
    let plugin_key = StorageKey::new("score").expect("key");

    let mut chunk = Chunk::new(ChunkPos::new(1, 1));
    let block = BlockPos::new(20, 64, 20);
    chunk
        .set_block(block, BlockStateId::new(3))
        .expect("inside");

    {
        let store = RedbStore::open(&path).expect("open");
        store
            .save_chunk(
                chunk_key,
                ChunkRecord::new(SchemaVersion::new(8), chunk.clone()),
            )
            .await
            .expect("save chunk");
        store
            .save_player(
                player,
                PlayerRecord::new(SchemaVersion::new(8), GameMode::Adventure, vec![7, 7])
                    .expect("player"),
            )
            .await
            .expect("save player");
        store
            .put(&plugin, plugin_key.clone(), b"99".to_vec())
            .await
            .expect("put");
        // Drop the store (and its database handle) at the end of this scope.
    }

    // Reopen the same file and read everything back.
    let store = RedbStore::open(&path).expect("reopen");

    let loaded_chunk = store
        .load_chunk(chunk_key)
        .await
        .expect("load")
        .expect("present");
    assert_eq!(loaded_chunk.schema_version(), SchemaVersion::new(8));
    assert_eq!(
        loaded_chunk.chunk().get_block(block),
        Some(BlockStateId::new(3))
    );

    let loaded_player = store
        .load_player(player)
        .await
        .expect("load")
        .expect("present");
    assert_eq!(loaded_player.game_mode(), GameMode::Adventure);
    assert_eq!(loaded_player.data(), &[7, 7]);

    assert_eq!(
        store.get(&plugin, &plugin_key).await.expect("ok"),
        Some(b"99".to_vec())
    );
}

#[tokio::test]
async fn missing_keys_return_none_not_error() {
    let (_dir, store) = fresh_store();
    let chunk_key = ChunkKey::new(world(), overworld(), ChunkPos::ORIGIN);
    let entity_key = EntityKey::new(world(), overworld(), EntityId::new(999));
    let player = PlayerId::offline("ghost");
    let plugin = PluginId::new("nope");
    let key = StorageKey::new("absent").expect("valid key");

    assert_eq!(store.load_chunk(chunk_key).await.expect("ok"), None);
    assert_eq!(store.load_entity(entity_key).await.expect("ok"), None);
    assert_eq!(store.load_player(player).await.expect("ok"), None);
    assert_eq!(store.get(&plugin, &key).await.expect("ok"), None);
    // Deleting something that is not there is a clean `false`, not an error.
    assert!(!store.delete_chunk(chunk_key).await.expect("ok"));
    assert!(!store.delete_entity(entity_key).await.expect("ok"));
    assert!(!store.delete_player(player).await.expect("ok"));
    assert!(!store.delete(&plugin, &key).await.expect("ok"));
    assert!(store.keys(&plugin).await.expect("ok").is_empty());
}

#[tokio::test]
async fn overwrite_replaces_previous_value() {
    let (_dir, store) = fresh_store();
    let id = PlayerId::offline("dup");
    store
        .save_player(
            id,
            PlayerRecord::new(SchemaVersion::new(1), GameMode::Survival, vec![1]).expect("ok"),
        )
        .await
        .expect("first save");
    store
        .save_player(
            id,
            PlayerRecord::new(SchemaVersion::new(9), GameMode::Spectator, vec![2]).expect("ok"),
        )
        .await
        .expect("second save");

    let loaded = store.load_player(id).await.expect("ok").expect("present");
    assert_eq!(loaded.schema_version(), SchemaVersion::new(9));
    assert_eq!(loaded.game_mode(), GameMode::Spectator);
    assert_eq!(loaded.data(), &[2]);
}

#[tokio::test]
async fn oversized_plugin_value_is_rejected() {
    let (_dir, store) = fresh_store();
    let plugin = PluginId::new("greedy");
    let key = StorageKey::new("blob").expect("valid key");
    let err = store
        .put(&plugin, key, vec![0u8; MAX_PLUGIN_VALUE_LEN + 1])
        .await
        .expect_err("oversized value rejected");
    assert!(matches!(err, ServerError::Capacity(_)));
}

#[tokio::test]
async fn store_is_shareable_behind_a_trait_object() {
    use std::sync::Arc;

    let (_dir, store) = fresh_store();
    let store = Arc::new(store);
    let world_store: Arc<dyn WorldStore> = store.clone();
    let key = ChunkKey::new(world(), overworld(), ChunkPos::ORIGIN);
    world_store
        .save_chunk(
            key,
            ChunkRecord::new(SchemaVersion::new(1), Chunk::new(ChunkPos::ORIGIN)),
        )
        .await
        .expect("save via trait object");
    assert!(world_store.load_chunk(key).await.expect("ok").is_some());
}
