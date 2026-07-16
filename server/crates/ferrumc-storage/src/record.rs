//! Versioned record types that the store persists and returns.
//!
//! Each record carries a [`SchemaVersion`] so a future backend can detect and
//! migrate data written by an older build. A [`ChunkRecord`] holds a structured
//! [`Chunk`] because the world model lives in a dependency of this crate;
//! entities and players have no shared model yet, so their records carry an
//! opaque, length-bounded serialized payload owned by the simulation layer.

use ferrumc_core::{GameMode, PlayerId};
use ferrumc_math::{BlockPos, ChunkPos, LocalBlockPos};
use ferrumc_world::{
    decode_block_entity, encode_block_entity, BlockStateId, Chunk, MAX_BLOCK_ENTITIES,
    MAX_BLOCK_ENTITY_PAYLOAD_LEN, SECTION_COUNT, SECTION_VOLUME,
};

use crate::error::StorageError;
use crate::schema::SchemaVersion;

/// World floor `y` (the overworld's `dimension::MIN_Y`, `-64`).
///
/// Mirrors [`crate::codec`]'s constant of the same name: rebuilding a chunk from
/// an overlay must map a section index to an absolute `y` because [`Chunk`]'s
/// only public mutator, [`Chunk::set_block`], takes an absolute [`BlockPos`].
/// Drift is guarded by the overlay round-trip tests.
const OVERLAY_WORLD_FLOOR_Y: i32 = -64;

/// Returns the world `y` of the bottom block of section `index`, or `None` if the
/// index is out of range (`>= SECTION_COUNT`).
fn overlay_section_base_y(index: usize) -> Option<i32> {
    if index >= SECTION_COUNT {
        return None;
    }
    // `index < 24`, so `* 16 < 384` fits an `i32` without overflow.
    let offset = i32::try_from(index.checked_mul(16)?).ok()?;
    Some(OVERLAY_WORLD_FLOOR_Y + offset)
}

/// Maps a flat section index in `0..SECTION_VOLUME` to its [`LocalBlockPos`].
///
/// Mirrors [`LocalBlockPos::index`]'s `YZX` ordering. The axes are masked to
/// `0..16`, so the position is always valid; the fallback is unreachable and
/// exists only to keep the mapping panic-free.
fn overlay_local_pos(index: usize) -> LocalBlockPos {
    let x = (index & 0xF) as u8;
    let z = ((index >> 4) & 0xF) as u8;
    let y = ((index >> 8) & 0xF) as u8;
    LocalBlockPos::new(x, y, z).unwrap_or(LocalBlockPos::from_block(BlockPos::new(0, 0, 0)))
}

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
///
/// `PartialEq` but not `Eq`: a [`Chunk`] may hold a chest block-entity whose
/// [`ItemStack`](ferrumc_items::ItemStack) NBT component data is `PartialEq`-only.
#[derive(Debug, Clone, PartialEq)]
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

/// The maximum number of sections one [`ChunkOverlayRecord`] can carry: one per
/// chunk section ([`SECTION_COUNT`], 24). A decoded record claiming more is
/// rejected as malformed.
pub const MAX_OVERLAY_SECTIONS: usize = SECTION_COUNT;

/// The maximum number of block entities one [`ChunkOverlayRecord`] can carry.
///
/// Mirrors a [`Chunk`]'s own [`MAX_BLOCK_ENTITIES`] cap so a record can hold a
/// fully-populated chunk's worth and no more; a decoded record (or a file)
/// declaring more is rejected before any allocation, so a corrupt or hostile
/// record cannot drive an unbounded reservation.
pub const MAX_OVERLAY_BLOCK_ENTITIES: usize = MAX_BLOCK_ENTITIES;

/// First overlay [`SchemaVersion`] whose on-disk encoding carries a block-entity
/// section.
///
/// Overlays written under an earlier version (v2, block states only) carry no
/// block-entity bytes; the codec keys its block-entity (de)serialization on this
/// threshold so an old record loads with an empty block-entity set instead of
/// being misread. The simulation stamps current overlays at or above this version.
pub const OVERLAY_SCHEMA_WITH_BLOCK_ENTITIES: u32 = 3;

