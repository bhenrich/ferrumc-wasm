#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Stable, in-process plugin-facing API. No raw internals are exposed.
//!
//! This crate defines the contract between the plugin host and a plugin. A
//! plugin implements [`Plugin`] and is driven through its lifecycle by the host;
//! everything it can touch is a capability-gated facade:
//!
//! - [`WorldView`] — read-only world access.
//! - [`CommandSink`] — submit mutation [`WorldIntent`]s, never direct mutation.
//! - [`PermissionApi`] — query permissions.
//! - [`PluginStorageApi`] — private, namespaced key-value storage.
//! - [`EventRegistrar`] / [`CommandRegistrar`] — subscribe to events and
//!   register commands during setup.
//!
//! The [`abi`] module additionally defines the narrow C ABI used to load
//! plugins from dynamic libraries (see ADR-0006). It is the *only* part of this
//! crate that is layout-stable across Rust versions.
//!
//! Access is mediated by a [`CapabilityManifest`] of [`Capability`] grants and
//! delivered through the [`SetupContext`], [`EventContext`], and
//! [`TeardownContext`] the host passes to lifecycle hooks. The world, sink,
//! permission, and storage traits are deliberately *shells* here: the
//! simulation and storage layers inject the concrete implementations, keeping
//! this crate free of any dependency on simulation or world internals.

pub mod abi;

mod capability;
mod command;
mod context;
mod error;
mod event;
mod metadata;
mod permission;
mod plugin;
mod sink;
mod storage;
mod world;

pub use capability::{Capability, CapabilityManifest};
pub use command::CommandRegistrar;
pub use context::{EventContext, SetupContext, TeardownContext};
pub use error::{CapabilityError, IntentError, PluginError, StorageError};
pub use event::{EventKind, EventRegistrar, PluginEvent};
pub use metadata::PluginMetadata;
pub use permission::PermissionApi;
pub use plugin::Plugin;
pub use sink::CommandSink;
// `WorldIntent` lives in `ferrumc-math` (the lowest crate that owns its
// coordinate fields and can see core's id/text types) so the simulation and
// session layers can route intents without depending on this crate. Re-exported
// here for backward compatibility: plugins keep importing it from the plugin API.
pub use ferrumc_math::WorldIntent;
pub use storage::{PluginStorageApi, MAX_KEY_LEN, MAX_VALUE_LEN};
pub use world::WorldView;

/// Re-export of [`semver::Version`], the type used for [`PluginMetadata`]
/// versions, so plugins need not depend on `semver` directly.
pub use semver::Version;
