//! Private byte encoding for redb keys and record values.
//!
//! redb stores raw `&[u8]` keys and values, so this module is the single place
//! that turns typed keys ([`ChunkKey`], [`EntityKey`], [`PlayerId`], and a
//! plugin's namespaced key) and versioned records ([`ChunkRecord`],
//! [`EntityRecord`], [`PlayerRecord`]) into bytes and back. Nothing here is
//! public: the encoding is an implementation detail of [`crate::RedbStore`] and
//! must never leak across the crate boundary.
//!
//! # Layout (all integers big-endian)
//!
//! - chunk key: `world(u32) ++ dimension(u32) ++ chunk_x(i32) ++ chunk_z(i32)`
//! - entity key: `world(u32) ++ dimension(u32) ++ entity(i32)`
//! - player key: the 16 raw bytes of the player UUID
//! - plugin key: `plugin_len(u32) ++ plugin_id_bytes ++ key_bytes`. The length
//!   prefix makes the `(plugin, key)` split unambiguous, so two plugins can
//!   never collide and a prefix scan over `plugin_len ++ plugin_id_bytes`
//!   enumerates exactly one plugin's keys.
//! - chunk value: `schema(u32) ++ x(i32) ++ z(i32) ++ section_count(u8)` then,
//!   per section, a tag byte (`0` = all air, `1` = a dense list of
//!   [`ferrumc_world::SECTION_VOLUME`] block-state ids as `u32`).
//! - entity value: `schema(u32) ++ opaque_payload`
//! - player value: `schema(u32) ++ game_mode(u8) ++ opaque_payload`
//!
//! Reads validate every length and reject malformed bytes with
//! [`StorageError::Backend`] rather than panicking, since corrupt persisted
//! bytes are an internal-invariant failure, not untrusted client input.

use ferrumc_core::{GameMode, PlayerId, PluginId};
use ferrumc_math::{BlockPos, ChunkPos, LocalBlockPos};
use ferrumc_world::{BlockStateId, Chunk, SECTION_COUNT, SECTION_VOLUME};

use crate::error::StorageError;
use crate::key::{ChunkKey, EntityKey, StorageKey};
use crate::record::{ChunkRecord, EntityRecord, PlayerRecord};
use crate::schema::SchemaVersion;

/// World floor `y`, mirroring `ferrumc_world`'s overworld geometry
/// (`dimension::MIN_Y == -64`).
///
/// The world crate does not export this, yet rebuilding a chunk from bytes must
/// map a section index to an absolute `y` because the only public block mutator,
/// [`Chunk::set_block`], takes an absolute [`BlockPos`]. The
/// `chunk_full_height_round_trips` integration test exercises the bottom and top
/// sections, so a drift in this constant would fail loudly rather than silently
/// corrupt data.
const WORLD_FLOOR_Y: i32 = -64;

/// Section tag: every block in the section is [`BlockStateId::AIR`].
const SECTION_TAG_AIR: u8 = 0;

/// Section tag: a dense list of [`SECTION_VOLUME`] block-state ids follows.
const SECTION_TAG_DENSE: u8 = 1;

/// Builds the [`StorageError::Backend`] used for malformed persisted bytes.
fn corrupt(detail: impl Into<String>) -> StorageError {
    StorageError::backend(detail.into())
}

/// A panic-free cursor over persisted bytes.
///
/// Every read is length-checked; an underrun returns [`StorageError::Backend`]
/// rather than panicking, so truncated or malformed records degrade to a
/// classified error.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Wraps `data` at offset zero.
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Returns the next `n` bytes, advancing the cursor, or an error if fewer
    /// than `n` bytes remain.
    fn take(&mut self, n: usize) -> Result<&'a [u8], StorageError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| corrupt("record length overflow"))?;
        let slice = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| corrupt("record truncated"))?;
        self.pos = end;
        Ok(slice)
    }

    /// Reads one byte.
    fn read_u8(&mut self) -> Result<u8, StorageError> {
        Ok(self.take(1)?[0])
    }

    /// Reads a big-endian `u32`.
    fn read_u32(&mut self) -> Result<u32, StorageError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| corrupt("expected 4 bytes for u32"))?;
        Ok(u32::from_be_bytes(bytes))
    }

    /// Reads a big-endian `i32`.
    fn read_i32(&mut self) -> Result<i32, StorageError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| corrupt("expected 4 bytes for i32"))?;
        Ok(i32::from_be_bytes(bytes))
    }

    /// Returns the unread remainder, consuming the rest of the cursor.
    fn rest(&mut self) -> &'a [u8] {
        let tail = &self.data[self.pos..];
        self.pos = self.data.len();
        tail
    }
}

