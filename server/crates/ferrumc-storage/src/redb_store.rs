//! A redb-backed implementation of every storage trait.
//!
//! [`RedbStore`] is the durable backend chosen as the project default (see
//! `docs/adr/0007-storage-backend.md`). It keeps a single [`redb::Database`]
//! behind an [`Arc`]; that raw handle is private and never escapes the crate, so
//! no caller (and no plugin) can obtain a database handle or transaction, per the
//! storage and plugin models in `CLAUDE.md`.
//!
//! # Tables
//!
//! Each data category lives in its own table, plus a metadata table that records
//! the on-disk [format version](STORE_FORMAT_VERSION):
//!
//! - `ferrumc:meta` — schema/format metadata (`&str -> u64`)
//! - `ferrumc:chunk`, `ferrumc:entity`, `ferrumc:player`, `ferrumc:plugin` —
//!   raw `&[u8] -> &[u8]`, with typed keys and versioned records encoded by the
//!   private [`crate::codec`] module.
//!
//! # Async strategy
//!
//! redb transactions are synchronous and block, so every operation runs inside
//! [`tokio::task::spawn_blocking`]: the transaction (and any record
//! encode/decode) executes on a blocking thread, never on an async executor
//! worker. The trait methods are therefore only usable from within a Tokio
//! runtime. Batched saves commit the whole batch in one transaction, so a flush
//! is a single atomic commit.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use ferrumc_core::{PlayerId, PluginId, Result, ServerError};
use redb::{Database, ReadableTable, TableDefinition};

use crate::codec;
use crate::error::StorageError;
use crate::key::{ChunkKey, EntityKey, StorageKey};
use crate::record::{
    BlockMutationLogRecord, ChunkOverlayRecord, ChunkRecord, EntityRecord, PlayerRecord,
};
use crate::store::{
    journal_id_range, PlayerStore, PluginStore, WorldStore, MAX_PLUGIN_VALUE_LEN, MAX_SAVE_BATCH,
};

/// On-disk layout version recorded in the metadata table.
///
/// Distinct from a record's [`crate::SchemaVersion`]: this versions the overall
/// table/byte layout of the database file. Opening a file written under a
/// different value fails rather than risking a misread.
const STORE_FORMAT_VERSION: u64 = 1;

/// Metadata key under which [`STORE_FORMAT_VERSION`] is stored.
const META_FORMAT_KEY: &str = "format_version";

/// Metadata key storing the greatest durably allocated mutation-journal ID.
const META_LAST_MUTATION_ID_KEY: &str = "last_mutation_id";

/// Metadata table: small typed key-value entries describing the database itself.
const META_TABLE: TableDefinition<'_, &str, u64> = TableDefinition::new("ferrumc:meta");

/// Chunk table: [`ChunkKey`] bytes to [`ChunkRecord`] bytes.
const CHUNK_TABLE: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("ferrumc:chunk");

/// Entity table: [`EntityKey`] bytes to [`EntityRecord`] bytes.
const ENTITY_TABLE: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("ferrumc:entity");

/// Player table: [`PlayerId`] bytes to [`PlayerRecord`] bytes.
const PLAYER_TABLE: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("ferrumc:player");

/// Plugin table: namespaced `(plugin, key)` bytes to the raw stored value.
const PLUGIN_TABLE: TableDefinition<'_, &[u8], &[u8]> = TableDefinition::new("ferrumc:plugin");

/// Chunk-overlay table: [`ChunkKey`] bytes to [`ChunkOverlayRecord`] bytes.
///
/// Holds only the player-modified sections of a chunk; an untouched generated
/// chunk never appears here, so generated terrain costs zero storage.
const CHUNK_OVERLAY_TABLE: TableDefinition<'_, &[u8], &[u8]> =
    TableDefinition::new("ferrumc:chunk_overlay");

/// Mutation-journal table: monotonic entry id (`u64` big-endian bytes) to a
/// [`BlockMutationLogRecord`]. Append-only; keyed by id so a forward scan yields
/// the journal in order.
const MUTATION_LOG_TABLE: TableDefinition<'_, &[u8], &[u8]> =
    TableDefinition::new("ferrumc:mutation_log");

/// Wraps any redb error as a classified [`ServerError`].
fn backend_err<E: fmt::Display>(err: E) -> ServerError {
    StorageError::backend(err.to_string()).into()
}

/// Wraps a `spawn_blocking` join failure (the blocking task panicked or was
/// cancelled) as a classified [`ServerError`].
///
/// Generic over the displayable error so it can be passed directly to `map_err`
/// without the closure that a concrete-typed by-value parameter would require.
fn join_err<E: fmt::Display>(err: E) -> ServerError {
    StorageError::backend(format!("storage task failed: {err}")).into()
}

