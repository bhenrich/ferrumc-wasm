//! The canonical [`ItemStack`] and the *trusted* (clientbound) slot encoder.
//!
//! An [`ItemStack`] is the server's authoritative item form: a validated
//! [`ItemId`], a non-zero count (or the empty slot), and a component patch. Its
//! trusted encoder emits the count-first `Slot` wire form with *typed,
//! unprefixed* component data — the form a 1.21.8 client expects on clientbound
//! packets. The matching decoder lives under `#[cfg(test)]` only, because the
//! server never decodes a trusted slot in production (clientbound is
//! server-to-client).

use std::num::NonZeroU8;

use ferrumc_codec::write_var_int;
use ferrumc_nbt::write_network_root;

use crate::component::{ComponentPatch, ComponentValue};
use crate::item_id::ItemId;
use crate::untrusted::ItemValidationError;
use crate::wire::{nbt_limits, write_count};

/// The canonical (trusted) item stack.
///
/// Either empty (no item, count 0, empty patch) or a present item with a
/// non-zero count and a component patch. Construct with [`ItemStack::empty`] or
/// [`ItemStack::new`]; the latter takes a [`NonZeroU8`] so a present stack can
/// never have a zero count.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemStack {
    item: Option<ItemId>,
    count: u8,
    components: ComponentPatch,
}

impl ItemStack {
    /// The empty slot (no item).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            item: None,
            count: 0,
            components: ComponentPatch::empty(),
        }
    }

    /// Creates a present stack with a non-zero count and a component patch.
    #[must_use]
    pub fn new(item: ItemId, count: NonZeroU8, components: ComponentPatch) -> Self {
        Self {
            item: Some(item),
            count: count.get(),
            components,
        }
    }

    /// The item, or `None` for the empty slot.
    #[must_use]
    pub fn item(&self) -> Option<ItemId> {
        self.item
    }

    /// The stack count (0 for the empty slot).
    #[must_use]
    pub fn count(&self) -> u8 {
        self.count
    }

    /// The component patch.
    #[must_use]
    pub fn components(&self) -> &ComponentPatch {
        &self.components
    }

    /// Whether this stack is internally consistent: an empty slot has count 0
    /// and an empty patch; a present stack has a count in `1..=max_stack`.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        match self.item {
            None => self.count == 0 && self.components.is_empty(),
            Some(item) => self.count >= 1 && self.count <= item.max_stack(),
        }
    }

    /// The block-state id this stack places, or `None` if it holds no item or a
    /// non-block item (air included — see [`ItemId::placeable_block`]).
    #[must_use]
    pub fn placeable_block(&self) -> Option<u32> {
        self.item.and_then(ItemId::placeable_block)
    }

    /// Encodes this stack as a *trusted* `Slot` into `out`.
    ///
    /// Empty slots encode as a single `itemCount = 0` byte. A present stack
    /// encodes `itemCount`, `itemId`, the added/removed counts, each added
    /// component as typed unprefixed data, then each removed component type id.
    ///
    /// Propagates an NBT encoding error if an NBT-valued component exceeds the
    /// [`NbtLimits`](ferrumc_nbt::NbtLimits) caps.
    pub fn encode_slot(&self, out: &mut Vec<u8>) -> Result<(), ItemValidationError> {
        match self.item {
            None => {
                // itemCount 0 marks the empty slot; nothing follows.
                write_var_int(out, 0);
            }
            Some(item) => {
                write_var_int(out, i32::from(self.count));
                write_var_int(out, item.id());
                write_count(out, self.components.added.len());
                write_count(out, self.components.removed.len());
                let limits = nbt_limits();
                for value in &self.components.added {
                    encode_component_trusted(out, value, &limits)?;
                }
                for removed in &self.components.removed {
                    write_var_int(out, removed.get());
                }
            }
        }
        Ok(())
    }
}

