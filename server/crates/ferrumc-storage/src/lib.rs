#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod codec;
mod error;
mod key;
mod memory;
mod record;
mod redb_store;
mod schema;
mod store;

pub use error::StorageError;
pub use key::{ChunkKey, EntityKey, StorageKey, MAX_PLUGIN_KEY_LEN};
pub use memory::InMemoryStore;
pub use record::{
    BlockMutationLogRecord, ChunkOverlayRecord, ChunkRecord, EntityRecord, MutationActor,
    MutationLogCause, PlayerRecord, MAX_ENTITY_DATA_LEN, MAX_OVERLAY_SECTIONS, MAX_PLAYER_DATA_LEN,
};
pub use redb_store::RedbStore;
pub use schema::SchemaVersion;
pub use store::{PlayerStore, PluginStore, WorldStore, MAX_PLUGIN_VALUE_LEN, MAX_SAVE_BATCH};