/// Decodes an eight-byte big-endian mutation-journal key.
fn mutation_id_from_key(key: &[u8]) -> std::result::Result<u64, StorageError> {
    let bytes: [u8; 8] = key
        .try_into()
        .map_err(|_| StorageError::MalformedJournalKey { len: key.len() })?;
    Ok(u64::from_be_bytes(bytes))
}

/// A durable, redb-backed store implementing [`WorldStore`], [`PlayerStore`],
/// and [`PluginStore`].
///
/// Construct one with [`RedbStore::open`] and share it with `Arc` to hand the
/// simulation and plugin layers `Arc<dyn WorldStore>` (and friends). The
/// underlying database handle is private and never exposed.
pub struct RedbStore {
    /// The redb handle. Private and `Arc`-shared so it can be cloned into each
    /// `spawn_blocking` closure; it never leaves this crate.
    db: Arc<Database>,
}

impl fmt::Debug for RedbStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Deliberately opaque: the raw database handle is not part of the API.
        f.debug_struct("RedbStore").finish_non_exhaustive()
    }
}

impl RedbStore {
    /// Opens the database at `path`, creating it if it does not exist.
    ///
    /// On a fresh file the metadata and data tables are created and the
    /// [format version](STORE_FORMAT_VERSION) is stamped. On an existing file the
    /// recorded version is checked and a mismatch is rejected with
    /// [`ServerError::Internal`] (via [`StorageError::Backend`]) rather than
    /// risking a misread of an incompatible layout.
    ///
    /// This is synchronous I/O and must be called outside the async hot path
    /// (for example during startup), not from inside a running tick.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::create(path).map_err(backend_err)?;
        Self::initialize(&db)?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Ensures every table exists and the format version is present and
    /// compatible, in a single write transaction.
    fn initialize(db: &Database) -> Result<()> {
        let txn = db.begin_write().map_err(backend_err)?;
        // Create the data tables up front so later read transactions never fail
        // with `TableDoesNotExist` on a brand-new database.
        txn.open_table(CHUNK_TABLE).map_err(backend_err)?;
        txn.open_table(ENTITY_TABLE).map_err(backend_err)?;
        txn.open_table(PLAYER_TABLE).map_err(backend_err)?;
        txn.open_table(PLUGIN_TABLE).map_err(backend_err)?;
        // Overlay + journal tables are created lazily here too. They are purely
        // additive to the v1 layout (no existing table changes shape), so opening
        // a pre-existing v1 database simply gains two empty tables rather than
        // tripping a format-version mismatch — see docs/adr/0007 for the rationale.
        txn.open_table(CHUNK_OVERLAY_TABLE).map_err(backend_err)?;
        let journal_last_id = {
            let journal = txn.open_table(MUTATION_LOG_TABLE).map_err(backend_err)?;
            let last_id = journal
                .last()
                .map_err(backend_err)?
                .map(|(key, _value)| mutation_id_from_key(key.value()))
                .transpose()?;
            last_id
        };
        {
            let mut meta = txn.open_table(META_TABLE).map_err(backend_err)?;
            let existing = meta
                .get(META_FORMAT_KEY)
                .map_err(backend_err)?
                .map(|guard| guard.value());
            match existing {
                Some(found) if found != STORE_FORMAT_VERSION => {
                    return Err(StorageError::backend(format!(
                        "unsupported store format version {found} (expected {STORE_FORMAT_VERSION})"
                    ))
                    .into());
                }
                Some(_) => {}
                None => {
                    meta.insert(META_FORMAT_KEY, STORE_FORMAT_VERSION)
                        .map_err(backend_err)?;
                }
            }

            // Older databases have journal rows but no durable sequence key.
            // Reconcile once on open from the B-tree's greatest key; appends
            // thereafter read only this metadata entry inside their write
            // transaction, so allocation never scans the journal hot path.
            let stored_last_id = meta
                .get(META_LAST_MUTATION_ID_KEY)
                .map_err(backend_err)?
                .map(|guard| guard.value());
            let reconciled_last_id = match (stored_last_id, journal_last_id) {
                (Some(stored), Some(journal)) => Some(stored.max(journal)),
                (stored @ Some(_), None) => stored,
                (None, journal) => journal,
            };
            if reconciled_last_id != stored_last_id {
                if let Some(last_id) = reconciled_last_id {
                    meta.insert(META_LAST_MUTATION_ID_KEY, last_id)
                        .map_err(backend_err)?;
                }
            }
        }
        txn.commit().map_err(backend_err)?;
        Ok(())
    }
}

