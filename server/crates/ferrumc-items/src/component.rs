//! The data-component model: [`ComponentValue`], [`ComponentPatch`],
//! [`ComponentTypeId`], and the 1.21.8 component type-id constants.
//!
//! A 1.21.8 item carries a *patch* over its default data components: a set of
//! components to add (each with a typed value) and a set of component types to
//! remove. The component type ids are a closed `0..=95` enum
//! ([`MAX_COMPONENT_TYPE_ID`]); this module names the handful the slot codec
//! understands and carries the rest as [`ComponentValue::Opaque`].

use ferrumc_nbt::NbtTag;

use crate::wire::MAX_COMPONENT_BYTES;

/// A data-component type id (the `SlotComponentType` varint enum on the wire).
///
/// Valid ids are `0..=`[`MAX_COMPONENT_TYPE_ID`]; values outside that range are
/// rejected (stripped) during untrusted normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ComponentTypeId(i32);

impl ComponentTypeId {
    /// Wraps a raw wire id, which may be out of range or dangerous.
    ///
    /// The newtype intentionally accepts any `i32` straight off the wire; validity
    /// is a separate query via [`is_in_range`](Self::is_in_range) /
    /// [`is_dangerous`](Self::is_dangerous), which untrusted normalization consults
    /// before keeping a component.
    #[must_use]
    pub fn new(raw: i32) -> Self {
        Self(raw)
    }

    /// The raw wire id.
    #[must_use]
    pub fn get(self) -> i32 {
        self.0
    }

    /// Whether this id is within the valid `0..=`[`MAX_COMPONENT_TYPE_ID`] range.
    #[must_use]
    pub fn is_in_range(self) -> bool {
        (0..=MAX_COMPONENT_TYPE_ID).contains(&self.0)
    }

    /// Whether this component is one that hostile-input handling must strip from
    /// a creative slot.
    ///
    /// These all carry attacker-controlled arbitrary NBT
    /// ([`BLOCK_ENTITY_DATA`], [`ENTITY_DATA`], [`BUCKET_ENTITY_DATA`],
    /// [`CONTAINER_LOOT`]) or nested item trees ([`CONTAINER`],
    /// [`CHARGED_PROJECTILES`], [`BUNDLE_CONTENTS`]). They are bounded on decode
    /// (never recursively parsed), but the server never trusts a creative client
    /// to author them, and would otherwise relay the opaque bytes back out
    /// verbatim, so they are dropped at the input boundary rather than kept as a
    /// passthrough.
    #[must_use]
    pub fn is_dangerous(self) -> bool {
        matches!(
            self.0,
            CHARGED_PROJECTILES
                | BUNDLE_CONTENTS
                | ENTITY_DATA
                | BUCKET_ENTITY_DATA
                | BLOCK_ENTITY_DATA
                | CONTAINER
                | CONTAINER_LOOT
        )
    }
}

/// `custom_data` (0): arbitrary NBT, wire form `anonymousNbt`.
pub const CUSTOM_DATA: i32 = 0;
/// `max_stack_size` (1): a varint, valid `1..=99`.
pub const MAX_STACK_SIZE: i32 = 1;
/// `max_damage` (2): a varint durability cap.
pub const MAX_DAMAGE: i32 = 2;
/// `damage` (3): a varint current damage.
pub const DAMAGE: i32 = 3;
/// `unbreakable` (4): a void (zero-byte) flag.
pub const UNBREAKABLE: i32 = 4;
/// `custom_name` (5): a text component, wire form `anonymousNbt`.
pub const CUSTOM_NAME: i32 = 5;
/// `item_name` (6): the default item name, wire form `anonymousNbt`.
pub const ITEM_NAME: i32 = 6;
/// `charged_projectiles` (40): a nested item tree — stripped on input.
pub const CHARGED_PROJECTILES: i32 = 40;
/// `bundle_contents` (41): a nested item tree — stripped on input.
pub const BUNDLE_CONTENTS: i32 = 41;
/// `entity_data` (49): arbitrary entity NBT — stripped on input.
pub const ENTITY_DATA: i32 = 49;
/// `bucket_entity_data` (50): arbitrary captured-entity NBT — stripped on input.
pub const BUCKET_ENTITY_DATA: i32 = 50;
/// `block_entity_data` (51): arbitrary block-entity NBT — stripped on input.
pub const BLOCK_ENTITY_DATA: i32 = 51;
/// `container` (66): a nested item tree — stripped on input.
pub const CONTAINER: i32 = 66;
/// `container_loot` (70): a deferred loot-table reference — stripped on input.
pub const CONTAINER_LOOT: i32 = 70;
/// The highest valid component type id in 1.21.8 (96 types, ids `0..=95`).
pub const MAX_COMPONENT_TYPE_ID: i32 = 95;

/// A server-authored opaque data component: an in-range, non-dangerous, bounded
/// payload carried as its raw *trusted* (typed, unprefixed) wire bytes.
///
/// Construct only through the checked [`OpaqueComponent::new`], which rejects an
/// out-of-range or dangerous type id and an over-cap payload, so an
/// [`OpaqueComponent`] can never carry bytes the trusted slot encoder must not
/// emit. Untrusted (client-authored) components are never wrapped here — they are
/// stripped at the input boundary; this type exists for *server*-authored
/// passthroughs only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueComponent {
    type_id: i32,
    raw: Vec<u8>,
}

