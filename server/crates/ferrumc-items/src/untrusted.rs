//! The *untrusted* (serverbound) slot wire form and its hostile-input
//! normalization.
//!
//! A creative client sends a `set_creative_slot` packet whose item is an
//! `UntrustedSlot`: the same count-first framing as the trusted `Slot`, but each
//! component's data is a *varint-length-prefixed* blob ([`ByteArray`]). That
//! framing lets [`UntrustedItemStack::decode`] bound, skip, and strip components
//! without ever parsing their internals, which is exactly what hostile-input
//! handling needs.
//!
//! [`UntrustedItemStack::into_validated`] turns the wire form into a canonical
//! [`ItemStack`]: it rejects an unknown item id, clamps the count to
//! `1..=max_stack`, strips dangerous and out-of-range components, and validates
//! the supported components (NBT against [`NbtLimits`](ferrumc_nbt::NbtLimits),
//! varints for the integer components). Anything malformed is an `Err`, never a
//! panic.
//!
//! [`ByteArray`]: https://minecraft.wiki/w/Java_Edition_protocol/Slot_data

use std::num::NonZeroU8;

use ferrumc_codec::{BoundedBytes, BoundedReader};
use ferrumc_nbt::read_network_root;

use crate::component::{
    ComponentPatch, ComponentTypeId, ComponentValue, CUSTOM_DATA, CUSTOM_NAME, DAMAGE, MAX_DAMAGE,
    MAX_STACK_SIZE, UNBREAKABLE,
};
use crate::item_id::ItemId;
use crate::stack::ItemStack;
use crate::wire::{nbt_limits, MAX_COMPONENTS, MAX_COMPONENTS_TOTAL_BYTES, MAX_COMPONENT_BYTES};

/// The maximum modeled `max_stack_size` component value (vanilla allows `1..=99`).
const MAX_STACK_SIZE_VALUE: i32 = 99;

/// Errors raised while decoding or validating a serverbound item stack.
///
/// Each variant classifies a distinct failure so callers can react precisely.
/// The enum is `#[non_exhaustive]`: new failure modes may be added without a
/// breaking change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ItemValidationError {
    /// The item id is not present in the 1.21.8 registry.
    #[error("item id {0} is not in the 1.21.8 registry")]
    UnknownItemId(i32),

    /// The slot declared more components than [`MAX_COMPONENTS`] allows.
    #[error("slot declares {count} components, exceeding the maximum of {max}")]
    TooManyComponents {
        /// The declared component count (added + removed).
        count: usize,
        /// The configured maximum.
        max: usize,
    },

    /// The combined component data exceeded [`MAX_COMPONENTS_TOTAL_BYTES`].
    #[error("component data totals {total} bytes, exceeding the maximum of {max}")]
    ComponentsTooLarge {
        /// The running total of component data bytes.
        total: usize,
        /// The configured maximum.
        max: usize,
    },

    /// A supported component's data could not be parsed (bad varint, trailing
    /// bytes, or a non-empty `unbreakable`).
    #[error("component type {type_id} had malformed data")]
    MalformedComponent {
        /// The offending component type id.
        type_id: i32,
    },

    /// A `SetContainerContent` body declared more slots than [`MAX_WINDOW_SLOTS`]
    /// allows.
    ///
    /// [`MAX_WINDOW_SLOTS`]: crate::wire::MAX_WINDOW_SLOTS
    #[error("container declares {count} slots, exceeding the maximum of {max}")]
    WindowTooLarge {
        /// The declared slot count.
        count: usize,
        /// The configured maximum.
        max: usize,
    },

    /// A low-level bounded-reader failure (short read, oversized blob, negative
    /// length, and similar).
    #[error(transparent)]
    Codec(#[from] ferrumc_codec::CodecError),

    /// An NBT component failed to decode within the [`NbtLimits`] caps.
    ///
    /// [`NbtLimits`]: ferrumc_nbt::NbtLimits
    #[error(transparent)]
    Nbt(#[from] ferrumc_nbt::NbtError),
}

/// The serverbound (untrusted) item-stack wire form.
///
/// `item_id` is the raw id straight off the wire (it may not exist); `count` is
/// the raw count clamped only to the `u8` range; `added` holds each component's
/// `(type_id, raw_bytes)` with the bytes still opaque; `removed` holds raw
/// component type ids. Use [`into_validated`](Self::into_validated) to normalize
/// into a canonical [`ItemStack`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrustedItemStack {
    item_id: Option<i32>,
    count: u8,
    added: Vec<(i32, Vec<u8>)>,
    removed: Vec<i32>,
}

