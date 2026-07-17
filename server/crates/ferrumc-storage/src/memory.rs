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
use crate::store::{
    journal_id_range, PlayerStore, PluginStore, WorldStore, MAX_PLUGIN_VALUE_LEN, MAX_SAVE_BATCH,
};
use crate::{JournalAppendReceipt, JournalBatchId};

/// One committed idempotent batch, retained for exact normalized-payload replay.
#[derive(Debug)]
struct CommittedJournalBatch {
    receipt: JournalAppendReceipt,
    records: Vec<BlockMutationLogRecord>,
}

/// In-memory journal state protected by one lock so allocation and append are atomic.
#[derive(Debug, Default)]
struct MutationJournalState {
    last_id: Option<u64>,
    records: Vec<BlockMutationLogRecord>,
    receipts: HashMap<JournalBatchId, CommittedJournalBatch>,
}

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
    /// The append-only block-mutation journal and its storage-owned sequence.
    mutation_log: RwLock<MutationJournalState>,
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
        let Some((first_id, final_id)) = journal_id_range(log.last_id, mutations.len())? else {
            return Ok(());
        };
        log.records.extend(
            mutations
                .into_iter()
                .zip(first_id..=final_id)
                .map(|(record, id)| record.with_storage_id(id)),
        );
        log.last_id = Some(final_id);
        Ok(())
    }

    async fn append_block_mutation_batch(
        &self,
        batch_id: JournalBatchId,
        mutations: Vec<BlockMutationLogRecord>,
    ) -> Result<JournalAppendReceipt> {
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

        if let Some(committed) = log.receipts.get(&batch_id) {
            if committed.receipt.len() != mutations.len() {
                return Err(StorageError::JournalBatchConflict { batch_id }.into());
            }
            let normalized: Vec<_> = committed
                .receipt
                .id_range()
                .into_iter()
                .flatten()
                .zip(mutations.iter())
                .map(|(id, record)| record.with_storage_id(id))
                .collect();
            if normalized != committed.records {
                return Err(StorageError::JournalBatchConflict { batch_id }.into());
            }
            return Ok(committed.receipt);
        }

        let range = journal_id_range(log.last_id, mutations.len())?;
        let receipt = JournalAppendReceipt::from_range(batch_id, range, mutations.len());
        let normalized: Vec<_> = receipt
            .id_range()
            .into_iter()
            .flatten()
            .zip(mutations.iter())
            .map(|(id, record)| record.with_storage_id(id))
            .collect();
        log.records.extend(normalized.iter().copied());
        if let Some(final_id) = receipt.last_id() {
            log.last_id = Some(final_id);
        }
        log.receipts.insert(
            batch_id,
            CommittedJournalBatch {
                receipt,
                records: normalized,
            },
        );
        Ok(receipt)
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

#[cfg(test)]
mod tests {
    use ferrumc_math::BlockPos;
    use ferrumc_world::BlockStateId;

    use super::*;
    use crate::{MutationActor, MutationLogCause, SchemaVersion};

    fn mutation(provisional_id: u64, tick: u64) -> BlockMutationLogRecord {
        BlockMutationLogRecord::new(
            SchemaVersion::new(1),
            provisional_id,
            tick,
            MutationActor::System,
            BlockPos::new(0, 64, 0),
            BlockStateId::new(1),
            BlockStateId::new(2),
            MutationLogCause::Test,
        )
    }

    #[tokio::test]
    async fn in_memory_journal_assigns_storage_owned_ids() {
        let store = InMemoryStore::new();
        store
            .append_block_mutations(vec![mutation(90, 1), mutation(90, 2)])
            .await
            .expect("first append");
        store
            .append_block_mutations(vec![mutation(0, 3)])
            .await
            .expect("second append");

        let journal = store.mutation_log.read().expect("journal lock");
        let ids: Vec<_> = journal
            .records
            .iter()
            .map(BlockMutationLogRecord::id)
            .collect();
        let ticks: Vec<_> = journal
            .records
            .iter()
            .map(BlockMutationLogRecord::tick)
            .collect();
        assert_eq!(ids, vec![0, 1, 2]);
        assert_eq!(ticks, vec![1, 2, 3]);
        assert_eq!(journal.last_id, Some(2));
    }

    #[tokio::test]
    async fn in_memory_journal_overflow_preserves_state() {
        let store = InMemoryStore::new();
        {
            let mut journal = store.mutation_log.write().expect("journal lock");
            journal.last_id = Some(u64::MAX);
            journal.records.push(mutation(u64::MAX, 1));
        }

        let error = store
            .append_block_mutations(vec![mutation(0, 2)])
            .await
            .expect_err("exhausted sequence");
        assert!(matches!(error, ferrumc_core::ServerError::Capacity(_)));
        let journal = store.mutation_log.read().expect("journal lock");
        assert_eq!(journal.last_id, Some(u64::MAX));
        assert_eq!(journal.records.len(), 1);
        assert_eq!(journal.records[0].id(), u64::MAX);
    }

    #[tokio::test]
    async fn in_memory_journal_receipts_are_idempotent() {
        let store = InMemoryStore::new();
        let id = JournalBatchId::from_bytes([4; 16]);
        let original = store
            .append_block_mutation_batch(id, vec![mutation(90, 10), mutation(91, 11)])
            .await
            .expect("first append");
        let replay = store
            .append_block_mutation_batch(id, vec![mutation(0, 10), mutation(1, 11)])
            .await
            .expect("provisional ids are normalized");
        assert_eq!(replay, original);

        let error = store
            .append_block_mutation_batch(id, vec![mutation(0, 12), mutation(1, 13)])
            .await
            .expect_err("different normalized payload conflicts");
        assert!(matches!(error, ferrumc_core::ServerError::InvalidState(_)));
        let journal = store.mutation_log.read().expect("journal lock");
        assert_eq!(journal.records.len(), 2);
        assert_eq!(journal.receipts.len(), 1);
    }

    #[tokio::test]
    async fn in_memory_empty_journal_receipt_reserves_its_token() {
        let store = InMemoryStore::new();
        let id = JournalBatchId::from_bytes([5; 16]);
        let receipt = store
            .append_block_mutation_batch(id, Vec::new())
            .await
            .expect("empty receipt");
        assert!(receipt.is_empty());
        assert_eq!(
            store
                .append_block_mutation_batch(id, Vec::new())
                .await
                .expect("empty replay"),
            receipt
        );
        let error = store
            .append_block_mutation_batch(id, vec![mutation(0, 1)])
            .await
            .expect_err("empty token cannot identify a nonempty batch");
        assert!(matches!(error, ferrumc_core::ServerError::InvalidState(_)));
    }
}