/// Encodes a [`ChunkKey`] as 16 big-endian bytes.
pub(crate) fn chunk_key_bytes(key: ChunkKey) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&key.world().get().to_be_bytes());
    out[4..8].copy_from_slice(&key.dimension().get().to_be_bytes());
    out[8..12].copy_from_slice(&key.pos().x().to_be_bytes());
    out[12..16].copy_from_slice(&key.pos().z().to_be_bytes());
    out
}

/// Encodes an [`EntityKey`] as 12 big-endian bytes.
pub(crate) fn entity_key_bytes(key: EntityKey) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[0..4].copy_from_slice(&key.world().get().to_be_bytes());
    out[4..8].copy_from_slice(&key.dimension().get().to_be_bytes());
    out[8..12].copy_from_slice(&key.entity().get().to_be_bytes());
    out
}

/// Encodes a [`PlayerId`] as its 16 raw UUID bytes.
pub(crate) fn player_key_bytes(id: PlayerId) -> [u8; 16] {
    *id.as_uuid().as_bytes()
}

/// Builds the namespace prefix (`plugin_len ++ plugin_id_bytes`) shared by every
/// key a plugin owns. A forward range scan from this prefix enumerates exactly
/// that plugin's keys.
pub(crate) fn plugin_prefix(plugin: &PluginId) -> Vec<u8> {
    let id = plugin.as_str().as_bytes();
    let mut out = Vec::with_capacity(4 + id.len());
    // `as u32`: a plugin id long enough to overflow `u32` is not a real input;
    // the cast is documented and the prefix stays self-describing regardless.
    out.extend_from_slice(&(id.len() as u32).to_be_bytes());
    out.extend_from_slice(id);
    out
}

/// Builds the full namespaced key bytes for `(plugin, key)`.
pub(crate) fn plugin_key_bytes(plugin: &PluginId, key: &StorageKey) -> Vec<u8> {
    let mut out = plugin_prefix(plugin);
    out.extend_from_slice(key.as_str().as_bytes());
    out
}

/// Recovers the [`StorageKey`] from a stored plugin key that begins with
/// `prefix`, or an error if the key is malformed.
pub(crate) fn plugin_key_from_bytes(
    prefix: &[u8],
    full: &[u8],
) -> Result<StorageKey, StorageError> {
    let suffix = full
        .get(prefix.len()..)
        .ok_or_else(|| corrupt("plugin key shorter than its namespace prefix"))?;
    let text =
        core::str::from_utf8(suffix).map_err(|_| corrupt("plugin key is not valid UTF-8"))?;
    StorageKey::new(text)
}

/// Maps a flat section index in `0..SECTION_VOLUME` to its [`LocalBlockPos`].
///
/// Mirrors [`LocalBlockPos::index`]'s `YZX` ordering. The axes are masked to
/// `0..16`, so the position is always valid; the `air` fallback is unreachable
/// and exists only to keep the function panic-free.
fn local_pos(index: usize) -> LocalBlockPos {
    let x = (index & 0xF) as u8;
    let z = ((index >> 4) & 0xF) as u8;
    let y = ((index >> 8) & 0xF) as u8;
    LocalBlockPos::new(x, y, z).unwrap_or(LocalBlockPos::from_block(BlockPos::new(0, 0, 0)))
}

/// Encodes a [`ChunkRecord`] to bytes.
pub(crate) fn encode_chunk_record(record: &ChunkRecord) -> Vec<u8> {
    let chunk = record.chunk();
    let sections = chunk.sections();

    let mut out = Vec::new();
    out.extend_from_slice(&record.schema_version().get().to_be_bytes());
    out.extend_from_slice(&chunk.pos().x().to_be_bytes());
    out.extend_from_slice(&chunk.pos().z().to_be_bytes());
    // `as u8`: `SECTION_COUNT` is 24, well within `u8`.
    out.push(sections.len() as u8);

    for section in sections {
        if section.is_empty() {
            out.push(SECTION_TAG_AIR);
        } else {
            out.push(SECTION_TAG_DENSE);
            for index in 0..SECTION_VOLUME {
                let state = section.get(local_pos(index)).as_u32();
                out.extend_from_slice(&state.to_be_bytes());
            }
        }
    }
    out
}

