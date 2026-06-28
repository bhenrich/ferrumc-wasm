//! The dedicated, off-tick storage worker.
//!
//! The simulation/session driver must never block a tick on disk I/O, so it does
//! not persist anything itself. Instead it emits [`StorageFlushRequest`]s — chunk
//! overlays (only player-modified sections) and journal entries — onto a
//! **bounded** channel, and this worker drains them on its own task and commits
//! them through the [`WorldStore`].
//!
//! # Batching and backpressure
//!
//! Pending work is flushed when either threshold is reached — at least
//! [`FLUSH_CHUNK_THRESHOLD`] queued items, or every [`FLUSH_EVERY_N_TICKS`] ticks
//! of wall time — so small edits still land promptly without a commit per chunk.
//! The channel is bounded: when it fills, the driver's end-of-tick flush *defers*
//! (it keeps the chunks persist-dirty and retries next tick) rather than blocking
//! the tick. On shutdown the driver sends a final flush and drops its sender; the
//! worker observes the closed channel, drains everything still pending, and exits,
//! so a graceful shutdown loses no edits.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use ferrumc_observability::CounterRegistry;
use ferrumc_storage::{
    BlockMutationLogRecord, ChunkKey, ChunkOverlayRecord, WorldStore, MAX_SAVE_BATCH,
};

/// One unit of persistence work emitted by the driver at a flush point.
///
/// Carries the chunk overlays captured from persist-dirty chunks and the block
/// mutations journaled this flush. Either vector may be empty.
pub(crate) struct StorageFlushRequest {
    /// Chunk overlays to upsert (only player-modified sections).
    pub(crate) overlays: Vec<(ChunkKey, ChunkOverlayRecord)>,
    /// Journal entries to append.
    pub(crate) mutations: Vec<BlockMutationLogRecord>,
}

/// Flush pending work at least this often (measured in driver ticks) even when
/// the count threshold has not been reached, so a lone edit is not stranded.
const FLUSH_EVERY_N_TICKS: u32 = 4;

/// Flush as soon as this many overlays (or journal entries) accumulate, bounding
/// how much work one commit defers and how large the in-worker buffer grows.
const FLUSH_CHUNK_THRESHOLD: usize = 128;

/// Runs the storage worker until the request channel closes, then performs a
/// final drain and returns.
///
/// `rx` is the receiving half of the bounded flush channel; `store` is the shared
/// world store; `metrics` records flush latency; `tick_period` sizes the periodic
/// flush timer.
pub(crate) async fn run_storage_worker(
    mut rx: mpsc::Receiver<StorageFlushRequest>,
    store: Arc<dyn WorldStore>,
    metrics: Arc<CounterRegistry>,
    tick_period: Duration,
) {
    let mut pending_overlays: Vec<(ChunkKey, ChunkOverlayRecord)> = Vec::new();
    let mut pending_mutations: Vec<BlockMutationLogRecord> = Vec::new();

    let mut flush_timer = tokio::time::interval(tick_period.saturating_mul(FLUSH_EVERY_N_TICKS));
    // A late timer must not fire a burst of catch-up flushes.
    flush_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_req = rx.recv() => {
                if let Some(req) = maybe_req {
                    pending_overlays.extend(req.overlays);
                    pending_mutations.extend(req.mutations);
                    if pending_overlays.len() >= FLUSH_CHUNK_THRESHOLD
                        || pending_mutations.len() >= FLUSH_CHUNK_THRESHOLD
                    {
                        flush(
                            store.as_ref(),
                            &metrics,
                            &mut pending_overlays,
                            &mut pending_mutations,
                        )
                        .await;
                    }
                } else {
                    // The driver dropped its sender on shutdown: drain everything
                    // still pending so a graceful stop persists every edit, then exit.
                    flush(
                        store.as_ref(),
                        &metrics,
                        &mut pending_overlays,
                        &mut pending_mutations,
                    )
                    .await;
                    break;
                }
            }
            _ = flush_timer.tick() => {
                flush(
                    store.as_ref(),
                    &metrics,
                    &mut pending_overlays,
                    &mut pending_mutations,
                )
                .await;
            }
        }
    }
}

/// Commits all pending overlays and journal entries, draining both buffers.
///
/// Each is committed in [`MAX_SAVE_BATCH`]-sized batches so a large flush never
/// trips the store's per-call batch limit. A failed batch is logged and skipped;
/// the worker keeps going rather than tearing down the task.
async fn flush(
    store: &dyn WorldStore,
    metrics: &CounterRegistry,
    overlays: &mut Vec<(ChunkKey, ChunkOverlayRecord)>,
    mutations: &mut Vec<BlockMutationLogRecord>,
) {
    if overlays.is_empty() && mutations.is_empty() {
        return;
    }

    let start = Instant::now();

    while !overlays.is_empty() {
        let take = overlays.len().min(MAX_SAVE_BATCH);
        let batch: Vec<_> = overlays.drain(..take).collect();
        if let Err(err) = store.save_chunk_overlays(batch).await {
            tracing::warn!(%err, "failed to flush chunk overlays");
        }
    }

    while !mutations.is_empty() {
        let take = mutations.len().min(MAX_SAVE_BATCH);
        let batch: Vec<_> = mutations.drain(..take).collect();
        if let Err(err) = store.append_block_mutations(batch).await {
            tracing::warn!(%err, "failed to append block mutations");
        }
    }

    // `as u64`: a flush taking longer than ~584 million years would be required to
    // overflow, so the millisecond cast is effectively lossless.
    metrics.record_storage_flush_ms(start.elapsed().as_millis() as u64);
}
