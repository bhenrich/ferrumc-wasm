//! The read-only [`ServerSnapshot`] surface and its lock-light [`SnapshotPublisher`].
//!
//! A [`ServerSnapshot`] is a single, owned, bounded, serializable picture of the
//! whole server at one tick. It is built once per tick by the application driver
//! from this crate's [`CounterRegistry`] (the metric-derived fields) folded with
//! the app-supplied [`ServerSnapshotParts`] (world/runtime context the registry
//! cannot see), then handed to readers through a [`SnapshotPublisher`].
//!
//! The publisher is an `Arc<RwLock<Arc<ServerSnapshot>>>`: [`publish`] takes the
//! write lock only long enough to swap an `Arc` pointer (nanoseconds, never held
//! across a tick) and [`latest`] takes the read lock only long enough to clone the
//! `Arc`. Both recover a poisoned lock via [`PoisonError::into_inner`] so a
//! panicked writer can never wedge the readers, mirroring the metric tables'
//! poison-safe locking. This keeps the crate a leaf: it uses only `std` sync
//! primitives and `serde`, with no async runtime and no extra dependencies.
//!
//! [`publish`]: SnapshotPublisher::publish
//! [`latest`]: SnapshotPublisher::latest

use std::sync::{Arc, PoisonError, RwLock};

use serde::{Deserialize, Serialize};

use crate::metrics::CounterRegistry;
use crate::trace::PacketState;

/// A comprehensive, bounded, read-only snapshot of the server's current state.
///
/// Built once per tick (after the metrics are recorded) without holding any lock
/// across the shard, and serializable to JSON. The dashboard consumes this
/// without importing any simulation or application internals.
///
/// Every field is an inert value: the struct carries no behaviour and no
/// invariants, so its fields are public by design (it is a serialization DTO, the
/// same precedent as [`MetricsSnapshot`](crate::MetricsSnapshot)).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerSnapshot {
    /// Human-readable build string (for example `"ferrumc 0.2.0-dev"`).
    pub build: String,
    /// Server start time as a Unix timestamp in seconds.
    pub started_at: u64,
    /// Current uptime in seconds.
    pub uptime_secs: u64,
    /// Current simulation tick number.
    pub tick: u64,

    // --- Tick performance ---
    /// Effective ticks per second over the last wall-clock second (`20.0` is
    /// healthy at the default rate).
    pub tps: f64,
    /// 50th-percentile tick duration in milliseconds over the sliding window.
    pub tick_p50_ms: f64,
    /// 95th-percentile tick duration in milliseconds over the sliding window.
    pub tick_p95_ms: f64,
    /// 99th-percentile tick duration in milliseconds over the sliding window.
    pub tick_p99_ms: f64,

    // --- Players ---
    /// Number of players currently connected.
    pub players_online: usize,
    /// Per-player snapshot list.
    pub players: Vec<PlayerSnapshot>,

    // --- World ---
    /// Resident (loaded) chunk count.
    pub chunks_loaded: usize,
    /// Chunks marked network-dirty (needs client sync). Not yet surfaced by the
    /// chunk map, so currently reported as `0`.
    pub chunks_dirty: usize,
    /// Chunks marked persist-dirty (gameplay-edited, needs storage flush).
    /// Currently an approximation derived from the chunk map's persist-dirty flag.
    pub chunks_persist_dirty: usize,
    /// Cumulative `ferrumc_chunk_sent_total`.
    pub chunk_sent_total: u64,
    /// Cumulative `ferrumc_chunk_unloaded_total`.
    pub chunk_unloaded_total: u64,

    // --- Network ---
    /// Largest outbound queue depth observed across all sessions.
    pub network_outbound_queue_len_max: u64,
    /// Per-player network metrics. Populated by the network lane; empty here.
    pub network_per_player: Vec<NetworkMetricsSnapshot>,

    // --- Storage ---
    /// Most recent storage flush latency in milliseconds.
    pub storage_flush_ms_last: u64,
    /// Mean storage flush latency in milliseconds.
    pub storage_flush_ms_avg: f64,

    // --- Block mutations ---
    /// Block-break accept/reject counts.
    pub block_breaks: MutationCountSnapshot,
    /// Block-place accept/reject counts.
    pub block_places: MutationCountSnapshot,

    // --- Packet decode errors ---
    /// Recent decode errors keyed by `(state, packet)`.
    pub decode_errors_recent: Vec<DecodeErrorSnapshot>,
    /// Count of distinct decode-error keys that did not fit the fixed table.
    pub decode_errors_overflow: u64,

    // --- Plugins ---
    /// Per-plugin decision counts. Populated by the plugin lane; empty here.
    pub plugin_decisions: Vec<PluginDecisionSnapshot>,

    // --- Packet trace summaries ---
    /// Top inbound packets by frequency. Populated by the network lane; empty here.
    pub inbound_trace_summary: PacketTraceSummary,
    /// Top outbound packets by frequency. Populated by the network lane; empty here.
    pub outbound_trace_summary: PacketTraceSummary,
}