#[async_trait]
impl WorldStore for RedbStore {
    async fn load_chunk(&self, key: ChunkKey) -> Result<Option<ChunkRecord>> {
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<Option<ChunkRecord>> {
            let txn = db.begin_read().map_err(backend_err)?;
            let table = txn.open_table(CHUNK_TABLE).map_err(backend_err)?;
            let key_bytes = codec::chunk_key_bytes(key);
            let Some(guard) = table.get(key_bytes.as_slice()).map_err(backend_err)? else {
                return Ok(None);
            };
            Ok(Some(codec::decode_chunk_record(guard.value())?))
        });
        join.await.map_err(join_err)?
    }

    async fn save_chunk(&self, key: ChunkKey, record: ChunkRecord) -> Result<()> {
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let txn = db.begin_write().map_err(backend_err)?;
            {
                let mut table = txn.open_table(CHUNK_TABLE).map_err(backend_err)?;
                let key_bytes = codec::chunk_key_bytes(key);
                let value = codec::encode_chunk_record(&record);
                table
                    .insert(key_bytes.as_slice(), value.as_slice())
                    .map_err(backend_err)?;
            }
            txn.commit().map_err(backend_err)?;
            Ok(())
        });
        join.await.map_err(join_err)?
    }

    async fn save_chunks(&self, chunks: Vec<(ChunkKey, ChunkRecord)>) -> Result<()> {
        if chunks.len() > MAX_SAVE_BATCH {
            return Err(StorageError::BatchTooLarge {
                len: chunks.len(),
                max: MAX_SAVE_BATCH,
            }
            .into());
        }
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let txn = db.begin_write().map_err(backend_err)?;
            {
                // The whole batch commits in one transaction: a single atomic
                // flush rather than one commit per chunk.
                let mut table = txn.open_table(CHUNK_TABLE).map_err(backend_err)?;
                for (key, record) in &chunks {
                    let key_bytes = codec::chunk_key_bytes(*key);
                    let value = codec::encode_chunk_record(record);
                    table
                        .insert(key_bytes.as_slice(), value.as_slice())
                        .map_err(backend_err)?;
                }
            }
            txn.commit().map_err(backend_err)?;
            Ok(())
        });
        join.await.map_err(join_err)?
    }

    async fn delete_chunk(&self, key: ChunkKey) -> Result<bool> {
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<bool> {
            let txn = db.begin_write().map_err(backend_err)?;
            let removed = {
                let mut table = txn.open_table(CHUNK_TABLE).map_err(backend_err)?;
                let key_bytes = codec::chunk_key_bytes(key);
                let old = table.remove(key_bytes.as_slice()).map_err(backend_err)?;
                old.is_some()
            };
            txn.commit().map_err(backend_err)?;
            Ok(removed)
        });
        join.await.map_err(join_err)?
    }

    async fn load_chunk_overlay(&self, key: ChunkKey) -> Result<Option<ChunkOverlayRecord>> {
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<Option<ChunkOverlayRecord>> {
            let txn = db.begin_read().map_err(backend_err)?;
            let table = txn.open_table(CHUNK_OVERLAY_TABLE).map_err(backend_err)?;
            let key_bytes = codec::chunk_key_bytes(key);
            let Some(guard) = table.get(key_bytes.as_slice()).map_err(backend_err)? else {
                return Ok(None);
            };
            Ok(Some(codec::decode_chunk_overlay_record(guard.value())?))
        });
        join.await.map_err(join_err)?
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
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let txn = db.begin_write().map_err(backend_err)?;
            {
                // One atomic commit for the whole overlay batch, mirroring
                // `save_chunks`.
                let mut table = txn.open_table(CHUNK_OVERLAY_TABLE).map_err(backend_err)?;
                for (key, record) in &overlays {
                    let key_bytes = codec::chunk_key_bytes(*key);
                    let value = codec::encode_chunk_overlay_record(record);
                    table
                        .insert(key_bytes.as_slice(), value.as_slice())
                        .map_err(backend_err)?;
                }
            }
            txn.commit().map_err(backend_err)?;
            Ok(())
        });
        join.await.map_err(join_err)?
    }

    async fn append_block_mutations(&self, mutations: Vec<BlockMutationLogRecord>) -> Result<()> {
        if mutations.len() > MAX_SAVE_BATCH {
            return Err(StorageError::BatchTooLarge {
                len: mutations.len(),
                max: MAX_SAVE_BATCH,
            }
            .into());
        }
        if mutations.is_empty() {
            return Ok(());
        }
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let txn = db.begin_write().map_err(backend_err)?;
            let previous_last_id = {
                let meta = txn.open_table(META_TABLE).map_err(backend_err)?;
                let last_id = meta
                    .get(META_LAST_MUTATION_ID_KEY)
                    .map_err(backend_err)?
                    .map(|guard| guard.value());
                last_id
            };
            let Some((first_id, final_id)) = journal_id_range(previous_last_id, mutations.len())?
            else {
                return Ok(());
            };
            {
                let mut table = txn.open_table(MUTATION_LOG_TABLE).map_err(backend_err)?;

                // A stale/corrupt sequence must never turn `insert` into a
                // replacement. Preflight the complete range before writing.
                for id in first_id..=final_id {
                    let key_bytes = id.to_be_bytes();
                    if table
                        .get(key_bytes.as_slice())
                        .map_err(backend_err)?
                        .is_some()
                    {
                        return Err(StorageError::JournalIdCollision { id }.into());
                    }
                }

                for (record, id) in mutations.into_iter().zip(first_id..=final_id) {
                    let record = record.with_storage_id(id);
                    let key_bytes = id.to_be_bytes();
                    let value = codec::encode_mutation_log_record(&record);
                    let replaced = table
                        .insert(key_bytes.as_slice(), value.as_slice())
                        .map_err(backend_err)?;
                    if replaced.is_some() {
                        return Err(StorageError::JournalIdCollision { id }.into());
                    }
                }
            }
            {
                let mut meta = txn.open_table(META_TABLE).map_err(backend_err)?;
                meta.insert(META_LAST_MUTATION_ID_KEY, final_id)
                    .map_err(backend_err)?;
            }
            txn.commit().map_err(backend_err)?;
            Ok(())
        });
        join.await.map_err(join_err)?
    }

    async fn load_entity(&self, key: EntityKey) -> Result<Option<EntityRecord>> {
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<Option<EntityRecord>> {
            let txn = db.begin_read().map_err(backend_err)?;
            let table = txn.open_table(ENTITY_TABLE).map_err(backend_err)?;
            let key_bytes = codec::entity_key_bytes(key);
            let Some(guard) = table.get(key_bytes.as_slice()).map_err(backend_err)? else {
                return Ok(None);
            };
            Ok(Some(codec::decode_entity_record(guard.value())?))
        });
        join.await.map_err(join_err)?
    }

    async fn save_entity(&self, key: EntityKey, record: EntityRecord) -> Result<()> {
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let txn = db.begin_write().map_err(backend_err)?;
            {
                let mut table = txn.open_table(ENTITY_TABLE).map_err(backend_err)?;
                let key_bytes = codec::entity_key_bytes(key);
                let value = codec::encode_entity_record(&record);
                table
                    .insert(key_bytes.as_slice(), value.as_slice())
                    .map_err(backend_err)?;
            }
            txn.commit().map_err(backend_err)?;
            Ok(())
        });
        join.await.map_err(join_err)?
    }

    async fn save_entities(&self, entities: Vec<(EntityKey, EntityRecord)>) -> Result<()> {
        if entities.len() > MAX_SAVE_BATCH {
            return Err(StorageError::BatchTooLarge {
                len: entities.len(),
                max: MAX_SAVE_BATCH,
            }
            .into());
        }
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let txn = db.begin_write().map_err(backend_err)?;
            {
                let mut table = txn.open_table(ENTITY_TABLE).map_err(backend_err)?;
                for (key, record) in &entities {
                    let key_bytes = codec::entity_key_bytes(*key);
                    let value = codec::encode_entity_record(record);
                    table
                        .insert(key_bytes.as_slice(), value.as_slice())
                        .map_err(backend_err)?;
                }
            }
            txn.commit().map_err(backend_err)?;
            Ok(())
        });
        join.await.map_err(join_err)?
    }

    async fn delete_entity(&self, key: EntityKey) -> Result<bool> {
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<bool> {
            let txn = db.begin_write().map_err(backend_err)?;
            let removed = {
                let mut table = txn.open_table(ENTITY_TABLE).map_err(backend_err)?;
                let key_bytes = codec::entity_key_bytes(key);
                let old = table.remove(key_bytes.as_slice()).map_err(backend_err)?;
                old.is_some()
            };
            txn.commit().map_err(backend_err)?;
            Ok(removed)
        });
        join.await.map_err(join_err)?
    }
}

