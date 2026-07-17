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
//! [`FLUSH_CHUNK_THRESHOLD`] queued items, or every [`FLUSH_EVERY_N_TICKS`] driver
//! tick periods — so small edits still land promptly without a commit per chunk.
//! The channel is bounded: when it fills, the driver's end-of-tick flush *defers*
//! (it keeps the chunks persist-dirty and retries next tick) rather than blocking
//! the tick. Failed commits retain the exact tokenized journal batch for bounded
//! retry. If that retry budget is exhausted, the worker requests server shutdown
//! and returns the storage error instead of accepting more work or reporting a
//! false durability success.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::MissedTickBehavior;

use ferrumc_core::{Result as ServerResult, ServerError};
use ferrumc_observability::CounterRegistry;
use ferrumc_storage::{
    BlockMutationLogRecord, ChunkKey, ChunkOverlayRecord, JournalAppendReceipt, JournalBatchId,
    WorldStore, MAX_SAVE_BATCH,
};

/// One unit of persistence work emitted by the driver at a flush point.
///
/// Carries the chunk overlays captured from persist-dirty chunks and the block
/// mutations journaled this flush. Either vector may be empty.
pub(crate) struct StorageFlushRequest {
    /// Chunk overlays to upsert (only player-modified sections).
    pub(crate) overlays: Vec<(ChunkKey, ChunkOverlayRecord)>,
    /// Tokenized journal sub-batches, each already within the storage call bound.
    mutation_batches: Vec<PendingJournalBatch>,
    /// Optional single-shot durability result. `Ok(())` is sent only after this
    /// request and all previously buffered work have committed (overlay saves plus
    /// journal receipts). The first failed attempt sends its classified `Err`
    /// while the worker retains the exact batch for bounded recovery.
    ///
    /// `Some` only on a durability barrier (see
    /// [`release_chunks_acked`](crate::driver)), where the driver must not drop a
    /// chunk's tickets until the placed-block overlay is durable — otherwise a
    /// fast rejoin could read a stale baseline before the write lands. `None` for
    /// the fire-and-forget per-tick and shutdown flushes, which never block on a
    /// commit. The receiver may be gone (the connection disconnected); the send is
    /// best-effort and a dropped receiver is ignored.
    pub(crate) ack: Option<oneshot::Sender<ServerResult<()>>>,
}

impl StorageFlushRequest {
    /// Builds one request and freezes a durable idempotency token onto every
    /// `MAX_SAVE_BATCH`-sized journal slice before channel delivery.
    pub(crate) fn new(
        overlays: Vec<(ChunkKey, ChunkOverlayRecord)>,
        mutations: Vec<BlockMutationLogRecord>,
    ) -> Self {
        Self {
            overlays,
            mutation_batches: PendingJournalBatch::split(mutations),
            ack: None,
        }
    }
}

/// One logical journal append whose token survives every retry.
#[derive(Debug, Clone)]
struct PendingJournalBatch {
    batch_id: JournalBatchId,
    records: Vec<BlockMutationLogRecord>,
}

impl PendingJournalBatch {
    fn split(records: Vec<BlockMutationLogRecord>) -> Vec<Self> {
        let mut batches = Vec::with_capacity(records.len().div_ceil(MAX_SAVE_BATCH));
        let mut records = records.into_iter();
        loop {
            let batch: Vec<_> = records.by_ref().take(MAX_SAVE_BATCH).collect();
            if batch.is_empty() {
                break;
            }
            batches.push(Self {
                batch_id: JournalBatchId::generate(),
                records: batch,
            });
        }
        batches
    }
}

/// Flush pending work at least this often (measured in driver ticks) even when
/// the count threshold has not been reached, so a lone edit is not stranded.
const FLUSH_EVERY_N_TICKS: u32 = 4;

/// Flush as soon as this many overlays (or journal entries) accumulate, bounding
/// how much work one commit defers and how large the in-worker buffer grows.
const FLUSH_CHUNK_THRESHOLD: usize = 128;

/// Maximum number of attempts for one retained flush before the worker stops
/// accepting requests and asks the server to shut down.
const MAX_FLUSH_ATTEMPTS: u32 = 3;

