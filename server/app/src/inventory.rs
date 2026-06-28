//! Per-connection authoritative player inventory (window 0).
//!
//! The player inventory is connection-local state: the connection task is the
//! sole writer of every slot (it validates hostile creative-slot bytes through
//! [`ferrumc_items`] before storing) and owns the mandatory clientbound sends
//! that keep the client's view in sync. The simulation never sees the inventory;
//! it only receives the resolved block-state a place produces.
//!
//! The [`GameMode`] mirror held alongside is seeded to creative on join and
//! updated by the connection whenever it processes `/gamemode`. Because the
//! connection is the sole writer of mode changes, the mirror cannot drift, so the
//! "is this player authoritatively creative" gate the creative-slot handler needs
//! is a synchronous, lock-free local read instead of a per-packet driver
//! round-trip (creative-slot packets are frequent during menu editing).

use std::num::NonZeroU8;

use ferrumc_core::GameMode;
use ferrumc_items::{
    encode_container_content_payload, ComponentPatch, ItemId, ItemStack, ItemValidationError,
};

/// The player-inventory window id (window 0 is always open).
pub(crate) const WINDOW_ID: i32 = 0;

/// Number of slots in the player inventory window.
///
/// Layout: `0` craft-out, `1..=4` craft-in, `5..=8` armor, `9..=35` main,
/// `36..=44` hotbar, `45` offhand.
pub(crate) const SLOT_COUNT: usize = 46;

/// Index of the first hotbar slot (the slot for selected hotbar index `0`).
pub(crate) const HOTBAR_START: usize = 36;

/// Number of selectable hotbar slots (indices `0..=8`).
const HOTBAR_LEN: u8 = 9;

/// The starter creative kit placed in the hotbar (slots `36..=44`) on join: one
/// simple, placeable full block per hotbar slot. Every name is a placeable block
/// item in the pinned 1.21.8 registry.
const HOTBAR_KIT: [&str; HOTBAR_LEN as usize] = [
    "stone",
    "dirt",
    "oak_planks",
    "glass",
    "cobblestone",
    "bricks",
    "oak_log",
    "white_wool",
    "sand",
];

/// Count of each kit item placed in the hotbar.
const KIT_STACK_COUNT: u8 = 64;

/// The window state id carried by the first `SetContainerContent` on join. It is
/// bumped on every subsequent change so the client can validate clicks against it.
const INITIAL_STATE_ID: i32 = 1;

/// A server-authoritative 46-slot player inventory plus the selected hotbar index,
/// the window state id, and the connection-local game-mode mirror.
pub(crate) struct PlayerInventory {
    /// Every window-0 slot, indexed `0..SLOT_COUNT`.
    slots: [ItemStack; SLOT_COUNT],
    /// The window state id, sent on every clientbound inventory update and bumped
    /// on every change (wrapping).
    state_id: i32,
    /// The selected hotbar index, always `0..=8`.
    selected: u8,
    /// The connection-local mirror of the player's authoritative game mode.
    game_mode: GameMode,
}

impl PlayerInventory {
    /// Builds an inventory seeded with the creative starter kit in the hotbar and
    /// the given game-mode mirror.
    pub(crate) fn with_creative_kit(game_mode: GameMode) -> Self {
        let mut slots: [ItemStack; SLOT_COUNT] = std::array::from_fn(|_| ItemStack::empty());
        let count = NonZeroU8::new(KIT_STACK_COUNT).unwrap_or(NonZeroU8::MIN);
        for (offset, name) in HOTBAR_KIT.iter().enumerate() {
            // Every kit name is a known registry item, so this resolves; an
            // unexpected miss leaves the slot empty rather than panicking.
            if let Some(item) = ItemId::from_name(name) {
                slots[HOTBAR_START + offset] = ItemStack::new(item, count, ComponentPatch::empty());
            }
        }
        Self {
            slots,
            state_id: INITIAL_STATE_ID,
            selected: 0,
            game_mode,
        }
    }

    /// The current window state id.
    pub(crate) fn state_id(&self) -> i32 {
        self.state_id
    }

    /// The connection-local mirror of the player's game mode.
    pub(crate) fn game_mode(&self) -> GameMode {
        self.game_mode
    }

    /// Updates the game-mode mirror (the connection is the sole writer, after it
    /// processes a `/gamemode`).
    pub(crate) fn set_game_mode(&mut self, mode: GameMode) {
        self.game_mode = mode;
    }

    /// The stack in slot `index`, or `None` if `index` is out of range.
    pub(crate) fn slot(&self, index: usize) -> Option<&ItemStack> {
        self.slots.get(index)
    }

    /// The stack in the currently selected hotbar slot.
    ///
    /// `selected` is always `0..=8`, so the computed index is always a valid
    /// hotbar slot (`36..=44`).
    pub(crate) fn held(&self) -> &ItemStack {
        &self.slots[HOTBAR_START + self.selected as usize]
    }

    /// Sets the selected hotbar index, ignoring an out-of-range value.
    ///
    /// Returns `true` if `index` was a valid hotbar index (`0..=8`) and the
    /// selection was updated, `false` otherwise (selection unchanged).
    pub(crate) fn set_selected(&mut self, index: u8) -> bool {
        if index < HOTBAR_LEN {
            self.selected = index;
            true
        } else {
            false
        }
    }

