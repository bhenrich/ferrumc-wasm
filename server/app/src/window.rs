//! Per-connection open-container window state.
//!
//! The player inventory (window 0) is always open and modelled by
//! [`PlayerInventory`](crate::inventory::PlayerInventory). A *container* window —
//! a chest's `generic_9x3` screen — is opened on demand and tracked here while it
//! is on screen: the assigned window id, the chest's world position, a mirror of
//! the chest's [`CHEST_SLOTS`] item slots, the carried (cursor) item, and the
//! window's own state id.
//!
//! The chest contents are authoritative in the simulation; this mirror exists so
//! the connection can render the window and resolve clicks. Every chest-slot
//! mutation round-trips to the simulation and the mirror is refreshed from the
//! authoritative reply, so the mirror can never drift into a dupe.

use ferrumc_items::{encode_container_content_payload, ItemStack, ItemValidationError};
use ferrumc_math::BlockPos;
use ferrumc_world::CHEST_SLOTS;

use crate::inventory::{PlayerInventory, MAIN_START, PLAYER_WINDOW_END};

/// The `minecraft:menu` registry id for a single chest's `generic_9x3` GUI.
///
/// Verified against the pinned 1.21.8 menu registry (`generic_9x3` = 2).
pub(crate) const GENERIC_9X3_TYPE: i32 = 2;

/// The window state id stamped on the first `SetContainerContent`, bumped on every
/// subsequent send so the client validates clicks against the latest value.
const INITIAL_STATE_ID: i32 = 1;

/// The number of player-inventory slots shown in a container window (main grid +
/// hotbar), appended after the container's own slots.
const PLAYER_SECTION: usize = PLAYER_WINDOW_END - MAIN_START;

/// Total slot count of an open chest window: the chest plus the player section.
pub(crate) const CHEST_WINDOW_SLOTS: usize = CHEST_SLOTS + PLAYER_SECTION;

/// Which logical slot a raw chest-window slot index addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowSlot {
    /// A chest container slot, `0..`[`CHEST_SLOTS`].
    Chest(usize),
    /// A player-inventory slot (an index into [`PlayerInventory`]).
    Player(usize),
}

/// A container window the player currently has open.
pub(crate) struct OpenContainer {
    window_id: i32,
    position: BlockPos,
    /// Mirror of the chest's [`CHEST_SLOTS`] slots (authoritative copy is in the sim).
    slots: Vec<ItemStack>,
    cursor: ItemStack,
    state_id: i32,
}

impl OpenContainer {
    /// Opens a chest window `window_id` over the chest at `position`, seeded with
    /// the simulation's `slots` snapshot and an empty cursor.
    fn new(window_id: i32, position: BlockPos, slots: Vec<ItemStack>) -> Self {
        Self {
            window_id,
            position,
            slots,
            cursor: ItemStack::empty(),
            state_id: INITIAL_STATE_ID,
        }
    }

    /// The assigned window id (the client echoes it on click / close).
    pub(crate) fn window_id(&self) -> i32 {
        self.window_id
    }

    /// The world position of the open chest.
    pub(crate) fn position(&self) -> BlockPos {
        self.position
    }

    /// The current window state id.
    pub(crate) fn state_id(&self) -> i32 {
        self.state_id
    }

    /// The carried (cursor) item.
    pub(crate) fn cursor(&self) -> &ItemStack {
        &self.cursor
    }

    /// Replaces the cursor (with the authoritative result of a sim click).
    pub(crate) fn set_cursor(&mut self, cursor: ItemStack) {
        self.cursor = cursor;
    }

    /// A mutable reference to the cursor, for a local player-slot exchange.
    pub(crate) fn cursor_mut(&mut self) -> &mut ItemStack {
        &mut self.cursor
    }

    /// Refreshes the chest-slot mirror from an authoritative sim snapshot.
    pub(crate) fn set_chest_slots(&mut self, slots: Vec<ItemStack>) {
        self.slots = slots;
    }

    /// Advances the window state id (wrapping) before a clientbound resync.
    pub(crate) fn bump_state_id(&mut self) {
        self.state_id = self.state_id.wrapping_add(1);
    }

    /// Maps a raw wire slot index to the [`WindowSlot`] it addresses, or `None`
    /// for an out-of-window index (including the `-999` click-outside sentinel).
    ///
    /// Layout (`generic_9x3`): `0..27` chest, `27..63` the player section, which
    /// maps to player-inventory indices `MAIN_START..PLAYER_WINDOW_END`.
    pub(crate) fn classify_slot(raw: i16) -> Option<WindowSlot> {
        let raw = usize::try_from(raw).ok()?;
        if raw < CHEST_SLOTS {
            Some(WindowSlot::Chest(raw))
        } else if raw < CHEST_WINDOW_SLOTS {
            // Shift past the chest section into the player-inventory window region.
            Some(WindowSlot::Player(raw - CHEST_SLOTS + MAIN_START))
        } else {
            None
        }
    }