/// One modified chunk section inside a [`ChunkOverlayRecord`].
///
/// Holds the section's full dense block-state list (`SECTION_VOLUME` entries in
/// `YZX` order) so applying it overwrites the generated baseline for that section
/// exactly — including air, so a broken block does not resurrect from the
/// baseline. Internal to the crate; never crosses the crate boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OverlaySection {
    /// Section index, 0-based from the bottom of the world (`< SECTION_COUNT`).
    index: u8,
    /// Exactly [`SECTION_VOLUME`] block-state ids in `YZX` order.
    blocks: Vec<BlockStateId>,
}

impl OverlaySection {
    /// Builds a section, rejecting a block list that is not exactly
    /// [`SECTION_VOLUME`] entries with [`StorageError::Backend`].
    pub(crate) fn new(index: u8, blocks: Vec<BlockStateId>) -> Result<Self, StorageError> {
        if blocks.len() != SECTION_VOLUME {
            return Err(StorageError::backend(format!(
                "overlay section block count {} (expected {SECTION_VOLUME})",
                blocks.len()
            )));
        }
        Ok(Self { index, blocks })
    }

    /// Returns the section index.
    pub(crate) fn index(&self) -> u8 {
        self.index
    }

    /// Returns the section's dense block-state list (`SECTION_VOLUME` entries).
    pub(crate) fn blocks(&self) -> &[BlockStateId] {
        &self.blocks
    }
}

/// A versioned, persistable snapshot of **only** the sections of one chunk that a
/// gameplay mutation changed.
///
/// Unlike [`ChunkRecord`] (a whole chunk column), an overlay carries just the
/// player-modified sections; the clean sections are reconstructed on load by
/// regenerating the flat baseline and then [applying](Self::apply_to_chunk) the
/// overlay over it. A freshly generated, never-edited chunk has an empty
/// persist-dirty set (see [`Chunk::persist_dirty_sections`]) and therefore yields
/// **no** overlay record at all, so untouched terrain occupies zero storage.
///
/// The record is self-describing for round-tripping: it carries its
/// [`SchemaVersion`], the chunk [`ChunkPos`], a `dirty_section_mask` bitmask of
/// which sections it holds, the server tick it was captured at, and the chunk's
/// block entities. The owning `(world, dimension)` lives in the [`crate::ChunkKey`]
/// it is stored under, not in the value.
///
/// # Block entities
///
/// Unlike block states (carried only for the persist-dirty sections, with the
/// clean sections reconstructed from the regenerated baseline), block entities
/// have **no** baseline source — the flat generator produces none — so an overlay
/// carries the chunk's **entire** block-entity set whenever it is emitted. Each is
/// stored as its [`BlockPos`] plus an opaque, length-bounded payload produced by
/// `ferrumc-world` (storage stays ignorant of sign/chest internals); the payload
/// is decoded back into a block entity by [`apply_to_chunk`](Self::apply_to_chunk),
/// which skips an individually corrupt one rather than failing the chunk load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkOverlayRecord {
    schema_version: SchemaVersion,
    pos: ChunkPos,
    dirty_section_mask: u32,
    sections: Vec<OverlaySection>,
    updated_at_tick: u64,
    /// The chunk's block entities as `(position, opaque payload)`, bounded by
    /// [`MAX_OVERLAY_BLOCK_ENTITIES`] entries and [`MAX_BLOCK_ENTITY_PAYLOAD_LEN`]
    /// bytes each. Empty for a record written under a pre-v3 schema.
    block_entities: Vec<(BlockPos, Vec<u8>)>,
}

