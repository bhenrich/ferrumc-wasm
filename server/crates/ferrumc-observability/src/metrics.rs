//! The metric sink: atomic-backed counters and gauges named after the locked-in
//! metric set, plus the per-tick record, the shared server clock, and the JSON
//! snapshot a future exporter can scrape.
//!
//! The metric names are fixed now so they survive into a real exporter:
//!
//! - `ferrumc_tick_ms{shard}`
//! - `ferrumc_session_outbound_queue_len{session}`
//! - `ferrumc_chunk_sent_total`
//! - `ferrumc_chunk_unloaded_total`
//! - `ferrumc_block_mutation_total{kind,result}`
//! - `ferrumc_storage_flush_ms`
//! - `ferrumc_packet_decode_error_total{state,packet}`
//! - `ferrumc_plugin_metrics` (bounded per-plugin JSON snapshot group)
//!
//! Cardinality-free counters are plain [`AtomicU64`]s touched lock-free on the
//! hot path. The *labelled* tables (`tick_ms{shard}`,
//! `packet_decode_error_total{state,packet}`, and the per-plugin group) are
//! fixed-capacity and sit behind small per-registry [`Mutex`]es that are only
//! taken on cold or low-frequency paths (a tick is recorded by the single driver
//! task ~20x/s; decode errors and plugin completions are comparatively rare;
//! snapshots are on demand) — never on the per-packet / per-chunk hot path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use ferrumc_core::Tick;
use serde::Serialize;

use crate::plugin_metrics::{
    PluginInvocationObservation, PluginMetricRecordOutcome, PluginMetricRegistry,
    PluginMetricsSnapshot,
};
use crate::trace::PacketState;
use crate::watchdog::WatchdogSnapshot;

/// Number of distinct shards the `ferrumc_tick_ms{shard}` table can hold before
/// further shards fold into the overflow counter. One shard runs today; 16 keeps
/// headroom while staying a tiny fixed array.
const TICK_TABLE_CAPACITY: usize = 16;

/// Number of distinct `(state, packet)` keys the decode-error table can hold
/// before further keys fold into the overflow bucket. Sized for the realistic
/// (5 states x modelled packets) space with headroom.
const DECODE_ERROR_TABLE_CAPACITY: usize = 64;

/// Microseconds per millisecond, for the `*_ms` snapshot conversions.
const US_PER_MS: f64 = 1_000.0;

/// Capacity of the sliding tick-duration window used for percentile reporting.
/// 600 samples is 30 seconds at the 20 TPS default — a fixed `[u32; 600]` array
/// (2.4 KiB) that never grows.
const TICK_DURATION_WINDOW: usize = 600;

/// The kind dimension of `ferrumc_block_mutation_total{kind,result}`.
///
/// The discriminants double as the row index into the counter grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MutationKind {
    /// A block break (the world writes air).
    Break = 0,
    /// A block place.
    Place = 1,
}

/// The result dimension of `ferrumc_block_mutation_total{kind,result}`.
///
/// The discriminants double as the column index into the counter grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MutationResult {
    /// The edit was applied by the simulation.
    Accepted = 0,
    /// The edit was vetoed (for example by spawn protection).
    Rejected = 1,
}

/// One tick's worth of measurements, fed to [`CounterRegistry::record_tick`] and
/// also emitted as a structured tracing event by the driver.
///
/// Fields are public: this is an inert per-tick record meant to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TickMetrics {
    /// The shard's X coordinate.
    pub shard_x: i32,
    /// The shard's Z coordinate.
    pub shard_z: i32,
    /// The tick this record describes.
    pub tick: Tick,
    /// Wall-clock duration of the tick in microseconds (millisecond precision is
    /// derived in the snapshot).
    pub duration_us: u64,
    /// How many inputs were drained into the shard this tick.
    pub inputs_drained: usize,
    /// How many outputs the tick emitted.
    pub outputs_emitted: usize,
    /// How many players the shard held.
    pub players: usize,
    /// The shard inbox depth observed for the tick.
    pub inbox_len: usize,
}

/// One row of the fixed-capacity `ferrumc_tick_ms{shard}` table.
#[derive(Debug, Clone, Copy)]
struct TickRow {
    shard_x: i32,
    shard_z: i32,
    last_us: u64,
    sum_us: u64,
    count: u64,
}

