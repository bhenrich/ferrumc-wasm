//! Typed keys that address records in a store.
//!
//! World data is addressed by `(world, dimension, position)` rather than a raw
//! coordinate tuple, satisfying the "coordinates are typed" coding standard.
//! Players are keyed directly by their [`ferrumc_core::PlayerId`], which is
//! already a globally unique identity, so no wrapper key exists for them.

use ferrumc_core::{DimensionId, EntityId, WorldId};
use ferrumc_math::ChunkPos;

use crate::error::StorageError;

/// Maximum accepted length, in bytes, of a [`StorageKey`].
///
/// Plugin storage keys are untrusted input, so they are bounded before being
/// stored. 512 bytes is generous for any reasonable namespaced key while still
/// capping memory spent on a single key.
pub const MAX_PLUGIN_KEY_LEN: usize = 512;

/// Addresses a single chunk column within a specific world and dimension.
///
/// The position is a typed [`ChunkPos`]; storage never accepts a raw
/// `(i32, i32)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkKey {
    world: WorldId,
    dimension: DimensionId,
    pos: ChunkPos,
}

impl ChunkKey {
    /// Builds a chunk key for `pos` in the given `world` and `dimension`.
    pub const fn new(world: WorldId, dimension: DimensionId, pos: ChunkPos) -> Self {
        Self {
            world,
            dimension,
            pos,
        }
    }

    /// Returns the world this chunk belongs to.
    pub const fn world(self) -> WorldId {
        self.world
    }

    /// Returns the dimension this chunk belongs to.
    pub const fn dimension(self) -> DimensionId {
        self.dimension
    }

    /// Returns the chunk column position.
    pub const fn pos(self) -> ChunkPos {
        self.pos
    }
}

/// Addresses a single entity within a specific world and dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntityKey {
    world: WorldId,
    dimension: DimensionId,
    entity: EntityId,
}

impl EntityKey {
    /// Builds an entity key for `entity` in the given `world` and `dimension`.
    pub const fn new(world: WorldId, dimension: DimensionId, entity: EntityId) -> Self {
        Self {
            world,
            dimension,
            entity,
        }
    }

    /// Returns the world this entity belongs to.
    pub const fn world(self) -> WorldId {
        self.world
    }

    /// Returns the dimension this entity belongs to.
    pub const fn dimension(self) -> DimensionId {
        self.dimension
    }

    /// Returns the entity id.
    pub const fn entity(self) -> EntityId {
        self.entity
    }
}

/// A validated key into a plugin's private key-value namespace.
///
/// Keys come from plugin code and are therefore treated as untrusted: the byte
/// length is bounded by [`MAX_PLUGIN_KEY_LEN`] at construction so a single key
/// cannot consume unbounded memory. The contents are otherwise opaque to
/// storage; isolation between plugins is enforced by the store, not the key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StorageKey(String);

impl StorageKey {
    /// Builds a storage key, rejecting any input longer than
    /// [`MAX_PLUGIN_KEY_LEN`] bytes with [`StorageError::KeyTooLong`].
    pub fn new(key: impl Into<String>) -> Result<Self, StorageError> {
        let key = key.into();
        if key.len() > MAX_PLUGIN_KEY_LEN {
            return Err(StorageError::KeyTooLong {
                len: key.len(),
                max: MAX_PLUGIN_KEY_LEN,
            });
        }
        Ok(Self(key))
    }

    /// Returns the key as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for StorageKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_math::ChunkPos;

    #[test]
    fn chunk_key_exposes_components() {
        let key = ChunkKey::new(WorldId::new(1), DimensionId::new(2), ChunkPos::new(-3, 4));
        assert_eq!(key.world(), WorldId::new(1));
        assert_eq!(key.dimension(), DimensionId::new(2));
        assert_eq!(key.pos(), ChunkPos::new(-3, 4));
    }

    #[test]
    fn entity_key_exposes_components() {
        let key = EntityKey::new(WorldId::new(0), DimensionId::new(0), EntityId::new(42));
        assert_eq!(key.entity(), EntityId::new(42));
    }

    #[test]
    fn storage_key_round_trips_within_bound() {
        let key = StorageKey::new("spawn-protect:region").expect("within bound");
        assert_eq!(key.as_str(), "spawn-protect:region");
        assert_eq!(key.to_string(), "spawn-protect:region");
        // The maximum-length key is accepted.
        let max = "k".repeat(MAX_PLUGIN_KEY_LEN);
        assert!(StorageKey::new(max).is_ok());
    }

    #[test]
    fn storage_key_rejects_oversized_input() {
        let too_long = "k".repeat(MAX_PLUGIN_KEY_LEN + 1);
        let err = StorageKey::new(too_long).expect_err("over bound");
        assert_eq!(
            err,
            StorageError::KeyTooLong {
                len: MAX_PLUGIN_KEY_LEN + 1,
                max: MAX_PLUGIN_KEY_LEN,
            }
        );
    }
}
