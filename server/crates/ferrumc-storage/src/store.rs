//! The async storage traits.
//!
//! # Async strategy
//!
//! The traits use [`async_trait`] rather than native async-fn-in-trait. This
//! boxes each returned future as `Box<dyn Future + Send>`, which keeps the
//! traits dyn-compatible (the simulation layer holds `Arc<dyn WorldStore>`) and
//! guarantees the futures are `Send` so they can be driven on any runtime
//! worker. Native AFIT cannot express the `Send` bound on a trait method and
//! trips the `async_fn_in_trait` lint under `-D warnings`.
//!
//! # Bounded shapes and backpressure
//!
//! The M16 backend ([`crate::InMemoryStore`]) services every call inline, so no
//! request ever queues. The async signatures exist so a later worker-thread
//! backend can move work onto a dedicated thread behind a *bounded* channel
//! (per the storage model) without changing any caller. Batched save methods
//! reject requests larger than [`MAX_SAVE_BATCH`] up front with
//! [`crate::StorageError::BatchTooLarge`] so an oversized batch can never force
//! an unbounded allocation; plugin values are bounded by
//! [`MAX_PLUGIN_VALUE_LEN`].

use async_trait::async_trait;
use ferrumc_core::{PlayerId, PluginId, Result};

use crate::key::{ChunkKey, EntityKey, StorageKey};
use crate::record::{
    BlockMutationLogRecord, ChunkOverlayRecord, ChunkRecord, EntityRecord, PlayerRecord,
};

/// Maximum number of items accepted in a single batched save call.
///
/// Bounds the work and allocation a single request can demand. A batch larger
/// than this is rejected with [`crate::StorageError::BatchTooLarge`] rather than
/// processed.
pub const MAX_SAVE_BATCH: usize = 4096;

/// Maximum accepted length, in bytes, of a plugin storage value.
///
/// Plugin values are untrusted input and bounded before being stored. 1 MiB is
/// generous for key-value state while capping a single entry.
pub const MAX_PLUGIN_VALUE_LEN: usize = 1024 * 1024;

/// Persists and retrieves world data: chunk columns and entities.
///
/// Reads of a missing key return `Ok(None)` (or `Ok(Vec::new())`), never an
/// error; an `Err` means the operation itself failed.
#[async_trait]
pub trait WorldStore: Send + Sync {
    /// Loads the chunk at `key`, or `Ok(None)` if no chunk is stored there.
    async fn load_chunk(&self, key: ChunkKey) -> Result<Option<ChunkRecord>>;

    /// Saves `record` at `key`, overwriting any existing chunk there.
    async fn save_chunk(&self, key: ChunkKey, record: ChunkRecord) -> Result<()>;

    /// Saves a batch of chunks, overwriting any existing chunk at each key.
    ///
    /// Rejects a batch larger than [`MAX_SAVE_BATCH`] with
    /// [`crate::StorageError::BatchTooLarge`] before storing anything.
    async fn save_chunks(&self, chunks: Vec<(ChunkKey, ChunkRecord)>) -> Result<()>;

    /// Removes the chunk at `key`. Returns `Ok(true)` if a chunk was present,
    /// `Ok(false)` if there was nothing to remove.
    async fn delete_chunk(&self, key: ChunkKey) -> Result<bool>;

    /// Loads the overlay at `key`, or `Ok(None)` if no overlay is stored there.
    ///
    /// An overlay holds only the player-modified sections of a chunk; the caller
    /// reconstructs the full chunk by regenerating the flat baseline and applying
    /// the overlay over it (see [`ChunkOverlayRecord::apply_to_chunk`]).
    async fn load_chunk_overlay(&self, key: ChunkKey) -> Result<Option<ChunkOverlayRecord>>;

    /// Saves a batch of chunk overlays, overwriting any existing overlay at each
    /// key. An untouched generated chunk has no persist-dirty sections and so is
    /// never present in such a batch, keeping its storage footprint zero.
    ///
    /// Rejects a batch larger than [`MAX_SAVE_BATCH`] with
    /// [`crate::StorageError::BatchTooLarge`] before storing anything.
    async fn save_chunk_overlays(
        &self,
        overlays: Vec<(ChunkKey, ChunkOverlayRecord)>,
    ) -> Result<()>;