/// Fixed-capacity table behind the registry's `Mutex`; never grows past
/// [`TICK_TABLE_CAPACITY`].
#[derive(Debug)]
struct TickTable {
    rows: [Option<TickRow>; TICK_TABLE_CAPACITY],
    len: usize,
}

impl TickTable {
    fn new() -> Self {
        Self {
            rows: std::array::from_fn(|_| None),
            len: 0,
        }
    }
}

/// One row of the fixed-capacity `ferrumc_packet_decode_error_total` table.
#[derive(Debug, Clone, Copy)]
struct DecodeErrorRow {
    state: PacketState,
    packet: &'static str,
    count: u64,
}

/// Fixed-capacity decode-error table behind the registry's `Mutex`; never grows
/// past [`DECODE_ERROR_TABLE_CAPACITY`].
#[derive(Debug)]
struct DecodeErrorTable {
    rows: [Option<DecodeErrorRow>; DECODE_ERROR_TABLE_CAPACITY],
    len: usize,
}

impl DecodeErrorTable {
    fn new() -> Self {
        Self {
            rows: std::array::from_fn(|_| None),
            len: 0,
        }
    }
}

/// Locks a registry table, recovering the guard even if a previous holder
/// panicked (a poisoned counter is still safe to read and bump).
fn lock_table<T>(table: &Mutex<T>) -> MutexGuard<'_, T> {
    table.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Fixed-capacity ring of recent tick durations (microseconds), behind the
/// registry's `Mutex`. Overwrites the oldest sample on wrap; never grows past
/// [`TICK_DURATION_WINDOW`].
#[derive(Debug)]
struct TickDurationRing {
    samples: [u32; TICK_DURATION_WINDOW],
    /// Number of valid samples (saturates at the capacity).
    len: usize,
    /// Write cursor for the next sample.
    next: usize,
}

impl TickDurationRing {
    fn new() -> Self {
        Self {
            samples: [0; TICK_DURATION_WINDOW],
            len: 0,
            next: 0,
        }
    }

    /// Records one tick duration in microseconds, evicting the oldest on wrap.
    fn push(&mut self, sample_us: u32) {
        self.samples[self.next] = sample_us;
        self.next = (self.next + 1) % TICK_DURATION_WINDOW;
        if self.len < TICK_DURATION_WINDOW {
            self.len += 1;
        }
    }

    /// Returns `(p50, p95, p99)` tick durations in microseconds over the window,
    /// computed by nearest-rank over a bounded copy. Empty windows report zeros.
    fn percentiles_us(&self) -> (u64, u64, u64) {
        if self.len == 0 {
            return (0, 0, 0);
        }
        // Copy the valid prefix onto the stack (bounded by the fixed capacity),
        // sort it, then select — never touching the live ring under the lock.
        let mut buf = [0u32; TICK_DURATION_WINDOW];
        buf[..self.len].copy_from_slice(&self.samples[..self.len]);
        let sorted = &mut buf[..self.len];
        sorted.sort_unstable();
        (
            u64::from(nearest_rank(sorted, 50)),
            u64::from(nearest_rank(sorted, 95)),
            u64::from(nearest_rank(sorted, 99)),
        )
    }
}

/// Nearest-rank percentile selection over a non-empty sorted slice.
fn nearest_rank(sorted: &[u32], p: usize) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * sorted.len()).div_ceil(100);
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// The shared, lock-light metric sink.
///
/// Cloned behind an [`Arc`] and threaded through the app; every method takes
/// `&self`, so connection tasks and the driver share one instance. Public
/// recording methods are named after the metric family they feed.
#[derive(Debug)]
pub struct CounterRegistry {
    /// `ferrumc_chunk_sent_total`.
    chunk_sent_total: AtomicU64,
    /// `ferrumc_chunk_unloaded_total`.
    chunk_unloaded_total: AtomicU64,
    /// `ferrumc_block_mutation_total{kind,result}` as a `[kind][result]` grid.
    block_mutation_total: [[AtomicU64; 2]; 2],
    /// Running sum of `ferrumc_storage_flush_ms` samples.
    storage_flush_ms_sum: AtomicU64,
    /// Count of `ferrumc_storage_flush_ms` samples.
    storage_flush_count: AtomicU64,
    /// The most recent `ferrumc_storage_flush_ms` sample.
    storage_flush_ms_last: AtomicU64,
    /// Aggregate max of `ferrumc_session_outbound_queue_len{session}` (per-session
    /// values live in each `SessionDebugSnapshot`, not here, to bound cardinality).
    outbound_queue_len_max: AtomicU64,
    /// Count of decode-error keys that did not fit the fixed table.
    decode_error_overflow: AtomicU64,
    /// `ferrumc_tick_ms{shard}`.
    tick_table: Mutex<TickTable>,
    /// `ferrumc_packet_decode_error_total{state,packet}`.
    decode_errors: Mutex<DecodeErrorTable>,
    /// Sliding window of recent tick durations for percentile reporting; fed from
    /// the same single-writer `record_tick` path as the tick table.
    tick_durations: Mutex<TickDurationRing>,
    /// Bounded per-plugin callback metrics.
    plugin_metrics: PluginMetricRegistry,
}

