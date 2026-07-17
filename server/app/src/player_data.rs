//! The persisted per-player snapshot and its codec.
//!
//! [`PlayerData`] is the connection-owned view of everything that must survive a
//! player leaving and rejoining — or the server restarting: spawn-relative
//! position, look (yaw + pitch), the selected hotbar index, and the 46-slot
//! inventory layout. It is serialized into the opaque [`PlayerRecord`] payload
//! (`PlayerRecord.data`) while the player's [`GameMode`] rides the record's own
//! typed field; storage never interprets either, so the storage layer stays
//! byte-oriented and free of any inventory/connection types.
//!
//! # Versioning and robustness
//!
//! Every record is stamped with [`PLAYER_SCHEMA_VERSION`]. A record written under
//! a different version, or a payload that fails to decode, is rejected with a
//! classified [`PlayerLoadError`]. Only a confirmed missing storage record may
//! use fresh-player defaults, so an unreadable record can never be overwritten by
//! a later default-state leave save.
//!
//! # Inventory fidelity
//!
//! Each slot persists its registry item id and stack count, which restores the
//! full inventory *layout* (what is where, and how many). Data components
//! (custom name, damage, NBT, …) are intentionally not persisted in this version:
//! the trusted slot encoder and the untrusted slot decoder use different on-wire
//! framing, so they do not round-trip, and a lossy component codec would be worse
//! than none. Restoring item + count covers the creative building loadout this
//! milestone targets; component persistence is a follow-up that would bump
//! [`PLAYER_SCHEMA_VERSION`].

use std::error::Error;
use std::fmt;
use std::num::NonZeroU8;

use serde::{Deserialize, Serialize};

use ferrumc_core::{GameMode, PlayerId, ServerError};
use ferrumc_items::{ComponentPatch, ItemId, ItemStack};
use ferrumc_math::Vec3;
use ferrumc_storage::{PlayerRecord, PlayerStore, SchemaVersion, StorageError};

use crate::inventory::{PlayerInventory, SLOT_COUNT};

/// The schema version stamped on every [`PlayerRecord`] this module writes.
///
/// A loaded record carrying any other version is rejected as incompatible.
pub(crate) const PLAYER_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Largest magnitude accepted for a restored player coordinate.
///
/// This mirrors the simulation's move sanity boundary. Restoration must apply it
/// before shard selection and float-to-integer chunk conversion, because a saved
/// value bypasses the normal serverbound movement validator.
const MAX_RESTORED_POSITION_MAGNITUDE: f64 = 3.0e7;

/// Returns whether a persisted position is safe to route and admit.
pub(crate) fn is_valid_restored_position(position: Vec3) -> bool {
    let valid_axis = |coordinate: f64| {
        coordinate.is_finite() && coordinate.abs() <= MAX_RESTORED_POSITION_MAGNITUDE
    };
    valid_axis(position.x) && valid_axis(position.y) && valid_axis(position.z)
}

/// A classified failure to load a player's persisted state for admission.
#[derive(Debug)]
pub(crate) enum PlayerLoadError {
    /// The storage operation itself failed.
    BackendError {
        /// The classified storage-layer error.
        source: ServerError,
    },
    /// The record uses a schema this binary cannot interpret.
    Incompatible {
        /// Schema version carried by the record.
        found: SchemaVersion,
        /// Schema version this binary accepts.
        expected: SchemaVersion,
    },
    /// The current-schema payload is malformed.
    Corrupt {
        /// The bounded JSON decoder failure.
        source: serde_json::Error,
    },
}

impl fmt::Display for PlayerLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendError { source } => {
                write!(formatter, "player storage load failed: {source}")
            }
            Self::Incompatible { found, expected } => write!(
                formatter,
                "incompatible player schema {found}; expected {expected}"
            ),
            Self::Corrupt { source } => {
                write!(formatter, "corrupt player record payload: {source}")
            }
        }
    }
}

impl Error for PlayerLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BackendError { source } => Some(source),
            Self::Corrupt { source } => Some(source),
            Self::Incompatible { .. } => None,
        }
    }
}

/// The only two successful player-load outcomes.
///
/// `NotFound` is a confirmed store miss and may start fresh. `Restored` carries
/// a record whose schema and payload were both validated.
#[derive(Debug)]
pub(crate) enum PlayerLoad {
    /// No record exists for this player.
    NotFound,
    /// A current-schema record decoded successfully.
    Restored {
        /// The decoded app-owned state.
        data: PlayerData,
        /// The typed game mode stored beside the payload.
        game_mode: GameMode,
    },
}