#[async_trait]
impl PlayerStore for RedbStore {
    async fn load_player(&self, id: PlayerId) -> Result<Option<PlayerRecord>> {
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<Option<PlayerRecord>> {
            let txn = db.begin_read().map_err(backend_err)?;
            let table = txn.open_table(PLAYER_TABLE).map_err(backend_err)?;
            let key_bytes = codec::player_key_bytes(id);
            let Some(guard) = table.get(key_bytes.as_slice()).map_err(backend_err)? else {
                return Ok(None);
            };
            Ok(Some(codec::decode_player_record(guard.value())?))
        });
        join.await.map_err(join_err)?
    }

    async fn save_player(&self, id: PlayerId, record: PlayerRecord) -> Result<()> {
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let txn = db.begin_write().map_err(backend_err)?;
            {
                let mut table = txn.open_table(PLAYER_TABLE).map_err(backend_err)?;
                let key_bytes = codec::player_key_bytes(id);
                let value = codec::encode_player_record(&record);
                table
                    .insert(key_bytes.as_slice(), value.as_slice())
                    .map_err(backend_err)?;
            }
            txn.commit().map_err(backend_err)?;
            Ok(())
        });
        join.await.map_err(join_err)?
    }

    async fn delete_player(&self, id: PlayerId) -> Result<bool> {
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<bool> {
            let txn = db.begin_write().map_err(backend_err)?;
            let removed = {
                let mut table = txn.open_table(PLAYER_TABLE).map_err(backend_err)?;
                let key_bytes = codec::player_key_bytes(id);
                let old = table.remove(key_bytes.as_slice()).map_err(backend_err)?;
                old.is_some()
            };
            txn.commit().map_err(backend_err)?;
            Ok(removed)
        });
        join.await.map_err(join_err)?
    }
}

