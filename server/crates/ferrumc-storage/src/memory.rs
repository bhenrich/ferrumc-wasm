//! An owned, in-memory implementation of every storage trait.
//!
//! [`InMemoryStore`] backs each category with a plain [`HashMap`] guarded by an
//! [`RwLock`] for interior mutability behind the `&self` trait methods. It is an
//! ordinary owned value, **not** a global: a caller constructs one and shares it
//! (typically as `Arc<InMemoryStore>`), so there is no global mutable state.
//! This is the M16 backend used by tests and the test harness; the redb/LMDB
//! worker-thread backend lands in a later milestone.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use ferrumc_core::{PlayerId, PluginId, Result};

use crate::error::StorageError;
use crate::key::{ChunkKey, EntityKey, StorageKey};
use crate::record::{
    BlockMutationLogRecord, ChunkOverlayRecord, ChunkRecord, EntityRecord, PlayerRecord,
};
use crate::store::{PlayerStore, PluginStore, WorldStore, MAX_PLUGIN_VALUE_LEN, MAX_SAVE_BATCH};

/// An in-memory store implementing [`WorldStore`], [`PlayerStore`], and
/// [`PluginStore`].
///
/// All data lives in process memory and is lost when the value is dropped.
/// Construct one with [`InMemoryStore::new`] and share it with `Arc` to give the
/// simulation and plugin layers `Arc<dyn WorldStore>` (and friends).
#[derive(Debug, Default)]
pub struct InMemoryStore {
    chunks: RwLock<HashMap<ChunkKey, ChunkRecord>>,
    /// Per-chunk overlays (only player-modified sections), keyed independently of
    /// the full-chunk map above.
    overlays: RwLock<HashMap<ChunkKey, ChunkOverlayRecord>>,
    /// The append-only block-mutation journal, in append order.
    mutation_log: RwLock<Vec<BlockMutationLogRecord>>,
    entities: RwLock<HashMap<EntityKey, EntityRecord>>,
    players: RwLock<HashMap<PlayerId, PlayerRecord>>,
    /// Each plugin id maps to its own private key-value namespace. Nesting the
    /// maps is what enforces cross-plugin isolation: a lookup can only ever
    /// reach one plugin's inner map.
    plugins: RwLock<HashMap<PluginId, HashMap<StorageKey, Vec<u8>>>>,
}

impl InMemoryStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

/// Builds the backend error used when a lock is poisoned.
///
/// A lock is only poisoned if a thread panicked while holding it. This crate
/// never panics under a lock, so reaching this is an internal invariant
/// violation rather than recoverable state.
fn poisoned(what: &str) -> StorageError {
    StorageError::backend(format!("{what} lock poisoned"))
}

#[async_trait]
impl WorldStore for InMemoryStore {
    async fn load_chunk(&self, key: ChunkKey) -> Result<Option<ChunkRecord>> {
        let map = self.chunks.read().map_err(|_| poisoned("chunk"))?;
        Ok(map.get(&key).cloned())
    }

    async fn save_chunk(&self, key: ChunkKey, record: ChunkRecord) -> Result<()> {
        let mut map = self.chunks.write().map_err(|_| poisoned("chunk"))?;
        map.insert(key, record);
        Ok(())
    }

    async fn save_chunks(&self, chunks: Vec<(ChunkKey, ChunkRecord)>) -> Result<()> {
        if chunks.len() > MAX_SAVE_BATCH {
            return Err(StorageError::BatchTooLarge {
                len: chunks.len(),
                max: MAX_SAVE_BATCH,
            }
            .into());
        }
        let mut map = self.chunks.write().map_err(|_| poisoned("chunk"))?;
        for (key, record) in chunks {
            map.insert(key, record);
        }
        Ok(())
    }

    async fn delete_chunk(&self, key: ChunkKey) -> Result<bool> {
        let mut map = self.chunks.write().map_err(|_| poisoned("chunk"))?;
        Ok(map.remove(&key).is_some())
    }

    async fn load_chunk_overlay(&self, key: ChunkKey) -> Result<Option<ChunkOverlayRecord>> {
        let map = self.overlays.read().map_err(|_| poisoned("overlay"))?;
        Ok(map.get(&key).cloned())
    }

