#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! See the crate README for the trusted/untrusted slot distinction and the
//! hostile-input normalization rules. The module layout:
//!
//! * [`item_id`] — the registry-validated [`ItemId`].
//! * [`component`] — the data-component model ([`ComponentValue`],
//!   [`ComponentPatch`], [`ComponentTypeId`]) and the component type-id constants.
//! * [`stack`] — the canonical [`ItemStack`] and its trusted slot encoder.
//! * [`untrusted`] — the [`UntrustedItemStack`] wire form, its decoder, and
//!   `into_validated`.
//! * [`wire`] — shared wire bounds and the `SetContainerContent` payload builder.

pub mod component;
pub mod item_id;
pub mod stack;
pub mod untrusted;
pub mod wire;

pub use component::{ComponentPatch, ComponentTypeId, ComponentValue, OpaqueComponent};
pub use item_id::ItemId;
pub use stack::ItemStack;
pub use untrusted::{ItemValidationError, UntrustedItemStack};
pub use wire::{
    encode_container_content_payload, MAX_COMPONENTS, MAX_COMPONENTS_TOTAL_BYTES,
    MAX_COMPONENT_BYTES, MAX_WINDOW_SLOTS,
};