/// Loads and validates the one player record used for Play admission.
///
/// Only the storage trait's `Ok(None)` result becomes [`PlayerLoad::NotFound`].
/// Every error, including a backend that incorrectly reports a not-found error
/// instead of `Ok(None)`, remains a [`PlayerLoadError::BackendError`].
pub(crate) async fn load_player_for_join(
    store: &dyn PlayerStore,
    player: PlayerId,
) -> Result<PlayerLoad, PlayerLoadError> {
    match store.load_player(player).await {
        Ok(Some(record)) => {
            let game_mode = record.game_mode();
            let data = PlayerData::from_record(&record)?;
            Ok(PlayerLoad::Restored { data, game_mode })
        }
        Ok(None) => Ok(PlayerLoad::NotFound),
        Err(source) => Err(PlayerLoadError::BackendError { source }),
    }
}

/// One persisted inventory slot: a registry item id and a stack count.
///
/// An empty slot is `item_id == 0` (no item) with `count == 0`. On restore an
/// unknown id, a zero count, or a count above the item's maximum is normalized
/// to a clean stack (or an empty slot), so a corrupt entry never produces an
/// invalid [`ItemStack`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedSlot {
    /// The registry item id, or `0` for an empty slot.
    item_id: i32,
    /// The stack count (`0` for an empty slot).
    count: u8,
}

impl PersistedSlot {
    /// Captures one [`ItemStack`] as its id + count, collapsing an empty stack to
    /// the empty-slot encoding.
    fn capture(stack: &ItemStack) -> Self {
        match stack.item() {
            Some(item) => Self {
                item_id: item.id(),
                count: stack.count(),
            },
            None => Self {
                item_id: 0,
                count: 0,
            },
        }
    }

    /// Rebuilds an [`ItemStack`], normalizing any out-of-range data to a valid
    /// stack or the empty slot — never panicking on a hostile persisted value.
    fn restore(self) -> ItemStack {
        let Some(item) = ItemId::new(self.item_id) else {
            return ItemStack::empty();
        };
        // Clamp the count into `1..=max_stack`; a zero or over-cap count from a
        // corrupt blob must not produce an inconsistent stack.
        let clamped = self.count.min(item.max_stack());
        let Some(count) = NonZeroU8::new(clamped) else {
            return ItemStack::empty();
        };
        ItemStack::new(item, count, ComponentPatch::empty())
    }
}

/// A versioned, serializable snapshot of one player's persisted state.
///
/// The [`GameMode`] is *not* held here: it lives in the typed
/// [`PlayerRecord::game_mode`] field. Construct one from live connection state
/// with [`PlayerData::capture`] and turn it back into a seeded
/// [`PlayerInventory`] with [`PlayerData::restore_inventory`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlayerData {
    /// Player position, x component (world space).
    x: f64,
    /// Player position, y component (world space).
    y: f64,
    /// Player position, z component (world space).
    z: f64,
    /// Look yaw, in degrees.
    yaw: f32,
    /// Look pitch, in degrees.
    pitch: f32,
    /// The selected hotbar index (`0..=8`).
    selected_slot: u8,
    /// The 46 inventory slots, in window-0 order.
    slots: Vec<PersistedSlot>,
}

impl PlayerData {
    /// Captures the live connection state into a persistable snapshot.
    ///
    /// `position`, `yaw`, and `pitch` are the connection-tracked look/position;
    /// the selected slot and every inventory slot are read from `inventory`.
    pub(crate) fn capture(
        position: Vec3,
        yaw: f32,
        pitch: f32,
        inventory: &PlayerInventory,
    ) -> Self {
        let slots = (0..SLOT_COUNT)
            .map(|index| {
                inventory.slot(index).map_or(
                    PersistedSlot {
                        item_id: 0,
                        count: 0,
                    },
                    PersistedSlot::capture,
                )
            })
            .collect();
        Self {
            x: position.x,
            y: position.y,
            z: position.z,
            yaw,
            pitch,
            selected_slot: inventory.selected(),
            slots,
        }
    }

    /// The restored player position as a typed [`Vec3`].
    pub(crate) fn position(&self) -> Vec3 {
        Vec3::new(self.x, self.y, self.z)
    }

    /// The restored look yaw, in degrees.
    pub(crate) fn yaw(&self) -> f32 {
        self.yaw
    }

    /// The restored look pitch, in degrees.
    pub(crate) fn pitch(&self) -> f32 {
        self.pitch
    }

    /// Rebuilds the authoritative [`PlayerInventory`] for `game_mode` from the
    /// persisted slots and selected index.
    ///
    /// Missing slots (a short or empty list from an older or corrupt blob) and
    /// out-of-range entries restore as empty, so the result is always a valid
    /// 46-slot window.
    pub(crate) fn restore_inventory(&self, game_mode: GameMode) -> PlayerInventory {
        let mut slots: [ItemStack; SLOT_COUNT] = std::array::from_fn(|_| ItemStack::empty());
        for (index, slot) in self.slots.iter().take(SLOT_COUNT).enumerate() {
            slots[index] = slot.restore();
        }
        PlayerInventory::from_persisted(game_mode, self.selected_slot, slots)
    }