/// Decodes a [`ChunkRecord`] from bytes, rejecting malformed input with
/// [`StorageError::Backend`].
pub(crate) fn decode_chunk_record(bytes: &[u8]) -> Result<ChunkRecord, StorageError> {
    let mut reader = Reader::new(bytes);
    let schema = SchemaVersion::new(reader.read_u32()?);
    let x = reader.read_i32()?;
    let z = reader.read_i32()?;
    let section_count = usize::from(reader.read_u8()?);
    if section_count != SECTION_COUNT {
        return Err(corrupt(format!(
            "chunk section count {section_count} (expected {SECTION_COUNT})"
        )));
    }

    let pos = ChunkPos::new(x, z);
    let mut chunk = Chunk::new(pos);

    for section_index in 0..section_count {
        match reader.read_u8()? {
            SECTION_TAG_AIR => {}
            SECTION_TAG_DENSE => {
                // `section_index < SECTION_COUNT (24)`, so this never fails; the
                // checked conversion keeps the cast lint-clean and panic-free.
                let section_i32 =
                    i32::try_from(section_index).map_err(|_| corrupt("section index overflow"))?;
                let base_y = WORLD_FLOOR_Y + section_i32 * 16;
                let origin = pos.origin_block(base_y);
                for index in 0..SECTION_VOLUME {
                    let raw = reader.read_u32()?;
                    if raw == BlockStateId::AIR.as_u32() {
                        continue;
                    }
                    let local = local_pos(index);
                    let block = BlockPos::new(
                        origin.x() + i32::from(local.x()),
                        base_y + i32::from(local.y()),
                        origin.z() + i32::from(local.z()),
                    );
                    chunk
                        .set_block(block, BlockStateId::new(raw))
                        .map_err(|e| corrupt(format!("chunk block out of range: {e}")))?;
                }
            }
            other => return Err(corrupt(format!("unknown chunk section tag {other}"))),
        }
    }

    // A chunk freshly read from disk has no writes pending a flush.
    chunk.clear_dirty();
    Ok(ChunkRecord::new(schema, chunk))
}

/// Encodes an [`EntityRecord`] to bytes.
pub(crate) fn encode_entity_record(record: &EntityRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + record.data().len());
    out.extend_from_slice(&record.schema_version().get().to_be_bytes());
    out.extend_from_slice(record.data());
    out
}

/// Decodes an [`EntityRecord`] from bytes.
pub(crate) fn decode_entity_record(bytes: &[u8]) -> Result<EntityRecord, StorageError> {
    let mut reader = Reader::new(bytes);
    let schema = SchemaVersion::new(reader.read_u32()?);
    let data = reader.rest().to_vec();
    EntityRecord::new(schema, data)
}

/// Encodes a [`PlayerRecord`] to bytes.
pub(crate) fn encode_player_record(record: &PlayerRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + record.data().len());
    out.extend_from_slice(&record.schema_version().get().to_be_bytes());
    out.push(record.game_mode().as_id());
    out.extend_from_slice(record.data());
    out
}