/// One connected player's state snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    /// The player's UUID as a `u128` (the canonical 128-bit identity).
    pub player_id: u128,
    /// The player's display name.
    pub name: String,
    /// The player's current position.
    pub position: Vec3Snapshot,
    /// The chunk column the player currently occupies.
    pub chunk: ChunkPosSnapshot,
    /// The player's game mode label (`"survival"`/`"creative"`/`"adventure"`/`"spectator"`).
    pub gamemode: String,
    /// The player's outbound queue depth. Fed by the network lane; `0` here.
    pub outbound_queue_len: usize,
    /// Bytes received from the player. Fed by the network lane; `0` here.
    pub network_in_bytes: u64,
    /// Bytes sent to the player. Fed by the network lane; `0` here.
    pub network_out_bytes: u64,
    /// Frames decoded from the player. Fed by the network lane; `0` here.
    pub frames_decoded: u64,
    /// Frames encoded to the player. Fed by the network lane; `0` here.
    pub frames_encoded: u64,
    /// Total packets dropped for the player. Fed by the network lane; `0` here.
    pub packets_dropped_total: u64,
}

/// Per-player network metrics summary, fed by the network lane.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkMetricsSnapshot {
    /// The player's display name.
    pub player_name: String,
    /// Frames decoded from the player.
    pub frames_in: u64,
    /// Bytes received from the player.
    pub bytes_in: u64,
    /// Frames encoded to the player.
    pub frames_out: u64,
    /// Bytes sent to the player.
    pub bytes_out: u64,
    /// Frames that exceeded the per-connection budget.
    pub over_budget: u64,
    /// Packets dropped per outbound priority.
    pub dropped: [u64; 4],
}

/// Per-plugin event decision counts, fed by the plugin lane.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginDecisionSnapshot {
    /// The plugin's name.
    pub plugin_name: String,
    /// The plugin's folded decision counts.
    pub decisions: PluginDecisions,
}

/// Allow/Deny/Replace/Panic counts for one plugin.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PluginDecisions {
    /// Events the plugin allowed.
    pub allow: u64,
    /// Events the plugin vetoed.
    pub deny: u64,
    /// Events the plugin replaced.
    pub replace: u64,
    /// Plugin panics recovered from.
    pub panic: u64,
}

/// Accepted/rejected counts for one block-mutation kind.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct MutationCountSnapshot {
    /// Edits the simulation applied.
    pub accepted: u64,
    /// Edits that were vetoed.
    pub rejected: u64,
}

/// One decode-error row.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecodeErrorSnapshot {
    /// The connection state the failure occurred in.
    pub state: String,
    /// The packet label.
    pub packet: String,
    /// How many times this `(state, packet)` failed to decode.
    pub count: u64,
}