/// Initial retry delay. Later attempts double this delay; the small bound above
/// caps both the total delay and time spent not receiving from the bounded queue.
const RETRY_BASE_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy)]
struct RetryPolicy {
    max_attempts: u32,
    base_delay: Duration,
}

impl RetryPolicy {
    const PRODUCTION: Self = Self {
        max_attempts: MAX_FLUSH_ATTEMPTS,
        base_delay: RETRY_BASE_DELAY,
    };

    fn delay_after_failure(self, failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(u32::BITS - 1);
        let multiplier = 1_u32.checked_shl(shift).unwrap_or(u32::MAX);
        self.base_delay.saturating_mul(multiplier)
    }
}

#[derive(Debug, Default)]
struct PendingStorage {
    overlays: Vec<(ChunkKey, ChunkOverlayRecord)>,
    mutation_batches: VecDeque<PendingJournalBatch>,
    mutation_count: usize,
}

impl PendingStorage {
    fn push(&mut self, request: StorageFlushRequest) -> Option<oneshot::Sender<ServerResult<()>>> {
        let StorageFlushRequest {
            overlays,
            mutation_batches,
            ack,
        } = request;
        self.overlays.extend(overlays);
        for batch in mutation_batches {
            // This count is only compared with a small flush threshold. Saturation
            // therefore preserves the only meaningful predicate (`>= threshold`)
            // even for a theoretically enormous accepted request.
            self.mutation_count = self.mutation_count.saturating_add(batch.records.len());
            self.mutation_batches.push_back(batch);
        }
        ack
    }

    fn is_empty(&self) -> bool {
        self.overlays.is_empty() && self.mutation_batches.is_empty()
    }

    fn reached_threshold(&self) -> bool {
        self.overlays.len() >= FLUSH_CHUNK_THRESHOLD || self.mutation_count >= FLUSH_CHUNK_THRESHOLD
    }
}

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
    shutdown: watch::Sender<bool>,
) -> ServerResult<()> {
    run_storage_worker_with_policy(
        &mut rx,
        store.as_ref(),
        metrics.as_ref(),
        tick_period,
        &shutdown,
        RetryPolicy::PRODUCTION,
    )
    .await
}

async fn run_storage_worker_with_policy(
    rx: &mut mpsc::Receiver<StorageFlushRequest>,
    store: &dyn WorldStore,
    metrics: &CounterRegistry,
    tick_period: Duration,
    shutdown: &watch::Sender<bool>,
    retry_policy: RetryPolicy,
) -> ServerResult<()> {
    let mut pending = PendingStorage::default();

    let mut flush_timer = tokio::time::interval(tick_period.saturating_mul(FLUSH_EVERY_N_TICKS));
    // A late timer must not fire a burst of catch-up flushes.
    flush_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Tokio intervals expose their first tick immediately. Consume that startup
    // tick so the periodic branch represents an elapsed flush period rather than
    // racing the first request/channel-close event.
    flush_timer.tick().await;

    loop {
        tokio::select! {
            maybe_req = rx.recv() => {
                if let Some(req) = maybe_req {
                    let ack = pending.push(req);
                    let force_flush = ack.is_some();
                    // An acked request forces a full-buffer drain regardless of the
                    // count thresholds: the placed-block overlay it is meant to make
                    // durable has usually already been moved into this buffer by an
                    // earlier per-tick `try_flush_persist_dirty`, so the only way to
                    // guarantee it is committed before the ack fires is to commit
                    // everything still pending here.
                    if force_flush || pending.reached_threshold() {
                        if let Err(error) = flush_with_recovery(
                            store,
                            metrics,
                            &mut pending,
                            ack,
                            retry_policy,
                        )
                        .await
                        {
                            let _ = shutdown.send(true);
                            return Err(error);
                        }
                    }
                } else {
                    // The driver dropped its sender on shutdown: drain everything
                    // still pending so a graceful stop persists every edit, then exit.
                    let result = flush_with_recovery(
                        store,
                        metrics,
                        &mut pending,
                        None,
                        retry_policy,
                    )
                    .await;
                    if result.is_err() {
                        let _ = shutdown.send(true);
                    }
                    return result;
                }
            }
            _ = flush_timer.tick() => {
                if let Err(error) = flush_with_recovery(
                    store,
                    metrics,
                    &mut pending,
                    None,
                    retry_policy,
                )
                .await
                {
                    let _ = shutdown.send(true);
                    return Err(error);
                }
            }
        }
    }
}