/// Encodes one component as trusted (typed, unprefixed) wire data.
fn encode_component_trusted(
    out: &mut Vec<u8>,
    value: &ComponentValue,
    limits: &ferrumc_nbt::NbtLimits,
) -> Result<(), ItemValidationError> {
    write_var_int(out, value.type_id());
    match value {
        ComponentValue::MaxStackSize(n) => write_var_int(out, i32::from(*n)),
        // max_damage and damage share the same varint wire form (the preceding
        // type id already disambiguates them).
        ComponentValue::MaxDamage(n) | ComponentValue::Damage(n) => write_var_int(out, *n),
        // `unbreakable` is a void component: the type id alone, no data.
        ComponentValue::Unbreakable => {}
        ComponentValue::CustomName(tag) | ComponentValue::CustomData(tag) => {
            // anonymousNbt: a network-root (unnamed) NBT value, unprefixed.
            let bytes = write_network_root(tag, limits)?;
            out.extend_from_slice(&bytes);
        }
        // Opaque carries already-typed wire bytes verbatim (no length prefix).
        ComponentValue::Opaque { raw, .. } => out.extend_from_slice(raw),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ferrumc_codec::{BoundedReader, CodecError};
    use ferrumc_nbt::{read_network_root_with_consumed, NbtCompound, NbtTag};

    use super::*;
    use crate::component::{ComponentTypeId, CUSTOM_DATA, CUSTOM_NAME, DAMAGE, MAX_STACK_SIZE};

    /// Reads a `VarInt` at `pos` over a fresh sub-reader, advancing `pos`.
    fn read_var_int_advance(data: &[u8], pos: &mut usize) -> Result<i32, ItemValidationError> {
        let slice =
            data.get(*pos..)
                .ok_or(ItemValidationError::Codec(CodecError::UnexpectedEof {
                    needed: 1,
                    remaining: 0,
                }))?;
        let mut reader = BoundedReader::new(slice);
        let value = reader.read_var_int()?;
        *pos += reader.position();
        Ok(value)
    }

    /// Reads a non-negative count at `pos`, advancing `pos`.
    fn read_len_advance(data: &[u8], pos: &mut usize) -> Result<usize, ItemValidationError> {
        let value = read_var_int_advance(data, pos)?;
        usize::try_from(value).map_err(|_| ItemValidationError::MalformedComponent { type_id: -1 })
    }

    /// Test-only trusted `Slot` decoder: the inverse of [`ItemStack::encode_slot`].
    ///
    /// Returns the decoded stack and the number of bytes consumed. Errors on an
    /// unknown item id or a trusted component type the encoder never emits (those
    /// have no length prefix, so they cannot be skipped).
    fn decode_trusted_slot(data: &[u8]) -> Result<(ItemStack, usize), ItemValidationError> {
        let mut pos = 0usize;
        let item_count = read_var_int_advance(data, &mut pos)?;
        if item_count == 0 {
            return Ok((ItemStack::empty(), pos));
        }
        let item_id_raw = read_var_int_advance(data, &mut pos)?;
        let item =
            ItemId::new(item_id_raw).ok_or(ItemValidationError::UnknownItemId(item_id_raw))?;
        let added_count = read_len_advance(data, &mut pos)?;
        let removed_count = read_len_advance(data, &mut pos)?;
        let limits = nbt_limits();

        let mut added = Vec::with_capacity(added_count);
        for _ in 0..added_count {
            let type_id = read_var_int_advance(data, &mut pos)?;
            let value = match type_id {
                MAX_STACK_SIZE => {
                    let raw = read_var_int_advance(data, &mut pos)?;
                    ComponentValue::MaxStackSize(u8::try_from(raw).unwrap())
                }
                DAMAGE => ComponentValue::Damage(read_var_int_advance(data, &mut pos)?),
                crate::component::MAX_DAMAGE => {
                    ComponentValue::MaxDamage(read_var_int_advance(data, &mut pos)?)
                }
                crate::component::UNBREAKABLE => ComponentValue::Unbreakable,
                CUSTOM_DATA | CUSTOM_NAME => {
                    let (tag, consumed) = read_network_root_with_consumed(&data[pos..], &limits)?;
                    pos += consumed;
                    if type_id == CUSTOM_DATA {
                        ComponentValue::CustomData(tag)
                    } else {
                        ComponentValue::CustomName(tag)
                    }
                }
                other => return Err(ItemValidationError::MalformedComponent { type_id: other }),
            };
            added.push(value);
        }

        let mut removed = Vec::with_capacity(removed_count);
        for _ in 0..removed_count {
            removed.push(ComponentTypeId(read_var_int_advance(data, &mut pos)?));
        }

        let count = NonZeroU8::new(u8::try_from(item_count).unwrap()).unwrap();
        Ok((
            ItemStack::new(item, count, ComponentPatch { added, removed }),
            pos,
        ))
    }

    fn nz(n: u8) -> NonZeroU8 {
        NonZeroU8::new(n).unwrap()
    }

    #[test]
    fn empty_slot_round_trips() {
        let stack = ItemStack::empty();
        let mut buf = Vec::new();
        stack.encode_slot(&mut buf).unwrap();
        assert_eq!(buf, vec![0]); // single itemCount=0 byte
        let (decoded, consumed) = decode_trusted_slot(&buf).unwrap();
        assert_eq!(decoded, stack);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn plain_item_round_trips() {
        let stack = ItemStack::new(ItemId::new(1).unwrap(), nz(64), ComponentPatch::empty());
        let mut buf = Vec::new();
        stack.encode_slot(&mut buf).unwrap();
        // itemCount=64, itemId=1, added=0, removed=0.
        assert_eq!(buf, vec![64, 1, 0, 0]);
        let (decoded, consumed) = decode_trusted_slot(&buf).unwrap();
        assert_eq!(decoded, stack);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn item_with_typed_components_round_trips() {
        let mut name = NbtCompound::new();
        name.push("text", NbtTag::String("Excalibur".to_owned()));
        let stack = ItemStack::new(
            ItemId::from_name("diamond_sword").unwrap(),
            nz(1),
            ComponentPatch {
                added: vec![
                    ComponentValue::MaxStackSize(99),
                    ComponentValue::Damage(42),
                    ComponentValue::Unbreakable,
                    ComponentValue::CustomName(NbtTag::Compound(name)),
                ],
                removed: vec![ComponentTypeId(DAMAGE), ComponentTypeId(MAX_STACK_SIZE)],
            },
        );
        let mut buf = Vec::new();
        stack.encode_slot(&mut buf).unwrap();
        let (decoded, consumed) = decode_trusted_slot(&buf).unwrap();
        assert_eq!(decoded, stack);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn is_valid_classifies_stacks() {
        assert!(ItemStack::empty().is_valid());
        assert!(
            ItemStack::new(ItemId::new(1).unwrap(), nz(64), ComponentPatch::empty()).is_valid()
        );
        // diamond_sword max_stack is 1, so a count of 2 is invalid.
        assert!(!ItemStack::new(
            ItemId::from_name("diamond_sword").unwrap(),
            nz(2),
            ComponentPatch::empty()
        )
        .is_valid());
    }
}