/// A packet-trace frequency summary (top packets by count), fed by the network lane.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PacketTraceSummary {
    /// The most frequent packets in the trace window.
    pub top_packets: Vec<PacketFrequency>,
}

/// One `(packet, state, count)` frequency row.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PacketFrequency {
    /// The packet label.
    pub packet_name: String,
    /// The connection state the packet was seen in.
    pub state: String,
    /// How many times the packet appeared in the trace window.
    pub count: usize,
}

/// A position snapshot, decoupled from `ferrumc-math` so this stays a leaf crate.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Vec3Snapshot {
    /// The x component.
    pub x: f64,
    /// The y component.
    pub y: f64,
    /// The z component.
    pub z: f64,
}

/// A chunk coordinate snapshot.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ChunkPosSnapshot {
    /// The chunk x coordinate.
    pub x: i32,
    /// The chunk z coordinate.
    pub z: i32,
}

/// The application-supplied half of a [`ServerSnapshot`].
///
/// The driver fills this with world and runtime context the metric registry
/// cannot see (build string, uptime, tick, effective TPS, the player roster, and
/// chunk residence), then calls [`CounterRegistry::server_snapshot`] to fold in
/// the metric-derived fields. Fields are public because this is a plain transfer
/// struct handed straight into the fold.
#[derive(Debug, Clone, Default)]
pub struct ServerSnapshotParts {
    /// Human-readable build string.
    pub build: String,
    /// Server start time as a Unix timestamp in seconds.
    pub started_at: u64,
    /// Current uptime in seconds.
    pub uptime_secs: u64,
    /// Current simulation tick number.
    pub tick: u64,
    /// Effective ticks per second over the last wall-clock second.
    pub tps: f64,
    /// Number of players currently connected.
    pub players_online: usize,
    /// Per-player snapshot list.
    pub players: Vec<PlayerSnapshot>,
    /// Resident (loaded) chunk count.
    pub chunks_loaded: usize,
    /// Chunks marked network-dirty.
    pub chunks_dirty: usize,
    /// Chunks marked persist-dirty.
    pub chunks_persist_dirty: usize,
    /// Per-plugin decision counts (empty until the plugin lane feeds them).
    pub plugin_decisions: Vec<PluginDecisionSnapshot>,
    /// Per-player network metrics (empty until the network lane feeds them).
    pub network_per_player: Vec<NetworkMetricsSnapshot>,
    /// Inbound packet-trace summary (empty until the network lane feeds it).
    pub inbound_trace_summary: PacketTraceSummary,
    /// Outbound packet-trace summary (empty until the network lane feeds it).
    pub outbound_trace_summary: PacketTraceSummary,
}

/// Maps the observability-local [`PacketState`] onto its lowercase label.
pub(crate) fn packet_state_label(state: PacketState) -> String {
    match state {
        PacketState::Handshaking => "handshaking",
        PacketState::Status => "status",
        PacketState::Login => "login",
        PacketState::Configuration => "configuration",
        PacketState::Play => "play",
    }
    .to_string()
}

