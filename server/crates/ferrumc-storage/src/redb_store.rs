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
use crate::record::{ChunkRecord, EntityRecord, PlayerRecord};
use crate::store::{PlayerStore, PluginStore, WorldStore, MAX_PLUGIN_VALUE_LEN, MAX_SAVE_BATCH};

/// On-disk layout version recorded in the metadata table.
///
/// Distinct from a record's [`crate::SchemaVersion`]: this versions the overall
/// table/byte layout of the database file. Opening a file written under a
/// different value fails rather than risking a misread.
const STORE_FORMAT_VERSION: u64 = 1;

/// Metadata key under which [`STORE_FORMAT_VERSION`] is stored.
const META_FORMAT_KEY: &str = "format_version";

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
