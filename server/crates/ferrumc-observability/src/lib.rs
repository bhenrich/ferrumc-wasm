#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! Logging, metrics, and tracing infrastructure.
//!
//! This is a low-coupling leaf crate: it depends only on `ferrumc-core` (for
//! [`ferrumc_core::Tick`]) plus `tracing`/`serde`/`serde_json`, and defines its
//! own [`Direction`]/[`PacketState`] enums so it never pulls in the networking or
//! protocol crates. The integrating crate (`ferrumc-app`) owns the glue that maps
//! its concrete packet/state types onto these.
//!
//! It provides three things:
//!
//! - A fixed-capacity [`RingBuffer`] (const-generic capacity, no `unsafe`, no
//!   heap growth) used to retain recent packet traces.
//! - Per-connection packet tracing ([`PacketTrace`], [`SessionDebug`]) that can
//!   be [`dumped`](SessionDebug::dump) on disconnect or a decode error.
//! - A [`CounterRegistry`] of atomic-backed counters/gauges named after the
//!   locked-in metric set, a per-tick [`TickMetrics`] record, a shared
//!   [`ServerClock`], and a [`MetricsSnapshot`] for a future exporter.
//! - A read-only [`ServerSnapshot`] (folded from the registry plus app-supplied
//!   [`ServerSnapshotParts`]) published lock-light through a [`SnapshotPublisher`]
//!   for the localhost dashboard to render.
//! - A dedicated-thread [`PluginWatchdog`] that detects slow or non-returning
//!   plugin callbacks and publishes bounded diagnostics. Detection is not
//!   preemption: the watchdog never cancels a callback or unloads native code.

mod metrics;
mod net_telemetry;
mod ring;
mod snapshot;
mod trace;
mod watchdog;

pub use metrics::{
    BlockMutationResults, BlockMutationTotals, CounterRegistry, DecodeErrorEntry,
    DecodeErrorTotals, MetricsSnapshot, MutationKind, MutationResult, QueueLenGauge, ServerClock,
    StorageFlushStats, TickMetrics, TickMsEntry,
};
pub use net_telemetry::{
    ConnNetTelemetry, NetTelemetryHub, NetTelemetryParts, PacketTally, PlayerNetCounters,
    DEFAULT_TOP_N, HUB_CAPACITY, TALLY_CAPACITY,
};
pub use ring::RingBuffer;
pub use snapshot::{
    ChunkPosSnapshot, DecodeErrorSnapshot, MutationCountSnapshot, NetworkMetricsSnapshot,
    PacketFrequency, PacketTraceSummary, PlayerSnapshot, PluginDecisionSnapshot, PluginDecisions,
    ServerSnapshot, ServerSnapshotParts, SnapshotPublisher, Vec3Snapshot,
};
pub use trace::{Direction, PacketState, PacketTrace, SessionDebug, SessionDebugSnapshot};
pub use watchdog::{
    PluginWatchdog, PluginWatchdogConfig, PluginWatchdogHandle, TracingWatchdogReporter,
    WatchdogActiveCall, WatchdogBeginError, WatchdogCallGuard, WatchdogCallId, WatchdogCallback,
    WatchdogConfigError, WatchdogCrashReport, WatchdogDiagnostic, WatchdogHardAction,
    WatchdogHealth, WatchdogLabelError, WatchdogReporter, WatchdogSnapshot, WatchdogStartError,
    WatchdogThreadDisposition, WatchdogThreshold, ACTIVE_CALLBACK_CAPACITY,
    DIAGNOSTIC_HISTORY_CAPACITY, RETIRED_THREAD_CAPACITY,
};