    /// Stores the (already-validated) `stack` into slot `index` and bumps the
    /// state id.
    ///
    /// The caller must bounds-check `index` (`0..SLOT_COUNT`) and validate `stack`
    /// through `ferrumc_items` first; an out-of-range index is a no-op.
    pub(crate) fn set_creative_slot(&mut self, index: usize, stack: ItemStack) {
        if let Some(slot) = self.slots.get_mut(index) {
            *slot = stack;
            self.bump_state_id();
        }
    }

    /// Advances the window state id (wrapping), used after a resync so the client
    /// validates subsequent clicks against the new value.
    pub(crate) fn bump_state_id(&mut self) {
        self.state_id = self.state_id.wrapping_add(1);
    }

    /// Builds the `SetContainerContent` body for window 0: all [`SLOT_COUNT`]
    /// slots followed by an empty carried (cursor) item.
    ///
    /// # Errors
    ///
    /// Propagates an NBT encoding error from a component (none in the starter
    /// kit), or [`ItemValidationError::WindowTooLarge`] — unreachable here since
    /// [`SLOT_COUNT`] is far below the window cap.
    pub(crate) fn container_content_payload(&self) -> Result<Vec<u8>, ItemValidationError> {
        encode_container_content_payload(&self.slots, &ItemStack::empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stone is item id `1`, glass `195`, sand `59` in the pinned registry.
    const STONE_ITEM: i32 = 1;
    const GLASS_ITEM: i32 = 195;
    const SAND_ITEM: i32 = 59;

    fn item_id_at(inv: &PlayerInventory, slot: usize) -> Option<i32> {
        inv.slot(slot).and_then(ItemStack::item).map(ItemId::id)
    }

    #[test]
    fn creative_kit_populates_only_the_hotbar() {
        let inv = PlayerInventory::with_creative_kit(GameMode::Creative);
        assert_eq!(inv.state_id(), INITIAL_STATE_ID);
        assert_eq!(inv.selected(), 0);
        // The hotbar carries the kit; the first and last entries pin the layout.
        assert_eq!(item_id_at(&inv, 36), Some(STONE_ITEM));
        assert_eq!(item_id_at(&inv, 44), Some(SAND_ITEM));
        assert_eq!(inv.slot(36).unwrap().count(), KIT_STACK_COUNT);
        // Non-hotbar slots stay empty.
        for slot in 0..HOTBAR_START {
            assert!(inv.slot(slot).unwrap().item().is_none(), "slot {slot}");
        }
        assert!(inv.slot(45).unwrap().item().is_none());
    }

    #[test]
    fn held_follows_selection_and_rejects_out_of_range() {
        let mut inv = PlayerInventory::with_creative_kit(GameMode::Creative);
        // Default selection is hotbar slot 0 -> inventory slot 36 (stone).
        assert_eq!(inv.held().item().map(ItemId::id), Some(STONE_ITEM));
        // Selecting hotbar index 3 -> inventory slot 39 (glass).
        assert!(inv.set_selected(3));
        assert_eq!(inv.held().item().map(ItemId::id), Some(GLASS_ITEM));
        // An out-of-range index is ignored and leaves the selection untouched.
        assert!(!inv.set_selected(9));
        assert_eq!(inv.selected(), 3);
        assert_eq!(inv.held().item().map(ItemId::id), Some(GLASS_ITEM));
    }

    #[test]
    fn set_creative_slot_stores_and_bumps_state() {
        let mut inv = PlayerInventory::with_creative_kit(GameMode::Creative);
        let before = inv.state_id();
        let glass = ItemStack::new(
            ItemId::new(GLASS_ITEM).unwrap(),
            NonZeroU8::new(64).unwrap(),
            ComponentPatch::empty(),
        );
        inv.set_creative_slot(9, glass);
        assert_eq!(item_id_at(&inv, 9), Some(GLASS_ITEM));
        assert_eq!(inv.state_id(), before + 1);
        // An out-of-range index is a no-op (no store, no bump).
        inv.set_creative_slot(SLOT_COUNT, ItemStack::empty());
        assert_eq!(inv.state_id(), before + 1);
    }

    #[test]
    fn container_payload_encodes_every_slot() {
        let inv = PlayerInventory::with_creative_kit(GameMode::Creative);
        let payload = inv.container_content_payload().expect("payload encodes");
        // The body starts with the slot count varint (46 < 128 -> one byte).
        assert_eq!(payload[0], SLOT_COUNT as u8);
        // 46 slots + a carried item means more than just the count byte.
        assert!(payload.len() > SLOT_COUNT);
    }

    #[test]
    fn game_mode_mirror_round_trips() {
        let mut inv = PlayerInventory::with_creative_kit(GameMode::Creative);
        assert_eq!(inv.game_mode(), GameMode::Creative);
        inv.set_game_mode(GameMode::Survival);
        assert_eq!(inv.game_mode(), GameMode::Survival);
    }

    impl PlayerInventory {
        /// Test-only accessor for the selected hotbar index.
        fn selected(&self) -> u8 {
            self.selected
        }
    }
}
