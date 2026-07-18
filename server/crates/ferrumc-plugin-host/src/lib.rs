// This crate needs FFI (libloading + `extern "C"`) to load dynamic plugins, so
// it cannot `forbid` unsafe. It `deny`s it instead and scopes a single
// `#[allow(unsafe_code)]` to the `dynamic::ffi` module; see
// `docs/safety/ferrumc-plugin-host.md`.
#![deny(unsafe_code)]
#![warn(missing_docs)]

//! In-process plugin host: registry, lifecycle, event dispatch, and containment.
//!
//! The host owns compiled-in [`Plugin`](ferrumc_plugin_api::Plugin) trait
//! objects plus validated trusted native factories and drives both through
//! their lifecycle. It enforces three properties the rest of the server relies
//! on:
//!
//! - **Compiled-in panic containment.** Calls through the Rust
//!   [`Plugin`](ferrumc_plugin_api::Plugin) trait are wrapped in
//!   [`std::panic::catch_unwind`]; an unwinding compiled-in plugin is disabled
//!   and not called again. See [`PluginHost::dispatch_event`].
//! - **Capability gating.** Each plugin is granted a
//!   [`CapabilityManifest`](ferrumc_plugin_api::CapabilityManifest); the
//!   contexts handed to its hooks deny any facade it was not granted.
//! - **Time budgeting.** Each call is timed against a [`CallBudget`]; overruns
//!   are recorded (and optionally disable the plugin).
//!
//! Storage is partitioned per plugin by a [`PluginStorageBackend`]; the host
//! hands each plugin a view bound to its own namespace, so namespaces are
//! separated by construction.
//!
//! ## Dynamic plugin loading
//!
//! Beyond compiled-in plugins, [`PluginLoader`] loads plugins from `cdylib`
//! files in a directory across the narrow C ABI defined in
//! [`ferrumc_plugin_api::abi`] (see ADR-0006). Each library is opened, its ABI
//! version checked, its metadata read across the boundary, and the result
//! wrapped in an adapter that implements [`Plugin`](ferrumc_plugin_api::Plugin)
//! for legacy app compatibility. Calls that return enter the host's ordinary
//! budget accounting; a process-aborting library failure cannot be recovered by
//! that adapter. Scan failures are reported as classified [`LoadError`]s and do
//! not stop later directory entries. All compatibility FFI lives in one audited
//! `#[allow(unsafe_code)]` module; see
//! `docs/safety/ferrumc-plugin-host.md`.
//!
//! The current trusted-native path accepts
//! [`LoadedPlugin`](ferrumc_plugin_loader::LoadedPlugin) through
//! [`PluginHost::register_trusted_native`]. Callers use
//! [`PluginHost::dispatch_event_with_native_context`] to provide tick/shard
//! metadata. Native callbacks can subscribe to events and stage the `MESSAGE`
//! and `TELEPORT` intent subset in a bounded transaction; after callback
//! success, those effects are submitted to the same caller-owned
//! [`CommandSink`](ferrumc_plugin_api::CommandSink) used by compiled-in
//! plugins. The context-free [`PluginHost::dispatch_event`] deliberately does
//! not fabricate native ABI metadata.
//!
//! ## A note on `catch_unwind` and unwind safety
//!
//! Catching panics is the one place this crate must reason about unwinding. The
//! compiled-in plugin call closures capture `&mut` plugin state, which is not
//! [`std::panic::UnwindSafe`], so they are wrapped in
//! [`std::panic::AssertUnwindSafe`]. This is sound here — and requires no
//! `unsafe` — because a compiled-in plugin that unwinds is immediately moved to
//! the disabled state and never called again. Its internal state may be left
//! inconsistent by the unwind, but since nothing ever observes that state afterward, the
//! "assertion" of unwind safety holds. Panic containment depends on the standard
//! unwinding panic strategy; under `panic = "abort"` a panic aborts the process
//! before it can be caught.

mod budget;
mod dynamic;
mod error;
mod host;
mod native_runtime;
mod state;
mod storage;

pub use budget::{BudgetOutcome, CallBudget};
pub use dynamic::{DirLoadReport, LoadError, PluginLoader};
pub use error::{HostError, NativeLifecycleHook};
pub use host::{
    DispatchReport, HostConfig, NativeCallbackFailure, NativeCallbackFailureRecord,
    NativeCapabilityDenial, NativeEventContext, PluginDecisionReport, PluginHost,
    ResolvedBlockDecision, ResolvedDecision, ResolvedEventOutcome,
};
pub use state::{DisableReason, PluginState, PluginStats};
pub use storage::{InMemoryPluginStorage, PluginStorageBackend};
