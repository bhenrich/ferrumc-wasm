//! Block-state identifiers.

use ferrumc_registry::block_state;

/// A Minecraft block-state id: the integer a chunk section's palette stores and
/// the protocol writes on the wire to identify one specific block state.
///
/// This is a thin newtype over the `u32` ids that [`ferrumc_registry`] returns.
/// The internal representation is private so the id space stays a closed,
/// validated value type rather than an open `u32` field. The default value is
/// [`BlockStateId::AIR`] (raw `0`), matching the convention that an unset block
/// is air.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockStateId(u32);

impl BlockStateId {
    /// The block-state id for `minecraft:air` (raw `0`).
    pub const AIR: Self = Self(block_state::AIR);

    /// Wraps a raw protocol block-state id.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw protocol block-state id.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// Returns `true` if this id is [`BlockStateId::AIR`].
    #[must_use]
    pub const fn is_air(self) -> bool {
        self.0 == block_state::AIR
    }

    /// Resolves the default block-state id for a block resource location (for
    /// example `"minecraft:stone"` or the bare `"stone"`), returning `None` if
    /// the block is not part of the registry's pinned flat-world set.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        ferrumc_registry::default_block_state_id(name).map(Self)
    }
}

impl Default for BlockStateId {
    /// The default block-state id is [`BlockStateId::AIR`].
    fn default() -> Self {
        Self::AIR
    }
}

impl From<u32> for BlockStateId {
    fn from(raw: u32) -> Self {
        Self(raw)
    }
}

impl From<BlockStateId> for u32 {
    fn from(id: BlockStateId) -> Self {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::BlockStateId;

    #[test]
    fn default_is_air() {
        assert_eq!(BlockStateId::default(), BlockStateId::AIR);
        assert_eq!(BlockStateId::AIR.as_u32(), 0);
        assert!(BlockStateId::default().is_air());
    }

    #[test]
    fn new_and_accessor_round_trip() {
        let id = BlockStateId::new(85);
        assert_eq!(id.as_u32(), 85);
        assert!(!id.is_air());
        assert_eq!(u32::from(id), 85);
        assert_eq!(BlockStateId::from(85u32), id);
    }

    #[test]
    fn from_name_uses_registry() {
        assert_eq!(BlockStateId::from_name("air"), Some(BlockStateId::AIR));
        assert_eq!(
            BlockStateId::from_name("minecraft:stone"),
            Some(BlockStateId::new(1))
        );
        assert_eq!(BlockStateId::from_name("diamond_block"), None);
    }
}