impl ChunkOverlayRecord {
    /// Captures the persist-*edited* sections of `chunk` into an overlay record
    /// stamped with `schema_version` and `updated_at_tick`.
    ///
    /// The cumulative [`Chunk::persist_edited_sections`] set is captured — **every**
    /// section a gameplay edit has touched since the chunk's baseline, not just those
    /// dirtied since the last flush — each as its full dense block list. Capturing the
    /// complete set on every flush is what keeps the store's last-write-wins overlay
    /// overwrite a complete snapshot, so edits to different sections on different flush
    /// ticks cannot overwrite one another on reload. A chunk with no edited sections
    /// produces an empty record (caller should skip persisting it); callers gate the
    /// *flush* on [`Chunk::persist_dirty_sections`]`.any()`, but the *capture* reads
    /// the cumulative [`Chunk::persist_edited_sections`] (a superset of that gate).
    ///
    /// The chunk's **entire** block-entity set is captured (not just those in the
    /// dirty sections) whenever `schema_version` is at least
    /// [`OVERLAY_SCHEMA_WITH_BLOCK_ENTITIES`], because block entities have no
    /// baseline to reconstruct from. Each is serialized with `ferrumc-world` into a
    /// bounded payload; the capture is bounded to [`MAX_OVERLAY_BLOCK_ENTITIES`]
    /// entries (a chunk already caps its own block-entity count there).
    #[must_use]
    pub fn from_chunk(
        schema_version: SchemaVersion,
        pos: ChunkPos,
        chunk: &Chunk,
        updated_at_tick: u64,
    ) -> Self {
        let mut sections = Vec::new();
        let mut mask: u32 = 0;
        for index in chunk.persist_edited_sections().dirty_indices() {
            let Some(section) = chunk.section(index) else {
                continue;
            };
            let mut blocks = Vec::with_capacity(SECTION_VOLUME);
            for flat in 0..SECTION_VOLUME {
                blocks.push(section.get(overlay_local_pos(flat)));
            }
            // `index < SECTION_COUNT == 24 < 32`, so the shift and the `u8` cast
            // are both lossless.
            mask |= 1u32 << index;
            sections.push(OverlaySection {
                index: index as u8,
                blocks,
            });
        }

        let mut block_entities = Vec::new();
        if schema_version.get() >= OVERLAY_SCHEMA_WITH_BLOCK_ENTITIES {
            for (be_pos, entity) in chunk.block_entities() {
                if block_entities.len() >= MAX_OVERLAY_BLOCK_ENTITIES {
                    break;
                }
                let mut payload = Vec::new();
                encode_block_entity(entity, &mut payload);
                // A real block entity encodes well under the cap; defensively skip a
                // pathological oversized payload rather than persisting a blob the
                // decoder would reject anyway.
                if payload.len() > MAX_BLOCK_ENTITY_PAYLOAD_LEN {
                    continue;
                }
                block_entities.push((be_pos, payload));
            }
        }

        Self {
            schema_version,
            pos,
            dirty_section_mask: mask,
            sections,
            updated_at_tick,
            block_entities,
        }
    }

    /// Reassembles a record from decoded parts, validating internal consistency.
    ///
    /// Rejects (with [`StorageError::Backend`]) a mask with bits set beyond
    /// [`SECTION_COUNT`], a section count that disagrees with the mask, a section
    /// whose index is not the matching set bit, more than
    /// [`MAX_OVERLAY_BLOCK_ENTITIES`] block entities, or a block-entity payload
    /// longer than [`MAX_BLOCK_ENTITY_PAYLOAD_LEN`] — so a corrupt persisted record
    /// can never reconstruct an inconsistent or unbounded overlay.
    pub(crate) fn from_parts(
        schema_version: SchemaVersion,
        pos: ChunkPos,
        dirty_section_mask: u32,
        sections: Vec<OverlaySection>,
        updated_at_tick: u64,
        block_entities: Vec<(BlockPos, Vec<u8>)>,
    ) -> Result<Self, StorageError> {
        // No section beyond the world's section count may be claimed.
        if dirty_section_mask >> SECTION_COUNT != 0 {
            return Err(StorageError::backend(format!(
                "overlay mask {dirty_section_mask:#x} sets a bit beyond {SECTION_COUNT} sections"
            )));
        }
        if usize::try_from(dirty_section_mask.count_ones()).unwrap_or(usize::MAX) != sections.len()
        {
            return Err(StorageError::backend(
                "overlay section count disagrees with the dirty-section mask",
            ));
        }
        if sections.len() > MAX_OVERLAY_SECTIONS {
            return Err(StorageError::backend(format!(
                "overlay carries {} sections (maximum {MAX_OVERLAY_SECTIONS})",
                sections.len()
            )));
        }
        for section in &sections {
            let index = usize::from(section.index());
            if index >= SECTION_COUNT || (dirty_section_mask >> index) & 1 == 0 {
                return Err(StorageError::backend(format!(
                    "overlay section index {index} not present in mask {dirty_section_mask:#x}"
                )));
            }
        }
        if block_entities.len() > MAX_OVERLAY_BLOCK_ENTITIES {
            return Err(StorageError::backend(format!(
                "overlay carries {} block entities (maximum {MAX_OVERLAY_BLOCK_ENTITIES})",
                block_entities.len()
            )));
        }
        for (be_pos, payload) in &block_entities {
            if payload.len() > MAX_BLOCK_ENTITY_PAYLOAD_LEN {
                return Err(StorageError::backend(format!(
                    "block-entity payload at {be_pos:?} is {} bytes (maximum {MAX_BLOCK_ENTITY_PAYLOAD_LEN})",
                    payload.len()
                )));
            }
        }
        Ok(Self {
            schema_version,
            pos,
            dirty_section_mask,
            sections,
            updated_at_tick,
            block_entities,
        })
    }