    async fn save_chunk_overlays(
        &self,
        overlays: Vec<(ChunkKey, ChunkOverlayRecord)>,
    ) -> Result<()> {
        if overlays.len() > MAX_SAVE_BATCH {
            return Err(StorageError::BatchTooLarge {
                len: overlays.len(),
                max: MAX_SAVE_BATCH,
            }
            .into());
        }
        let mut map = self.overlays.write().map_err(|_| poisoned("overlay"))?;
        for (key, record) in overlays {
            map.insert(key, record);
        }
        Ok(())
    }

    async fn append_block_mutations(&self, mutations: Vec<BlockMutationLogRecord>) -> Result<()> {
        if mutations.len() > MAX_SAVE_BATCH {
            return Err(StorageError::BatchTooLarge {
                len: mutations.len(),
                max: MAX_SAVE_BATCH,
            }
            .into());
        }
        let mut log = self
            .mutation_log
            .write()
            .map_err(|_| poisoned("mutation log"))?;
        log.extend(mutations);
        Ok(())
    }

    async fn load_entity(&self, key: EntityKey) -> Result<Option<EntityRecord>> {
        let map = self.entities.read().map_err(|_| poisoned("entity"))?;
        Ok(map.get(&key).cloned())
    }

    async fn save_entity(&self, key: EntityKey, record: EntityRecord) -> Result<()> {
        let mut map = self.entities.write().map_err(|_| poisoned("entity"))?;
        map.insert(key, record);
        Ok(())
    }

    async fn save_entities(&self, entities: Vec<(EntityKey, EntityRecord)>) -> Result<()> {
        if entities.len() > MAX_SAVE_BATCH {
            return Err(StorageError::BatchTooLarge {
                len: entities.len(),
                max: MAX_SAVE_BATCH,
            }
            .into());
        }
        let mut map = self.entities.write().map_err(|_| poisoned("entity"))?;
        for (key, record) in entities {
            map.insert(key, record);
        }
        Ok(())
    }

    async fn delete_entity(&self, key: EntityKey) -> Result<bool> {
        let mut map = self.entities.write().map_err(|_| poisoned("entity"))?;
        Ok(map.remove(&key).is_some())
    }
}

#[async_trait]
impl PlayerStore for InMemoryStore {
    async fn load_player(&self, id: PlayerId) -> Result<Option<PlayerRecord>> {
        let map = self.players.read().map_err(|_| poisoned("player"))?;
        Ok(map.get(&id).cloned())
    }

    async fn save_player(&self, id: PlayerId, record: PlayerRecord) -> Result<()> {
        let mut map = self.players.write().map_err(|_| poisoned("player"))?;
        map.insert(id, record);
        Ok(())
    }

    async fn delete_player(&self, id: PlayerId) -> Result<bool> {
        let mut map = self.players.write().map_err(|_| poisoned("player"))?;
        Ok(map.remove(&id).is_some())
    }
}

#[async_trait]
impl PluginStore for InMemoryStore {
    async fn get(&self, plugin: &PluginId, key: &StorageKey) -> Result<Option<Vec<u8>>> {
        let map = self.plugins.read().map_err(|_| poisoned("plugin"))?;
        Ok(map.get(plugin).and_then(|ns| ns.get(key)).cloned())
    }

    async fn put(&self, plugin: &PluginId, key: StorageKey, value: Vec<u8>) -> Result<()> {
        if value.len() > MAX_PLUGIN_VALUE_LEN {
            return Err(StorageError::ValueTooLarge {
                len: value.len(),
                max: MAX_PLUGIN_VALUE_LEN,
            }
            .into());
        }
        let mut map = self.plugins.write().map_err(|_| poisoned("plugin"))?;
        map.entry(plugin.clone()).or_default().insert(key, value);
        Ok(())
    }

    async fn delete(&self, plugin: &PluginId, key: &StorageKey) -> Result<bool> {
        let mut map = self.plugins.write().map_err(|_| poisoned("plugin"))?;
        let Some(ns) = map.get_mut(plugin) else {
            return Ok(false);
        };
        Ok(ns.remove(key).is_some())
    }

    async fn keys(&self, plugin: &PluginId) -> Result<Vec<StorageKey>> {
        let map = self.plugins.read().map_err(|_| poisoned("plugin"))?;
        Ok(map
            .get(plugin)
            .map(|ns| ns.keys().cloned().collect())
            .unwrap_or_default())
    }
}