impl CounterRegistry {
    /// Creates a registry with every counter at zero and both tables empty.
    #[must_use]
    pub fn new() -> Self {
        Self {
            chunk_sent_total: AtomicU64::new(0),
            chunk_unloaded_total: AtomicU64::new(0),
            block_mutation_total: std::array::from_fn(|_| {
                std::array::from_fn(|_| AtomicU64::new(0))
            }),
            storage_flush_ms_sum: AtomicU64::new(0),
            storage_flush_count: AtomicU64::new(0),
            storage_flush_ms_last: AtomicU64::new(0),
            outbound_queue_len_max: AtomicU64::new(0),
            decode_error_overflow: AtomicU64::new(0),
            tick_table: Mutex::new(TickTable::new()),
            decode_errors: Mutex::new(DecodeErrorTable::new()),
            tick_durations: Mutex::new(TickDurationRing::new()),
            plugin_metrics: PluginMetricRegistry::new(),
        }
    }

    /// Adds `n` to `ferrumc_chunk_sent_total`.
    pub fn incr_chunk_sent(&self, n: u64) {
        self.chunk_sent_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Adds `n` to `ferrumc_chunk_unloaded_total`.
    pub fn incr_chunk_unloaded(&self, n: u64) {
        self.chunk_unloaded_total.fetch_add(n, Ordering::Relaxed);
    }

    /// Bumps `ferrumc_block_mutation_total{kind,result}` for one edit.
    pub fn record_block_mutation(&self, kind: MutationKind, result: MutationResult) {
        self.block_mutation_total[kind as usize][result as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Records one `ferrumc_storage_flush_ms` sample (sum, count, and last).
    pub fn record_storage_flush_ms(&self, ms: u64) {
        self.storage_flush_ms_sum.fetch_add(ms, Ordering::Relaxed);
        self.storage_flush_count.fetch_add(1, Ordering::Relaxed);
        self.storage_flush_ms_last.store(ms, Ordering::Relaxed);
    }

    /// Records one tick into `ferrumc_tick_ms{shard}` (last, sum, count per shard).
    ///
    /// New shards beyond [`TICK_TABLE_CAPACITY`] are dropped rather than growing
    /// the table; a single shard runs today, so this is headroom, not a limit hit.
    pub fn record_tick(&self, metrics: &TickMetrics) {
        // Feed the percentile window first (a separate lock scope from the table),
        // saturating a pathologically long tick into the `u32` microsecond sample.
        {
            let mut ring = lock_table(&self.tick_durations);
            ring.push(u32::try_from(metrics.duration_us).unwrap_or(u32::MAX));
        }

        let mut table = lock_table(&self.tick_table);
        let len = table.len;
        for row in table.rows.iter_mut().take(len).flatten() {
            if row.shard_x == metrics.shard_x && row.shard_z == metrics.shard_z {
                row.last_us = metrics.duration_us;
                row.sum_us = row.sum_us.saturating_add(metrics.duration_us);
                row.count = row.count.saturating_add(1);
                return;
            }
        }
        if len < TICK_TABLE_CAPACITY {
            table.rows[len] = Some(TickRow {
                shard_x: metrics.shard_x,
                shard_z: metrics.shard_z,
                last_us: metrics.duration_us,
                sum_us: metrics.duration_us,
                count: 1,
            });
            table.len = len + 1;
        }
    }

    /// Updates the `ferrumc_session_outbound_queue_len{session}` aggregate max
    /// gauge with one sampled depth.
    pub fn observe_outbound_queue_len(&self, depth: usize) {
        self.outbound_queue_len_max
            .fetch_max(depth as u64, Ordering::Relaxed);
    }

    /// Bumps `ferrumc_packet_decode_error_total{state,packet}` for one failure.
    ///
    /// `packet` is a `&'static str` label. Keys beyond
    /// [`DECODE_ERROR_TABLE_CAPACITY`] fold into the overflow bucket instead of
    /// growing the table.
    pub fn record_packet_decode_error(&self, state: PacketState, packet: &'static str) {
        let mut table = lock_table(&self.decode_errors);
        let len = table.len;
        for row in table.rows.iter_mut().take(len).flatten() {
            if row.state == state && row.packet == packet {
                row.count = row.count.saturating_add(1);
                return;
            }
        }
        if len < DECODE_ERROR_TABLE_CAPACITY {
            table.rows[len] = Some(DecodeErrorRow {
                state,
                packet,
                count: 1,
            });
            table.len = len + 1;
        } else {
            self.decode_error_overflow.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records one completed plugin callback into the bounded per-plugin table.
    ///
    /// The observation is one authoritative callback outcome, so an overlapping
    /// panic or capability-denial representation must not be submitted again.
    /// Capacity loss is telemetry-only and never changes callback behavior.
    pub fn record_plugin_invocation(
        &self,
        observation: PluginInvocationObservation,
    ) -> PluginMetricRecordOutcome {
        self.plugin_metrics.record(observation)
    }

    /// Replaces every retained plugin's `hung` gauge from one watchdog snapshot.
    ///
    /// The caller must be the single publication owner and supply snapshots in
    /// order from the one authoritative process watchdog. Applying snapshots
    /// from multiple watchdogs independently would make each projection replace
    /// the others.
    ///
    /// A plugin is hung while at least one of its active callbacks has crossed
    /// the hard watchdog threshold. Completing one of multiple hard callbacks
    /// therefore cannot clear the gauge. The monotonic callback counters are
    /// unchanged by this projection.
    pub fn sync_plugin_hung_status(&self, watchdog: &WatchdogSnapshot) {
        self.plugin_metrics.sync_hung_from_watchdog(watchdog);
    }

    /// Builds an owned, serializable snapshot of every metric.
    ///
    /// The top-level JSON keys are the exact metric names so a future exporter
    /// can map them one-to-one.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        let tick_ms = {
            let table = lock_table(&self.tick_table);
            table
                .rows
                .iter()
                .take(table.len)
                .flatten()
                .map(|row| TickMsEntry {
                    shard: format!("{},{}", row.shard_x, row.shard_z),
                    last_ms: row.last_us as f64 / US_PER_MS,
                    avg_ms: if row.count == 0 {
                        0.0
                    } else {
                        row.sum_us as f64 / row.count as f64 / US_PER_MS
                    },
                    count: row.count,
                })
                .collect()
        };

        let packet_decode_error_total = {
            let table = lock_table(&self.decode_errors);
            let entries = table
                .rows
                .iter()
                .take(table.len)
                .flatten()
                .map(|row| DecodeErrorEntry {
                    state: row.state,
                    packet: row.packet,
                    count: row.count,
                })
                .collect();
            DecodeErrorTotals {
                entries,
                overflow: self.decode_error_overflow.load(Ordering::Relaxed),
            }
        };

        let flush_count = self.storage_flush_count.load(Ordering::Relaxed);
        let flush_sum = self.storage_flush_ms_sum.load(Ordering::Relaxed);

        MetricsSnapshot {
            tick_ms,
            session_outbound_queue_len: QueueLenGauge {
                max: self.outbound_queue_len_max.load(Ordering::Relaxed),
            },
            chunk_sent_total: self.chunk_sent_total.load(Ordering::Relaxed),
            chunk_unloaded_total: self.chunk_unloaded_total.load(Ordering::Relaxed),
            block_mutation_total: BlockMutationTotals {
                break_kind: BlockMutationResults {
                    accepted: self.block_mutation_total[MutationKind::Break as usize]
                        [MutationResult::Accepted as usize]
                        .load(Ordering::Relaxed),
                    rejected: self.block_mutation_total[MutationKind::Break as usize]
                        [MutationResult::Rejected as usize]
                        .load(Ordering::Relaxed),
                },
                place: BlockMutationResults {
                    accepted: self.block_mutation_total[MutationKind::Place as usize]
                        [MutationResult::Accepted as usize]
                        .load(Ordering::Relaxed),
                    rejected: self.block_mutation_total[MutationKind::Place as usize]
                        [MutationResult::Rejected as usize]
                        .load(Ordering::Relaxed),
                },
            },
            storage_flush_ms: StorageFlushStats {
                last_ms: self.storage_flush_ms_last.load(Ordering::Relaxed),
                sum_ms: flush_sum,
                count: flush_count,
                avg_ms: if flush_count == 0 {
                    0.0
                } else {
                    flush_sum as f64 / flush_count as f64
                },
            },
            packet_decode_error_total,
            plugin_metrics: self.plugin_metrics.snapshot(),
        }
    }

    /// Returns `(p50, p95, p99)` tick durations in milliseconds over the sliding
    /// window. Used by [`server_snapshot`](Self::server_snapshot) to fold the tick
    /// percentiles into the dashboard's [`ServerSnapshot`](crate::ServerSnapshot).
    pub(crate) fn tick_percentiles_ms(&self) -> (f64, f64, f64) {
        let (p50, p95, p99) = lock_table(&self.tick_durations).percentiles_us();
        (
            p50 as f64 / US_PER_MS,
            p95 as f64 / US_PER_MS,
            p99 as f64 / US_PER_MS,
        )
    }

    /// Dumps every metric: one structured `info` event carrying the JSON
    /// snapshot, targeted at `ferrumc::observability::metrics`. Never panics.
    pub fn dump(&self) {
        let snapshot = self.snapshot();
        let json = serde_json::to_string(&snapshot)
            .unwrap_or_else(|err| format!("{{\"error\":\"failed to serialize metrics: {err}\"}}"));
        tracing::info!(
            target: "ferrumc::observability::metrics",
            json = %json,
            "metrics snapshot"
        );
    }
}

impl Default for CounterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A serializable snapshot of every metric, keyed by the exact metric names.
///
/// Fields are public (and renamed) so the JSON keys are the canonical metric
/// names a future exporter scrapes.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    /// `ferrumc_tick_ms{shard}` as a per-shard list.
    #[serde(rename = "ferrumc_tick_ms")]
    pub tick_ms: Vec<TickMsEntry>,
    /// `ferrumc_session_outbound_queue_len{session}` aggregate gauge.
    #[serde(rename = "ferrumc_session_outbound_queue_len")]
    pub session_outbound_queue_len: QueueLenGauge,
    /// `ferrumc_chunk_sent_total`.
    #[serde(rename = "ferrumc_chunk_sent_total")]
    pub chunk_sent_total: u64,
    /// `ferrumc_chunk_unloaded_total`.
    #[serde(rename = "ferrumc_chunk_unloaded_total")]
    pub chunk_unloaded_total: u64,
    /// `ferrumc_block_mutation_total{kind,result}`.
    #[serde(rename = "ferrumc_block_mutation_total")]
    pub block_mutation_total: BlockMutationTotals,
    /// `ferrumc_storage_flush_ms` summary.
    #[serde(rename = "ferrumc_storage_flush_ms")]
    pub storage_flush_ms: StorageFlushStats,
    /// `ferrumc_packet_decode_error_total{state,packet}`.
    #[serde(rename = "ferrumc_packet_decode_error_total")]
    pub packet_decode_error_total: DecodeErrorTotals,
    /// Bounded per-plugin callback rows and degradation counts.
    #[serde(rename = "ferrumc_plugin_metrics")]
    pub plugin_metrics: PluginMetricsSnapshot,
}

/// One shard's row in the `ferrumc_tick_ms` snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct TickMsEntry {
    /// The `"x,z"` shard label.
    pub shard: String,
    /// The most recent tick duration in milliseconds.
    pub last_ms: f64,
    /// The mean tick duration in milliseconds.
    pub avg_ms: f64,
    /// How many ticks have been recorded for this shard.
    pub count: u64,
}

/// The `ferrumc_session_outbound_queue_len` aggregate gauge.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct QueueLenGauge {
    /// The largest outbound queue depth observed across all sessions.
    pub max: u64,
}

