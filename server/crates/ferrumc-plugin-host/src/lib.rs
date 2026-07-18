// This crate needs FFI (libloading + `extern "C"`) to load dynamic plugins, so
// it cannot `forbid` unsafe. It `deny`s it instead and scopes a single
// `#[allow(unsafe_code)]` to the `dynamic::ffi` module; see
// `docs/safety/ferrumc-plugin-host.md`.
#![deny(unsafe_code)]
#![warn(missing_docs)]

//! In-process plugin host: registry, lifecycle, event dispatch, and failure handling.
//!
//! The host owns compiled-in [`Plugin`](ferrumc_plugin_api::Plugin) trait
//! objects plus validated trusted native factories and drives both through
//! their lifecycle. It enforces three properties the rest of the server relies
//! on:
//!
//! - **Compiled-in catch-and-disable handling.** Calls through the Rust
//!   [`Plugin`](ferrumc_plugin_api::Plugin) trait are wrapped in
//!   [`std::panic::catch_unwind`]; an unwinding compiled-in plugin is disabled
//!   and no later plugin hook is called on that retained instance. See
//!   [`PluginHost::dispatch_event`].
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
//! that adapter. Per-entry read, load, and registration failures that return are
//! reported as classified [`LoadError`]s, and later entries are attempted; an
//! unreadable directory is returned as the outer error. All compatibility FFI
//! lives in one audited `#[allow(unsafe_code)]` module; see
//! `docs/safety/ferrumc-plugin-host.md`.
//!
//! ## Trusted-native ABI runtime
//!
//! The current trusted-native path accepts
//! [`LoadedPlugin`](ferrumc_plugin_loader::LoadedPlugin) through
//! [`PluginHost::register_trusted_native`]. It supports event subscription,
//! the `MESSAGE` and `TELEPORT` intent subset, and vetoable block edits through
//! [`ferrumc_plugin_api::Capability::ReceiveEvents`],
//! [`ferrumc_plugin_api::Capability::SubmitIntents`], and
//! [`ferrumc_plugin_api::Capability::VetoBlockEdits`], respectively. Callers
//! use [`PluginHost::dispatch_event_with_native_context`],
//! [`PluginHost::dispatch_block_place_decision_with_native_context`], or
//! [`PluginHost::dispatch_block_break_decision_with_native_context`] to provide
//! a [`NativeEventContext`]. That context can carry caller-attested simulation
//! metadata or the documented connection-side sentinels: tick zero and an
//! invalid shard resource handle.
//!
//! Native callbacks stage intents and exactly one block decision in a bounded
//! transaction. After callback success, intents are submitted to the same
//! caller-owned [`CommandSink`](ferrumc_plugin_api::CommandSink) used by
//! compiled-in plugins, and the native decision participates in the same
//! registration-order fold. The context-free [`PluginHost::dispatch_event`]
//! deliberately does not fabricate native ABI metadata; context-free block
//! decision dispatch likewise fails closed when an enabled trusted-native veto
//! plugin cannot be called.
//!
//! The host does not catch panics inside trusted native code. When cooperating
//! SDK or plugin code returns normally with
//! [`FC_PLUGIN_PANIC`](ferrumc_plugin_abi::FC_PLUGIN_PANIC), the host discards
//! that callback's staged commands, disables the plugin, and produces a
//! [`NativePanicRecord`].
//! This fail-stop path cannot recover from `panic=abort`,
//! `std::process::abort`, segmentation faults, undefined behavior, deadlocks,
//! foreign exceptions, or malicious memory corruption; those failures may
//! hang, corrupt, or terminate the process before a status returns.
//!
//! ## A note on `catch_unwind` and unwind safety
//!
//! Catching panics is the one place this crate must reason about unwinding. The
//! compiled-in plugin call closures capture `&mut` plugin state, which is not
//! [`std::panic::UnwindSafe`], so they are wrapped in
//! [`std::panic::AssertUnwindSafe`]. The assertion is limited to letting the
//! host catch the unwind and terminally prevent later
//! [`Plugin`](ferrumc_plugin_api::Plugin) hooks on that retained instance; it
//! requires no `unsafe`. It is not transactional: storage writes, submitted
//! intents, registered command handlers, or shared-state mutations completed
//! before the unwind can remain observable. The boxed value is also dropped
//! normally with the host. Catch-and-disable handling depends on the standard
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
    NativeCapabilityDenial, NativeEventContext, NativePanicRecord, PluginDecisionReport,
    PluginHost, ResolvedBlockDecision, ResolvedDecision, ResolvedEventOutcome,
};
pub use state::{DisableReason, PluginState, PluginStats};
pub use storage::{InMemoryPluginStorage, PluginStorageBackend};
