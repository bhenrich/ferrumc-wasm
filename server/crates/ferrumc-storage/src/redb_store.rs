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
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::mem::size_of;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use ferrumc_core::{PlayerId, PluginId, Result, ServerError};
use redb::{Database, ReadableTable, TableDefinition, TableHandle};

use crate::codec;
use crate::error::StorageError;
use crate::key::{ChunkKey, EntityKey, StorageKey};
use crate::record::{
    BlockMutationLogRecord, ChunkOverlayRecord, ChunkRecord, EntityRecord, PlayerRecord,
};
use crate::store::{
    journal_id_range, PlayerStore, PluginStore, WorldStore, MAX_PLUGIN_VALUE_LEN, MAX_SAVE_BATCH,
};
use crate::{JournalAppendReceipt, JournalBatchId};

/// On-disk layout version recorded in the metadata table.
///
/// Distinct from a record's [`crate::SchemaVersion`]: this versions the overall
/// table/byte layout of the database file. Opening a file written under a
/// different value fails rather than risking a misread.
const STORE_FORMAT_VERSION: u64 = 2;

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

/// Mutation-batch receipt table: 16-byte [`JournalBatchId`] to a fixed-width
/// durable sequence range. Receipt insertion is atomic with its journal rows.
const MUTATION_BATCH_TABLE: TableDefinition<'_, &[u8], &[u8]> =
    TableDefinition::new("ferrumc:mutation_batch");

/// `first_id(u64) ++ record_count(u64)`.
const JOURNAL_RECEIPT_ENCODED_LEN: usize = 16;

/// Whether this process atomically created the database file.
#[derive(Clone, Copy)]
enum StoreOrigin {
    Fresh,
    Existing,
}

/// Wraps any redb error as a classified [`ServerError`].
fn backend_err<E: fmt::Display>(err: E) -> ServerError {
    StorageError::backend(err.to_string()).into()
}

/// Builds the stable operator-facing refusal for an incompatible durable format.
fn incompatible_data_err() -> ServerError {
    StorageError::IncompatiblePreAlphaData.into()
}

/// Classifies redb's own durable-format mismatch without hiding other backend
/// failures such as locks or I/O errors.
fn database_open_err(err: redb::DatabaseError) -> ServerError {
    match err {
        redb::DatabaseError::UpgradeRequired(_) => incompatible_data_err(),
        redb::DatabaseError::Storage(redb::StorageError::Io(io_error))
            if matches!(
                io_error.kind(),
                ErrorKind::InvalidData | ErrorKind::UnexpectedEof
            ) =>
        {
            incompatible_data_err()
        }
        other => backend_err(other),
    }
}

/// Classifies a known table's shape mismatch as an incompatible `FerrumC`
/// format, while retaining backend classification for operational failures.
fn schema_table_err(err: redb::TableError) -> ServerError {
    match err {
        redb::TableError::TableTypeMismatch { .. }
        | redb::TableError::TableIsMultimap(_)
        | redb::TableError::TableIsNotMultimap(_)
        | redb::TableError::TypeDefinitionChanged { .. } => incompatible_data_err(),
        other => backend_err(other),
    }
}