/// The `{kind}` breakdown of `ferrumc_block_mutation_total`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct BlockMutationTotals {
    /// Counts for block breaks.
    #[serde(rename = "break")]
    pub break_kind: BlockMutationResults,
    /// Counts for block places.
    pub place: BlockMutationResults,
}

/// The `{result}` breakdown of one block-mutation kind.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct BlockMutationResults {
    /// Edits the simulation applied.
    pub accepted: u64,
    /// Edits that were vetoed.
    pub rejected: u64,
}

/// The `ferrumc_storage_flush_ms` summary (last/sum/count plus a derived mean).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct StorageFlushStats {
    /// The most recent flush duration in milliseconds.
    pub last_ms: u64,
    /// The total of all flush durations in milliseconds.
    pub sum_ms: u64,
    /// How many flushes have been recorded.
    pub count: u64,
    /// The mean flush duration in milliseconds.
    pub avg_ms: f64,
}

/// The `ferrumc_packet_decode_error_total` table plus its overflow bucket.
#[derive(Debug, Clone, Serialize)]
pub struct DecodeErrorTotals {
    /// One entry per recorded `(state, packet)` key.
    pub entries: Vec<DecodeErrorEntry>,
    /// Count of distinct keys that did not fit the fixed table.
    pub overflow: u64,
}