    /// Returns the schema version this record was written under.
    #[must_use]
    pub fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the chunk column position this overlay belongs to.
    #[must_use]
    pub fn pos(&self) -> ChunkPos {
        self.pos
    }

    /// Returns the bitmask of sections this overlay carries (bit `i` set means
    /// section `i` is present).
    #[must_use]
    pub fn dirty_section_mask(&self) -> u32 {
        self.dirty_section_mask
    }

    /// Returns the server tick at which this overlay was captured.
    #[must_use]
    pub fn updated_at_tick(&self) -> u64 {
        self.updated_at_tick
    }

    /// Returns the number of modified sections this overlay carries.
    #[must_use]
    pub fn section_count(&self) -> usize {
        self.sections.len()
    }

    /// Returns the indices of the sections this overlay carries, ascending from the
    /// bottom of the world.
    ///
    /// Pairs with [`dirty_section_mask`](Self::dirty_section_mask): it yields exactly
    /// that mask's set bits as `usize` indices. The persistence load path uses it to
    /// reseed a reloaded chunk's cumulative persist-edited set (via
    /// `Chunk::restore_persist_edited_section`) so the chunk's next overlay flush
    /// re-captures these sections in full, matching how block entities are always
    /// captured whole.
    pub fn section_indices(&self) -> impl Iterator<Item = usize> + '_ {
        (0..SECTION_COUNT).filter(move |&index| (self.dirty_section_mask >> index) & 1 == 1)
    }

    /// Returns the number of block entities this overlay carries.
    #[must_use]
    pub fn block_entity_count(&self) -> usize {
        self.block_entities.len()
    }

    /// Returns the overlay's modified sections, in ascending section-index order.
    pub(crate) fn sections(&self) -> &[OverlaySection] {
        &self.sections
    }

    /// Returns the overlay's block entities as `(position, opaque payload)` pairs,
    /// for the codec to serialize.
    pub(crate) fn block_entities(&self) -> &[(BlockPos, Vec<u8>)] {
        &self.block_entities
    }

    /// Applies this overlay onto `chunk`, overwriting each carried section in full
    /// (air included) so the result matches the chunk as it was when captured, then
    /// reconstructs the chunk's block entities.
    ///
    /// `chunk` is expected to be the freshly generated flat baseline for the same
    /// position; only the persist-dirty sections are replaced, leaving the
    /// untouched sections as generated. Block entities are applied **after** the
    /// block states so each lands on its restored block.
    ///
    /// Block-entity reconstruction is defensive: an individual payload that fails
    /// to decode (corrupt, truncated, or written by an incompatible build), or
    /// whose position is rejected by [`Chunk::set_block_entity`], is **logged and
    /// skipped** rather than failing the whole load — so one bad block entity can
    /// never blank out a chunk. The block itself still loads (signs/chests lazily
    /// recreate a blank block entity on the next interaction).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::Backend`] if a section index is out of range or a
    /// computed block position is rejected by [`Chunk::set_block`] (which cannot
    /// happen for a well-formed record, but is handled rather than panicking). A
    /// malformed block entity is *not* an error (it is skipped, see above).
    pub fn apply_to_chunk(&self, chunk: &mut Chunk) -> Result<(), StorageError> {
        let pos = chunk.pos();
        for section in &self.sections {
            let index = usize::from(section.index());
            let base_y = overlay_section_base_y(index).ok_or_else(|| {
                StorageError::backend(format!("overlay section index {index} out of range"))
            })?;
            let origin = pos.origin_block(base_y);
            for (flat, &state) in section.blocks.iter().enumerate() {
                let local = overlay_local_pos(flat);
                let block = BlockPos::new(
                    origin.x() + i32::from(local.x()),
                    base_y + i32::from(local.y()),
                    origin.z() + i32::from(local.z()),
                );
                chunk.set_block(block, state).map_err(|e| {
                    StorageError::backend(format!("overlay block out of range: {e}"))
                })?;
            }
        }

        for (be_pos, payload) in &self.block_entities {
            match decode_block_entity(payload) {
                Ok(entity) => {
                    if let Err(e) = chunk.set_block_entity(*be_pos, entity) {
                        // The block entity decoded but could not be placed (out of
                        // the chunk's column, or the map is full): skip it, keep the
                        // chunk.
                        tracing::warn!(
                            chunk = ?pos,
                            block = ?be_pos,
                            error = %e,
                            "skipping a persisted block entity that could not be placed",
                        );
                    }
                }
                Err(e) => {
                    // Corrupt / incompatible payload: skip this one block entity but
                    // still load the chunk.
                    tracing::warn!(
                        chunk = ?pos,
                        block = ?be_pos,
                        error = %e,
                        "skipping a corrupt persisted block entity",
                    );
                }
            }
        }
        Ok(())
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

/// Who caused a logged block mutation.
///
/// Mirrors the simulation layer's notion of an actor without depending on it:
/// either a specific [`PlayerId`] or a non-player system source (command, plugin,
/// or an internal process). Stored in the append-only mutation journal so a
/// future crash-recovery or rollback can attribute each change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MutationActor {
    /// A specific player caused the mutation.
    Player(PlayerId),
    /// A non-player source (command, plugin, or internal process) caused it.
    System,
}

