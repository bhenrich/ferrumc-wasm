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
//! - **Time budgeting.** Successful compiled enable/event/decision hooks,
//!   successful trusted-native initialization, and returning trusted-native
//!   event/decision calls are measured against a [`CallBudget`]. Dispatch
//!   overruns can optionally disable later calls. The budget cannot preempt a
//!   call that has not returned.
//!
//! Storage is partitioned per plugin by a [`PluginStorageBackend`]; the host
//! hands each plugin a view bound to its own namespace, so namespaces are
//! separated by construction.
//!
//! ## Dynamic plugin loading
//!
//! Beyond compiled-in plugins, [`PluginLoader`] loads plugins from `cdylib`
//! files in a directory across the narrow C ABI defined in
//! [`ferrumc_plugin_api::abi`] (see ADR-0006). The operator-trusted entrypoint
//! returns a raw pointer; the loader rejects null, constructs the reference
//! promised by that ABI, and checks its reported version before using the
//! remaining fields. Metadata is then copied across the boundary and the result
//! wrapped in an adapter that implements
//! [`Plugin`](ferrumc_plugin_api::Plugin) for legacy app compatibility.
//! Registration and lifecycle use the ordinary host API: enable calls are
//! timed, while shutdown calls are not. A process-aborting library failure
//! cannot be recovered by that adapter. Per-entry read, load, and registration
//! failures that return are reported as classified [`LoadError`]s, and later
//! entries are attempted; an unreadable directory is returned as the outer
//! error. All compatibility FFI lives in one audited `#[allow(unsafe_code)]`
//! module; see
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
//! Native event callbacks retain bounded intents in a callback-local stage.
//! Block-decision callbacks may retain the full intent bound plus exactly one
//! required decision. A non-success callback status, boundary error, or
//! capability denial discards that stage before the caller-owned
//! [`CommandSink`](ferrumc_plugin_api::CommandSink) is touched. Every service
//! error on a decision stage, and a decision command routed to another stage,
//! also discards it. Invalid event-resource provenance and an unavailable
//! dimension facade are stage-poisoning on every route. Other validation or
//! capacity errors on notification and initialization stages reject only that
//! operation, so earlier intents may remain staged if the callback returns
//! success. Successful intents are submitted to the sink in order; if a later
//! submission is rejected, an earlier accepted intent can remain. A native
//! decision participates in the same registration-order fold as compiled-in
//! decisions. The context-free [`PluginHost::dispatch_event`] deliberately
//! does not fabricate native ABI metadata; context-free block decision dispatch
//! likewise fails closed when an enabled trusted-native veto plugin cannot be
//! called.
//!
//! The host does not catch panics inside trusted native code. When cooperating
//! SDK or plugin code returns normally from an event or block-decision callback
//! with [`FC_PLUGIN_PANIC`](ferrumc_plugin_abi::FC_PLUGIN_PANIC), the host
//! discards that callback's staged commands, disables the plugin, and produces
//! a [`NativePanicRecord`].
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