async fn flush_with_recovery(
    store: &dyn WorldStore,
    metrics: &CounterRegistry,
    pending: &mut PendingStorage,
    ack: Option<oneshot::Sender<ServerResult<()>>>,
    retry_policy: RetryPolicy,
) -> ServerResult<()> {
    let first_result = flush_once(store, metrics, pending).await;
    if let Some(ack) = ack {
        let _ = ack.send(first_result.clone());
    }
    let first_error = match first_result {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    tracing::warn!(
        error = %first_error,
        attempt = 1,
        max_attempts = retry_policy.max_attempts,
        "storage flush failed; retaining batch for bounded retry"
    );
    retry_pending(store, metrics, pending, first_error, retry_policy).await
}

async fn retry_pending(
    store: &dyn WorldStore,
    metrics: &CounterRegistry,
    pending: &mut PendingStorage,
    mut last_error: ServerError,
    retry_policy: RetryPolicy,
) -> ServerResult<()> {
    let mut attempts = 1_u32;
    while attempts < retry_policy.max_attempts {
        let delay = retry_policy.delay_after_failure(attempts);
        if delay.is_zero() {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(delay).await;
        }
        attempts = attempts.saturating_add(1);
        match flush_once(store, metrics, pending).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    attempt = attempts,
                    max_attempts = retry_policy.max_attempts,
                    "storage flush retry failed"
                );
                last_error = error;
            }
        }
    }
    Err(last_error)
}

/// Attempts to commit all pending work, retaining the first failed batch and
/// everything after it. Successful prefixes are removed only after the store
/// confirms them.
async fn flush_once(
    store: &dyn WorldStore,
    metrics: &CounterRegistry,
    pending: &mut PendingStorage,
) -> ServerResult<()> {
    if pending.is_empty() {
        return Ok(());
    }

    let start = Instant::now();
    let result = flush_inner(store, pending).await;

    // `as u64`: a flush taking longer than ~584 million years would be required to
    // overflow, so the millisecond cast is effectively lossless.
    metrics.record_storage_flush_ms(start.elapsed().as_millis() as u64);
    result
}

async fn flush_inner(store: &dyn WorldStore, pending: &mut PendingStorage) -> ServerResult<()> {
    while !pending.overlays.is_empty() {
        let take = pending.overlays.len().min(MAX_SAVE_BATCH);
        let batch = pending.overlays[..take].to_vec();
        store.save_chunk_overlays(batch).await?;
        pending.overlays.drain(..take);
    }

    while let Some(batch) = pending.mutation_batches.front().cloned() {
        let receipt = store
            .append_block_mutation_batch(batch.batch_id, batch.records.clone())
            .await?;
        validate_receipt(&batch, receipt)?;
        if pending.mutation_count < batch.records.len() {
            return Err(ServerError::internal(
                "storage worker pending mutation count underflow",
            ));
        }
        pending.mutation_count -= batch.records.len();
        pending.mutation_batches.pop_front();
    }

    Ok(())
}