/// Why a block mutation happened, recorded in the journal.
///
/// A storage-local mirror of the simulation's `MutationCause` (which this crate
/// must not depend on), reduced to a stable tag. The acting player, when any,
/// rides [`MutationActor`] rather than this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MutationLogCause {
    /// A creative-mode player break/place.
    PlayerCreative,
    /// A command execution (e.g. `/setblock`).
    Command,
    /// A plugin submission via a command sink.
    Plugin,
    /// A test or replay harness.
    Test,
}

impl MutationLogCause {
    /// Returns the stable wire tag for this cause.
    pub(crate) fn as_id(self) -> u8 {
        match self {
            MutationLogCause::PlayerCreative => 0,
            MutationLogCause::Command => 1,
            MutationLogCause::Plugin => 2,
            MutationLogCause::Test => 3,
        }
    }

    /// Recovers a cause from its wire tag, or `None` if the tag is unknown.
    ///
    /// Pairs with [`MutationLogCause::as_id`] in the journal decoder, which is
    /// test-only this milestone (the journal is write-only in production).
    #[cfg(test)]
    pub(crate) fn from_id(id: u8) -> Option<Self> {
        match id {
            0 => Some(MutationLogCause::PlayerCreative),
            1 => Some(MutationLogCause::Command),
            2 => Some(MutationLogCause::Plugin),
            3 => Some(MutationLogCause::Test),
            _ => None,
        }
    }
}