    /// Appends `mutations` to the block-mutation journal in order.
    ///
    /// The journal is append-only: storage replaces each record's provisional
    /// ID with a durable, strictly increasing sequence ID in the same atomic
    /// commit that writes the batch. Existing entries are never overwritten.
    /// Rejects a batch larger than [`MAX_SAVE_BATCH`] with
    /// [`crate::StorageError::BatchTooLarge`] and sequence exhaustion with
    /// [`crate::StorageError::JournalSequenceExhausted`] before storing anything.
    async fn append_block_mutations(&self, mutations: Vec<BlockMutationLogRecord>) -> Result<()>;

    /// Loads the entity at `key`, or `Ok(None)` if no entity is stored there.
    async fn load_entity(&self, key: EntityKey) -> Result<Option<EntityRecord>>;

    /// Saves `record` at `key`, overwriting any existing entity there.
    async fn save_entity(&self, key: EntityKey, record: EntityRecord) -> Result<()>;

    /// Saves a batch of entities, overwriting any existing entity at each key.
    ///
    /// Rejects a batch larger than [`MAX_SAVE_BATCH`] with
    /// [`crate::StorageError::BatchTooLarge`] before storing anything.
    async fn save_entities(&self, entities: Vec<(EntityKey, EntityRecord)>) -> Result<()>;

    /// Removes the entity at `key`. Returns `Ok(true)` if an entity was present,
    /// `Ok(false)` otherwise.
    async fn delete_entity(&self, key: EntityKey) -> Result<bool>;
}

/// Computes the inclusive journal ID range for one append without mutating state.
pub(crate) fn journal_id_range(
    last_id: Option<u64>,
    requested: usize,
) -> std::result::Result<Option<(u64, u64)>, crate::StorageError> {
    if requested == 0 {
        return Ok(None);
    }

    let first_id = match last_id {
        Some(id) => id
            .checked_add(1)
            .ok_or(crate::StorageError::JournalSequenceExhausted {
                last_id: id,
                requested,
            })?,
        None => 0,
    };
    let additional = u64::try_from(requested - 1).map_err(|_| {
        crate::StorageError::JournalSequenceExhausted {
            last_id: last_id.unwrap_or(0),
            requested,
        }
    })?;
    let final_id =
        first_id
            .checked_add(additional)
            .ok_or(crate::StorageError::JournalSequenceExhausted {
                last_id: last_id.unwrap_or(0),
                requested,
            })?;
    Ok(Some((first_id, final_id)))
}

/// Persists and retrieves per-player records.
///
/// Players are keyed by their globally unique [`PlayerId`]. Loading an unknown
/// player returns `Ok(None)`, never an error.
#[async_trait]
pub trait PlayerStore: Send + Sync {
    /// Loads the record for `id`, or `Ok(None)` if the player is unknown.
    async fn load_player(&self, id: PlayerId) -> Result<Option<PlayerRecord>>;

    /// Saves `record` for `id`, overwriting any existing record.
    async fn save_player(&self, id: PlayerId, record: PlayerRecord) -> Result<()>;

    /// Removes the record for `id`. Returns `Ok(true)` if a record was present,
    /// `Ok(false)` otherwise.
    async fn delete_player(&self, id: PlayerId) -> Result<bool>;
}

/// Per-plugin private key-value storage.
///
/// Every operation is scoped to a [`PluginId`]: a plugin can only read, write,
/// or enumerate keys within its own namespace, and never observes another
/// plugin's data. Loading an unknown key returns `Ok(None)`, never an error.
#[async_trait]
pub trait PluginStore: Send + Sync {
    /// Returns the value stored for `key` in `plugin`'s namespace, or `Ok(None)`
    /// if the key is unset.
    async fn get(&self, plugin: &PluginId, key: &StorageKey) -> Result<Option<Vec<u8>>>;

    /// Stores `value` under `key` in `plugin`'s namespace, overwriting any
    /// existing value.
    ///
    /// Rejects a value longer than [`MAX_PLUGIN_VALUE_LEN`] with
    /// [`crate::StorageError::ValueTooLarge`] before storing anything.
    async fn put(&self, plugin: &PluginId, key: StorageKey, value: Vec<u8>) -> Result<()>;

    /// Removes `key` from `plugin`'s namespace. Returns `Ok(true)` if a value
    /// was present, `Ok(false)` otherwise.
    async fn delete(&self, plugin: &PluginId, key: &StorageKey) -> Result<bool>;

    /// Returns every key currently set in `plugin`'s namespace, in unspecified
    /// order. Other plugins' keys are never included.
    async fn keys(&self, plugin: &PluginId) -> Result<Vec<StorageKey>>;
}