fn validate_receipt(
    batch: &PendingJournalBatch,
    receipt: JournalAppendReceipt,
) -> ServerResult<()> {
    if receipt.batch_id() != batch.batch_id || receipt.len() != batch.records.len() {
        return Err(ServerError::internal(
            "storage worker received a mismatched journal receipt",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use ferrumc_core::{DimensionId, Tick, WorldId};
    use ferrumc_math::{BlockPos, ChunkPos};
    use ferrumc_storage::{JournalBatchId, MutationActor, MutationLogCause, SchemaVersion};
    use ferrumc_testkit::{
        FaultInjectingStore, FaultOperation, FaultStage, FaultStoreAttempt, FaultTraceEntry,
    };
    use ferrumc_world::{BlockStateId, Chunk};

    const TEST_RETRY_POLICY: RetryPolicy = RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::ZERO,
    };

    fn mutation(tick: Tick) -> BlockMutationLogRecord {
        BlockMutationLogRecord::new(
            SchemaVersion::new(1),
            0,
            tick.get(),
            MutationActor::System,
            BlockPos::new(1, 64, 1),
            BlockStateId::new(1),
            BlockStateId::new(2),
            MutationLogCause::Test,
        )
    }

    fn overlay() -> (ChunkKey, ChunkOverlayRecord) {
        let pos = ChunkPos::new(2, -3);
        let mut chunk = Chunk::new(pos);
        let block = BlockPos::new(33, 64, -47);
        chunk
            .set_block(block, BlockStateId::new(9))
            .expect("test block is inside the chunk");
        chunk.mark_persist_dirty(block);
        (
            ChunkKey::new(WorldId::new(1), DimensionId::new(2), pos),
            ChunkOverlayRecord::from_chunk(SchemaVersion::new(3), pos, &chunk, 7),
        )
    }

    fn request(
        overlays: Vec<(ChunkKey, ChunkOverlayRecord)>,
        batch_id: JournalBatchId,
        mutations: Vec<BlockMutationLogRecord>,
        ack: Option<oneshot::Sender<ServerResult<()>>>,
    ) -> StorageFlushRequest {
        StorageFlushRequest {
            overlays,
            mutation_batches: if mutations.is_empty() {
                Vec::new()
            } else {
                vec![PendingJournalBatch {
                    batch_id,
                    records: mutations,
                }]
            },
            ack,
        }
    }

    fn acked_request(
        batch_id: JournalBatchId,
        mutations: Vec<BlockMutationLogRecord>,
    ) -> (StorageFlushRequest, oneshot::Receiver<ServerResult<()>>) {
        let (ack, acknowledged) = oneshot::channel();
        (
            request(Vec::new(), batch_id, mutations, Some(ack)),
            acknowledged,
        )
    }

    fn spawn_test_worker(
        store: Arc<FaultInjectingStore>,
        rx: mpsc::Receiver<StorageFlushRequest>,
        shutdown: watch::Sender<bool>,
    ) -> tokio::task::JoinHandle<ServerResult<()>> {
        tokio::spawn(async move {
            let mut rx = rx;
            let metrics = CounterRegistry::new();
            run_storage_worker_with_policy(
                &mut rx,
                store.as_ref(),
                &metrics,
                Duration::from_secs(1),
                &shutdown,
                TEST_RETRY_POLICY,
            )
            .await
        })
    }

    fn tokenized_attempts(store: &FaultInjectingStore) -> Vec<FaultStoreAttempt> {
        store
            .attempted_state()
            .expect("mutation attempts")
            .operations()
            .iter()
            .filter(|attempt| attempt.operation() == FaultOperation::AppendBlockMutationBatch)
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn failed_batch_remains_pending_and_flush_barrier_errors() {
        let store = Arc::new(FaultInjectingStore::new());
        store
            .fail_next_before_commit()
            .expect("schedule pre-commit failure");
        let retry_gate = store
            .hold_next_before_commit()
            .expect("hold retained retry before commit");
        // Four slots are enough for the one request plus deterministic cleanup;
        // saturation behavior belongs to the production 256-entry channel.
        let (tx, rx) = mpsc::channel(4);
        let (shutdown, _shutdown_rx) = watch::channel(false);
        let worker = spawn_test_worker(Arc::clone(&store), rx, shutdown);
        let batch_id = JournalBatchId::from_bytes([0x17; 16]);
        let expected = vec![mutation(Tick::new(7))];
        let (request, acknowledged) = acked_request(batch_id, expected.clone());
        tx.send(request).await.expect("submit accepted mutation");

        acknowledged
            .await
            .expect("worker answers the durability barrier")
            .expect_err("a failed durability barrier must not report success");
        retry_gate.wait_until_reached().await;
        assert!(store
            .snapshot()
            .expect("failed snapshot")
            .block_mutations()
            .is_empty());
        let attempts = tokenized_attempts(&store);
        assert_eq!(attempts.len(), 2);
        assert!(attempts
            .iter()
            .all(|attempt| attempt.journal_batch_id() == Some(batch_id)));
        assert!(attempts
            .iter()
            .all(|attempt| attempt.block_mutations() == Some(expected.as_slice())));

        // This empty request is a barrier over the already-retained batch. It must
        // not manufacture a second logical journal request to recover the failure.
        let (barrier, barrier_acknowledged) =
            acked_request(JournalBatchId::from_bytes([0x18; 16]), Vec::new());
        tx.send(barrier).await.expect("submit retry barrier");
        retry_gate.release();
        barrier_acknowledged
            .await
            .expect("worker answers retry barrier")
            .expect("retained retry commits");

        let snapshot = store.snapshot().expect("recovered snapshot");
        assert_eq!(snapshot.block_mutations().len(), 1);
        assert_eq!(tokenized_attempts(&store).len(), 2);

        drop(tx);
        worker
            .await
            .expect("worker task joins")
            .expect("worker exits cleanly");
    }

    #[tokio::test]
    async fn retry_after_unknown_commit_outcome_cannot_be_exactly_once() {
        let store = Arc::new(FaultInjectingStore::new());
        store
            .lose_next_ack_after_commit()
            .expect("schedule lost acknowledgement");
        let retry_gate = store
            .hold_next_before_commit()
            .expect("hold receipt replay");
        let (tx, rx) = mpsc::channel(4);
        let (shutdown, _shutdown_rx) = watch::channel(false);
        let worker = spawn_test_worker(Arc::clone(&store), rx, shutdown);
        let batch_id = JournalBatchId::from_bytes([0x28; 16]);
        let expected = vec![mutation(Tick::new(8))];
        let (request, acknowledged) = acked_request(batch_id, expected.clone());
        tx.send(request).await.expect("submit logical batch");
        acknowledged
            .await
            .expect("worker answers first barrier")
            .expect_err("commit acknowledgement was deliberately lost");

        retry_gate.wait_until_reached().await;
        let committed = store.snapshot().expect("unknown-outcome snapshot");
        assert_eq!(committed.block_mutations().len(), 1);
        let attempts = tokenized_attempts(&store);
        assert_eq!(attempts.len(), 2);
        assert!(attempts
            .iter()
            .all(|attempt| attempt.journal_batch_id() == Some(batch_id)));
        assert!(attempts
            .iter()
            .all(|attempt| attempt.block_mutations() == Some(expected.as_slice())));

        let (barrier, barrier_acknowledged) =
            acked_request(JournalBatchId::from_bytes([0x29; 16]), Vec::new());
        tx.send(barrier).await.expect("submit recovery barrier");
        retry_gate.release();
        barrier_acknowledged
            .await
            .expect("worker answers recovery barrier")
            .expect("receipt replay recovers the retained batch");

        assert_eq!(
            store
                .snapshot()
                .expect("unknown-outcome snapshot")
                .block_mutations()
                .len(),
            1,
            "an error-triggered retry must not duplicate a committed journal batch"
        );
        assert_eq!(tokenized_attempts(&store).len(), 2);
        let trace = store.trace().expect("unknown-outcome trace");
        assert!(trace
            .iter()
            .all(|entry| { entry.operation() == FaultOperation::AppendBlockMutationBatch }));
        assert_eq!(
            trace.iter().map(FaultTraceEntry::stage).collect::<Vec<_>>(),
            [
                FaultStage::Attempted,
                FaultStage::Committed,
                FaultStage::AcknowledgementLost,
                FaultStage::Attempted,
                FaultStage::HeldBeforeCommit,
                FaultStage::ReceiptReplayed,
                FaultStage::Succeeded,
            ]
        );

        drop(tx);
        worker
            .await
            .expect("worker task joins")
            .expect("worker exits cleanly");
    }

    #[tokio::test]
    async fn failed_overlay_stays_ahead_of_journal_until_recovery() {
        let store = Arc::new(FaultInjectingStore::new());
        store
            .fail_next_before_commit()
            .expect("fail the first overlay attempt");
        let retry_gate = store
            .hold_next_before_commit()
            .expect("hold the retained overlay retry");
        let (tx, rx) = mpsc::channel(4);
        let (shutdown, _shutdown_rx) = watch::channel(false);
        let worker = spawn_test_worker(Arc::clone(&store), rx, shutdown);
        let (key, overlay) = overlay();
        let batch_id = JournalBatchId::from_bytes([0x48; 16]);
        let expected = vec![mutation(Tick::new(10))];
        let (ack, acknowledged) = oneshot::channel();
        tx.send(request(
            vec![(key, overlay.clone())],
            batch_id,
            expected.clone(),
            Some(ack),
        ))
        .await
        .expect("submit overlay and journal batch");

        acknowledged
            .await
            .expect("worker answers failed overlay barrier")
            .expect_err("failed overlay cannot acknowledge the journal behind it");
        retry_gate.wait_until_reached().await;
        let before_recovery = store.snapshot().expect("pre-recovery snapshot");
        assert!(before_recovery.chunk_overlay(key).is_none());
        assert!(before_recovery.block_mutations().is_empty());
        let attempts = store
            .attempted_state()
            .expect("pre-recovery attempts")
            .operations()
            .to_vec();
        assert_eq!(attempts.len(), 2);
        assert!(attempts
            .iter()
            .all(|attempt| attempt.operation() == FaultOperation::SaveChunkOverlays));
        assert!(attempts
            .iter()
            .all(|attempt| { attempt.chunk_overlays() == Some(&[(key, overlay.clone())]) }));

        let (barrier, barrier_acknowledged) =
            acked_request(JournalBatchId::from_bytes([0x49; 16]), Vec::new());
        tx.send(barrier).await.expect("submit recovery barrier");
        retry_gate.release();
        barrier_acknowledged
            .await
            .expect("worker answers recovery barrier")
            .expect("overlay then journal recover in order");

        let recovered = store.snapshot().expect("recovered overlay snapshot");
        assert_eq!(recovered.chunk_overlay(key), Some(&overlay));
        assert_eq!(recovered.block_mutations().len(), 1);
        let journal_attempts = tokenized_attempts(&store);
        assert_eq!(journal_attempts.len(), 1);
        assert_eq!(journal_attempts[0].journal_batch_id(), Some(batch_id));
        assert_eq!(
            journal_attempts[0].block_mutations(),
            Some(expected.as_slice())
        );

        drop(tx);
        worker
            .await
            .expect("worker task joins")
            .expect("worker exits cleanly");
    }

    #[tokio::test]
    async fn committed_overlay_does_not_hide_failed_journal_barrier() {
        let store = Arc::new(FaultInjectingStore::new());
        let overlay_response = store
            .hold_next_response()
            .expect("hold committed overlay response");
        store
            .fail_next_before_commit()
            .expect("fail the following journal attempt");
        let retry_gate = store
            .hold_next_before_commit()
            .expect("hold retained journal retry");
        let (tx, rx) = mpsc::channel(4);
        let (shutdown, _shutdown_rx) = watch::channel(false);
        let worker = spawn_test_worker(Arc::clone(&store), rx, shutdown);
        let (key, overlay) = overlay();
        let batch_id = JournalBatchId::from_bytes([0x58; 16]);
        let expected = vec![mutation(Tick::new(11))];
        let (ack, acknowledged) = oneshot::channel();
        tx.send(request(
            vec![(key, overlay.clone())],
            batch_id,
            expected,
            Some(ack),
        ))
        .await
        .expect("submit split durability request");

        overlay_response.wait_until_reached().await;
        let overlay_committed = store.snapshot().expect("held overlay snapshot");
        assert_eq!(overlay_committed.chunk_overlay(key), Some(&overlay));
        assert!(overlay_committed.block_mutations().is_empty());
        overlay_response.release();
        acknowledged
            .await
            .expect("worker answers split barrier")
            .expect_err("journal failure keeps the whole barrier unsuccessful");
        retry_gate.wait_until_reached().await;
        assert_eq!(tokenized_attempts(&store).len(), 2);

        let (barrier, barrier_acknowledged) =
            acked_request(JournalBatchId::from_bytes([0x59; 16]), Vec::new());
        tx.send(barrier)
            .await
            .expect("submit journal recovery barrier");
        retry_gate.release();
        barrier_acknowledged
            .await
            .expect("worker answers journal recovery barrier")
            .expect("retained journal obtains its receipt");

        let recovered = store.snapshot().expect("split recovery snapshot");
        assert_eq!(recovered.chunk_overlay(key), Some(&overlay));
        assert_eq!(recovered.block_mutations().len(), 1);
        assert_eq!(tokenized_attempts(&store).len(), 2);

        drop(tx);
        worker
            .await
            .expect("worker task joins")
            .expect("worker exits cleanly");
    }

    #[tokio::test]
    async fn later_journal_slice_failure_keeps_committed_prefix_and_retries_once() {
        let store = Arc::new(FaultInjectingStore::new());
        let first_response = store
            .hold_next_response()
            .expect("hold the first committed slice response");
        let (tx, rx) = mpsc::channel(4);
        let (shutdown, _shutdown_rx) = watch::channel(false);
        let worker = spawn_test_worker(Arc::clone(&store), rx, shutdown);
        let mutations = (0..=MAX_SAVE_BATCH)
            .map(|index| {
                mutation(Tick::new(
                    u64::try_from(index).expect("test mutation index fits u64"),
                ))
            })
            .collect();
        let mut request = StorageFlushRequest::new(Vec::new(), mutations);
        let first_batch_id = request.mutation_batches[0].batch_id;
        let second_batch_id = request.mutation_batches[1].batch_id;
        let (ack, acknowledged) = oneshot::channel();
        request.ack = Some(ack);
        tx.send(request).await.expect("submit two journal slices");

        first_response.wait_until_reached().await;
        let first_committed = store.snapshot().expect("first-slice snapshot");
        assert_eq!(first_committed.block_mutations().len(), MAX_SAVE_BATCH);
        assert!(first_committed.journal_receipt(first_batch_id).is_some());
        assert!(first_committed.journal_receipt(second_batch_id).is_none());
        store
            .fail_next_before_commit()
            .expect("fail the second journal slice");
        let retry_gate = store
            .hold_next_before_commit()
            .expect("hold the retained second slice");
        first_response.release();

        acknowledged
            .await
            .expect("worker answers multi-slice barrier")
            .expect_err("a later slice failure must fail the whole barrier");
        retry_gate.wait_until_reached().await;
        let failed_second = store.snapshot().expect("failed second-slice snapshot");
        assert_eq!(failed_second.block_mutations().len(), MAX_SAVE_BATCH);
        assert!(failed_second.journal_receipt(first_batch_id).is_some());
        assert!(failed_second.journal_receipt(second_batch_id).is_none());
        let attempts = tokenized_attempts(&store);
        assert_eq!(attempts.len(), 3);
        assert_eq!(
            attempts
                .iter()
                .map(FaultStoreAttempt::journal_batch_id)
                .collect::<Vec<_>>(),
            [
                Some(first_batch_id),
                Some(second_batch_id),
                Some(second_batch_id),
            ]
        );
        assert_eq!(
            attempts
                .iter()
                .map(FaultStoreAttempt::item_count)
                .collect::<Vec<_>>(),
            [MAX_SAVE_BATCH, 1, 1]
        );

        let (barrier, barrier_acknowledged) =
            acked_request(JournalBatchId::from_bytes([0x60; 16]), Vec::new());
        tx.send(barrier)
            .await
            .expect("submit second-slice recovery barrier");
        retry_gate.release();
        barrier_acknowledged
            .await
            .expect("worker answers second-slice recovery barrier")
            .expect("retained second slice obtains its receipt");

        let recovered = store.snapshot().expect("multi-slice recovery snapshot");
        assert_eq!(recovered.block_mutations().len(), MAX_SAVE_BATCH + 1);
        assert!(recovered.journal_receipt(first_batch_id).is_some());
        assert!(recovered.journal_receipt(second_batch_id).is_some());
        assert_eq!(tokenized_attempts(&store).len(), 3);

        drop(tx);
        worker
            .await
            .expect("worker task joins")
            .expect("worker exits cleanly");
    }

    #[tokio::test]
    async fn exhausted_retry_budget_requests_shutdown_and_closes_intake() {
        let store = Arc::new(FaultInjectingStore::new());
        for _ in 0..TEST_RETRY_POLICY.max_attempts {
            store
                .return_next_commit_error()
                .expect("schedule bounded commit failure");
        }
        let (tx, rx) = mpsc::channel(4);
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let worker = spawn_test_worker(Arc::clone(&store), rx, shutdown);
        let batch_id = JournalBatchId::from_bytes([0x39; 16]);
        let expected = vec![mutation(Tick::new(9))];
        let (first_request, acknowledged) = acked_request(batch_id, expected.clone());
        tx.send(first_request).await.expect("submit terminal batch");
        let later_batch_id = JournalBatchId::from_bytes([0x3A; 16]);
        tx.send(request(
            Vec::new(),
            later_batch_id,
            vec![mutation(Tick::new(10))],
            None,
        ))
        .await
        .expect("queue later work behind terminal batch");

        acknowledged
            .await
            .expect("worker answers terminal barrier")
            .expect_err("first failed attempt cannot acknowledge durability");
        shutdown_rx
            .changed()
            .await
            .expect("worker keeps shutdown receiver connected");
        assert!(*shutdown_rx.borrow());
        worker
            .await
            .expect("worker task joins")
            .expect_err("retry exhaustion is terminal");
        assert!(tx.is_closed());
        assert!(store
            .snapshot()
            .expect("terminal snapshot")
            .block_mutations()
            .is_empty());
        let attempts = tokenized_attempts(&store);
        assert_eq!(attempts.len(), TEST_RETRY_POLICY.max_attempts as usize);
        assert!(attempts
            .iter()
            .all(|attempt| attempt.journal_batch_id() == Some(batch_id)));
        assert!(attempts
            .iter()
            .all(|attempt| attempt.block_mutations() == Some(expected.as_slice())));
        assert!(attempts
            .iter()
            .all(|attempt| attempt.journal_batch_id() != Some(later_batch_id)));
    }

    #[tokio::test]
    async fn failed_final_drain_returns_error_instead_of_clean_shutdown() {
        let store = Arc::new(FaultInjectingStore::new());
        for _ in 0..TEST_RETRY_POLICY.max_attempts {
            store
                .return_next_commit_error()
                .expect("schedule final-drain failure");
        }
        let (tx, rx) = mpsc::channel(2);
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let worker = spawn_test_worker(Arc::clone(&store), rx, shutdown);
        let batch_id = JournalBatchId::from_bytes([0x68; 16]);
        tx.send(request(
            Vec::new(),
            batch_id,
            vec![mutation(Tick::new(12))],
            None,
        ))
        .await
        .expect("submit final-drain batch");
        drop(tx);

        let error = worker
            .await
            .expect("worker task joins")
            .expect_err("failed final drain must surface its error");
        assert!(matches!(error, ServerError::Internal { .. }));
        shutdown_rx
            .changed()
            .await
            .expect("shutdown sender remains connected");
        assert!(*shutdown_rx.borrow());
        assert_eq!(
            tokenized_attempts(&store).len(),
            TEST_RETRY_POLICY.max_attempts as usize
        );
        assert!(store
            .snapshot()
            .expect("failed final snapshot")
            .block_mutations()
            .is_empty());
    }

    #[test]
    fn request_freezes_one_token_per_bounded_journal_slice() {
        let mutations = (0..=MAX_SAVE_BATCH)
            .map(|index| {
                mutation(Tick::new(
                    u64::try_from(index).expect("test mutation index fits u64"),
                ))
            })
            .collect();
        let request = StorageFlushRequest::new(Vec::new(), mutations);

        assert_eq!(request.mutation_batches.len(), 2);
        assert_eq!(request.mutation_batches[0].records.len(), MAX_SAVE_BATCH);
        assert_eq!(request.mutation_batches[1].records.len(), 1);
    }

    #[test]
    fn production_retry_backoff_is_bounded_and_exponential() {
        assert_eq!(
            RetryPolicy::PRODUCTION.delay_after_failure(1),
            Duration::from_millis(25)
        );
        assert_eq!(
            RetryPolicy::PRODUCTION.delay_after_failure(2),
            Duration::from_millis(50)
        );
        assert_eq!(RetryPolicy::PRODUCTION.max_attempts, 3);
    }
}