/// A single entry in the append-only block-mutation journal.
///
/// Each accepted gameplay block edit appends one of these, recording the
/// monotonically increasing `id`, the server `tick`, the [`MutationActor`], the
/// block [`BlockPos`], the `old_state`/`new_state`, and the [`MutationLogCause`].
/// The journal is write-only this milestone; it is the foundation for a future
/// crash-replay / rollback that re-applies (or undoes) edits over a regenerated
/// or overlay-loaded baseline. Carries its own [`SchemaVersion`] per the
/// versioned-record rule, independent of any table layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockMutationLogRecord {
    schema_version: SchemaVersion,
    id: u64,
    tick: u64,
    actor: MutationActor,
    pos: BlockPos,
    old_state: BlockStateId,
    new_state: BlockStateId,
    cause: MutationLogCause,
}

impl BlockMutationLogRecord {
    /// Builds a journal entry.
    ///
    /// `id` is provisional input retained for codecs and detached records. A
    /// [`crate::WorldStore`] replaces it with a storage-owned durable sequence
    /// ID atomically when the record is appended.
    #[allow(clippy::too_many_arguments)] // a journal entry is an inherently wide, flat record
    #[must_use]
    pub fn new(
        schema_version: SchemaVersion,
        id: u64,
        tick: u64,
        actor: MutationActor,
        pos: BlockPos,
        old_state: BlockStateId,
        new_state: BlockStateId,
        cause: MutationLogCause,
    ) -> Self {
        Self {
            schema_version,
            id,
            tick,
            actor,
            pos,
            old_state,
            new_state,
            cause,
        }
    }

    /// Returns the schema version this entry was written under.
    #[must_use]
    pub fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the monotonic journal sequence id.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Replaces the provisional ID with a storage-owned durable sequence ID.
    #[must_use]
    pub(crate) fn with_storage_id(mut self, id: u64) -> Self {
        self.id = id;
        self
    }

    /// Returns the server tick the mutation happened on.
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Returns who caused the mutation.
    #[must_use]
    pub fn actor(&self) -> MutationActor {
        self.actor
    }

    /// Returns the mutated block position.
    #[must_use]
    pub fn pos(&self) -> BlockPos {
        self.pos
    }

    /// Returns the block state before the mutation.
    #[must_use]
    pub fn old_state(&self) -> BlockStateId {
        self.old_state
    }

    /// Returns the block state after the mutation.
    #[must_use]
    pub fn new_state(&self) -> BlockStateId {
        self.new_state
    }