/// One `(state, packet)` row in the decode-error snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct DecodeErrorEntry {
    /// The connection state the failure occurred in.
    pub state: PacketState,
    /// The packet label.
    pub packet: &'static str,
    /// How many times this `(state, packet)` failed to decode.
    pub count: u64,
}

/// A shared, monotonic server clock so connection tasks can stamp traces with
/// the current simulation tick (which they otherwise never see).
///
/// The single writer is the driver, which calls [`set`](Self::set) once per
/// tick; the many readers are connection tasks calling [`now`](Self::now). It is
/// explicit shared state passed through `ConnContext`, not a global static, and
/// is read-mostly: a relaxed [`AtomicU64`] is enough because a trace only needs
/// the most recently published tick, not a synchronized one.
#[derive(Debug, Clone, Default)]
pub struct ServerClock(Arc<AtomicU64>);

impl ServerClock {
    /// Creates a clock starting at tick zero.
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(0)))
    }

    /// Reads the most recently published tick.
    #[must_use]
    pub fn now(&self) -> Tick {
        Tick::new(self.0.load(Ordering::Relaxed))
    }

    /// Publishes `tick` as the current tick (driver only).
    pub fn set(&self, tick: Tick) {
        self.0.store(tick.get(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_increment_independently() {
        let reg = CounterRegistry::new();
        reg.incr_chunk_sent(3);
        reg.incr_chunk_sent(2);
        reg.incr_chunk_unloaded(4);
        let snap = reg.snapshot();
        assert_eq!(snap.chunk_sent_total, 5);
        assert_eq!(snap.chunk_unloaded_total, 4);
    }

    #[test]
    fn block_mutation_grid_is_keyed_by_kind_and_result() {
        let reg = CounterRegistry::new();
        reg.record_block_mutation(MutationKind::Break, MutationResult::Accepted);
        reg.record_block_mutation(MutationKind::Break, MutationResult::Accepted);
        reg.record_block_mutation(MutationKind::Place, MutationResult::Rejected);
        let snap = reg.snapshot();
        assert_eq!(snap.block_mutation_total.break_kind.accepted, 2);
        assert_eq!(snap.block_mutation_total.break_kind.rejected, 0);
        assert_eq!(snap.block_mutation_total.place.accepted, 0);
        assert_eq!(snap.block_mutation_total.place.rejected, 1);
    }

    #[test]
    fn storage_flush_tracks_last_sum_count_avg() {
        let reg = CounterRegistry::new();
        reg.record_storage_flush_ms(10);
        reg.record_storage_flush_ms(20);
        let s = reg.snapshot().storage_flush_ms;
        assert_eq!(s.last_ms, 20);
        assert_eq!(s.sum_ms, 30);
        assert_eq!(s.count, 2);
        assert!((s.avg_ms - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn tick_table_keys_by_shard_and_averages() {
        let reg = CounterRegistry::new();
        reg.record_tick(&TickMetrics {
            shard_x: 0,
            shard_z: 0,
            tick: Tick::new(1),
            duration_us: 1_000,
            inputs_drained: 1,
            outputs_emitted: 0,
            players: 1,
            inbox_len: 0,
        });
        reg.record_tick(&TickMetrics {
            shard_x: 0,
            shard_z: 0,
            tick: Tick::new(2),
            duration_us: 3_000,
            inputs_drained: 0,
            outputs_emitted: 0,
            players: 1,
            inbox_len: 0,
        });
        let snap = reg.snapshot();
        assert_eq!(snap.tick_ms.len(), 1);
        let entry = &snap.tick_ms[0];
        assert_eq!(entry.shard, "0,0");
        assert_eq!(entry.count, 2);
        assert!((entry.last_ms - 3.0).abs() < f64::EPSILON);
        assert!((entry.avg_ms - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn queue_len_gauge_keeps_the_max() {
        let reg = CounterRegistry::new();
        reg.observe_outbound_queue_len(5);
        reg.observe_outbound_queue_len(2);
        reg.observe_outbound_queue_len(9);
        assert_eq!(reg.snapshot().session_outbound_queue_len.max, 9);
    }

    #[test]
    fn decode_errors_key_by_state_and_packet() {
        let reg = CounterRegistry::new();
        reg.record_packet_decode_error(PacketState::Play, "malformed_body");
        reg.record_packet_decode_error(PacketState::Play, "malformed_body");
        reg.record_packet_decode_error(PacketState::Login, "unknown_packet");
        let totals = reg.snapshot().packet_decode_error_total;
        assert_eq!(totals.entries.len(), 2);
        assert_eq!(totals.overflow, 0);
        let play = totals
            .entries
            .iter()
            .find(|e| e.state == PacketState::Play && e.packet == "malformed_body")
            .unwrap();
        assert_eq!(play.count, 2);
    }

    #[test]
    fn decode_error_table_overflows_into_the_bucket() {
        let reg = CounterRegistry::new();
        // Distinct labels beyond the table capacity must fold into overflow, not
        // grow the table.
        let labels: Vec<String> = (0..DECODE_ERROR_TABLE_CAPACITY + 5)
            .map(|i| format!("packet_{i}"))
            .collect();
        for label in &labels {
            // Leak to obtain a &'static str for the test only.
            let static_label: &'static str = Box::leak(label.clone().into_boxed_str());
            reg.record_packet_decode_error(PacketState::Play, static_label);
        }
        let totals = reg.snapshot().packet_decode_error_total;
        assert_eq!(totals.entries.len(), DECODE_ERROR_TABLE_CAPACITY);
        assert_eq!(totals.overflow, 5);
    }

    #[test]
    fn snapshot_json_uses_exact_metric_names() {
        let reg = CounterRegistry::new();
        reg.incr_chunk_sent(1);
        reg.record_block_mutation(MutationKind::Break, MutationResult::Accepted);
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&reg.snapshot()).unwrap()).unwrap();
        for key in [
            "ferrumc_tick_ms",
            "ferrumc_session_outbound_queue_len",
            "ferrumc_chunk_sent_total",
            "ferrumc_chunk_unloaded_total",
            "ferrumc_block_mutation_total",
            "ferrumc_storage_flush_ms",
            "ferrumc_packet_decode_error_total",
            "ferrumc_plugin_metrics",
        ] {
            assert!(json.get(key).is_some(), "missing metric key {key}");
        }
        assert_eq!(json["ferrumc_chunk_sent_total"], 1);
        assert_eq!(json["ferrumc_block_mutation_total"]["break"]["accepted"], 1);
    }

    #[test]
    fn server_clock_publishes_and_reads_the_tick() {
        let clock = ServerClock::new();
        assert_eq!(clock.now(), Tick::ZERO);
        clock.set(Tick::new(77));
        assert_eq!(clock.now(), Tick::new(77));
        // Clones share the same underlying cell.
        let reader = clock.clone();
        clock.set(Tick::new(78));
        assert_eq!(reader.now(), Tick::new(78));
    }
}
