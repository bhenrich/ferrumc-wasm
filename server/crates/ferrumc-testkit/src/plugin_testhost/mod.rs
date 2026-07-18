//! Deterministic shared-SDK plugin replay through both packaging modes.
//!
//! [`PluginTestHost`] runs the same author-facing plugin contract either
//! through a compiled-in [`BuiltinPluginFactory`][ferrumc_plugin_sdk_builtin::BuiltinPluginFactory]
//! or through an actual ABI-system `cdylib`. Both paths execute load, the
//! caller's ordered synthetic events, and unload. The dynamic library follows
//! the ABI-system resident-library policy; this harness does not pretend that
//! native code can be hot-unloaded.
//!
//! # Replay contract
//!
//! Effective capabilities are the plugin's requested manifest intersected with
//! the host's offered grants. Every lifecycle or event call receives a fresh
//! bounded effect stage. A successful call commits that stage atomically, while
//! a failed call discards all of its mutations and any decision. Diagnostics
//! are the deliberate exception: bounded diagnostics are retained separately,
//! including diagnostics emitted before a callback fails. Reads within a call
//! observe only state committed by earlier calls, never effects already staged
//! by the current call. If a live instance fails, the runner consumes it
//! through best-effort shutdown and discards cleanup effects.
//!
//! Event logs contain at most [`MAX_SCHEDULED_EVENTS`] entries and ticks must be
//! nondecreasing. Each callback admits at most the caller-selected capacity,
//! capped by [`MAX_CALLBACK_EFFECTS`]. Diagnostics, storage fields, storage key
//! projections, command trees, and ABI payloads retain the shared SDK's
//! explicit limits. The runner uses no clock, sleep, random source, socket, or
//! asynchronous task, so identical seeds and events have identical semantics.
//! Passive notification events must have been subscribed during load.
//!
//! # Canonical result
//!
//! [`SemanticDigest`] is SHA-256 over a domain-separated semantic-v1 encoding
//! of committed effects followed by final state. The encoding uses explicit
//! discriminants, lengths, little-endian integers, UUID bytes, and normalized
//! floating-point bits (`-0.0` and `0.0` encode identically). Map/set-backed
//! state is sorted; effects, registered commands, and messages retain commit
//! order. Diagnostics, library paths, opaque resource handles, raw ABI
//! envelopes, package metadata, and target-specific artifact details are
//! excluded. Unsupported future SDK values fail classification instead of
//! receiving an unstable debug-string encoding.
//!
//! # Dynamic ABI details
//!
//! Dynamic events carry a nonzero shard handle, while world calls use a
//! distinct nonzero dimension handle; neither enters the semantic result. A
//! loaded chunk with no explicitly seeded block entry reads as block-state
//! zero in both packaging modes, while an unloaded chunk has no block state.
//!
//! The current Packet 55 bridge obtains the dimension handle through a
//! `ReadWorld`-gated request before emitting `set_block`. Consequently a
//! dynamic plugin exercising that operation currently needs both `ReadWorld`
//! and `SubmitIntents`, even though the built-in operation itself needs only
//! `SubmitIntents`. Parity fixtures that set blocks must request and receive
//! both until the bridge contract is revised.
//!
//! The integration fixture is a nested test-only crate built with Cargo's
//! locked, offline mode into a repository-local artifact directory. That build
//! is regression machinery, not part of replay semantics or digest input.

mod backend;
mod codec;
mod error;
mod runner;
mod state;

pub use error::{PluginCallbackPhase, PluginFailureKind, PluginReplayFailure, PluginTestHostError};
pub use runner::{
    PluginTestHost, ScheduledPluginEvent, MAX_CALLBACK_EFFECTS, MAX_SCHEDULED_EVENTS,
};
pub use state::{
    PermissionSetting, PluginDiagnostic, PluginDiagnosticPhase, PluginEffect, PluginRun,
    PluginStateSnapshot, ScheduledTimer, SemanticDigest, StorageEntry,
};