/// Decodes a [`PlayerRecord`] from bytes.
pub(crate) fn decode_player_record(bytes: &[u8]) -> Result<PlayerRecord, StorageError> {
    let mut reader = Reader::new(bytes);
    let schema = SchemaVersion::new(reader.read_u32()?);
    let mode_id = reader.read_u8()?;
    let game_mode = GameMode::from_id(mode_id)
        .ok_or_else(|| corrupt(format!("invalid game mode {mode_id}")))?;
    let data = reader.rest().to_vec();
    PlayerRecord::new(schema, game_mode, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_core::{DimensionId, EntityId, WorldId};

    #[test]
    fn chunk_key_bytes_are_stable_and_ordered_by_field() {
        let key = ChunkKey::new(WorldId::new(1), DimensionId::new(2), ChunkPos::new(-1, 3));
        let bytes = chunk_key_bytes(key);
        assert_eq!(&bytes[0..4], &1u32.to_be_bytes());
        assert_eq!(&bytes[4..8], &2u32.to_be_bytes());
        assert_eq!(&bytes[8..12], &(-1i32).to_be_bytes());
        assert_eq!(&bytes[12..16], &3i32.to_be_bytes());
    }

    #[test]
    fn entity_key_bytes_round_trip_fields() {
        let key = EntityKey::new(WorldId::new(7), DimensionId::new(0), EntityId::new(-9));
        let bytes = entity_key_bytes(key);
        assert_eq!(&bytes[0..4], &7u32.to_be_bytes());
        assert_eq!(&bytes[8..12], &(-9i32).to_be_bytes());
    }

    #[test]
    fn plugin_keys_with_shared_string_do_not_collide() {
        // "ab" + "x" must differ from "a" + "bx" thanks to the length prefix.
        let a = plugin_key_bytes(&PluginId::new("ab"), &StorageKey::new("x").expect("key"));
        let b = plugin_key_bytes(&PluginId::new("a"), &StorageKey::new("bx").expect("key"));
        assert_ne!(a, b);
    }

    #[test]
    fn plugin_key_round_trips_through_prefix() {
        let plugin = PluginId::new("spawn-protect");
        let key = StorageKey::new("region:0:0").expect("key");
        let full = plugin_key_bytes(&plugin, &key);
        let prefix = plugin_prefix(&plugin);
        assert!(full.starts_with(&prefix));
        let recovered = plugin_key_from_bytes(&prefix, &full).expect("recover");
        assert_eq!(recovered, key);
    }

    #[test]
    fn chunk_record_round_trips_blocks_and_schema() {
        let mut chunk = Chunk::new(ChunkPos::new(2, -5));
        // Bottom (y = -64), an interior block, and the top (y = 319).
        for (block, raw) in [
            (BlockPos::new(33, -64, -77), 1u32),
            (BlockPos::new(40, 5, -70), 42),
            (BlockPos::new(47, 319, -65), 7),
        ] {
            chunk
                .set_block(block, BlockStateId::new(raw))
                .expect("in range");
        }
        let record = ChunkRecord::new(SchemaVersion::new(9), chunk.clone());

        let decoded = decode_chunk_record(&encode_chunk_record(&record)).expect("decode");
        assert_eq!(decoded.schema_version(), SchemaVersion::new(9));
        assert_eq!(decoded.chunk().pos(), ChunkPos::new(2, -5));
        for (block, raw) in [
            (BlockPos::new(33, -64, -77), 1u32),
            (BlockPos::new(40, 5, -70), 42),
            (BlockPos::new(47, 319, -65), 7),
        ] {
            assert_eq!(
                decoded.chunk().get_block(block),
                Some(BlockStateId::new(raw))
            );
        }
        // A loaded chunk is clean: nothing is pending a flush.
        assert!(!decoded.chunk().dirty_sections().any());
    }

    #[test]
    fn empty_chunk_round_trips() {
        let record = ChunkRecord::new(SchemaVersion::new(1), Chunk::new(ChunkPos::ORIGIN));
        let decoded = decode_chunk_record(&encode_chunk_record(&record)).expect("decode");
        assert_eq!(decoded.chunk(), &Chunk::new(ChunkPos::ORIGIN));
    }

    #[test]
    fn entity_and_player_records_round_trip() {
        let entity = EntityRecord::new(SchemaVersion::new(3), vec![1, 2, 3, 4]).expect("entity");
        let decoded = decode_entity_record(&encode_entity_record(&entity)).expect("decode");
        assert_eq!(decoded.schema_version(), SchemaVersion::new(3));
        assert_eq!(decoded.data(), &[1, 2, 3, 4]);

        let player =
            PlayerRecord::new(SchemaVersion::new(5), GameMode::Spectator, vec![9]).expect("player");
        let decoded = decode_player_record(&encode_player_record(&player)).expect("decode");
        assert_eq!(decoded.schema_version(), SchemaVersion::new(5));
        assert_eq!(decoded.game_mode(), GameMode::Spectator);
        assert_eq!(decoded.data(), &[9]);
    }

    #[test]
    fn decoders_reject_truncated_bytes() {
        assert!(decode_chunk_record(&[0, 0, 0]).is_err());
        assert!(decode_entity_record(&[0, 0]).is_err());
        assert!(decode_player_record(&[0, 0, 0, 0]).is_err());
    }

    #[test]
    fn player_decoder_rejects_invalid_game_mode() {
        // schema(4) + game_mode(1) where the mode id is out of range.
        let bytes = [0u8, 0, 0, 1, 200];
        assert!(decode_player_record(&bytes).is_err());
    }

    #[test]
    fn chunk_decoder_rejects_wrong_section_count() {
        // schema + x + z + a section count that is not SECTION_COUNT.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&0i32.to_be_bytes());
        bytes.extend_from_slice(&0i32.to_be_bytes());
        bytes.push(1);
        assert!(decode_chunk_record(&bytes).is_err());
    }
}
