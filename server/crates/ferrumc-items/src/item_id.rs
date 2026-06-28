//! [`ItemId`]: a protocol item id validated against the 1.21.8 registry.

use ferrumc_registry::item;

/// A protocol item id that is guaranteed to exist in the 1.21.8 item registry.
///
/// Construct one with [`ItemId::new`] (raw id) or [`ItemId::from_name`]
/// (resource location); both return `None` for an unknown item, so a constructed
/// `ItemId` always resolves to a name, a max stack size, and (for block items) a
/// placeable block state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(i32);

impl ItemId {
    /// Creates an [`ItemId`] from a raw protocol id, or `None` if the id is not
    /// in the registry.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferrumc_items::ItemId;
    ///
    /// assert!(ItemId::new(1).is_some()); // stone
    /// assert!(ItemId::new(-1).is_none());
    /// ```
    #[must_use]
    pub fn new(id: i32) -> Option<Self> {
        item::lookup_item_name(id).map(|_| Self(id))
    }

    /// Creates an [`ItemId`] from a resource location (namespaced or bare), or
    /// `None` if the name is unknown.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferrumc_items::ItemId;
    ///
    /// assert_eq!(ItemId::from_name("stone"), ItemId::new(1));
    /// assert_eq!(ItemId::from_name("minecraft:stone"), ItemId::new(1));
    /// assert!(ItemId::from_name("notch:stone").is_none());
    /// ```
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        item::lookup_item_protocol_id(name).map(Self)
    }

    /// The raw protocol id.
    #[must_use]
    pub fn id(self) -> i32 {
        self.0
    }

    /// The canonical namespaced resource location (e.g. `"minecraft:stone"`).
    #[must_use]
    pub fn name(self) -> &'static str {
        // Infallible: a constructed `ItemId` is always in the registry.
        item::lookup_item_name(self.0).unwrap_or("minecraft:air")
    }

    /// The maximum stack size for this item (e.g. 64 for stone, 1 for a sword).
    #[must_use]
    pub fn max_stack(self) -> u8 {
        item::item_max_stack(self.0)
    }

    /// Whether this is `minecraft:air` (id 0), the registry placeholder a client
    /// sends to mean "empty slot".
    #[must_use]
    pub fn is_air(self) -> bool {
        self.0 == AIR_ITEM_ID
    }

    /// The block-state id this item places, or `None` if it is not a placeable
    /// block.
    ///
    /// `minecraft:air` is treated as non-placeable (returns `None`) even though
    /// the raw registry maps it to block-state 0, so callers never "place" air.
    #[must_use]
    pub fn placeable_block(self) -> Option<u32> {
        // Air is in the raw mapping (id 0 -> state 0) but is not a real placement.
        if self.0 == AIR_ITEM_ID {
            return None;
        }
        item::item_to_block_state(self.0)
    }
}

/// The protocol item id for `minecraft:air`.
const AIR_ITEM_ID: i32 = 0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_items_resolve() {
        let stone = ItemId::new(1).unwrap();
        assert_eq!(stone.id(), 1);
        assert_eq!(stone.name(), "minecraft:stone");
        assert_eq!(stone.max_stack(), 64);
        assert_eq!(stone.placeable_block(), Some(1));

        let sword = ItemId::from_name("diamond_sword").unwrap();
        assert_eq!(sword.id(), 895);
        assert_eq!(sword.max_stack(), 1);
        assert_eq!(sword.placeable_block(), None); // not a block
    }

    #[test]
    fn air_is_not_placeable() {
        let air = ItemId::new(AIR_ITEM_ID).unwrap();
        assert_eq!(air.name(), "minecraft:air");
        assert_eq!(air.placeable_block(), None);
    }

    #[test]
    fn unknown_ids_and_names_rejected() {
        assert!(ItemId::new(-1).is_none());
        assert!(ItemId::new(i32::MAX).is_none());
        assert!(ItemId::from_name("definitely_not_real").is_none());
        assert!(ItemId::from_name("notch:stone").is_none());
    }
}