impl OpaqueComponent {
    /// Builds a checked opaque component, or `None` if `type_id` is out of range
    /// ([`ComponentTypeId::is_in_range`]) or dangerous
    /// ([`ComponentTypeId::is_dangerous`]), or if `raw` exceeds
    /// [`MAX_COMPONENT_BYTES`](crate::MAX_COMPONENT_BYTES).
    #[must_use]
    pub fn new(type_id: i32, raw: Vec<u8>) -> Option<Self> {
        let tid = ComponentTypeId::new(type_id);
        if !tid.is_in_range() || tid.is_dangerous() || raw.len() > MAX_COMPONENT_BYTES {
            return None;
        }
        Some(Self { type_id, raw })
    }

    /// The component type id.
    #[must_use]
    pub fn type_id(&self) -> i32 {
        self.type_id
    }

    /// The component's trusted wire bytes (typed payload, unprefixed).
    #[must_use]
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
}

/// A single data-component value.
///
/// A few common types are modeled with typed variants; everything else (a
/// recognized-but-unmodeled type, or a server-authored passthrough) rides in
/// [`ComponentValue::Opaque`], which carries the component's *trusted* (typed,
/// unprefixed) wire bytes verbatim.
#[derive(Debug, Clone, PartialEq)]
pub enum ComponentValue {
    /// `max_stack_size` (1): the per-stack item cap, `1..=99`.
    MaxStackSize(u8),
    /// `damage` (3): current durability damage.
    Damage(i32),
    /// `max_damage` (2): maximum durability.
    MaxDamage(i32),
    /// `unbreakable` (4): the item ignores durability loss.
    Unbreakable,
    /// `custom_name` (5): a rename, as a text-component NBT value.
    CustomName(NbtTag),
    /// `custom_data` (0): arbitrary attached NBT.
    CustomData(NbtTag),
    /// Any other (server-authored) component, carried as its raw *trusted* wire
    /// bytes under the given type id (no length prefix — see the crate README).
    Opaque(OpaqueComponent),
}

impl ComponentValue {
    /// The wire component type id this value encodes to.
    #[must_use]
    pub fn type_id(&self) -> i32 {
        match self {
            Self::MaxStackSize(_) => MAX_STACK_SIZE,
            Self::Damage(_) => DAMAGE,
            Self::MaxDamage(_) => MAX_DAMAGE,
            Self::Unbreakable => UNBREAKABLE,
            Self::CustomName(_) => CUSTOM_NAME,
            Self::CustomData(_) => CUSTOM_DATA,
            Self::Opaque(c) => c.type_id(),
        }
    }
}

/// A patch over an item's default data components: values to add and types to
/// remove, in wire order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComponentPatch {
    /// Components to add, in wire order.
    added: Vec<ComponentValue>,
    /// Component types to remove, in wire order.
    removed: Vec<ComponentTypeId>,
}

impl ComponentPatch {
    /// Builds a patch from the components to add and the types to remove, in wire
    /// order.
    #[must_use]
    pub fn new(added: Vec<ComponentValue>, removed: Vec<ComponentTypeId>) -> Self {
        Self { added, removed }
    }

    /// An empty patch (no additions, no removals).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The components to add, in wire order.
    #[must_use]
    pub fn added(&self) -> &[ComponentValue] {
        &self.added
    }

    /// The component types to remove, in wire order.
    #[must_use]
    pub fn removed(&self) -> &[ComponentTypeId] {
        &self.removed
    }

    /// Whether the patch carries no additions and no removals.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }

    /// The total number of component entries (added + removed).
    #[must_use]
    pub fn len(&self) -> usize {
        self.added.len() + self.removed.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_id_mapping_is_stable() {
        assert_eq!(ComponentValue::MaxStackSize(64).type_id(), MAX_STACK_SIZE);
        assert_eq!(ComponentValue::Damage(3).type_id(), DAMAGE);
        assert_eq!(ComponentValue::MaxDamage(100).type_id(), MAX_DAMAGE);
        assert_eq!(ComponentValue::Unbreakable.type_id(), UNBREAKABLE);
        assert_eq!(
            ComponentValue::Opaque(OpaqueComponent::new(42, vec![1, 2, 3]).unwrap()).type_id(),
            42
        );
    }

    #[test]
    fn dangerous_and_range_classification() {
        // Every arbitrary-NBT / nested-item component is stripped on input.
        for tid in [
            CHARGED_PROJECTILES,
            BUNDLE_CONTENTS,
            ENTITY_DATA,
            BUCKET_ENTITY_DATA,
            BLOCK_ENTITY_DATA,
            CONTAINER,
            CONTAINER_LOOT,
        ] {
            assert!(
                ComponentTypeId::new(tid).is_dangerous(),
                "type {tid} must strip"
            );
        }
        // Plain typed/value components are not stripped.
        assert!(!ComponentTypeId::new(CUSTOM_DATA).is_dangerous());
        assert!(!ComponentTypeId::new(MAX_STACK_SIZE).is_dangerous());
        assert!(!ComponentTypeId::new(ITEM_NAME).is_dangerous());
        assert!(ComponentTypeId::new(0).is_in_range());
        assert!(ComponentTypeId::new(MAX_COMPONENT_TYPE_ID).is_in_range());
        assert!(!ComponentTypeId::new(96).is_in_range());
        assert!(!ComponentTypeId::new(-1).is_in_range());
    }

    #[test]
    fn patch_len_and_empty() {
        let patch = ComponentPatch::new(
            vec![ComponentValue::Unbreakable],
            vec![ComponentTypeId::new(DAMAGE)],
        );
        assert_eq!(patch.len(), 2);
        assert!(!patch.is_empty());
        assert!(ComponentPatch::empty().is_empty());
    }
}