/// Rejects an incomplete or unknown table catalog without opening or creating
/// any data table.
///
/// Adding or removing a durable table requires a format-version change, so an
/// existing current-version file cannot silently grow a missing table.
fn validate_existing_table_catalog(txn: &redb::WriteTransaction) -> Result<()> {
    let expected = [
        META_TABLE.name(),
        CHUNK_TABLE.name(),
        ENTITY_TABLE.name(),
        PLAYER_TABLE.name(),
        PLUGIN_TABLE.name(),
        CHUNK_OVERLAY_TABLE.name(),
        MUTATION_LOG_TABLE.name(),
        MUTATION_BATCH_TABLE.name(),
    ];
    let mut found = 0;
    for table in txn.list_tables().map_err(backend_err)? {
        if !expected.contains(&table.name()) {
            return Err(incompatible_data_err());
        }
        found += 1;
    }

    let has_multimap_tables = txn
        .list_multimap_tables()
        .map_err(backend_err)?
        .next()
        .is_some();
    if has_multimap_tables || found != expected.len() {
        return Err(incompatible_data_err());
    }
    Ok(())
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

/// Encodes a journal receipt into its fixed-width persisted representation.
fn encode_journal_receipt(
    receipt: JournalAppendReceipt,
) -> std::result::Result<[u8; JOURNAL_RECEIPT_ENCODED_LEN], StorageError> {
    let count = u64::try_from(receipt.len()).map_err(|_| StorageError::BatchTooLarge {
        len: receipt.len(),
        max: MAX_SAVE_BATCH,
    })?;
    let first_id = receipt.first_id().unwrap_or(0);
    let mut encoded = [0; JOURNAL_RECEIPT_ENCODED_LEN];
    let (first_out, count_out) = encoded.split_at_mut(size_of::<u64>());
    first_out.copy_from_slice(&first_id.to_be_bytes());
    count_out.copy_from_slice(&count.to_be_bytes());
    Ok(encoded)
}

/// Decodes and validates one fixed-width persisted journal receipt.
fn decode_journal_receipt(
    batch_id: JournalBatchId,
    encoded: &[u8],
) -> std::result::Result<JournalAppendReceipt, StorageError> {
    if encoded.len() != JOURNAL_RECEIPT_ENCODED_LEN {
        return Err(StorageError::MalformedJournalReceipt {
            batch_id,
            len: encoded.len(),
            expected: JOURNAL_RECEIPT_ENCODED_LEN,
        });
    }
    let (first_bytes, count_bytes) = encoded.split_at(size_of::<u64>());
    let first_bytes: [u8; 8] =
        first_bytes
            .try_into()
            .map_err(|_| StorageError::MalformedJournalReceipt {
                batch_id,
                len: encoded.len(),
                expected: JOURNAL_RECEIPT_ENCODED_LEN,
            })?;
    let count_bytes: [u8; 8] =
        count_bytes
            .try_into()
            .map_err(|_| StorageError::MalformedJournalReceipt {
                batch_id,
                len: encoded.len(),
                expected: JOURNAL_RECEIPT_ENCODED_LEN,
            })?;
    let first_id = u64::from_be_bytes(first_bytes);
    let count = u64::from_be_bytes(count_bytes);
    let max_count = u64::try_from(MAX_SAVE_BATCH).map_err(|_| {
        StorageError::backend("MAX_SAVE_BATCH does not fit the persisted receipt count")
    })?;
    let len = usize::try_from(count).map_err(|_| StorageError::InvalidJournalReceiptRange {
        batch_id,
        first_id,
        count,
    })?;
    if count > max_count || (count == 0 && first_id != 0) {
        return Err(StorageError::InvalidJournalReceiptRange {
            batch_id,
            first_id,
            count,
        });
    }
    let range = if count == 0 {
        None
    } else {
        let last_id =
            first_id
                .checked_add(count - 1)
                .ok_or(StorageError::InvalidJournalReceiptRange {
                    batch_id,
                    first_id,
                    count,
                })?;
        Some((first_id, last_id))
    };
    Ok(JournalAppendReceipt::from_range(batch_id, range, len))
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
    /// [`ServerError::InvalidState`] (via
    /// [`StorageError::IncompatiblePreAlphaData`]) rather than risking a misread
    /// of an incompatible layout.
    ///
    /// This is synchronous I/O and must be called outside the async hot path
    /// (for example during startup), not from inside a running tick.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let (db, origin) = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => (
                Database::builder().create_file(file).map_err(backend_err)?,
                StoreOrigin::Fresh,
            ),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => (
                Database::open(path).map_err(database_open_err)?,
                StoreOrigin::Existing,
            ),
            Err(err) => return Err(backend_err(err)),
        };
        Self::initialize(&db, origin)?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Ensures every table exists and the format version is present and
    /// compatible, in a single write transaction.
    fn initialize(db: &Database, origin: StoreOrigin) -> Result<()> {
        let txn = db.begin_write().map_err(backend_err)?;
        {
            // The format marker is the gate for every table and record below.
            // Checking it first prevents corrupt or legacy data from being
            // interpreted before compatibility is established.
            let mut meta = txn.open_table(META_TABLE).map_err(schema_table_err)?;
            let existing = meta
                .get(META_FORMAT_KEY)
                .map_err(backend_err)?
                .map(|guard| guard.value());
            match (origin, existing) {
                (_, Some(STORE_FORMAT_VERSION)) => {}
                (StoreOrigin::Fresh, None) => {
                    meta.insert(META_FORMAT_KEY, STORE_FORMAT_VERSION)
                        .map_err(backend_err)?;
                }
                (_, Some(_)) | (StoreOrigin::Existing, None) => {
                    return Err(incompatible_data_err());
                }
            }
        }
        if matches!(origin, StoreOrigin::Existing) {
            validate_existing_table_catalog(&txn)?;
        }

        // Create the data tables up front so later read transactions never fail
        // with `TableDoesNotExist` on a brand-new database.
        txn.open_table(CHUNK_TABLE).map_err(schema_table_err)?;
        txn.open_table(ENTITY_TABLE).map_err(schema_table_err)?;
        txn.open_table(PLAYER_TABLE).map_err(schema_table_err)?;
        txn.open_table(PLUGIN_TABLE).map_err(schema_table_err)?;
        // Keep every current table under the same initialization transaction.
        // If validation of current-format data fails, dropping this transaction
        // abandons initialization without modifying existing data.
        txn.open_table(CHUNK_OVERLAY_TABLE)
            .map_err(schema_table_err)?;
        txn.open_table(MUTATION_BATCH_TABLE)
            .map_err(schema_table_err)?;
        let journal_last_id = {
            let journal = txn
                .open_table(MUTATION_LOG_TABLE)
                .map_err(schema_table_err)?;
            // Current append transactions create a gap-free sequence from zero
            // and update its marker atomically. Validate every key once at
            // startup so old or corrupt rows cannot hide behind a valid final
            // key; appends still use only the metadata hot path.
            let mut last_id: Option<u64> = None;
            for entry in journal.iter().map_err(backend_err)? {
                let (key, _value) = entry.map_err(backend_err)?;
                let id = mutation_id_from_key(key.value()).map_err(|_| incompatible_data_err())?;
                let expected = match last_id {
                    Some(previous) => previous.checked_add(1).ok_or_else(incompatible_data_err)?,
                    None => 0,
                };
                if id != expected {
                    return Err(incompatible_data_err());
                }
                last_id = Some(id);
            }
            last_id
        };
        {
            let meta = txn.open_table(META_TABLE).map_err(schema_table_err)?;
            let stored_last_id = meta
                .get(META_LAST_MUTATION_ID_KEY)
                .map_err(backend_err)?
                .map(|guard| guard.value());
            let is_current_sequence_state = match (stored_last_id, journal_last_id) {
                (None, None) => true,
                (Some(stored), Some(journal)) => stored == journal,
                _ => false,
            };
            if !is_current_sequence_state {
                return Err(incompatible_data_err());
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
                let value = codec::encode_chunk_record(&record)?;
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
                    let value = codec::encode_chunk_record(record)?;
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
        let db = Arc::clone(&self.db);
        let join = tokio::task::spawn_blocking(move || -> Result<JournalAppendReceipt> {
            // A write transaction serializes same-token races and makes the
            // receipt, sequence metadata, and journal records one durable fact.
            let txn = db.begin_write().map_err(backend_err)?;
            let existing_receipt = {
                let receipts = txn.open_table(MUTATION_BATCH_TABLE).map_err(backend_err)?;
                let persisted = receipts
                    .get(batch_id.as_bytes().as_slice())
                    .map_err(backend_err)?;
                persisted
                    .map(|guard| decode_journal_receipt(batch_id, guard.value()))
                    .transpose()?
            };

            if let Some(receipt) = existing_receipt {
                if receipt.len() != mutations.len() {
                    return Err(StorageError::JournalBatchConflict { batch_id }.into());
                }
                if let Some(ids) = receipt.id_range() {
                    let journal = txn.open_table(MUTATION_LOG_TABLE).map_err(backend_err)?;
                    for (record, id) in mutations.iter().zip(ids) {
                        let expected =
                            codec::encode_mutation_log_record(&record.with_storage_id(id));
                        let key_bytes = id.to_be_bytes();
                        let stored = journal
                            .get(key_bytes.as_slice())
                            .map_err(backend_err)?
                            .ok_or(StorageError::JournalReceiptMissingRecord { batch_id, id })?;
                        if stored.value() != expected.as_slice() {
                            return Err(StorageError::JournalBatchConflict { batch_id }.into());
                        }
                    }
                }
                return Ok(receipt);
            }

            let previous_last_id = {
                let meta = txn.open_table(META_TABLE).map_err(backend_err)?;
                let last_id = meta
                    .get(META_LAST_MUTATION_ID_KEY)
                    .map_err(backend_err)?
                    .map(|guard| guard.value());
                last_id
            };
            let range = journal_id_range(previous_last_id, mutations.len())?;
            let receipt = JournalAppendReceipt::from_range(batch_id, range, mutations.len());

            if let Some(ids) = receipt.id_range() {
                let mut journal = txn.open_table(MUTATION_LOG_TABLE).map_err(backend_err)?;
                for id in ids.clone() {
                    let key_bytes = id.to_be_bytes();
                    if journal
                        .get(key_bytes.as_slice())
                        .map_err(backend_err)?
                        .is_some()
                    {
                        return Err(StorageError::JournalIdCollision { id }.into());
                    }
                }
                for (record, id) in mutations.iter().zip(ids) {
                    let record = record.with_storage_id(id);
                    let key_bytes = id.to_be_bytes();
                    let value = codec::encode_mutation_log_record(&record);
                    let replaced = journal
                        .insert(key_bytes.as_slice(), value.as_slice())
                        .map_err(backend_err)?;
                    if replaced.is_some() {
                        return Err(StorageError::JournalIdCollision { id }.into());
                    }
                }
            }
            if let Some(final_id) = receipt.last_id() {
                let mut meta = txn.open_table(META_TABLE).map_err(backend_err)?;
                meta.insert(META_LAST_MUTATION_ID_KEY, final_id)
                    .map_err(backend_err)?;
            }
            {
                let mut receipts = txn.open_table(MUTATION_BATCH_TABLE).map_err(backend_err)?;
                let encoded_receipt = encode_journal_receipt(receipt)?;
                let replaced = receipts
                    .insert(batch_id.as_bytes().as_slice(), encoded_receipt.as_slice())
                    .map_err(backend_err)?;
                if replaced.is_some() {
                    return Err(StorageError::backend(format!(
                        "journal receipt {batch_id} appeared inside one write transaction"
                    ))
                    .into());
                }
            }
            txn.commit().map_err(backend_err)?;
            Ok(receipt)
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

    use ferrumc_core::{DimensionId, WorldId};
    use ferrumc_math::{BlockPos, ChunkPos};
    use ferrumc_world::{
        BlockEntity, BlockStateId, Chunk, Sign, SignKind, MAX_BLOCK_ENTITY_PAYLOAD_LEN,
    };
    use tempfile::TempDir;
    use tokio::sync::Barrier;

    use super::*;
    use crate::{JournalBatchId, MutationActor, MutationLogCause, SchemaVersion};

    const INCOMPATIBLE_PRE_ALPHA_MESSAGE: &str = "This data was created by an incompatible pre-alpha build. Back it up or delete it before starting this release.";
    const SHORT_META_TABLE: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("ferrumc:meta");
    const WRONG_CHUNK_TABLE: TableDefinition<'_, &str, u64> = TableDefinition::new("ferrumc:chunk");

    fn assert_incompatible_open(path: &Path) {
        let error = RedbStore::open(path).expect_err("incompatible store must be refused");
        match &error {
            ServerError::InvalidState(message) => {
                assert_eq!(message, INCOMPATIBLE_PRE_ALPHA_MESSAGE);
            }
            other => panic!("expected typed incompatible-data refusal, got {other:?}"),
        }
        assert_eq!(
            error.to_string(),
            format!("invalid state: {INCOMPATIBLE_PRE_ALPHA_MESSAGE}")
        );
    }

    fn seed_store_version(path: &Path, version: u64) {
        let store = RedbStore::open(path).expect("open current store");
        let txn = store.db.begin_write().expect("write transaction");
        {
            let mut meta = txn.open_table(META_TABLE).expect("metadata table");
            meta.insert(META_FORMAT_KEY, version)
                .expect("seed format version");
        }
        txn.commit().expect("commit format marker");
    }

    fn seed_fixture_data_tables(txn: &redb::WriteTransaction, include_chunk: bool) {
        if include_chunk {
            txn.open_table(CHUNK_TABLE).expect("chunk table");
        }
        txn.open_table(ENTITY_TABLE).expect("entity table");
        txn.open_table(PLAYER_TABLE).expect("player table");
        txn.open_table(PLUGIN_TABLE).expect("plugin table");
        txn.open_table(CHUNK_OVERLAY_TABLE)
            .expect("chunk overlay table");
        txn.open_table(MUTATION_LOG_TABLE)
            .expect("mutation log table");
        txn.open_table(MUTATION_BATCH_TABLE)
            .expect("mutation batch table");
    }

    fn assert_missing_headers_are_refused(dir: &Path) {
        let path = dir.join("missing-header.redb");
        {
            let db = Database::create(&path).expect("create missing-header fixture");
            let txn = db.begin_write().expect("write transaction");
            {
                let mut meta = txn.open_table(META_TABLE).expect("metadata table");
                meta.insert(META_LAST_MUTATION_ID_KEY, 42)
                    .expect("seed unrelated metadata");
            }
            seed_fixture_data_tables(&txn, true);
            txn.commit().expect("commit fixture without a header");
        }
        assert_incompatible_open(&path);

        let db = Database::open(&path).expect("reopen missing-header fixture");
        let txn = db.begin_read().expect("read transaction");
        let meta = txn.open_table(META_TABLE).expect("metadata table remains");
        assert!(meta
            .get(META_FORMAT_KEY)
            .expect("read format marker")
            .is_none());
        assert_eq!(
            meta.get(META_LAST_MUTATION_ID_KEY)
                .expect("read unrelated metadata")
                .map(|guard| guard.value()),
            Some(42)
        );
    }

    fn assert_incomplete_current_catalog_is_refused(dir: &Path) {
        let path = dir.join("missing-chunk-table.redb");
        {
            let db = Database::create(&path).expect("create incomplete fixture");
            let txn = db.begin_write().expect("write transaction");
            {
                let mut meta = txn.open_table(META_TABLE).expect("metadata table");
                meta.insert(META_FORMAT_KEY, STORE_FORMAT_VERSION)
                    .expect("seed current format version");
            }
            seed_fixture_data_tables(&txn, false);
            txn.commit().expect("commit incomplete fixture");
        }
        assert_incompatible_open(&path);

        let db = Database::open(&path).expect("reopen incomplete fixture");
        let txn = db.begin_read().expect("read transaction");
        assert!(matches!(
            txn.open_table(CHUNK_TABLE),
            Err(redb::TableError::TableDoesNotExist(_))
        ));
    }

    fn assert_short_headers_are_refused(dir: &Path) {
        let path = dir.join("short-metadata-header.redb");
        {
            let db = Database::create(&path).expect("create short-header fixture");
            let txn = db.begin_write().expect("write transaction");
            {
                let mut meta = txn
                    .open_table(SHORT_META_TABLE)
                    .expect("raw metadata table");
                meta.insert(META_FORMAT_KEY, &[2_u8][..])
                    .expect("seed short format header");
            }
            seed_fixture_data_tables(&txn, true);
            txn.commit().expect("commit short format header");
        }
        assert_incompatible_open(&path);

        for len in [0_usize, size_of::<u64>() - 1] {
            let path = dir.join(format!("physical-header-{len}.redb"));
            std::fs::write(&path, vec![0_u8; len]).expect("seed physical short header");
            assert_incompatible_open(&path);
            assert_eq!(
                std::fs::metadata(&path)
                    .expect("short-header metadata")
                    .len(),
                u64::try_from(len).expect("fixture length fits u64")
            );
        }
    }

    fn assert_wrong_table_shape_is_refused(dir: &Path) {
        let path = dir.join("wrong-table-shape.redb");
        {
            let db = Database::create(&path).expect("create wrong-table fixture");
            let txn = db.begin_write().expect("write transaction");
            {
                let mut meta = txn.open_table(META_TABLE).expect("metadata table");
                meta.insert(META_FORMAT_KEY, STORE_FORMAT_VERSION)
                    .expect("seed current format version");
            }
            {
                let mut chunks = txn
                    .open_table(WRONG_CHUNK_TABLE)
                    .expect("wrong-shape chunk table");
                chunks.insert("sentinel", 7).expect("seed sentinel row");
            }
            seed_fixture_data_tables(&txn, false);
            txn.commit().expect("commit wrong-table fixture");
        }
        assert_incompatible_open(&path);

        let db = Database::open(&path).expect("reopen wrong-table fixture");
        let txn = db.begin_read().expect("read transaction");
        let chunks = txn
            .open_table(WRONG_CHUNK_TABLE)
            .expect("wrong-shape chunk table remains");
        assert_eq!(
            chunks
                .get("sentinel")
                .expect("read sentinel")
                .map(|guard| guard.value()),
            Some(7)
        );
    }

    fn assert_marker_gate_precedes_data_validation(dir: &Path) {
        let path = dir.join("future-with-malformed-data.redb");
        {
            let store = RedbStore::open(&path).expect("create current store");
            let txn = store.db.begin_write().expect("write transaction");
            {
                let mut meta = txn.open_table(META_TABLE).expect("metadata table");
                meta.insert(META_FORMAT_KEY, STORE_FORMAT_VERSION + 1)
                    .expect("seed future format version");
            }
            {
                let mut journal = txn
                    .open_table(MUTATION_LOG_TABLE)
                    .expect("mutation journal");
                journal
                    .insert(&[0_u8; 9][..], &[][..])
                    .expect("seed malformed current-format data");
            }
            txn.commit().expect("commit incompatible fixture");
        }
        assert_incompatible_open(&path);
    }

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

    fn receipt_count(store: &RedbStore) -> usize {
        let txn = store.db.begin_read().expect("read transaction");
        let table = txn.open_table(MUTATION_BATCH_TABLE).expect("receipt table");
        table.iter().expect("receipt iterator").count()
    }

    fn receipt_bytes(first_id: u64, count: u64) -> [u8; JOURNAL_RECEIPT_ENCODED_LEN] {
        let mut encoded = [0; JOURNAL_RECEIPT_ENCODED_LEN];
        let (first_out, count_out) = encoded.split_at_mut(size_of::<u64>());
        first_out.copy_from_slice(&first_id.to_be_bytes());
        count_out.copy_from_slice(&count.to_be_bytes());
        encoded
    }

    fn seed_receipt(store: &RedbStore, batch_id: JournalBatchId, encoded: &[u8]) {
        let txn = store.db.begin_write().expect("write transaction");
        {
            let mut table = txn.open_table(MUTATION_BATCH_TABLE).expect("receipt table");
            table
                .insert(batch_id.as_bytes().as_slice(), encoded)
                .expect("seed receipt");
        }
        txn.commit().expect("commit receipt");
    }

    fn batch_id(byte: u8) -> JournalBatchId {
        JournalBatchId::from_bytes([byte; 16])
    }

    fn chunk_key(pos: ChunkPos) -> ChunkKey {
        ChunkKey::new(WorldId::new(0), DimensionId::new(0), pos)
    }

    #[tokio::test]
    async fn redb_chunk_load_rejects_trailing_record_corruption() {
        let dir = TempDir::new().expect("temp dir");
        let store = RedbStore::open(dir.path().join("chunk.redb")).expect("open store");
        let key = chunk_key(ChunkPos::ORIGIN);
        store
            .save_chunk(
                key,
                ChunkRecord::new(SchemaVersion::new(7), Chunk::new(ChunkPos::ORIGIN)),
            )
            .await
            .expect("save chunk");

        let txn = store.db.begin_write().expect("write transaction");
        {
            let mut table = txn.open_table(CHUNK_TABLE).expect("chunk table");
            let key_bytes = codec::chunk_key_bytes(key);
            let mut corrupted = table
                .get(key_bytes.as_slice())
                .expect("read chunk")
                .expect("stored chunk")
                .value()
                .to_vec();
            corrupted.extend_from_slice(&[0xAA, 0xBB]);
            table
                .insert(key_bytes.as_slice(), corrupted.as_slice())
                .expect("replace with corrupt bytes");
        }
        txn.commit().expect("commit corruption");

        let error = store
            .load_chunk(key)
            .await
            .expect_err("trailing bytes must reject the whole value");
        assert!(matches!(error, ServerError::Internal { .. }));
        assert!(error
            .to_string()
            .contains("trailing bytes after full chunk record"));
    }

    #[tokio::test]
    async fn failed_chunk_batch_encoding_rolls_back_prior_replacement() {
        let dir = TempDir::new().expect("temp dir");
        let store = RedbStore::open(dir.path().join("chunk.redb")).expect("open store");
        let existing_key = chunk_key(ChunkPos::ORIGIN);
        store
            .save_chunk(
                existing_key,
                ChunkRecord::new(SchemaVersion::new(7), Chunk::new(ChunkPos::ORIGIN)),
            )
            .await
            .expect("seed existing chunk");

        let replacement = ChunkRecord::new(SchemaVersion::new(8), Chunk::new(ChunkPos::ORIGIN));
        let invalid_pos = ChunkPos::new(1, 0);
        let mut invalid_chunk = Chunk::new(invalid_pos);
        let mut oversized = Sign::new(SignKind::Sign);
        oversized.set_face_lines(
            true,
            std::array::from_fn(|_| "x".repeat(MAX_BLOCK_ENTITY_PAYLOAD_LEN)),
        );
        invalid_chunk
            .set_block_entity(invalid_pos.origin_block(0), BlockEntity::Sign(oversized))
            .expect("sign is in invalid fixture chunk");

        let error = store
            .save_chunks(vec![
                (existing_key, replacement),
                (
                    chunk_key(invalid_pos),
                    ChunkRecord::new(SchemaVersion::new(8), invalid_chunk),
                ),
            ])
            .await
            .expect_err("an unencodable member must reject the batch");
        assert!(matches!(error, ServerError::Capacity(_)));

        let existing = store
            .load_chunk(existing_key)
            .await
            .expect("load existing chunk")
            .expect("existing chunk remains");
        assert_eq!(existing.schema_version(), SchemaVersion::new(7));
        assert_eq!(
            store
                .load_chunk(chunk_key(invalid_pos))
                .await
                .expect("load absent invalid chunk"),
            None
        );
    }

    #[test]
    fn current_store_format_is_v2() {
        assert_eq!(STORE_FORMAT_VERSION, 2);
    }

    #[test]
    fn incompatible_schema_version_is_refused_with_exact_message() {
        let dir = TempDir::new().expect("temp dir");
        for (name, version) in [
            ("bogus.redb", 0),
            ("pre-packet-31.redb", 1),
            ("future.redb", STORE_FORMAT_VERSION + 1),
        ] {
            let path = dir.path().join(name);
            seed_store_version(&path, version);
            assert_incompatible_open(&path);
        }

        let current_path = dir.path().join("current.redb");
        drop(RedbStore::open(&current_path).expect("create current store"));
        drop(RedbStore::open(&current_path).expect("current store must reopen"));

        assert_missing_headers_are_refused(dir.path());
        assert_incomplete_current_catalog_is_refused(dir.path());
        assert_short_headers_are_refused(dir.path());
        assert_wrong_table_shape_is_refused(dir.path());
        assert_marker_gate_precedes_data_validation(dir.path());
    }

    #[tokio::test]
    async fn journal_receipt_replay_returns_original_range_without_append() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("journal.redb");
        let id = batch_id(7);
        let batch = vec![mutation(90, 10), mutation(91, 11)];

        let (original_receipt, original_snapshot) = {
            let store = RedbStore::open(&path).expect("open store");
            let receipt = store
                .append_block_mutation_batch(id, batch.clone())
                .await
                .expect("first append");
            (receipt, journal_snapshot(&store))
        };

        let reopened = RedbStore::open(&path).expect("reopen store");
        let replayed_receipt = reopened
            .append_block_mutation_batch(id, batch)
            .await
            .expect("idempotent replay");
        assert_eq!(replayed_receipt, original_receipt);
        assert_eq!(replayed_receipt.first_id(), Some(0));
        assert_eq!(replayed_receipt.last_id(), Some(1));
        assert_eq!(replayed_receipt.len(), 2);
        assert_eq!(journal_snapshot(&reopened), original_snapshot);
        assert_eq!(stored_last_id(&reopened), Some(1));
    }

    #[tokio::test]
    async fn journal_receipt_payload_mismatch_is_typed_error() {
        let dir = TempDir::new().expect("temp dir");
        let store = RedbStore::open(dir.path().join("journal.redb")).expect("open store");
        let id = batch_id(8);
        let original_receipt = store
            .append_block_mutation_batch(id, vec![mutation(0, 10)])
            .await
            .expect("first append");
        let before = journal_snapshot(&store);

        let error = store
            .append_block_mutation_batch(id, vec![mutation(0, 11)])
            .await
            .expect_err("same token with a different payload must fail");
        assert!(matches!(error, ServerError::InvalidState(_)));
        assert_eq!(journal_snapshot(&store), before);
        assert_eq!(stored_last_id(&store), Some(0));

        assert_eq!(
            store
                .append_block_mutation_batch(id, vec![mutation(99, 10)])
                .await
                .expect("the original normalized payload remains replayable"),
            original_receipt
        );
        let next = store
            .append_block_mutation_batch(batch_id(10), vec![mutation(0, 12)])
            .await
            .expect("a fresh token still receives the next id");
        assert_eq!(next.first_id(), Some(1));
        assert_eq!(next.last_id(), Some(1));
        assert_eq!(journal_snapshot(&store).len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_same_journal_token_appends_exactly_once() {
        let dir = TempDir::new().expect("temp dir");
        let store = Arc::new(RedbStore::open(dir.path().join("journal.redb")).expect("open store"));
        let barrier = Arc::new(Barrier::new(2));
        let id = batch_id(9);
        let batch = vec![mutation(0, 20), mutation(1, 21), mutation(2, 22)];
        let mut handles = Vec::with_capacity(2);

        for _ in 0..2 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let batch = batch.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                store.append_block_mutation_batch(id, batch).await
            }));
        }

        let first = handles
            .remove(0)
            .await
            .expect("first task did not panic")
            .expect("first append succeeds");
        let second = handles
            .remove(0)
            .await
            .expect("second task did not panic")
            .expect("second append succeeds");
        assert_eq!(first, second);
        assert_eq!(first.first_id(), Some(0));
        assert_eq!(first.last_id(), Some(2));
        assert_eq!(journal_snapshot(&store).len(), batch.len());
        assert_eq!(stored_last_id(&store), Some(2));
        assert_eq!(receipt_count(&store), 1);
    }

    #[tokio::test]
    async fn idempotent_journal_overflow_does_not_consume_token_or_sequence() {
        let dir = TempDir::new().expect("temp dir");
        let store = RedbStore::open(dir.path().join("journal.redb")).expect("open store");
        seed_journal(&store, Some(u64::MAX - 1), &[mutation(u64::MAX - 1, 1)]);
        let id = batch_id(11);
        let before = journal_snapshot(&store);

        let error = store
            .append_block_mutation_batch(id, vec![mutation(0, 2), mutation(1, 3)])
            .await
            .expect_err("two records cannot fit");
        assert!(matches!(error, ServerError::Capacity(_)));
        assert_eq!(journal_snapshot(&store), before);
        assert_eq!(stored_last_id(&store), Some(u64::MAX - 1));
        assert_eq!(receipt_count(&store), 0);

        let at_max = store
            .append_block_mutation_batch(id, vec![mutation(0, 2)])
            .await
            .expect("the failed token can be retried with a fitting batch");
        assert_eq!(at_max.first_id(), Some(u64::MAX));
        assert_eq!(at_max.last_id(), Some(u64::MAX));
        assert_eq!(
            store
                .append_block_mutation_batch(id, vec![mutation(9, 2)])
                .await
                .expect("max-id receipt replays after exhaustion"),
            at_max
        );
        assert_eq!(journal_snapshot(&store).len(), 2);
        assert_eq!(receipt_count(&store), 1);

        let error = store
            .append_block_mutation_batch(batch_id(12), vec![mutation(0, 4)])
            .await
            .expect_err("a fresh token is exhausted after u64::MAX");
        assert!(matches!(error, ServerError::Capacity(_)));
        assert_eq!(journal_snapshot(&store).len(), 2);
        assert_eq!(receipt_count(&store), 1);
    }

    #[test]
    fn journal_receipt_codec_rejects_malformed_values() {
        let id = batch_id(13);
        for malformed in [&[][..], &[0; 15], &[0; 17]] {
            let error = decode_journal_receipt(id, malformed).expect_err("malformed length");
            assert!(matches!(
                error,
                StorageError::MalformedJournalReceipt { len, .. } if len == malformed.len()
            ));
        }

        let invalid_empty = receipt_bytes(1, 0);
        assert!(matches!(
            decode_journal_receipt(id, &invalid_empty),
            Err(StorageError::InvalidJournalReceiptRange { .. })
        ));
        let over_limit = receipt_bytes(
            0,
            u64::try_from(MAX_SAVE_BATCH + 1).expect("save bound fits u64"),
        );
        assert!(matches!(
            decode_journal_receipt(id, &over_limit),
            Err(StorageError::InvalidJournalReceiptRange { .. })
        ));
        let overflowing = receipt_bytes(u64::MAX, 2);
        assert!(matches!(
            decode_journal_receipt(id, &overflowing),
            Err(StorageError::InvalidJournalReceiptRange { .. })
        ));
    }

    #[tokio::test]
    async fn journal_receipt_missing_record_fails_without_appending() {
        let dir = TempDir::new().expect("temp dir");
        let store = RedbStore::open(dir.path().join("journal.redb")).expect("open store");
        let id = batch_id(14);
        seed_receipt(&store, id, &receipt_bytes(0, 1));

        let error = store
            .append_block_mutation_batch(id, vec![mutation(0, 1)])
            .await
            .expect_err("receipt cannot acknowledge a missing row");
        assert!(matches!(error, ServerError::Internal { .. }));
        assert!(journal_snapshot(&store).is_empty());
        assert_eq!(stored_last_id(&store), None);
        assert_eq!(receipt_count(&store), 1);
    }

    #[tokio::test]
    async fn empty_journal_receipt_is_durable_without_advancing_sequence() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("journal.redb");
        let id = batch_id(15);
        let original = {
            let store = RedbStore::open(&path).expect("open store");
            let receipt = store
                .append_block_mutation_batch(id, Vec::new())
                .await
                .expect("empty receipt");
            assert!(receipt.is_empty());
            assert!(journal_snapshot(&store).is_empty());
            assert_eq!(stored_last_id(&store), None);
            assert_eq!(receipt_count(&store), 1);
            receipt
        };

        let reopened = RedbStore::open(&path).expect("reopen store");
        assert_eq!(
            reopened
                .append_block_mutation_batch(id, Vec::new())
                .await
                .expect("empty replay"),
            original
        );
        let conflict = reopened
            .append_block_mutation_batch(id, vec![mutation(0, 1)])
            .await
            .expect_err("empty token cannot be reused for data");
        assert!(matches!(conflict, ServerError::InvalidState(_)));
        let first_nonempty = reopened
            .append_block_mutation_batch(batch_id(16), vec![mutation(0, 1)])
            .await
            .expect("empty receipt did not consume a sequence");
        assert_eq!(first_nonempty.first_id(), Some(0));
    }

    #[tokio::test]
    async fn oversized_idempotent_batch_is_rejected_before_receipt() {
        let dir = TempDir::new().expect("temp dir");
        let store = RedbStore::open(dir.path().join("journal.redb")).expect("open store");
        let oversized = (0..=MAX_SAVE_BATCH)
            .map(|index| {
                let id = u64::try_from(index).expect("test id fits u64");
                mutation(id, id)
            })
            .collect();
        let error = store
            .append_block_mutation_batch(batch_id(17), oversized)
            .await
            .expect_err("oversized batch must fail");
        assert!(matches!(error, ServerError::Capacity(_)));
        assert!(journal_snapshot(&store).is_empty());
        assert_eq!(stored_last_id(&store), None);
        assert_eq!(receipt_count(&store), 0);
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
    async fn legacy_journal_without_sequence_is_refused_without_repair() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("journal.redb");
        let legacy_snapshot = {
            let store = RedbStore::open(&path).expect("open store");
            seed_journal(&store, None, &[mutation(0, 4), mutation(1, 9)]);
            journal_snapshot(&store)
        };

        assert_incompatible_open(&path);

        let db = Database::open(&path).expect("inspect refused legacy store");
        let txn = db.begin_read().expect("read refused legacy store");
        let meta = txn.open_table(META_TABLE).expect("metadata table");
        assert!(meta
            .get(META_LAST_MUTATION_ID_KEY)
            .expect("read sequence")
            .is_none());
        let journal = txn.open_table(MUTATION_LOG_TABLE).expect("journal table");
        let snapshot: Vec<_> = journal
            .iter()
            .expect("journal iterator")
            .map(|entry| {
                let (key, value) = entry.expect("journal entry");
                let key: [u8; 8] = key.value().try_into().expect("u64 journal key");
                (u64::from_be_bytes(key), value.value().to_vec())
            })
            .collect();
        assert_eq!(snapshot, legacy_snapshot);
    }

    #[tokio::test]
    async fn stale_sequence_metadata_is_refused_without_repair() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("journal.redb");
        let prior_snapshot = {
            let store = RedbStore::open(&path).expect("open store");
            seed_journal(&store, Some(0), &[mutation(0, 2), mutation(1, 9)]);
            journal_snapshot(&store)
        };

        assert_incompatible_open(&path);

        let db = Database::open(&path).expect("inspect refused stale-sequence store");
        let txn = db.begin_read().expect("read refused stale-sequence store");
        let meta = txn.open_table(META_TABLE).expect("metadata table");
        assert_eq!(
            meta.get(META_LAST_MUTATION_ID_KEY)
                .expect("read sequence")
                .map(|guard| guard.value()),
            Some(0)
        );
        let journal = txn.open_table(MUTATION_LOG_TABLE).expect("journal table");
        let snapshot: Vec<_> = journal
            .iter()
            .expect("journal iterator")
            .map(|entry| {
                let (key, value) = entry.expect("journal entry");
                let key: [u8; 8] = key.value().try_into().expect("u64 journal key");
                (u64::from_be_bytes(key), value.value().to_vec())
            })
            .collect();
        assert_eq!(snapshot, prior_snapshot);
    }

    #[test]
    fn impossible_journal_sequence_states_are_refused() {
        let dir = TempDir::new().expect("temp dir");

        let orphaned_sequence_path = dir.path().join("orphaned-sequence.redb");
        {
            let store =
                RedbStore::open(&orphaned_sequence_path).expect("create orphaned-sequence store");
            seed_journal(&store, Some(3), &[]);
        }
        assert_incompatible_open(&orphaned_sequence_path);
        {
            let db = Database::open(&orphaned_sequence_path)
                .expect("inspect refused orphaned-sequence store");
            let txn = db.begin_read().expect("read orphaned-sequence store");
            let meta = txn.open_table(META_TABLE).expect("metadata table");
            assert_eq!(
                meta.get(META_LAST_MUTATION_ID_KEY)
                    .expect("read sequence")
                    .map(|guard| guard.value()),
                Some(3)
            );
            let journal = txn.open_table(MUTATION_LOG_TABLE).expect("journal table");
            assert_eq!(journal.iter().expect("journal iterator").count(), 0);
        }

        let ahead_sequence_path = dir.path().join("ahead-sequence.redb");
        let ahead_snapshot = {
            let store = RedbStore::open(&ahead_sequence_path).expect("create ahead-sequence store");
            seed_journal(&store, Some(10), &[mutation(0, 2), mutation(1, 9)]);
            journal_snapshot(&store)
        };
        assert_incompatible_open(&ahead_sequence_path);
        {
            let db =
                Database::open(&ahead_sequence_path).expect("inspect refused ahead-sequence store");
            let txn = db.begin_read().expect("read ahead-sequence store");
            let meta = txn.open_table(META_TABLE).expect("metadata table");
            assert_eq!(
                meta.get(META_LAST_MUTATION_ID_KEY)
                    .expect("read sequence")
                    .map(|guard| guard.value()),
                Some(10)
            );
            let journal = txn.open_table(MUTATION_LOG_TABLE).expect("journal table");
            let snapshot: Vec<_> = journal
                .iter()
                .expect("journal iterator")
                .map(|entry| {
                    let (key, value) = entry.expect("journal entry");
                    let key: [u8; 8] = key.value().try_into().expect("u64 journal key");
                    (u64::from_be_bytes(key), value.value().to_vec())
                })
                .collect();
            assert_eq!(snapshot, ahead_snapshot);
        }
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

        assert_incompatible_open(&path);

        let db = Database::open(&path).expect("inspect refused malformed-key store");
        let txn = db.begin_read().expect("read refused malformed-key store");
        let journal = txn.open_table(MUTATION_LOG_TABLE).expect("journal table");
        assert!(journal
            .get(&[0xff; 9][..])
            .expect("read malformed journal key")
            .is_some());
    }

    #[test]
    fn opening_store_rejects_a_malformed_interior_journal_key() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("journal.redb");
        {
            let store = RedbStore::open(&path).expect("open store");
            seed_journal(&store, Some(1), &[mutation(0, 0), mutation(1, 1)]);
            let txn = store.db.begin_write().expect("write transaction");
            {
                let mut table = txn.open_table(MUTATION_LOG_TABLE).expect("journal table");
                table
                    .insert(&[0_u8; 9][..], &[0][..])
                    .expect("seed malformed interior key");
            }
            txn.commit().expect("commit malformed interior key");
        }

        assert_incompatible_open(&path);

        let db = Database::open(&path).expect("inspect refused malformed-key store");
        let txn = db.begin_read().expect("read refused malformed-key store");
        let journal = txn.open_table(MUTATION_LOG_TABLE).expect("journal table");
        assert!(journal
            .get(&[0_u8; 9][..])
            .expect("read malformed journal key")
            .is_some());
        assert_eq!(journal.iter().expect("journal iterator").count(), 3);
    }

    #[test]
    fn opening_store_rejects_a_gapped_journal_sequence() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("journal.redb");
        let prior_snapshot = {
            let store = RedbStore::open(&path).expect("open store");
            seed_journal(&store, Some(2), &[mutation(0, 0), mutation(2, 2)]);
            journal_snapshot(&store)
        };

        assert_incompatible_open(&path);

        let db = Database::open(&path).expect("inspect refused gapped store");
        let txn = db.begin_read().expect("read refused gapped store");
        let journal = txn.open_table(MUTATION_LOG_TABLE).expect("journal table");
        let snapshot: Vec<_> = journal
            .iter()
            .expect("journal iterator")
            .map(|entry| {
                let (key, value) = entry.expect("journal entry");
                let key: [u8; 8] = key.value().try_into().expect("u64 journal key");
                (u64::from_be_bytes(key), value.value().to_vec())
            })
            .collect();
        assert_eq!(snapshot, prior_snapshot);
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