impl UntrustedItemStack {
    /// Decodes an `UntrustedSlot` from `reader`.
    ///
    /// Reads the count-first framing; an `itemCount` of 0 yields the empty slot.
    /// Otherwise each added component's data is read as a length-prefixed blob
    /// (bounded by [`MAX_COMPONENT_BYTES`]), with the component count bounded by
    /// [`MAX_COMPONENTS`] and the combined data bounded by
    /// [`MAX_COMPONENTS_TOTAL_BYTES`]. The decoder does not require the reader to
    /// be fully drained; the caller (e.g. the creative-slot packet body) should
    /// check for trailing bytes itself.
    ///
    /// # Errors
    ///
    /// Returns [`ItemValidationError::TooManyComponents`] /
    /// [`ItemValidationError::ComponentsTooLarge`] when the declared sizes exceed
    /// the caps, or [`ItemValidationError::Codec`] on a malformed/truncated read.
    pub fn decode(reader: &mut BoundedReader<'_>) -> Result<Self, ItemValidationError> {
        let item_count = reader.read_var_int()?;
        if item_count == 0 {
            return Ok(Self {
                item_id: None,
                count: 0,
                added: Vec::new(),
                removed: Vec::new(),
            });
        }

        let item_id = reader.read_var_int()?;
        let added_count = reader.read_var_int_len()?;
        let removed_count = reader.read_var_int_len()?;

        // Bound the total entry count before allocating either vector.
        let total = added_count.saturating_add(removed_count);
        if total > MAX_COMPONENTS {
            return Err(ItemValidationError::TooManyComponents {
                count: total,
                max: MAX_COMPONENTS,
            });
        }

        let mut added = Vec::with_capacity(added_count);
        let mut total_bytes = 0usize;
        for _ in 0..added_count {
            let type_id = reader.read_var_int()?;
            // Each blob is bounded per-component; reading it validates the
            // declared length against the cap and the bytes available.
            let data = BoundedBytes::<MAX_COMPONENT_BYTES>::read(reader)?.into_inner();
            total_bytes = total_bytes.saturating_add(data.len());
            if total_bytes > MAX_COMPONENTS_TOTAL_BYTES {
                return Err(ItemValidationError::ComponentsTooLarge {
                    total: total_bytes,
                    max: MAX_COMPONENTS_TOTAL_BYTES,
                });
            }
            added.push((type_id, data));
        }

        let mut removed = Vec::with_capacity(removed_count);
        for _ in 0..removed_count {
            removed.push(reader.read_var_int()?);
        }

        // Clamp the raw count into the u8 range; semantic clamping to the item's
        // max stack happens in `into_validated`.
        let count = u8::try_from(item_count.clamp(1, i32::from(u8::MAX))).unwrap_or(u8::MAX);

        Ok(Self {
            item_id: Some(item_id),
            count,
            added,
            removed,
        })
    }