impl CounterRegistry {
    /// Folds the metric-derived fields of a [`ServerSnapshot`] (block mutations,
    /// storage flush last/avg, outbound-queue max, chunk totals, decode errors, and
    /// the tick-duration percentiles) with the app-supplied `parts`.
    ///
    /// Reads every counter through the existing on-demand [`snapshot`] plus the
    /// percentile window, so it never touches a hot-path atomic directly and never
    /// holds a lock across the caller's tick.
    ///
    /// [`snapshot`]: CounterRegistry::snapshot
    #[must_use]
    pub fn server_snapshot(&self, parts: ServerSnapshotParts) -> ServerSnapshot {
        let metrics = self.snapshot();
        let (p50, p95, p99) = self.tick_percentiles_ms();

        let decode_errors_recent = metrics
            .packet_decode_error_total
            .entries
            .iter()
            .map(|entry| DecodeErrorSnapshot {
                state: packet_state_label(entry.state),
                packet: entry.packet.to_string(),
                count: entry.count,
            })
            .collect();

        ServerSnapshot {
            build: parts.build,
            started_at: parts.started_at,
            uptime_secs: parts.uptime_secs,
            tick: parts.tick,
            tps: parts.tps,
            tick_p50_ms: p50,
            tick_p95_ms: p95,
            tick_p99_ms: p99,
            players_online: parts.players_online,
            players: parts.players,
            chunks_loaded: parts.chunks_loaded,
            chunks_dirty: parts.chunks_dirty,
            chunks_persist_dirty: parts.chunks_persist_dirty,
            chunk_sent_total: metrics.chunk_sent_total,
            chunk_unloaded_total: metrics.chunk_unloaded_total,
            network_outbound_queue_len_max: metrics.session_outbound_queue_len.max,
            network_per_player: parts.network_per_player,
            storage_flush_ms_last: metrics.storage_flush_ms.last_ms,
            storage_flush_ms_avg: metrics.storage_flush_ms.avg_ms,
            block_breaks: MutationCountSnapshot {
                accepted: metrics.block_mutation_total.break_kind.accepted,
                rejected: metrics.block_mutation_total.break_kind.rejected,
            },
            block_places: MutationCountSnapshot {
                accepted: metrics.block_mutation_total.place.accepted,
                rejected: metrics.block_mutation_total.place.rejected,
            },
            decode_errors_recent,
            decode_errors_overflow: metrics.packet_decode_error_total.overflow,
            plugin_decisions: parts.plugin_decisions,
            inbound_trace_summary: parts.inbound_trace_summary,
            outbound_trace_summary: parts.outbound_trace_summary,
        }
    }
}

/// A lock-light shared holder for the latest [`ServerSnapshot`].
///
/// The application driver publishes a fresh snapshot once per tick; readers (the
/// dashboard) clone the current `Arc` out cheaply. Cloning the publisher shares
/// the same underlying cell, so every clone observes the same latest snapshot.
#[derive(Debug, Clone)]
pub struct SnapshotPublisher {
    /// The shared cell: the outer `Arc` is shared by every clone of the publisher;
    /// the inner `Arc` is the swappable current snapshot.
    inner: Arc<RwLock<Arc<ServerSnapshot>>>,
}

impl SnapshotPublisher {
    /// Creates a publisher seeded with `initial`.
    #[must_use]
    pub fn new(initial: ServerSnapshot) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(initial))),
        }
    }

    /// Replaces the current snapshot with `snapshot`.
    ///
    /// Takes the write lock only to swap an `Arc` pointer, so it is effectively
    /// instantaneous and never held across a tick. Poison-safe.
    pub fn publish(&self, snapshot: ServerSnapshot) {
        let mut guard = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        *guard = Arc::new(snapshot);
    }

    /// Returns the current snapshot, cloning the `Arc` (cheap, not the struct).
    ///
    /// Takes the read lock only to clone the pointer. Poison-safe.
    #[must_use]
    pub fn latest(&self) -> Arc<ServerSnapshot> {
        let guard = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(&guard)
    }
}

