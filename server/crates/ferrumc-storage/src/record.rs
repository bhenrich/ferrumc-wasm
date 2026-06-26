//! Versioned record types that the store persists and returns.
//!
//! Each record carries a [`SchemaVersion`] so a future backend can detect and
//! migrate data written by an older build. A [`ChunkRecord`] holds a structured
//! [`Chunk`] because the world model lives in a dependency of this crate;
//! entities and players have no shared model yet, so their records carry an
//! opaque, length-bounded serialized payload owned by the simulation layer.

use ferrumc_core::GameMode;
use ferrumc_world::Chunk;

use crate::error::StorageError;
use crate::schema::SchemaVersion;

/// Maximum accepted length, in bytes, of an [`EntityRecord`] payload.
///
/// Entity snapshots are produced by the simulation layer and bounded here so a
/// single record cannot consume unbounded memory. 256 KiB comfortably holds an
/// entity's serialized component state.
pub const MAX_ENTITY_DATA_LEN: usize = 256 * 1024;

/// Maximum accepted length, in bytes, of a [`PlayerRecord`] payload.
///
/// Player data (inventory, position, statistics, ...) is larger than a typical
/// entity but still bounded. 1 MiB is generous while capping a single record.
pub const MAX_PLAYER_DATA_LEN: usize = 1024 * 1024;

/// A versioned, persistable snapshot of one chunk column.
///
/// Wraps a fully structured [`Chunk`] together with the [`SchemaVersion`] under
/// which it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRecord {
    schema_version: SchemaVersion,
    chunk: Chunk,
}

impl ChunkRecord {
    /// Builds a chunk record stamped with `schema_version`.
    ///
    /// A chunk is a fixed-shape value, so no length bound applies and this never
    /// fails.
    pub fn new(schema_version: SchemaVersion, chunk: Chunk) -> Self {
        Self {
            schema_version,
            chunk,
        }
    }

    /// Returns the schema version this record was written under.
    pub fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the stored chunk.
    pub fn chunk(&self) -> &Chunk {
        &self.chunk
    }

    /// Consumes the record and returns the owned chunk.
    pub fn into_chunk(self) -> Chunk {
        self.chunk
    }
}

/// A versioned, persistable snapshot of one entity.
///
/// The payload is the simulation layer's serialized entity state, opaque to
/// storage and bounded by [`MAX_ENTITY_DATA_LEN`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRecord {
    schema_version: SchemaVersion,
    data: Vec<u8>,
}

impl EntityRecord {
    /// Builds an entity record stamped with `schema_version`, rejecting a
    /// payload longer than [`MAX_ENTITY_DATA_LEN`] with
    /// [`StorageError::RecordTooLarge`].
    pub fn new(schema_version: SchemaVersion, data: Vec<u8>) -> Result<Self, StorageError> {
        if data.len() > MAX_ENTITY_DATA_LEN {
            return Err(StorageError::RecordTooLarge {
                len: data.len(),
                max: MAX_ENTITY_DATA_LEN,
            });
        }
        Ok(Self {
            schema_version,
            data,
        })
    }

    /// Returns the schema version this record was written under.
    pub fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the serialized entity payload.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Consumes the record and returns the owned payload.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

/// A versioned, persistable snapshot of one player.
///
/// Carries the player's [`GameMode`] as a typed field and the remaining player
/// state as an opaque payload bounded by [`MAX_PLAYER_DATA_LEN`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerRecord {
    schema_version: SchemaVersion,
    game_mode: GameMode,
    data: Vec<u8>,
}

impl PlayerRecord {
    /// Builds a player record stamped with `schema_version`, rejecting a payload
    /// longer than [`MAX_PLAYER_DATA_LEN`] with [`StorageError::RecordTooLarge`].
    pub fn new(
        schema_version: SchemaVersion,
        game_mode: GameMode,
        data: Vec<u8>,
    ) -> Result<Self, StorageError> {
        if data.len() > MAX_PLAYER_DATA_LEN {
            return Err(StorageError::RecordTooLarge {
                len: data.len(),
                max: MAX_PLAYER_DATA_LEN,
            });
        }
        Ok(Self {
            schema_version,
            game_mode,
            data,
        })
    }

    /// Returns the schema version this record was written under.
    pub fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the player's game mode.
    pub fn game_mode(&self) -> GameMode {
        self.game_mode
    }

    /// Returns the serialized player payload.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Consumes the record and returns the owned payload.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_math::{BlockPos, ChunkPos};
    use ferrumc_world::{BlockStateId, Chunk};

    #[test]
    fn chunk_record_preserves_version_and_chunk() {
        let mut chunk = Chunk::new(ChunkPos::ORIGIN);
        chunk
            .set_block(BlockPos::new(1, 2, 3), BlockStateId::new(1))
            .expect("in range");
        let record = ChunkRecord::new(SchemaVersion::new(3), chunk.clone());
        assert_eq!(record.schema_version(), SchemaVersion::new(3));
        assert_eq!(record.chunk(), &chunk);
        assert_eq!(record.into_chunk(), chunk);
    }

    #[test]
    fn entity_record_bounds_payload() {
        let ok = EntityRecord::new(SchemaVersion::new(1), vec![1, 2, 3]).expect("within bound");
        assert_eq!(ok.data(), &[1, 2, 3]);
        assert_eq!(ok.schema_version(), SchemaVersion::new(1));

        let err = EntityRecord::new(SchemaVersion::new(1), vec![0; MAX_ENTITY_DATA_LEN + 1])
            .expect_err("over bound");
        assert!(matches!(err, StorageError::RecordTooLarge { .. }));
    }

    #[test]
    fn player_record_bounds_payload_and_keeps_game_mode() {
        let ok = PlayerRecord::new(SchemaVersion::new(2), GameMode::Creative, vec![9])
            .expect("within bound");
        assert_eq!(ok.game_mode(), GameMode::Creative);
        assert_eq!(ok.into_data(), vec![9]);

        let err = PlayerRecord::new(
            SchemaVersion::new(2),
            GameMode::Survival,
            vec![0; MAX_PLAYER_DATA_LEN + 1],
        )
        .expect_err("over bound");
        assert!(matches!(err, StorageError::RecordTooLarge { .. }));
    }
}
