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
    encode_container_content_payload, left_click_exchange, ComponentPatch, ItemId, ItemStack,
    ItemValidationError,
};

/// The player-inventory window id (window 0 is always open).
pub(crate) const WINDOW_ID: i32 = 0;

/// First main-inventory slot (the storage grid; armor/craft occupy `0..9`).
pub(crate) const MAIN_START: usize = 9;

/// One past the last hotbar slot (`45` is the offhand, excluded from a container
/// window). `MAIN_START..PLAYER_WINDOW_END` is the 36-slot main+hotbar region a
/// chest window mirrors after its own slots.
pub(crate) const PLAYER_WINDOW_END: usize = 45;

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

    /// Builds an inventory restored from persisted state: the given slots verbatim,
    /// the (range-checked) selected hotbar index, and the game-mode mirror, stamped
    /// with the initial window state id.
    ///
    /// An out-of-range `selected` (not `0..=8`) clamps to `0` rather than producing
    /// an invalid selection, so a corrupt persisted value can never desync the held
    /// slot.
    pub(crate) fn from_persisted(
        game_mode: GameMode,
        selected: u8,
        slots: [ItemStack; SLOT_COUNT],
    ) -> Self {
        Self {
            slots,
            state_id: INITIAL_STATE_ID,
            selected: if selected < HOTBAR_LEN { selected } else { 0 },
            game_mode,
        }
    }

    /// The current window state id.
    pub(crate) fn state_id(&self) -> i32 {
        self.state_id
    }

    /// The selected hotbar index (always `0..=8`).
    pub(crate) fn selected(&self) -> u8 {
        self.selected
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

    /// A mutable reference to slot `index`, or `None` if out of range.
    ///
    /// Used by the container window handler to apply a conserving
    /// [`left_click_exchange`] against a player-inventory slot shown inside an open
    /// container window. The caller is responsible for the clientbound resync that
    /// keeps the client's view in step.
    pub(crate) fn slot_mut(&mut self, index: usize) -> Option<&mut ItemStack> {
        self.slots.get_mut(index)
    }

    /// The 36-slot main-inventory + hotbar region (`MAIN_START..PLAYER_WINDOW_END`)
    /// a container window mirrors after its own slots.
    ///
    /// The slice order — main grid then hotbar — is exactly the order the
    /// `generic_9x3` window expects for its player-inventory section, so the
    /// container payload builder can append it verbatim.
    pub(crate) fn window_slots(&self) -> &[ItemStack] {
        &self.slots[MAIN_START..PLAYER_WINDOW_END]
    }

    /// Deposits `cursor` back into the main inventory + hotbar, returning whatever
    /// could not fit (empty in practice).
    ///
    /// Used when a container window closes so a carried item is never lost: it
    /// first merges into matching stacks, then fills empty slots, applying the
    /// item-count-conserving [`left_click_exchange`] only to slots that are empty
    /// or hold the same item (never a different-item slot, which would swap an
    /// unrelated item onto the cursor). Bumps the window state id when anything was
    /// stored so the follow-up window-0 resync carries a fresh id. A non-empty
    /// return means the inventory was full; the caller logs it rather than dropping
    /// silently.
    #[must_use]
    pub(crate) fn deposit(&mut self, mut cursor: ItemStack) -> ItemStack {
        if cursor.item().is_none() {
            return cursor;
        }
        // Pass 1: top up existing matching stacks.
        for index in MAIN_START..PLAYER_WINDOW_END {
            if cursor.item().is_none() {
                break;
            }
            let slot = &mut self.slots[index];
            if slot.item().is_some()
                && slot.item() == cursor.item()
                && slot.components() == cursor.components()
            {
                left_click_exchange(slot, &mut cursor);
            }
        }
        // Pass 2: drop the remainder into the first empty slots.
        for index in MAIN_START..PLAYER_WINDOW_END {
            if cursor.item().is_none() {
                break;
            }
            let slot = &mut self.slots[index];
            if slot.item().is_none() {
                left_click_exchange(slot, &mut cursor);
            }
        }
        self.bump_state_id();
        cursor
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

    /// Builds the opaque `SetEquipment` body for the player's main hand: the slot
    /// byte `0x00` (slot 0 = main hand, high bit clear = the terminal entry)
    /// followed by the trusted [`ItemStack::encode_slot`] of the currently held
    /// item. Armor/offhand are out of scope for the v0 main-hand target.
    ///
    /// The session/router layer carries this opaque (it has no `ferrumc-items`
    /// dependency) and only prepends the router-owned entity id.
    ///
    /// # Errors
    ///
    /// Propagates an [`ItemValidationError`] from encoding the held [`ItemStack`]
    /// (e.g. an NBT component failure); the default creative-kit items never error.
    pub(crate) fn main_hand_equipment_body(&self) -> Result<Vec<u8>, ItemValidationError> {
        // Slot 0 (main hand), high bit clear so it is the single terminal entry.
        let mut body = vec![0x00u8];
        self.held().encode_slot(&mut body)?;
        Ok(body)
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
    fn deposit_merges_then_fills_empties_and_conserves() {
        let mut inv = PlayerInventory::with_creative_kit(GameMode::Creative);
        let stone_id = ItemId::new(STONE_ITEM).unwrap();
        // Seed main slot 9 with 40 stone; the hotbar already holds a 64 stone (36).
        inv.set_creative_slot(9, ItemStack::new(stone_id, nz(40), ComponentPatch::empty()));
        let before = total_stone(&inv) + 30; // we will deposit 30 more stone

        // Depositing 30 stone tops up slot 9 (40 -> 64) and leaves 6 for an empty slot.
        let leftover = inv.deposit(ItemStack::new(stone_id, nz(30), ComponentPatch::empty()));
        assert!(leftover.item().is_none(), "all 30 stone were stored");
        assert_eq!(total_stone(&inv), before, "no stone duplicated or lost");
        assert_eq!(inv.slot(9).unwrap().count(), 64, "slot 9 topped to max");
    }

    #[test]
    fn deposit_of_empty_cursor_is_a_noop() {
        let mut inv = PlayerInventory::with_creative_kit(GameMode::Creative);
        let before = inv.state_id();
        let leftover = inv.deposit(ItemStack::empty());
        assert!(leftover.item().is_none());
        assert_eq!(
            inv.state_id(),
            before,
            "an empty deposit does not bump state"
        );
    }

    /// Total count of stone across the whole inventory.
    fn total_stone(inv: &PlayerInventory) -> u32 {
        (0..SLOT_COUNT)
            .filter_map(|i| inv.slot(i))
            .filter(|s| s.item() == ItemId::new(STONE_ITEM))
            .map(|s| u32::from(s.count()))
            .sum()
    }

    fn nz(n: u8) -> NonZeroU8 {
        NonZeroU8::new(n).unwrap()
    }

    #[test]
    fn game_mode_mirror_round_trips() {
        let mut inv = PlayerInventory::with_creative_kit(GameMode::Creative);
        assert_eq!(inv.game_mode(), GameMode::Creative);
        inv.set_game_mode(GameMode::Survival);
        assert_eq!(inv.game_mode(), GameMode::Survival);
    }
}