impl Default for SnapshotPublisher {
    fn default() -> Self {
        Self::new(ServerSnapshot::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MutationKind, MutationResult, TickMetrics};
    use ferrumc_core::Tick;

    fn sample_parts() -> ServerSnapshotParts {
        ServerSnapshotParts {
            build: "ferrumc test".to_string(),
            started_at: 1_000,
            uptime_secs: 42,
            tick: 7,
            tps: 20.0,
            players_online: 1,
            players: vec![PlayerSnapshot {
                player_id: 0x1234,
                name: "Steve".to_string(),
                position: Vec3Snapshot {
                    x: 8.0,
                    y: 64.0,
                    z: 8.0,
                },
                chunk: ChunkPosSnapshot { x: 0, z: 0 },
                gamemode: "creative".to_string(),
                ..PlayerSnapshot::default()
            }],
            chunks_loaded: 25,
            chunks_dirty: 0,
            chunks_persist_dirty: 1,
            ..ServerSnapshotParts::default()
        }
    }

    #[test]
    fn default_snapshot_is_empty_and_bounded() {
        let snap = ServerSnapshot::default();
        assert!(snap.players.is_empty());
        assert!(snap.decode_errors_recent.is_empty());
        assert_eq!(snap.tick, 0);
        assert_eq!(snap.players_online, 0);
    }

    #[test]
    fn server_snapshot_folds_registry_and_parts() {
        let reg = CounterRegistry::new();
        reg.incr_chunk_sent(3);
        reg.record_block_mutation(MutationKind::Break, MutationResult::Accepted);
        reg.record_block_mutation(MutationKind::Place, MutationResult::Rejected);
        reg.record_storage_flush_ms(8);
        reg.observe_outbound_queue_len(11);

        let snap = reg.server_snapshot(sample_parts());

        // App-supplied context survives the fold.
        assert_eq!(snap.build, "ferrumc test");
        assert_eq!(snap.tick, 7);
        assert_eq!(snap.players_online, 1);
        assert_eq!(snap.players[0].name, "Steve");
        assert_eq!(snap.chunks_loaded, 25);
        assert_eq!(snap.chunks_persist_dirty, 1);

        // Registry-derived fields are folded in.
        assert_eq!(snap.chunk_sent_total, 3);
        assert_eq!(snap.block_breaks.accepted, 1);
        assert_eq!(snap.block_places.rejected, 1);
        assert_eq!(snap.storage_flush_ms_last, 8);
        assert_eq!(snap.network_outbound_queue_len_max, 11);
    }

    #[test]
    fn percentiles_reflect_recorded_tick_durations() {
        let reg = CounterRegistry::new();
        // Durations in microseconds: 1ms..=10ms.
        for ms in 1..=10u64 {
            reg.record_tick(&TickMetrics {
                shard_x: 0,
                shard_z: 0,
                tick: Tick::new(ms),
                duration_us: ms * 1_000,
                inputs_drained: 0,
                outputs_emitted: 0,
                players: 0,
                inbox_len: 0,
            });
        }
        let snap = reg.server_snapshot(ServerSnapshotParts::default());
        // p50 sits in the lower half, p99 at the top of the 1..=10ms window.
        assert!(snap.tick_p50_ms >= 4.0 && snap.tick_p50_ms <= 6.0);
        assert!((snap.tick_p99_ms - 10.0).abs() < f64::EPSILON);
        assert!(snap.tick_p95_ms <= snap.tick_p99_ms);
    }

    #[test]
    fn publisher_round_trips_the_latest_snapshot() {
        let publisher = SnapshotPublisher::default();
        assert_eq!(publisher.latest().tick, 0);

        let reg = CounterRegistry::new();
        let snap = reg.server_snapshot(sample_parts());
        publisher.publish(snap);

        let latest = publisher.latest();
        assert_eq!(latest.tick, 7);
        assert_eq!(latest.players[0].name, "Steve");

        // Clones share the same underlying cell.
        let reader = publisher.clone();
        publisher.publish(ServerSnapshot {
            tick: 99,
            ..ServerSnapshot::default()
        });
        assert_eq!(reader.latest().tick, 99);
    }

    #[test]
    fn snapshot_json_round_trips() {
        let reg = CounterRegistry::new();
        let snap = reg.server_snapshot(sample_parts());
        let json = serde_json::to_string(&snap).expect("serialize");
        let back: ServerSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.build, snap.build);
        assert_eq!(back.players.len(), snap.players.len());
        assert_eq!(back.players[0].player_id, 0x1234);
    }
}