#[async_trait]
impl PluginStore for RedbStore {
    async fn get(&self, plugin: &PluginId, key: &StorageKey) -> Result<Option<Vec<u8>>> {
        let key_bytes = codec::plugin_key_bytes(plugin, key);
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>> {
            let txn = db.begin_read().map_err(backend_err)?;
            let table = txn.open_table(PLUGIN_TABLE).map_err(backend_err)?;
            let Some(guard) = table.get(key_bytes.as_slice()).map_err(backend_err)? else {
                return Ok(None);
            };
            Ok(Some(guard.value().to_vec()))
        });
        join.await.map_err(join_err)?
    }

    async fn put(&self, plugin: &PluginId, key: StorageKey, value: Vec<u8>) -> Result<()> {
        if value.len() > MAX_PLUGIN_VALUE_LEN {
            return Err(StorageError::ValueTooLarge {
                len: value.len(),
                max: MAX_PLUGIN_VALUE_LEN,
            }
            .into());
        }
        let key_bytes = codec::plugin_key_bytes(plugin, &key);
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<()> {
            let txn = db.begin_write().map_err(backend_err)?;
            {
                let mut table = txn.open_table(PLUGIN_TABLE).map_err(backend_err)?;
                table
                    .insert(key_bytes.as_slice(), value.as_slice())
                    .map_err(backend_err)?;
            }
            txn.commit().map_err(backend_err)?;
            Ok(())
        });
        join.await.map_err(join_err)?
    }

    async fn delete(&self, plugin: &PluginId, key: &StorageKey) -> Result<bool> {
        let key_bytes = codec::plugin_key_bytes(plugin, key);
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<bool> {
            let txn = db.begin_write().map_err(backend_err)?;
            let removed = {
                let mut table = txn.open_table(PLUGIN_TABLE).map_err(backend_err)?;
                let old = table.remove(key_bytes.as_slice()).map_err(backend_err)?;
                old.is_some()
            };
            txn.commit().map_err(backend_err)?;
            Ok(removed)
        });
        join.await.map_err(join_err)?
    }

    async fn keys(&self, plugin: &PluginId) -> Result<Vec<StorageKey>> {
        let prefix = codec::plugin_prefix(plugin);
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<Vec<StorageKey>> {
            let txn = db.begin_read().map_err(backend_err)?;
            let table = txn.open_table(PLUGIN_TABLE).map_err(backend_err)?;
            let mut keys = Vec::new();
            // Scan forward from the namespace prefix; the entries for one plugin
            // are contiguous, so stop as soon as the prefix no longer matches.
            // This is what enforces enumeration isolation between plugins.
            for entry in table.range(prefix.as_slice()..).map_err(backend_err)? {
                let (stored_key, _value) = entry.map_err(backend_err)?;
                let stored_bytes = stored_key.value();
                if !stored_bytes.starts_with(prefix.as_slice()) {
                    break;
                }
                keys.push(codec::plugin_key_from_bytes(&prefix, stored_bytes)?);
            }
            Ok(keys)
        });
        join.await.map_err(join_err)?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrumc_math::BlockPos;
    use ferrumc_world::BlockStateId;
    use tempfile::TempDir;
    use tokio::sync::Barrier;

    use super::*;
    use crate::{MutationActor, MutationLogCause, SchemaVersion};

    fn mutation(local_id: u64, tick: u64) -> BlockMutationLogRecord {
        let coordinate = i32::try_from(tick).expect("test tick fits i32");
        let state = u32::try_from(tick).expect("test tick fits u32");
        BlockMutationLogRecord::new(
            SchemaVersion::new(1),
            local_id,
            tick,
            MutationActor::System,
            BlockPos::new(coordinate, 64, 0),
            BlockStateId::new(state),
            BlockStateId::new(state + 1),
            MutationLogCause::Test,
        )
    }

    fn journal_snapshot(store: &RedbStore) -> Vec<(u64, Vec<u8>)> {
        let txn = store.db.begin_read().expect("read transaction");
        let table = txn.open_table(MUTATION_LOG_TABLE).expect("journal table");
        table
            .iter()
            .expect("journal iterator")
            .map(|entry| {
                let (key, value) = entry.expect("journal entry");
                let key: [u8; 8] = key.value().try_into().expect("u64 journal key");
                (u64::from_be_bytes(key), value.value().to_vec())
            })
            .collect()
    }

    fn seed_journal(store: &RedbStore, last_id: Option<u64>, records: &[BlockMutationLogRecord]) {
        let txn = store.db.begin_write().expect("write transaction");
        {
            let mut table = txn.open_table(MUTATION_LOG_TABLE).expect("journal table");
            for record in records {
                let key = record.id().to_be_bytes();
                let value = codec::encode_mutation_log_record(record);
                table
                    .insert(key.as_slice(), value.as_slice())
                    .expect("seed journal record");
            }
        }
        {
            let mut meta = txn.open_table(META_TABLE).expect("metadata table");
            if let Some(last_id) = last_id {
                meta.insert(META_LAST_MUTATION_ID_KEY, last_id)
                    .expect("seed sequence");
            } else {
                meta.remove(META_LAST_MUTATION_ID_KEY)
                    .expect("remove sequence");
            }
        }
        txn.commit().expect("commit seed");
    }

    fn stored_last_id(store: &RedbStore) -> Option<u64> {
        let txn = store.db.begin_read().expect("read transaction");
        let meta = txn.open_table(META_TABLE).expect("metadata table");
        let last_id = meta
            .get(META_LAST_MUTATION_ID_KEY)
            .expect("read sequence")
            .map(|guard| guard.value());
        last_id
    }

    #[tokio::test]
    async fn journal_sequence_survives_restart_without_overwrite() {
        const REOPEN_CYCLES: u64 = 3;
        const RECORDS_PER_CYCLE: u64 = 3;

        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("journal.redb");
        let mut prior_snapshot = Vec::new();

        for cycle in 0..=REOPEN_CYCLES {
            let snapshot = {
                let store = RedbStore::open(&path).expect("open store");
                let tick_base = cycle * RECORDS_PER_CYCLE;
                let batch = (0..RECORDS_PER_CYCLE)
                    .map(|local_id| mutation(local_id, tick_base + local_id))
                    .collect();
                store
                    .append_block_mutations(batch)
                    .await
                    .expect("append mutation batch");
                journal_snapshot(&store)
            };

            let expected_len = usize::try_from((cycle + 1) * RECORDS_PER_CYCLE)
                .expect("test journal length fits usize");
            assert_eq!(
                snapshot.len(),
                expected_len,
                "restart must append instead of overwriting prior history"
            );
            assert_eq!(
                &snapshot[..prior_snapshot.len()],
                prior_snapshot.as_slice(),
                "every previously committed journal byte must remain unchanged"
            );
            prior_snapshot = snapshot;
        }

        for (expected_id, (stored_id, bytes)) in prior_snapshot.iter().enumerate() {
            let expected_id = u64::try_from(expected_id).expect("test id fits u64");
            assert_eq!(*stored_id, expected_id, "journal keys must be contiguous");
            let record = codec::decode_mutation_log_record(bytes).expect("decode journal record");
            assert_eq!(
                record.id(),
                expected_id,
                "encoded record id must match its durable key"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_journal_batches_receive_disjoint_atomic_ranges() {
        const TASKS: usize = 8;
        const RECORDS_PER_TASK: usize = 4;

        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(RedbStore::open(dir.path().join("journal.redb")).expect("open store"));
        let barrier = Arc::new(Barrier::new(TASKS));
        let mut handles = Vec::with_capacity(TASKS);

        for task in 0..TASKS {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                let tick_base = u64::try_from(task * 100).expect("test tick fits u64");
                let batch = (0..RECORDS_PER_TASK)
                    .map(|local| {
                        let local = u64::try_from(local).expect("local id fits u64");
                        mutation(10_000 + local, tick_base + local)
                    })
                    .collect();
                barrier.wait().await;
                store.append_block_mutations(batch).await
            }));
        }
        for handle in handles {
            handle
                .await
                .expect("append task did not panic")
                .expect("append succeeded");
        }

        let snapshot = journal_snapshot(&store);
        assert_eq!(snapshot.len(), TASKS * RECORDS_PER_TASK);
        let decoded: Vec<_> = snapshot
            .iter()
            .map(|(id, bytes)| {
                let record =
                    codec::decode_mutation_log_record(bytes).expect("decode journal record");
                assert_eq!(record.id(), *id, "encoded id must equal durable key");
                (*id, record.tick())
            })
            .collect();
        for (expected, (id, _tick)) in decoded.iter().enumerate() {
            assert_eq!(*id, u64::try_from(expected).expect("id fits u64"));
        }

        for task in 0..TASKS {
            let tick_base = u64::try_from(task * 100).expect("test tick fits u64");
            let mut batch: Vec<_> = decoded
                .iter()
                .copied()
                .filter(|(_id, tick)| *tick >= tick_base && *tick < tick_base + 100)
                .collect();
            batch.sort_by_key(|(_id, tick)| *tick);
            assert_eq!(batch.len(), RECORDS_PER_TASK, "task batch must survive");
            for pair in batch.windows(2) {
                assert_eq!(pair[1].0, pair[0].0 + 1, "one batch must be contiguous");
                assert_eq!(pair[1].1, pair[0].1 + 1, "batch order must be stable");
            }
        }
    }

    #[tokio::test]
    async fn duplicate_allocated_id_rejects_the_entire_batch() {
        let dir = TempDir::new().expect("temp dir");
        let store = RedbStore::open(dir.path().join("journal.redb")).expect("open store");
        let first = mutation(0, 10);
        let sentinel = mutation(2, 20);
        seed_journal(&store, Some(0), &[first, sentinel]);
        let before = journal_snapshot(&store);

        let error = store
            .append_block_mutations(vec![mutation(99, 30), mutation(99, 31)])
            .await
            .expect_err("collision must reject the append");
        assert!(matches!(error, ServerError::Internal { .. }));
        assert_eq!(
            journal_snapshot(&store),
            before,
            "collision must roll back every record in the batch"
        );
        assert_eq!(stored_last_id(&store), Some(0));
    }

    #[tokio::test]
    async fn journal_sequence_allows_u64_max_then_reports_exhaustion() {
        let dir = TempDir::new().expect("temp dir");
        let store = RedbStore::open(dir.path().join("journal.redb")).expect("open store");
        let penultimate = mutation(u64::MAX - 1, 10);
        seed_journal(&store, Some(u64::MAX - 1), &[penultimate]);
        let before = journal_snapshot(&store);

        let error = store
            .append_block_mutations(vec![mutation(0, 20), mutation(0, 21)])
            .await
            .expect_err("two ids cannot fit at the boundary");
        assert!(matches!(error, ServerError::Capacity(_)));
        assert_eq!(journal_snapshot(&store), before);
        assert_eq!(stored_last_id(&store), Some(u64::MAX - 1));

        store
            .append_block_mutations(vec![mutation(0, 30)])
            .await
            .expect("u64::MAX itself remains allocatable");
        let at_max = journal_snapshot(&store);
        assert_eq!(at_max.len(), 2);
        assert_eq!(at_max[1].0, u64::MAX);
        let record = codec::decode_mutation_log_record(&at_max[1].1).expect("decode max-id record");
        assert_eq!(record.id(), u64::MAX);
        assert_eq!(stored_last_id(&store), Some(u64::MAX));

        let error = store
            .append_block_mutations(vec![mutation(0, 40)])
            .await
            .expect_err("sequence after u64::MAX is exhausted");
        assert!(matches!(error, ServerError::Capacity(_)));
        assert_eq!(journal_snapshot(&store), at_max);
        assert_eq!(stored_last_id(&store), Some(u64::MAX));
    }

    #[tokio::test]
    async fn legacy_journal_without_sequence_resumes_after_greatest_key() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("journal.redb");
        let legacy_snapshot = {
            let store = RedbStore::open(&path).expect("open store");
            seed_journal(&store, None, &[mutation(4, 4), mutation(9, 9)]);
            journal_snapshot(&store)
        };

        let store = RedbStore::open(&path).expect("reopen legacy store");
        assert_eq!(stored_last_id(&store), Some(9));
        store
            .append_block_mutations(vec![mutation(0, 10)])
            .await
            .expect("append after legacy journal");
        let snapshot = journal_snapshot(&store);
        assert_eq!(
            &snapshot[..legacy_snapshot.len()],
            legacy_snapshot.as_slice()
        );
        assert_eq!(snapshot.last().map(|(id, _bytes)| *id), Some(10));
    }

    #[tokio::test]
    async fn stale_sequence_metadata_reconciles_to_greatest_journal_key() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("journal.redb");
        let prior_snapshot = {
            let store = RedbStore::open(&path).expect("open store");
            seed_journal(&store, Some(2), &[mutation(2, 2), mutation(9, 9)]);
            journal_snapshot(&store)
        };

        let store = RedbStore::open(&path).expect("reopen store");
        assert_eq!(stored_last_id(&store), Some(9));
        store
            .append_block_mutations(vec![mutation(0, 10)])
            .await
            .expect("append after reconciliation");
        let snapshot = journal_snapshot(&store);
        assert_eq!(&snapshot[..prior_snapshot.len()], prior_snapshot.as_slice());
        assert_eq!(snapshot.last().map(|(id, _bytes)| *id), Some(10));
    }

    #[tokio::test]
    async fn rejected_and_empty_batches_do_not_advance_sequence() {
        let dir = TempDir::new().expect("temp dir");
        let store = RedbStore::open(dir.path().join("journal.redb")).expect("open store");
        store
            .append_block_mutations(vec![mutation(99, 1)])
            .await
            .expect("seed sequence");
        let before = journal_snapshot(&store);

        store
            .append_block_mutations(Vec::new())
            .await
            .expect("empty append");
        assert_eq!(journal_snapshot(&store), before);
        assert_eq!(stored_last_id(&store), Some(0));

        let oversized = (0..=MAX_SAVE_BATCH)
            .map(|id| mutation(u64::try_from(id).expect("test id fits u64"), 2))
            .collect();
        let error = store
            .append_block_mutations(oversized)
            .await
            .expect_err("oversized append must be rejected");
        assert!(matches!(error, ServerError::Capacity(_)));
        assert_eq!(journal_snapshot(&store), before);
        assert_eq!(stored_last_id(&store), Some(0));
    }

    #[test]
    fn mutation_journal_keys_require_exactly_eight_bytes() {
        for malformed in [&[][..], &[0; 7], &[0; 9]] {
            let error = mutation_id_from_key(malformed).expect_err("malformed key");
            assert!(matches!(
                error,
                StorageError::MalformedJournalKey { len } if len == malformed.len()
            ));
        }
        assert_eq!(
            mutation_id_from_key(&u64::MAX.to_be_bytes()).expect("valid key"),
            u64::MAX
        );
    }

    #[test]
    fn opening_store_rejects_a_malformed_greatest_journal_key() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("journal.redb");
        {
            let store = RedbStore::open(&path).expect("open store");
            let txn = store.db.begin_write().expect("write transaction");
            {
                let mut table = txn.open_table(MUTATION_LOG_TABLE).expect("journal table");
                let malformed_key = [0xff; 9];
                let value = [0];
                table
                    .insert(malformed_key.as_slice(), value.as_slice())
                    .expect("seed malformed key");
            }
            txn.commit().expect("commit malformed key");
        }

        let error = RedbStore::open(&path).expect_err("malformed key must reject open");
        assert!(matches!(error, ServerError::Internal { .. }));
    }

    #[test]
    fn journal_sequence_range_reports_typed_exhaustion() {
        assert_eq!(
            journal_id_range(Some(u64::MAX - 1), 1).expect("max id is allocatable"),
            Some((u64::MAX, u64::MAX))
        );
        assert!(matches!(
            journal_id_range(Some(u64::MAX - 1), 2),
            Err(StorageError::JournalSequenceExhausted {
                last_id,
                requested: 2,
            }) if last_id == u64::MAX - 1
        ));
        assert!(matches!(
            journal_id_range(Some(u64::MAX), 1),
            Err(StorageError::JournalSequenceExhausted {
                last_id: u64::MAX,
                requested: 1,
            })
        ));
    }
}
