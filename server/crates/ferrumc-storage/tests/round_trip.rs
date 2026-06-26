//! Integration tests for the in-memory store: save/load round-trips, schema
//! preservation, plugin-namespace isolation, and missing-key semantics.

use ferrumc_core::{DimensionId, EntityId, GameMode, PlayerId, PluginId, ServerError, WorldId};
use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_storage::{
    ChunkKey, ChunkRecord, EntityKey, EntityRecord, InMemoryStore, PlayerRecord, PlayerStore,
    PluginStore, SchemaVersion, StorageKey, WorldStore, MAX_PLUGIN_VALUE_LEN, MAX_SAVE_BATCH,
};
use ferrumc_world::{BlockStateId, Chunk};

fn world() -> WorldId {
    WorldId::new(0)
}

fn overworld() -> DimensionId {
    DimensionId::new(0)
}

#[tokio::test]
async fn chunk_round_trips_and_preserves_schema() {
    let store = InMemoryStore::new();
    let key = ChunkKey::new(world(), overworld(), ChunkPos::new(2, -5));

    let mut chunk = Chunk::new(ChunkPos::new(2, -5));
    // Block coordinates must fall in chunk (2, -5): x in 32..=47, z in -80..=-65.
    let block = BlockPos::new(33, 7, -77);
    chunk
        .set_block(block, BlockStateId::new(1))
        .expect("block is inside the chunk");

    let record = ChunkRecord::new(SchemaVersion::new(11), chunk.clone());
    store.save_chunk(key, record).await.expect("save succeeds");

    let loaded = store
        .load_chunk(key)
        .await
        .expect("load succeeds")
        .expect("chunk is present");

    assert_eq!(loaded.schema_version(), SchemaVersion::new(11));
    assert_eq!(loaded.chunk(), &chunk);
    assert_eq!(
        loaded.chunk().get_block(block),
        Some(BlockStateId::new(1)),
        "block survives the round trip"
    );
}

#[tokio::test]
async fn entity_round_trips_and_preserves_schema() {
    let store = InMemoryStore::new();
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
    let store = InMemoryStore::new();
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
async fn missing_keys_return_none_not_error() {
    let store = InMemoryStore::new();
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
    assert!(!store.delete_player(player).await.expect("ok"));
    assert!(!store.delete(&plugin, &key).await.expect("ok"));
    assert!(store.keys(&plugin).await.expect("ok").is_empty());
}

#[tokio::test]
async fn plugin_namespaces_are_isolated() {
    let store = InMemoryStore::new();
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
async fn overwrite_replaces_previous_value() {
    let store = InMemoryStore::new();
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
async fn batch_save_round_trips_and_rejects_oversized_batch() {
    let store = InMemoryStore::new();

    let batch: Vec<_> = (0..3)
        .map(|i| {
            let key = EntityKey::new(world(), overworld(), EntityId::new(i));
            let record = EntityRecord::new(SchemaVersion::new(1), vec![i as u8]).expect("ok");
            (key, record)
        })
        .collect();
    store.save_entities(batch).await.expect("batch save");
    for i in 0..3 {
        let key = EntityKey::new(world(), overworld(), EntityId::new(i));
        assert!(store.load_entity(key).await.expect("ok").is_some());
    }

    // A batch larger than the cap is rejected as a capacity error. The keys do
    // not matter: the batch is rejected on size before anything is stored.
    let oversized: Vec<_> = (0..=MAX_SAVE_BATCH)
        .map(|_| {
            let key = ChunkKey::new(world(), overworld(), ChunkPos::ORIGIN);
            let record = ChunkRecord::new(SchemaVersion::new(1), Chunk::new(ChunkPos::ORIGIN));
            (key, record)
        })
        .collect();
    let err = store
        .save_chunks(oversized)
        .await
        .expect_err("oversized batch rejected");
    assert!(matches!(err, ServerError::Capacity(_)));
}

#[tokio::test]
async fn oversized_plugin_value_is_rejected() {
    let store = InMemoryStore::new();
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

    // Confirms the traits are dyn-compatible and `Send + Sync`, i.e. the
    // simulation layer can hold `Arc<dyn WorldStore>`.
    let store = Arc::new(InMemoryStore::new());
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