    /// Returns why the mutation happened.
    #[must_use]
    pub fn cause(&self) -> MutationLogCause {
        self.cause
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

    #[test]
    fn overlay_captures_only_persist_dirty_sections() {
        let mut chunk = Chunk::new(ChunkPos::new(0, 0));
        // Edit two blocks in different sections and mark them persist-dirty (the
        // sim layer's job), but also write a third block WITHOUT marking it.
        let a = BlockPos::new(1, 5, 1); // section 4
        let b = BlockPos::new(2, 70, 2); // section 8
        let c = BlockPos::new(3, 100, 3); // section 10, network-dirty only
        for pos in [a, b, c] {
            chunk
                .set_block(pos, BlockStateId::new(9))
                .expect("in range");
        }
        chunk.mark_persist_dirty(a);
        chunk.mark_persist_dirty(b);

        let overlay =
            ChunkOverlayRecord::from_chunk(SchemaVersion::new(2), chunk.pos(), &chunk, 42);
        assert_eq!(overlay.section_count(), 2);
        assert_eq!(overlay.updated_at_tick(), 42);
        assert_eq!(overlay.dirty_section_mask(), (1 << 4) | (1 << 8));
    }

    #[test]
    fn section_indices_match_the_dirty_section_mask() {
        let mut chunk = Chunk::new(ChunkPos::new(0, 0));
        let a = BlockPos::new(1, 5, 1); // section 4
        let b = BlockPos::new(2, 70, 2); // section 8
        for pos in [a, b] {
            chunk
                .set_block(pos, BlockStateId::new(9))
                .expect("in range");
            chunk.mark_persist_dirty(pos);
        }
        let overlay = ChunkOverlayRecord::from_chunk(SchemaVersion::new(3), chunk.pos(), &chunk, 0);
        let indices: Vec<usize> = overlay.section_indices().collect();
        assert_eq!(indices, vec![4, 8]);
        // The iterator is exactly the set bits of the mask.
        let from_mask: Vec<usize> = (0..SECTION_COUNT)
            .filter(|&i| (overlay.dirty_section_mask() >> i) & 1 == 1)
            .collect();
        assert_eq!(indices, from_mask);
    }

    #[test]
    fn overlay_round_trips_through_a_regenerated_baseline() {
        use ferrumc_world::FlatWorldGenerator;
        let pos = ChunkPos::new(-3, 2);
        let mut edited = FlatWorldGenerator::new().generate(pos);
        // Break the grass surface (-> air) and place stone above it; mark both.
        let broken = pos.origin_block(63); // y = 63 surface, section 7
        let placed = pos.origin_block(70); // y = 70 air, section 8
        edited
            .set_block(broken, BlockStateId::AIR)
            .expect("in range");
        edited
            .set_block(placed, BlockStateId::new(1))
            .expect("in range");
        edited.mark_persist_dirty(broken);
        edited.mark_persist_dirty(placed);

        let overlay = ChunkOverlayRecord::from_chunk(SchemaVersion::new(2), pos, &edited, 7);

        // Apply the overlay onto a fresh baseline and confirm both edits survive,
        // including the broken (air) block which must NOT resurrect from the
        // baseline's grass.
        let mut rebuilt = FlatWorldGenerator::new().generate(pos);
        overlay.apply_to_chunk(&mut rebuilt).expect("apply");
        assert_eq!(rebuilt.get_block(broken), Some(BlockStateId::AIR));
        assert_eq!(rebuilt.get_block(placed), Some(BlockStateId::new(1)));
        // An untouched column elsewhere is still the generated surface.
        let untouched = pos.origin_block(63);
        let untouched = BlockPos::new(untouched.x() + 5, 63, untouched.z() + 5);
        assert_ne!(rebuilt.get_block(untouched), Some(BlockStateId::AIR));
    }

    #[test]
    fn overlay_reconstructs_block_entities_on_apply() {
        // Item-level conservation is covered by `ferrumc-world`'s block-entity
        // codec tests and the end-to-end persistence integration test; here we
        // prove the storage record reconstructs the block-entity *map* (a sign with
        // text and a chest) through `apply_to_chunk` over a regenerated baseline.
        use ferrumc_world::{BlockEntity, ChestInventory, FlatWorldGenerator, Sign, SignKind};

        let pos = ChunkPos::new(0, 0);
        let mut chunk = FlatWorldGenerator::new().generate(pos);
        let sign_pos = pos.origin_block(64);
        let chest_pos = BlockPos::new(sign_pos.x() + 2, 70, sign_pos.z() + 2);

        let mut sign = Sign::new(SignKind::Sign);
        sign.set_face_lines(
            true,
            [
                "hello".to_owned(),
                "world".to_owned(),
                String::new(),
                "!".to_owned(),
            ],
        );
        let chest = ChestInventory::new();
        chunk
            .set_block_entity(sign_pos, BlockEntity::Sign(sign.clone()))
            .expect("set sign");
        chunk
            .set_block_entity(chest_pos, BlockEntity::Chest(chest.clone()))
            .expect("set chest");
        chunk.mark_persist_dirty(sign_pos);

        let overlay = ChunkOverlayRecord::from_chunk(SchemaVersion::new(3), pos, &chunk, 1);

        // Apply onto a fresh baseline and confirm both block entities reconstruct.
        let mut rebuilt = FlatWorldGenerator::new().generate(pos);
        overlay.apply_to_chunk(&mut rebuilt).expect("apply");
        assert_eq!(
            rebuilt.block_entity(sign_pos),
            Some(&BlockEntity::Sign(sign))
        );
        assert_eq!(
            rebuilt.block_entity(chest_pos),
            Some(&BlockEntity::Chest(chest))
        );
    }

    #[test]
    fn apply_skips_a_corrupt_block_entity_without_failing_the_chunk() {
        use ferrumc_world::FlatWorldGenerator;
        let pos = ChunkPos::new(0, 0);
        let be_pos = pos.origin_block(64);
        // `0xFF` is not a known block-entity tag, so the payload fails to decode.
        let overlay = ChunkOverlayRecord::from_parts(
            SchemaVersion::new(3),
            pos,
            0,
            Vec::new(),
            0,
            vec![(be_pos, vec![0xFF])],
        )
        .expect("record builds");

        let mut chunk = FlatWorldGenerator::new().generate(pos);
        // The corrupt block entity must be skipped, not fail the whole apply.
        overlay
            .apply_to_chunk(&mut chunk)
            .expect("apply tolerates a corrupt block entity");
        assert!(
            chunk.block_entity(be_pos).is_none(),
            "the corrupt block entity is skipped, leaving none",
        );
    }

    #[test]
    fn overlay_from_parts_rejects_too_many_block_entities() {
        let pos = ChunkPos::ORIGIN;
        let be_pos = pos.origin_block(64);
        let too_many = vec![(be_pos, vec![0u8]); MAX_OVERLAY_BLOCK_ENTITIES + 1];
        let err =
            ChunkOverlayRecord::from_parts(SchemaVersion::new(3), pos, 0, Vec::new(), 0, too_many)
                .expect_err("over the block-entity cap");
        assert!(matches!(err, StorageError::Backend(_)));
    }

    #[test]
    fn overlay_from_parts_rejects_oversized_block_entity_payload() {
        let pos = ChunkPos::ORIGIN;
        let be_pos = pos.origin_block(64);
        let oversized = vec![(be_pos, vec![0u8; MAX_BLOCK_ENTITY_PAYLOAD_LEN + 1])];
        let err =
            ChunkOverlayRecord::from_parts(SchemaVersion::new(3), pos, 0, Vec::new(), 0, oversized)
                .expect_err("over the payload cap");
        assert!(matches!(err, StorageError::Backend(_)));
    }

    #[test]
    fn overlay_from_parts_rejects_mask_section_mismatch() {
        let section = OverlaySection::new(3, vec![BlockStateId::AIR; SECTION_VOLUME]).expect("len");
        // Mask claims section 5 but the only section is index 3.
        let err = ChunkOverlayRecord::from_parts(
            SchemaVersion::new(2),
            ChunkPos::ORIGIN,
            1 << 5,
            vec![section],
            0,
            Vec::new(),
        )
        .expect_err("inconsistent");
        assert!(matches!(err, StorageError::Backend(_)));
    }

    #[test]
    fn overlay_section_rejects_wrong_length() {
        let err = OverlaySection::new(0, vec![BlockStateId::AIR; 10]).expect_err("short");
        assert!(matches!(err, StorageError::Backend(_)));
    }

    #[test]
    fn mutation_log_cause_round_trips_every_tag() {
        for cause in [
            MutationLogCause::PlayerCreative,
            MutationLogCause::Command,
            MutationLogCause::Plugin,
            MutationLogCause::Test,
        ] {
            assert_eq!(MutationLogCause::from_id(cause.as_id()), Some(cause));
        }
        assert_eq!(MutationLogCause::from_id(200), None);
    }

    #[test]
    fn mutation_log_record_exposes_its_fields() {
        let player = PlayerId::offline("alice");
        let record = BlockMutationLogRecord::new(
            SchemaVersion::new(1),
            7,
            42,
            MutationActor::Player(player),
            BlockPos::new(1, 2, 3),
            BlockStateId::AIR,
            BlockStateId::new(1),
            MutationLogCause::PlayerCreative,
        );
        assert_eq!(record.id(), 7);
        assert_eq!(record.tick(), 42);
        assert_eq!(record.actor(), MutationActor::Player(player));
        assert_eq!(record.pos(), BlockPos::new(1, 2, 3));
        assert_eq!(record.new_state(), BlockStateId::new(1));
        assert_eq!(record.cause(), MutationLogCause::PlayerCreative);
    }
}