    /// Normalizes this untrusted stack into a canonical [`ItemStack`].
    ///
    /// Applies hostile-input rules: an empty stack (and a present
    /// `minecraft:air` stack) normalizes to the empty slot; otherwise the item
    /// id must exist (else [`ItemValidationError::UnknownItemId`]), the count is
    /// clamped to `1..=max_stack`, and only the explicitly-modeled components are
    /// kept (parsed and validated). Every other client-authored component is
    /// stripped: the dangerous arbitrary-NBT / nested-item components
    /// ([`ComponentTypeId::is_dangerous`]), out-of-range component types, *and*
    /// any recognized-but-unmodeled in-range component. Nothing the client
    /// authored survives as an opaque passthrough, so malformed bytes can never
    /// be re-emitted as trusted clientbound data.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown item id or malformed supported-component
    /// data (bad varint or NBT exceeding the [`NbtLimits`](ferrumc_nbt::NbtLimits)
    /// caps).
    pub fn into_validated(self) -> Result<ItemStack, ItemValidationError> {
        let Some(id) = self.item_id else {
            // No item: a canonical empty slot, regardless of any stray fields.
            return Ok(ItemStack::empty());
        };

        let item = ItemId::new(id).ok_or(ItemValidationError::UnknownItemId(id))?;

        // `minecraft:air` (id 0) with a non-zero count is a non-vanilla "present
        // air" stack; normalize it to the canonical empty slot so it never
        // re-encodes as a nonsensical itemCount>0 / itemId=0 slot.
        if item.is_air() {
            return Ok(ItemStack::empty());
        }

        // Clamp into 1..=max_stack; max_stack is >= 1, so the result is non-zero.
        let clamped = self.count.clamp(1, item.max_stack());
        let count = NonZeroU8::new(clamped).unwrap_or(NonZeroU8::MIN);

        let limits = nbt_limits();
        let mut added = Vec::new();
        for (type_id, raw) in self.added {
            let tid = ComponentTypeId::new(type_id);
            // Strip the dangerous components and anything outside 0..=95.
            if tid.is_dangerous() || !tid.is_in_range() {
                continue;
            }
            let value = match type_id {
                CUSTOM_DATA => ComponentValue::CustomData(read_network_root(&raw, &limits)?),
                CUSTOM_NAME => ComponentValue::CustomName(read_network_root(&raw, &limits)?),
                MAX_STACK_SIZE => {
                    let raw_value = parse_single_varint(&raw, type_id)?;
                    let clamped = raw_value.clamp(1, MAX_STACK_SIZE_VALUE);
                    ComponentValue::MaxStackSize(u8::try_from(clamped).unwrap_or(u8::MAX))
                }
                MAX_DAMAGE => ComponentValue::MaxDamage(parse_single_varint(&raw, type_id)?),
                DAMAGE => ComponentValue::Damage(parse_single_varint(&raw, type_id)?),
                UNBREAKABLE => {
                    // `unbreakable` is void: any payload is non-conformant.
                    if !raw.is_empty() {
                        return Err(ItemValidationError::MalformedComponent { type_id });
                    }
                    ComponentValue::Unbreakable
                }
                // In range and not dangerous, but not specifically modeled: strip
                // it. A client-authored component the server does not model would
                // otherwise exit the trust boundary as opaque bytes the trusted
                // slot encoder re-emits verbatim (e.g. a malformed `item_name`),
                // so it is dropped rather than kept as a passthrough.
                _ => continue,
            };
            added.push(value);
        }

        let removed = self
            .removed
            .into_iter()
            .map(ComponentTypeId::new)
            .filter(|tid| tid.is_in_range() && !tid.is_dangerous())
            .collect();

        Ok(ItemStack::new(
            item,
            count,
            ComponentPatch::new(added, removed),
        ))
    }
}