    /// Encodes this snapshot, plus the typed `game_mode`, into a [`PlayerRecord`].
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the payload exceeds [`MAX_PLAYER_DATA_LEN`]
    /// (unreachable for a 46-slot inventory) or — never in practice — fails to
    /// serialize.
    ///
    /// [`MAX_PLAYER_DATA_LEN`]: ferrumc_storage::MAX_PLAYER_DATA_LEN
    pub(crate) fn to_record(&self, game_mode: GameMode) -> Result<PlayerRecord, StorageError> {
        let data = serde_json::to_vec(self)
            .map_err(|err| StorageError::backend(format!("serializing player data: {err}")))?;
        PlayerRecord::new(PLAYER_SCHEMA_VERSION, game_mode, data)
    }

    /// Decodes a [`PlayerData`] from a loaded [`PlayerRecord`].
    ///
    /// A stale schema and a malformed current-schema payload remain distinct,
    /// classified errors. Neither is absence and neither authorizes fresh-player
    /// defaults.
    pub(crate) fn from_record(record: &PlayerRecord) -> Result<Self, PlayerLoadError> {
        if record.schema_version() != PLAYER_SCHEMA_VERSION {
            return Err(PlayerLoadError::Incompatible {
                found: record.schema_version(),
                expected: PLAYER_SCHEMA_VERSION,
            });
        }
        serde_json::from_slice(record.data()).map_err(|source| PlayerLoadError::Corrupt { source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_inventory() -> PlayerInventory {
        let mut inv = PlayerInventory::with_creative_kit(GameMode::Creative);
        // Move the selector off the default and edit a non-hotbar slot so the
        // round-trip has something distinctive to preserve.
        assert!(inv.set_selected(5));
        inv
    }

    #[test]
    fn round_trips_through_a_record() {
        let inv = sample_inventory();
        let data = PlayerData::capture(Vec3::new(1.5, 64.0, -3.25), 90.0, -12.5, &inv);

        let record = data.to_record(GameMode::Creative).expect("encodes");
        assert_eq!(record.schema_version(), PLAYER_SCHEMA_VERSION);
        assert_eq!(record.game_mode(), GameMode::Creative);

        let decoded = PlayerData::from_record(&record).expect("decodes");
        // Full-struct equality covers position, yaw, pitch, selected slot, and every
        // inventory slot in one shot (avoiding bare float comparisons); the typed
        // position accessor is spot-checked separately.
        assert_eq!(decoded, data);
        assert_eq!(decoded.position(), Vec3::new(1.5, 64.0, -3.25));

        // The restored inventory matches the captured one slot-for-slot (item id +
        // count) and keeps the selected index.
        let restored = decoded.restore_inventory(GameMode::Creative);
        for index in 0..SLOT_COUNT {
            let before = inv.slot(index).and_then(ItemStack::item).map(ItemId::id);
            let after = restored
                .slot(index)
                .and_then(ItemStack::item)
                .map(ItemId::id);
            assert_eq!(before, after, "slot {index} item id");
            assert_eq!(
                inv.slot(index).map(ItemStack::count),
                restored.slot(index).map(ItemStack::count),
                "slot {index} count"
            );
        }
        assert_eq!(restored.selected(), 5);
        assert_eq!(restored.game_mode(), GameMode::Creative);
    }

    #[tokio::test]
    async fn survives_a_store_reload() {
        use ferrumc_core::PlayerId;
        use ferrumc_storage::{InMemoryStore, PlayerStore};

        // Save into the fake store, then load it back — the fast stand-in for a
        // server restart: the bytes that come back must decode to the same state.
        let store = InMemoryStore::new();
        let player = PlayerId::offline("Saad");
        let inv = sample_inventory();
        let data = PlayerData::capture(Vec3::new(10.0, 65.0, -7.0), 12.0, 34.0, &inv);
        store
            .save_player(player, data.to_record(GameMode::Creative).expect("encode"))
            .await
            .expect("save");

        let loaded = store
            .load_player(player)
            .await
            .expect("load")
            .expect("record present");
        assert_eq!(loaded.game_mode(), GameMode::Creative);
        assert_eq!(PlayerData::from_record(&loaded).expect("decodes"), data);
    }

    #[test]
    fn malformed_payload_is_classified_as_corrupt_without_panicking() {
        let malformed = [
            Vec::new(),
            vec![0xff, 0x00, 0x42],
            b"{".to_vec(),
            b"[]".to_vec(),
            br#"{"x":"wrong"}"#.to_vec(),
            br#"{"y":64.0,"z":0.0,"yaw":0.0,"pitch":0.0,"selected_slot":0,"slots":[]}"#
                .to_vec(),
            br#"{"x":null,"y":64.0,"z":0.0,"yaw":0.0,"pitch":0.0,"selected_slot":0,"slots":[]}"#
                .to_vec(),
            br#"{"x":"NaN","y":64.0,"z":0.0,"yaw":0.0,"pitch":0.0,"selected_slot":0,"slots":[]}"#
                .to_vec(),
            br#"{"x":[],"y":64.0,"z":0.0,"yaw":0.0,"pitch":0.0,"selected_slot":0,"slots":[]}"#
                .to_vec(),
            br#"{"x":{},"y":64.0,"z":0.0,"yaw":0.0,"pitch":0.0,"selected_slot":0,"slots":[]}"#
                .to_vec(),
            br#"{"x":1e400,"y":64.0,"z":0.0,"yaw":0.0,"pitch":0.0,"selected_slot":0,"slots":[]}"#
                .to_vec(),
            br#"{"x":0.0,"y":64.0,"z":0.0,"yaw":0.0,"pitch":0.0,"selected_slot":0,"slots":[]}trailing"#
                .to_vec(),
        ];
        for payload in malformed {
            let record =
                PlayerRecord::new(PLAYER_SCHEMA_VERSION, GameMode::Survival, payload.clone())
                    .expect("malformed payload remains within the storage bound");
            assert!(
                matches!(
                    PlayerData::from_record(&record),
                    Err(PlayerLoadError::Corrupt { .. })
                ),
                "payload {payload:?} was not classified as corrupt",
            );
        }
    }

    #[test]
    fn restored_position_validation_is_finite_and_inclusive_on_every_axis() {
        let edge = MAX_RESTORED_POSITION_MAGNITUDE;
        for position in [
            Vec3::ZERO,
            Vec3::new(edge, 0.0, 0.0),
            Vec3::new(-edge, 0.0, 0.0),
            Vec3::new(0.0, edge, 0.0),
            Vec3::new(0.0, -edge, 0.0),
            Vec3::new(0.0, 0.0, edge),
            Vec3::new(0.0, 0.0, -edge),
        ] {
            assert!(
                is_valid_restored_position(position),
                "boundary position {position:?} should be accepted",
            );
        }

        let outside = edge + 1.0;
        for position in [
            Vec3::new(outside, 0.0, 0.0),
            Vec3::new(-outside, 0.0, 0.0),
            Vec3::new(0.0, outside, 0.0),
            Vec3::new(0.0, -outside, 0.0),
            Vec3::new(0.0, 0.0, outside),
            Vec3::new(0.0, 0.0, -outside),
            Vec3::new(f64::NAN, 0.0, 0.0),
            Vec3::new(0.0, f64::INFINITY, 0.0),
            Vec3::new(0.0, 0.0, f64::NEG_INFINITY),
        ] {
            assert!(
                !is_valid_restored_position(position),
                "unsafe position {position:?} should be rejected",
            );
        }
    }

    #[test]
    fn incompatible_schema_version_is_classified() {
        let inv = sample_inventory();
        let data = PlayerData::capture(Vec3::ZERO, 0.0, 0.0, &inv);
        let bytes = serde_json::to_vec(&data).expect("encodes");
        // A valid payload under a *different* schema version must be treated as
        // incompatible rather than misread.
        for found in [SchemaVersion::new(0), SchemaVersion::new(9999)] {
            let record =
                PlayerRecord::new(found, GameMode::Creative, bytes.clone()).expect("within bound");
            assert!(matches!(
                PlayerData::from_record(&record),
                Err(PlayerLoadError::Incompatible {
                    found: actual,
                    expected: PLAYER_SCHEMA_VERSION,
                }) if actual == found
            ));
        }
    }

    #[test]
    fn restore_normalizes_corrupt_slots() {
        // A short slot list, an unknown item id, and an absurd count must all
        // restore to a valid window without panicking.
        let data = PlayerData {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            yaw: 0.0,
            pitch: 0.0,
            selected_slot: 200, // out of range
            slots: vec![
                PersistedSlot {
                    item_id: -1,
                    count: 200,
                }, // unknown id
                PersistedSlot {
                    item_id: 1,
                    count: 0,
                }, // zero count -> empty
            ],
        };
        let inv = data.restore_inventory(GameMode::Creative);
        assert!(inv.slot(0).expect("slot 0").item().is_none());
        assert!(inv.slot(1).expect("slot 1").item().is_none());
        // Out-of-range selected index clamps to a valid hotbar slot.
        assert!(inv.selected() <= 8);
    }
}
