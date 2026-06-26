#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! In-process plugin host: registry, lifecycle, event dispatch, and isolation.
//!
//! The host owns [`Plugin`](ferrumc_plugin_api::Plugin) trait objects and drives
//! them through their lifecycle. It enforces three properties the rest of the
//! server relies on:
//!
//! - **Panic isolation.** Every call into plugin code is wrapped in
//!   [`std::panic::catch_unwind`]; a panicking plugin is caught, disabled, and
//!   never called again, so it can never bring the host down. See
//!   [`PluginHost::dispatch_event`].
//! - **Capability gating.** Each plugin is granted a
//!   [`CapabilityManifest`](ferrumc_plugin_api::CapabilityManifest); the
//!   contexts handed to its hooks deny any facade it was not granted.
//! - **Time budgeting.** Each call is timed against a [`CallBudget`]; overruns
//!   are recorded (and optionally disable the plugin).
//!
//! Storage is partitioned per plugin by a [`PluginStorageBackend`]; the host
//! hands each plugin a view bound to its own namespace, so namespaces are
//! isolated by construction.
//!
//! ## A note on `catch_unwind` and unwind safety
//!
//! Catching panics is the one place this crate must reason about unwinding. The
//! plugin call closures capture `&mut` plugin state, which is not
//! [`std::panic::UnwindSafe`], so they are wrapped in
//! [`std::panic::AssertUnwindSafe`]. This is sound here — and requires no
//! `unsafe` — because a plugin that panics is immediately moved to the disabled
//! state and never called again. Its internal state may be left inconsistent by
//! the unwind, but since nothing ever observes that state afterward, the
//! "assertion" of unwind safety holds. Panic isolation depends on the standard
//! unwinding panic strategy; under `panic = "abort"` a panic aborts the process
//! before it can be caught.

mod budget;
mod error;
mod host;
mod state;
mod storage;

pub use budget::{BudgetOutcome, CallBudget};
pub use error::HostError;
pub use host::{DispatchReport, HostConfig, PluginHost};
pub use state::{DisableReason, PluginState, PluginStats};
pub use storage::{InMemoryPluginStorage, PluginStorageBackend};