    /// Builds the `SetContainerContent` body for this window: the chest slots, then
    /// the player's main+hotbar section, then the carried cursor item.
    ///
    /// # Errors
    ///
    /// Propagates an NBT encode error from a slot component, or
    /// [`ItemValidationError::WindowTooLarge`] — unreachable here since
    /// [`CHEST_WINDOW_SLOTS`] is far below the window cap.
    pub(crate) fn content_payload(
        &self,
        inventory: &PlayerInventory,
    ) -> Result<Vec<u8>, ItemValidationError> {
        let mut combined = Vec::with_capacity(CHEST_WINDOW_SLOTS);
        combined.extend_from_slice(&self.slots);
        combined.extend_from_slice(inventory.window_slots());
        encode_container_content_payload(&combined, &self.cursor)
    }
}

/// The connection's open-window state: at most one container window plus the
/// rolling window-id allocator.
pub(crate) struct WindowState {
    open: Option<OpenContainer>,
    next_window_id: i32,
}

impl WindowState {
    /// No container window open; the allocator starts at window id 1 (0 is the
    /// always-open player inventory).
    pub(crate) fn new() -> Self {
        Self {
            open: None,
            next_window_id: 1,
        }
    }

    /// Allocates the next container window id, cycling `1..=99` so a click for a
    /// just-closed window cannot be confused with a freshly opened one.
    fn allocate_window_id(&mut self) -> i32 {
        let id = self.next_window_id;
        self.next_window_id = if id >= 99 { 1 } else { id + 1 };
        id
    }

    /// Opens a chest window over `position` seeded with `slots`, returning its
    /// assigned id. Any previously open container is dropped by the caller first
    /// (returning its cursor), so this never silently discards a carried item.
    pub(crate) fn open_chest(&mut self, position: BlockPos, slots: Vec<ItemStack>) -> i32 {
        let window_id = self.allocate_window_id();
        self.open = Some(OpenContainer::new(window_id, position, slots));
        window_id
    }

    /// The open container window, if any.
    pub(crate) fn open(&self) -> Option<&OpenContainer> {
        self.open.as_ref()
    }

    /// The open container window mutably, if any.
    pub(crate) fn open_mut(&mut self) -> Option<&mut OpenContainer> {
        self.open.as_mut()
    }

    /// Takes (closes) the open container window, if any.
    pub(crate) fn take(&mut self) -> Option<OpenContainer> {
        self.open.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_slot_splits_chest_and_player_sections() {
        // Chest slots map 1:1.
        assert_eq!(OpenContainer::classify_slot(0), Some(WindowSlot::Chest(0)));
        assert_eq!(
            OpenContainer::classify_slot(26),
            Some(WindowSlot::Chest(26))
        );
        // The player section starts right after the chest at inventory MAIN_START.
        assert_eq!(
            OpenContainer::classify_slot(27),
            Some(WindowSlot::Player(MAIN_START))
        );
        // The last hotbar slot is inventory PLAYER_WINDOW_END - 1.
        let last = i16::try_from(CHEST_WINDOW_SLOTS - 1).unwrap();
        assert_eq!(
            OpenContainer::classify_slot(last),
            Some(WindowSlot::Player(PLAYER_WINDOW_END - 1))
        );
        // Out-of-window indices (and the -999 click-outside sentinel) are rejected.
        let past_end = i16::try_from(CHEST_WINDOW_SLOTS).unwrap();
        assert_eq!(OpenContainer::classify_slot(past_end), None);
        assert_eq!(OpenContainer::classify_slot(-1), None);
        assert_eq!(OpenContainer::classify_slot(-999), None);
    }

    #[test]
    fn window_ids_cycle_and_skip_zero() {
        let mut state = WindowState::new();
        let first = state.open_chest(
            BlockPos::new(0, 0, 0),
            vec![ItemStack::empty(); CHEST_SLOTS],
        );
        assert_eq!(first, 1);
        let second = state.open_chest(
            BlockPos::new(0, 0, 0),
            vec![ItemStack::empty(); CHEST_SLOTS],
        );
        assert_eq!(second, 2, "each open allocates a fresh id");
    }

    #[test]
    fn content_payload_has_all_window_slots_plus_cursor() {
        use ferrumc_core::GameMode;

        let inventory = PlayerInventory::with_creative_kit(GameMode::Creative);
        let open = OpenContainer::new(
            1,
            BlockPos::new(0, 0, 0),
            vec![ItemStack::empty(); CHEST_SLOTS],
        );
        let payload = open.content_payload(&inventory).expect("payload encodes");
        // The body starts with the slot-count varint (63 < 128 -> one byte).
        assert_eq!(payload[0], CHEST_WINDOW_SLOTS as u8);
    }
}
