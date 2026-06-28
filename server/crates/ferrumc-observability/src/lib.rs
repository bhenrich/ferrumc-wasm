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

mod metrics;
mod ring;
mod snapshot;
mod trace;

pub use metrics::{
    BlockMutationResults, BlockMutationTotals, CounterRegistry, DecodeErrorEntry,
    DecodeErrorTotals, MetricsSnapshot, MutationKind, MutationResult, QueueLenGauge, ServerClock,
    StorageFlushStats, TickMetrics, TickMsEntry,
};
pub use ring::RingBuffer;
pub use snapshot::{
    ChunkPosSnapshot, DecodeErrorSnapshot, MutationCountSnapshot, NetworkMetricsSnapshot,
    PacketFrequency, PacketTraceSummary, PlayerSnapshot, PluginDecisionSnapshot, PluginDecisions,
    ServerSnapshot, ServerSnapshotParts, SnapshotPublisher, Vec3Snapshot,
};
pub use trace::{Direction, PacketState, PacketTrace, SessionDebug, SessionDebugSnapshot};