/// Parses a component blob that must hold exactly one `VarInt`.
///
/// Rejects a malformed varint or any trailing bytes as
/// [`ItemValidationError::MalformedComponent`].
fn parse_single_varint(raw: &[u8], type_id: i32) -> Result<i32, ItemValidationError> {
    let mut reader = BoundedReader::new(raw);
    let value = reader
        .read_var_int()
        .map_err(|_| ItemValidationError::MalformedComponent { type_id })?;
    reader
        .finish()
        .map_err(|_| ItemValidationError::MalformedComponent { type_id })?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use ferrumc_codec::{write_var_int, BoundedBytes, BoundedReader};
    use ferrumc_nbt::{write_network_root, NbtCompound, NbtTag};

    use super::*;
    use crate::component::{
        BLOCK_ENTITY_DATA, BUCKET_ENTITY_DATA, BUNDLE_CONTENTS, CHARGED_PROJECTILES, CONTAINER,
        CONTAINER_LOOT, ENTITY_DATA, ITEM_NAME,
    };
    use crate::wire::write_count;

    impl UntrustedItemStack {
        /// Test-only constructor for a present stack.
        fn present(item_id: i32, count: u8, added: Vec<(i32, Vec<u8>)>, removed: Vec<i32>) -> Self {
            Self {
                item_id: Some(item_id),
                count,
                added,
                removed,
            }
        }

        /// Test-only encoder mirroring [`UntrustedItemStack::decode`]: component
        /// data is written length-prefixed (`ByteArray`).
        fn encode(&self, out: &mut Vec<u8>) {
            match self.item_id {
                None => write_var_int(out, 0),
                Some(id) => {
                    write_var_int(out, i32::from(self.count));
                    write_var_int(out, id);
                    write_count(out, self.added.len());
                    write_count(out, self.removed.len());
                    for (type_id, data) in &self.added {
                        write_var_int(out, *type_id);
                        BoundedBytes::<MAX_COMPONENT_BYTES>::new(data.clone())
                            .unwrap()
                            .write(out);
                    }
                    for type_id in &self.removed {
                        write_var_int(out, *type_id);
                    }
                }
            }
        }
    }

    /// Encodes a network-root NBT compound for use as component data.
    fn nbt_blob(tag: &NbtTag) -> Vec<u8> {
        write_network_root(tag, &nbt_limits()).unwrap()
    }

    /// A single-varint component blob.
    fn varint_blob(value: i32) -> Vec<u8> {
        let mut out = Vec::new();
        write_var_int(&mut out, value);
        out
    }

    #[test]
    fn empty_untrusted_slot_round_trips() {
        let stack = UntrustedItemStack {
            item_id: None,
            count: 0,
            added: Vec::new(),
            removed: Vec::new(),
        };
        let mut buf = Vec::new();
        stack.encode(&mut buf);
        assert_eq!(buf, vec![0]);
        let mut reader = BoundedReader::new(&buf);
        let decoded = UntrustedItemStack::decode(&mut reader).unwrap();
        assert_eq!(decoded, stack);
        assert!(reader.finish().is_ok());
    }

    #[test]
    fn present_untrusted_slot_round_trips() {
        let mut compound = NbtCompound::new();
        compound.push("note", NbtTag::String("hi".to_owned()));
        let stack = UntrustedItemStack::present(
            1,
            64,
            vec![
                (MAX_STACK_SIZE, varint_blob(16)),
                (CUSTOM_DATA, nbt_blob(&NbtTag::Compound(compound))),
            ],
            vec![DAMAGE],
        );
        let mut buf = Vec::new();
        stack.encode(&mut buf);
        let mut reader = BoundedReader::new(&buf);
        let decoded = UntrustedItemStack::decode(&mut reader).unwrap();
        assert_eq!(decoded, stack);
        assert!(reader.finish().is_ok());
    }

    #[test]
    fn into_validated_clamps_oversized_count() {
        // diamond_sword (id 895) has max_stack 1, so a wire count of 64 clamps.
        let stack = UntrustedItemStack::present(895, 64, Vec::new(), Vec::new());
        let validated = stack.into_validated().unwrap();
        assert_eq!(validated.count(), 1);
        assert_eq!(validated.item().unwrap().id(), 895);
    }

    #[test]
    fn into_validated_strips_dangerous_and_unknown_components() {
        let stack = UntrustedItemStack::present(
            1,
            1,
            vec![
                (BLOCK_ENTITY_DATA, vec![1, 2, 3]), // stripped
                (CONTAINER, vec![4, 5]),            // stripped
                (200, vec![6]),                     // out of range -> stripped
                (UNBREAKABLE, Vec::new()),          // kept
            ],
            vec![BLOCK_ENTITY_DATA, 300, DAMAGE],
        );
        let validated = stack.into_validated().unwrap();
        assert_eq!(
            validated.components().added(),
            [ComponentValue::Unbreakable]
        );
        // Only the in-range, non-dangerous removal survives.
        assert_eq!(
            validated.components().removed(),
            [ComponentTypeId::new(DAMAGE)]
        );
    }

    #[test]
    fn into_validated_strips_all_arbitrary_nbt_and_nested_item_components() {
        // Every dangerous component (arbitrary NBT or nested item tree) is
        // dropped on input, even though each is bounded as opaque bytes.
        let stack = UntrustedItemStack::present(
            1,
            1,
            vec![
                (CHARGED_PROJECTILES, vec![1]),
                (BUNDLE_CONTENTS, vec![2]),
                (ENTITY_DATA, vec![3]),
                (BUCKET_ENTITY_DATA, vec![4]),
                (BLOCK_ENTITY_DATA, vec![5]),
                (CONTAINER, vec![6]),
                (CONTAINER_LOOT, vec![7]),
                (UNBREAKABLE, Vec::new()), // the only survivor
            ],
            Vec::new(),
        );
        let validated = stack.into_validated().unwrap();
        assert_eq!(
            validated.components().added(),
            [ComponentValue::Unbreakable]
        );
    }

    #[test]
    fn into_validated_normalizes_present_air_to_empty() {
        // A present `minecraft:air` (id 0) stack is the non-vanilla "present air"
        // encoding; it collapses to the canonical empty slot.
        let stack = UntrustedItemStack::present(0, 5, vec![(UNBREAKABLE, Vec::new())], Vec::new());
        let validated = stack.into_validated().unwrap();
        assert_eq!(validated, ItemStack::empty());
    }

    #[test]
    fn into_validated_strips_unmodeled_inrange() {
        // `item_name` (6) is in range and not dangerous, but the server does not
        // model it: it is stripped rather than kept as an opaque passthrough.
        let stack = UntrustedItemStack::present(1, 1, vec![(ITEM_NAME, vec![9, 9, 9])], Vec::new());
        let validated = stack.into_validated().unwrap();
        assert!(validated.components().added().is_empty());
    }

    #[test]
    fn into_validated_strips_hostile_unmodeled_never_reemitted() {
        // A hostile creative slot authoring an in-range-but-unmodeled `item_name`
        // with a malformed payload must not survive the trust boundary: it is
        // dropped on input and can never be re-emitted as trusted clientbound bytes.
        let payload = vec![0xFF, 0xFF, 0xFF];
        let stack =
            UntrustedItemStack::present(1, 1, vec![(ITEM_NAME, payload.clone())], Vec::new());
        let validated = stack.into_validated().unwrap();
        assert!(validated.components().added().is_empty());

        // Encoding the validated stack as a trusted slot emits neither the
        // `item_name` type id nor the hostile payload bytes.
        let mut buf = Vec::new();
        validated.encode_slot(&mut buf).unwrap();
        assert!(
            !buf.contains(&u8::try_from(ITEM_NAME).unwrap()),
            "stripped item_name type id must not appear in the trusted slot"
        );
        assert!(
            !buf.windows(payload.len()).any(|window| window == payload),
            "hostile payload bytes must not appear in the trusted slot"
        );
    }

    #[test]
    fn into_validated_rejects_unknown_item_id() {
        let stack = UntrustedItemStack::present(999_999, 1, Vec::new(), Vec::new());
        assert_eq!(
            stack.into_validated().unwrap_err(),
            ItemValidationError::UnknownItemId(999_999)
        );
    }

    #[test]
    fn into_validated_rejects_malformed_nbt() {
        // A custom_data blob that is not valid NBT (root must be a compound).
        let stack =
            UntrustedItemStack::present(1, 1, vec![(CUSTOM_DATA, vec![0xFF, 0xFF])], Vec::new());
        assert!(matches!(
            stack.into_validated().unwrap_err(),
            ItemValidationError::Nbt(_)
        ));
    }

    #[test]
    fn into_validated_rejects_malformed_varint_component() {
        // max_stack_size blob with trailing junk after the varint.
        let stack =
            UntrustedItemStack::present(1, 1, vec![(MAX_STACK_SIZE, vec![0x01, 0x99])], Vec::new());
        assert_eq!(
            stack.into_validated().unwrap_err(),
            ItemValidationError::MalformedComponent {
                type_id: MAX_STACK_SIZE
            }
        );
    }

    #[test]
    fn into_validated_rejects_nonempty_unbreakable() {
        let stack = UntrustedItemStack::present(1, 1, vec![(UNBREAKABLE, vec![0x00])], Vec::new());
        assert_eq!(
            stack.into_validated().unwrap_err(),
            ItemValidationError::MalformedComponent {
                type_id: UNBREAKABLE
            }
        );
    }

    #[test]
    fn into_validated_clamps_max_stack_size_component() {
        let stack = UntrustedItemStack::present(
            1,
            1,
            vec![(MAX_STACK_SIZE, varint_blob(1000))],
            Vec::new(),
        );
        let validated = stack.into_validated().unwrap();
        assert_eq!(
            validated.components().added(),
            [ComponentValue::MaxStackSize(99)]
        );
    }

    #[test]
    fn decode_rejects_too_many_components() {
        // itemCount=1, itemId=1, addedCount=1000 (> MAX_COMPONENTS), removedCount=0.
        let mut buf = Vec::new();
        write_var_int(&mut buf, 1);
        write_var_int(&mut buf, 1);
        write_var_int(&mut buf, 1000);
        write_var_int(&mut buf, 0);
        let mut reader = BoundedReader::new(&buf);
        assert!(matches!(
            UntrustedItemStack::decode(&mut reader).unwrap_err(),
            ItemValidationError::TooManyComponents { .. }
        ));
    }

    #[test]
    fn decode_rejects_truncated_input() {
        // itemCount=1, itemId=1, then EOF before the component counts.
        let mut buf = Vec::new();
        write_var_int(&mut buf, 1);
        write_var_int(&mut buf, 1);
        let mut reader = BoundedReader::new(&buf);
        assert!(matches!(
            UntrustedItemStack::decode(&mut reader).unwrap_err(),
            ItemValidationError::Codec(_)
        ));
    }

    #[test]
    fn decode_rejects_oversized_component_blob() {
        // A component whose declared length exceeds MAX_COMPONENT_BYTES is
        // rejected before allocating.
        let mut buf = Vec::new();
        write_var_int(&mut buf, 1); // itemCount
        write_var_int(&mut buf, 1); // itemId
        write_var_int(&mut buf, 1); // addedCount
        write_var_int(&mut buf, 0); // removedCount
        write_var_int(&mut buf, CUSTOM_DATA); // component type
        write_var_int(&mut buf, i32::try_from(MAX_COMPONENT_BYTES + 1).unwrap()); // blob len
        let mut reader = BoundedReader::new(&buf);
        assert!(matches!(
            UntrustedItemStack::decode(&mut reader).unwrap_err(),
            ItemValidationError::Codec(_)
        ));
    }
}
