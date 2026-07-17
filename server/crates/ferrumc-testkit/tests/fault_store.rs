use std::sync::Arc;

use ferrumc_core::{DimensionId, ServerError, WorldId};
use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_storage::{
    BlockMutationLogRecord, ChunkKey, JournalBatchId, MutationActor, MutationLogCause,
    SchemaVersion, WorldStore, MAX_SAVE_BATCH,
};
use ferrumc_testkit::{
    FaultInjectingStore, FaultStage, FaultStoreAttempt, FaultTraceEntry, MAX_FAULT_SCHEDULE,
};
use ferrumc_world::BlockStateId;

fn mutation_with_id(id: u64, tick: u64) -> BlockMutationLogRecord {
    BlockMutationLogRecord::new(
        SchemaVersion::new(1),
        id,
        tick,
        MutationActor::System,
        BlockPos::new(1, 64, 1),
        BlockStateId::new(1),
        BlockStateId::new(2),
        MutationLogCause::Test,
    )
}

fn mutation(tick: u64) -> BlockMutationLogRecord {
    mutation_with_id(99, tick)
}

fn chunk_key() -> ChunkKey {
    ChunkKey::new(WorldId::new(1), DimensionId::new(2), ChunkPos::new(3, 4))
}

#[tokio::test]
async fn fault_store_replays_receipt_after_lost_ack() {
    let store = FaultInjectingStore::new();
    let batch_id = JournalBatchId::from_bytes([0x16; 16]);
    store
        .lose_next_ack_after_commit()
        .expect("schedule lost acknowledgement");

    store
        .append_block_mutation_batch(batch_id, vec![mutation(1)])
        .await
        .expect_err("the first commit acknowledgement is lost");
    let committed = store.snapshot().expect("committed snapshot");
    assert_eq!(committed.block_mutations().len(), 1);
    let original_receipt = committed
        .journal_receipt(batch_id)
        .expect("receipt is visible before its acknowledgement is lost");

    let receipt = store
        .append_block_mutation_batch(batch_id, vec![mutation_with_id(7, 1)])
        .await
        .expect("same-token retry replays the receipt");
    assert_eq!(receipt, original_receipt);
    assert_eq!(receipt.batch_id(), batch_id);
    assert_eq!(receipt.first_id(), Some(0));
    assert_eq!(receipt.last_id(), Some(0));
    assert_eq!(receipt.len(), 1);
    let replayed = store.snapshot().expect("replayed snapshot");
    assert_eq!(replayed.block_mutations().len(), 1);
    assert_eq!(replayed.committed_operations(), 1);
    assert_eq!(replayed.successful_responses(), 1);

    let attempts = store.attempted_state().expect("receipt attempts");
    assert_eq!(attempts.operations().len(), 2);
    assert!(attempts
        .operations()
        .iter()
        .all(|attempt| attempt.journal_batch_id() == Some(batch_id)));
    assert_eq!(
        attempts
            .operations()
            .iter()
            .map(FaultStoreAttempt::operation_id)
            .collect::<Vec<_>>(),
        [0, 3]
    );

    let trace = store.trace().expect("receipt trace");
    assert!(trace.iter().all(|entry| {
        entry.operation() == ferrumc_testkit::FaultOperation::AppendBlockMutationBatch
    }));
    assert_eq!(
        trace.iter().map(FaultTraceEntry::stage).collect::<Vec<_>>(),
        [
            FaultStage::Attempted,
            FaultStage::Committed,
            FaultStage::AcknowledgementLost,
            FaultStage::Attempted,
            FaultStage::ReceiptReplayed,
            FaultStage::Succeeded,
        ]
    );
    assert_eq!(
        trace
            .iter()
            .map(FaultTraceEntry::operation_id)
            .collect::<Vec<_>>(),
        [0, 0, 0, 3, 3, 3]
    );
}

#[tokio::test]
async fn receipt_payload_mismatch_after_lost_ack_is_typed_and_non_mutating() {
    let store = FaultInjectingStore::new();
    let batch_id = JournalBatchId::from_bytes([0x17; 16]);
    store
        .lose_next_ack_after_commit()
        .expect("schedule lost acknowledgement");
    store
        .append_block_mutation_batch(batch_id, vec![mutation(10)])
        .await
        .expect_err("commit acknowledgement is lost");

    let mismatch = store
        .append_block_mutation_batch(batch_id, vec![mutation(11)])
        .await
        .expect_err("different normalized payload conflicts");
    assert!(matches!(mismatch, ServerError::InvalidState(_)));
    let original = store
        .append_block_mutation_batch(batch_id, vec![mutation_with_id(0, 10)])
        .await
        .expect("the original normalized payload remains replayable");

    let snapshot = store.snapshot().expect("snapshot after conflict");
    assert_eq!(snapshot.block_mutations().len(), 1);
    assert_eq!(snapshot.block_mutations()[0].tick(), 10);
    assert_eq!(snapshot.journal_receipt(batch_id), Some(original));
    assert_eq!(snapshot.committed_operations(), 1);
    let trace = store.trace().expect("mismatch trace");
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.stage() == FaultStage::Committed)
            .count(),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.stage() == FaultStage::CommitFailure)
            .count(),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.stage() == FaultStage::ReceiptReplayed)
            .count(),
        1
    );
    for operation_id in [0, 3, 5] {
        assert!(trace
            .iter()
            .filter(|entry| entry.operation_id() == operation_id)
            .all(|entry| entry.operation()
                == ferrumc_testkit::FaultOperation::AppendBlockMutationBatch));
    }
}

#[tokio::test]
async fn receipt_path_preserves_pre_and_post_commit_cutpoints() {
    let store = Arc::new(FaultInjectingStore::new());

    store
        .fail_next_operation()
        .expect("schedule generic failure");
    store
        .append_block_mutation_batch(JournalBatchId::from_bytes([1; 16]), vec![mutation(1)])
        .await
        .expect_err("generic failure stops before commit");
    store
        .fail_next_before_commit()
        .expect("schedule pre-commit failure");
    store
        .append_block_mutation_batch(JournalBatchId::from_bytes([2; 16]), vec![mutation(2)])
        .await
        .expect_err("pre-commit failure stops before commit");
    store
        .return_next_commit_error()
        .expect("schedule commit error");
    store
        .append_block_mutation_batch(JournalBatchId::from_bytes([3; 16]), vec![mutation(3)])
        .await
        .expect_err("commit error leaves no receipt");

    let held_batch = JournalBatchId::from_bytes([4; 16]);
    let commit_gate = store
        .hold_next_before_commit()
        .expect("schedule commit hold");
    let pending_commit = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .append_block_mutation_batch(held_batch, vec![mutation(4)])
                .await
        })
    };
    commit_gate.wait_until_reached().await;
    assert!(store
        .snapshot()
        .expect("pre-commit snapshot")
        .journal_receipt(held_batch)
        .is_none());
    commit_gate.release();
    pending_commit
        .await
        .expect("held commit task completes")
        .expect("held commit succeeds");

    let held_response = JournalBatchId::from_bytes([5; 16]);
    let response_gate = store.hold_next_response().expect("schedule response hold");
    let pending_response = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .append_block_mutation_batch(held_response, vec![mutation(5)])
                .await
        })
    };
    response_gate.wait_until_reached().await;
    assert!(store
        .snapshot()
        .expect("post-commit snapshot")
        .journal_receipt(held_response)
        .is_some());
    response_gate.release();
    pending_response
        .await
        .expect("held response task completes")
        .expect("held response succeeds");

    let snapshot = store.snapshot().expect("cutpoint snapshot");
    assert_eq!(snapshot.block_mutations().len(), 2);
    assert_eq!(snapshot.block_mutations()[0].id(), 0);
    assert_eq!(snapshot.block_mutations()[1].id(), 1);
}

#[tokio::test]
async fn receipt_replay_proceeds_while_original_response_is_held() {
    let store = Arc::new(FaultInjectingStore::new());
    let batch_id = JournalBatchId::from_bytes([0x51; 16]);
    let response_gate = store.hold_next_response().expect("schedule response hold");
    let original = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .append_block_mutation_batch(batch_id, vec![mutation(51)])
                .await
        })
    };
    response_gate.wait_until_reached().await;
    assert!(!original.is_finished());

    let replay = store
        .append_block_mutation_batch(batch_id, vec![mutation_with_id(0, 51)])
        .await
        .expect("retry is not blocked behind response delivery");
    assert_eq!(
        store
            .snapshot()
            .expect("held snapshot")
            .block_mutations()
            .len(),
        1
    );

    response_gate.release();
    let original = original
        .await
        .expect("original task completes")
        .expect("original response succeeds");
    assert_eq!(original, replay);
    let trace = store.trace().expect("held replay trace");
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.stage() == FaultStage::Committed)
            .count(),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.stage() == FaultStage::ReceiptReplayed)
            .count(),
        1
    );
}

#[tokio::test]
async fn legacy_and_receipt_appends_share_one_sequence() {
    let store = FaultInjectingStore::new();
    store
        .append_block_mutations(vec![mutation(1)])
        .await
        .expect("legacy append");
    let receipt = store
        .append_block_mutation_batch(
            JournalBatchId::from_bytes([6; 16]),
            vec![mutation(2), mutation(3)],
        )
        .await
        .expect("receipt append");
    store
        .append_block_mutations(vec![mutation(4)])
        .await
        .expect("second legacy append");

    assert_eq!(receipt.first_id(), Some(1));
    assert_eq!(receipt.last_id(), Some(2));
    assert_eq!(
        store
            .snapshot()
            .expect("mixed journal snapshot")
            .block_mutations()
            .iter()
            .map(BlockMutationLogRecord::id)
            .collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
}

#[tokio::test]
async fn old_receipt_range_survives_an_intervening_legacy_append() {
    let store = FaultInjectingStore::new();
    let batch_id = JournalBatchId::from_bytes([0x61; 16]);
    let original = store
        .append_block_mutation_batch(batch_id, vec![mutation(1)])
        .await
        .expect("original receipt");
    store
        .append_block_mutations(vec![mutation(2)])
        .await
        .expect("intervening legacy append");
    let replay = store
        .append_block_mutation_batch(batch_id, vec![mutation_with_id(0, 1)])
        .await
        .expect("old token replays after sequence advances");

    assert_eq!(replay, original);
    assert_eq!(replay.first_id(), Some(0));
    assert_eq!(replay.last_id(), Some(0));
    assert_eq!(
        store
            .snapshot()
            .expect("intervening append snapshot")
            .block_mutations()
            .iter()
            .map(BlockMutationLogRecord::id)
            .collect::<Vec<_>>(),
        [0, 1]
    );
}

#[tokio::test]
async fn concurrent_same_token_calls_commit_once_and_share_a_receipt() {
    let store = Arc::new(FaultInjectingStore::new());
    let batch_id = JournalBatchId::from_bytes([7; 16]);
    let first = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .append_block_mutation_batch(batch_id, vec![mutation(7)])
                .await
        })
    };
    let second = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .append_block_mutation_batch(batch_id, vec![mutation_with_id(0, 7)])
                .await
        })
    };

    let first = first
        .await
        .expect("first task completes")
        .expect("first receipt");
    let second = second
        .await
        .expect("second task completes")
        .expect("second receipt");
    assert_eq!(first, second);
    let snapshot = store.snapshot().expect("concurrent snapshot");
    assert_eq!(snapshot.block_mutations().len(), 1);
    assert_eq!(snapshot.committed_operations(), 1);
    let trace = store.trace().expect("concurrent trace");
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.stage() == FaultStage::Committed)
            .count(),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.stage() == FaultStage::ReceiptReplayed)
            .count(),
        1
    );
}

#[tokio::test]
async fn concurrent_conflicting_payloads_produce_one_commit_and_one_typed_conflict() {
    let store = Arc::new(FaultInjectingStore::new());
    let batch_id = JournalBatchId::from_bytes([0x71; 16]);
    let first = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .append_block_mutation_batch(batch_id, vec![mutation(71)])
                .await
        })
    };
    let second = {
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            store
                .append_block_mutation_batch(batch_id, vec![mutation(72)])
                .await
        })
    };
    let first = first.await.expect("first conflicting task completes");
    let second = second.await.expect("second conflicting task completes");

    let (receipt, winning_tick) = match (first, second) {
        (Ok(receipt), Err(ServerError::InvalidState(_))) => (receipt, 71),
        (Err(ServerError::InvalidState(_)), Ok(receipt)) => (receipt, 72),
        outcomes => panic!("expected one receipt and one typed conflict, got {outcomes:?}"),
    };
    assert_eq!(receipt.batch_id(), batch_id);
    let snapshot = store.snapshot().expect("conflicting snapshot");
    assert_eq!(snapshot.block_mutations().len(), 1);
    assert_eq!(snapshot.block_mutations()[0].tick(), winning_tick);
    assert_eq!(snapshot.committed_operations(), 1);
    let trace = store.trace().expect("conflicting trace");
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.stage() == FaultStage::Committed)
            .count(),
        1
    );
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.stage() == FaultStage::CommitFailure)
            .count(),
        1
    );
}

#[tokio::test]
async fn closed_receipt_response_preserves_commit_for_replay() {
    let store = FaultInjectingStore::new();
    let batch_id = JournalBatchId::from_bytes([0x72; 16]);
    store.close_responses().expect("close responses");
    let first = store
        .append_block_mutation_batch(batch_id, vec![mutation(72)])
        .await
        .expect_err("closed response hides the committed receipt");
    assert!(matches!(first, ServerError::InvalidState(_)));
    let retry = store
        .append_block_mutation_batch(batch_id, vec![mutation_with_id(0, 72)])
        .await
        .expect_err("the replay response remains closed");
    assert!(matches!(retry, ServerError::InvalidState(_)));

    let snapshot = store.snapshot().expect("closed response snapshot");
    assert_eq!(snapshot.block_mutations().len(), 1);
    assert!(snapshot.journal_receipt(batch_id).is_some());
    assert_eq!(snapshot.committed_operations(), 1);
    let trace = store.trace().expect("closed response trace");
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry.stage() == FaultStage::ReceiptReplayed)
            .count(),
        1
    );
}

#[tokio::test]
async fn empty_receipt_reserves_its_token_without_consuming_a_sequence() {
    let store = FaultInjectingStore::new();
    let empty_id = JournalBatchId::from_bytes([8; 16]);
    let empty = store
        .append_block_mutation_batch(empty_id, Vec::new())
        .await
        .expect("empty receipt");
    assert!(empty.is_empty());
    assert_eq!(
        store
            .append_block_mutation_batch(empty_id, Vec::new())
            .await
            .expect("empty replay"),
        empty
    );
    let conflict = store
        .append_block_mutation_batch(empty_id, vec![mutation(1)])
        .await
        .expect_err("non-empty reuse conflicts");
    assert!(matches!(conflict, ServerError::InvalidState(_)));

    let first_non_empty = store
        .append_block_mutation_batch(JournalBatchId::from_bytes([9; 16]), vec![mutation(2)])
        .await
        .expect("first non-empty receipt");
    assert_eq!(first_non_empty.first_id(), Some(0));
    assert_eq!(
        store
            .snapshot()
            .expect("empty snapshot")
            .block_mutations()
            .len(),
        1
    );
}

#[tokio::test]
async fn oversized_receipt_batch_creates_neither_receipt_nor_sequence_gap() {
    let store = FaultInjectingStore::new();
    let oversized_id = JournalBatchId::from_bytes([10; 16]);
    let error = store
        .append_block_mutation_batch(
            oversized_id,
            vec![mutation(1); MAX_SAVE_BATCH.saturating_add(1)],
        )
        .await
        .expect_err("oversized batch is rejected");
    assert!(matches!(error, ServerError::Capacity(_)));
    assert!(store
        .snapshot()
        .expect("rejected snapshot")
        .journal_receipt(oversized_id)
        .is_none());

    let receipt = store
        .append_block_mutation_batch(
            JournalBatchId::from_bytes([11; 16]),
            vec![mutation(2); MAX_SAVE_BATCH],
        )
        .await
        .expect("maximum-size batch");
    assert_eq!(receipt.first_id(), Some(0));
    assert_eq!(receipt.last_id(), Some(4095));
    assert_eq!(receipt.len(), MAX_SAVE_BATCH);

    let next = store
        .append_block_mutation_batch(JournalBatchId::from_bytes([12; 16]), vec![mutation(3)])
        .await
        .expect("batch after maximum boundary");
    assert_eq!(next.first_id(), Some(4096));
}

fn assert_attempted_and_committed_state(store: &FaultInjectingStore) {
    let attempts = store.attempted_state().expect("attempted state");
    let attempted_ticks: Vec<_> = attempts
        .operations()
        .iter()
        .map(|attempt| attempt.block_mutations().expect("mutation append")[0].tick())
        .collect();
    assert_eq!(attempted_ticks, [1, 2, 3, 4, 5]);

    let snapshot = store.snapshot().expect("snapshot");
    let committed_ticks: Vec<_> = snapshot
        .block_mutations()
        .iter()
        .map(BlockMutationLogRecord::tick)
        .collect();
    assert_eq!(committed_ticks, [3, 4, 5]);
    assert_eq!(
        attempts
            .operations()
            .iter()
            .map(FaultStoreAttempt::operation_id)
            .collect::<Vec<_>>(),
        [0, 2, 4, 8, 12]
    );
}

fn assert_exact_trace(store: &FaultInjectingStore) {
    let trace = store.trace().expect("trace");
    let stages: Vec<_> = trace.iter().map(FaultTraceEntry::stage).collect();
    assert_eq!(
        stages,
        [
            FaultStage::Attempted,
            FaultStage::BeforeCommitFailure,
            FaultStage::Attempted,
            FaultStage::CommitFailure,
            FaultStage::Attempted,
            FaultStage::HeldBeforeCommit,
            FaultStage::Committed,
            FaultStage::Succeeded,
            FaultStage::Attempted,
            FaultStage::Committed,
            FaultStage::HeldResponse,
            FaultStage::Succeeded,
            FaultStage::Attempted,
            FaultStage::Committed,
            FaultStage::AcknowledgementLost,
        ]
    );
    assert_eq!(
        trace
            .iter()
            .map(FaultTraceEntry::operation_id)
            .collect::<Vec<_>>(),
        [0, 0, 2, 2, 4, 4, 4, 4, 8, 8, 8, 8, 12, 12, 12]
    );
    assert!(trace
        .windows(2)
        .all(|pair| pair[0].sequence() < pair[1].sequence()));
}

#[tokio::test]
async fn fault_store_exposes_deterministic_commit_cutpoints() {
    let store = Arc::new(FaultInjectingStore::new());

    store
        .fail_next_before_commit()
        .expect("schedule pre-commit failure");
    store
        .append_block_mutations(vec![mutation(1)])
        .await
        .expect_err("pre-commit failure");
    assert!(store
        .snapshot()
        .expect("snapshot")
        .block_mutations()
        .is_empty());

    store
        .return_next_commit_error()
        .expect("schedule commit error");
    store
        .append_block_mutations(vec![mutation(2)])
        .await
        .expect_err("commit error");
    assert!(store
        .snapshot()
        .expect("snapshot")
        .block_mutations()
        .is_empty());

    let commit_gate = store
        .hold_next_before_commit()
        .expect("schedule commit hold");
    let pending = {
        let store = Arc::clone(&store);
        tokio::spawn(async move { store.append_block_mutations(vec![mutation(3)]).await })
    };
    commit_gate.wait_until_reached().await;
    assert!(store
        .snapshot()
        .expect("snapshot")
        .block_mutations()
        .is_empty());
    assert!(!pending.is_finished());
    commit_gate.release();
    pending
        .await
        .expect("task completes")
        .expect("commit succeeds");

    let response_gate = store.hold_next_response().expect("schedule response hold");
    let awaiting_ack = {
        let store = Arc::clone(&store);
        tokio::spawn(async move { store.append_block_mutations(vec![mutation(4)]).await })
    };
    response_gate.wait_until_reached().await;
    assert_eq!(
        store.snapshot().expect("snapshot").block_mutations().len(),
        2,
        "response hold occurs after the commit"
    );
    assert!(!awaiting_ack.is_finished());
    response_gate.release();
    awaiting_ack
        .await
        .expect("task completes")
        .expect("ack succeeds");

    store
        .lose_next_ack_after_commit()
        .expect("schedule lost ack");
    store
        .append_block_mutations(vec![mutation(5)])
        .await
        .expect_err("commit succeeds but acknowledgement is lost");
    let snapshot = store.snapshot().expect("snapshot");
    assert_eq!(snapshot.block_mutations().len(), 3);
    assert_eq!(snapshot.committed_operations(), 3);
    assert_attempted_and_committed_state(&store);
    assert_exact_trace(&store);
}

#[tokio::test]
async fn injected_and_commit_failures_never_mutate_committed_state() {
    let store = FaultInjectingStore::new();
    store
        .fail_next_operation()
        .expect("schedule operation failure");
    store
        .append_block_mutations(vec![mutation(1)])
        .await
        .expect_err("injected failure");
    store
        .return_next_commit_error()
        .expect("schedule commit error");
    store
        .append_block_mutations(vec![mutation(2)])
        .await
        .expect_err("commit error");

    let snapshot = store.snapshot().expect("snapshot");
    assert!(snapshot.block_mutations().is_empty());
    assert_eq!(snapshot.attempted_operations(), 2);
    assert_eq!(snapshot.committed_operations(), 0);
    let stages: Vec<_> = store
        .trace()
        .expect("trace")
        .into_iter()
        .map(|entry| entry.stage())
        .collect();
    assert!(stages.contains(&FaultStage::InjectedFailure));
    assert!(stages.contains(&FaultStage::CommitFailure));
}

#[tokio::test]
async fn closed_request_and_response_sides_have_distinct_commit_results() {
    let request_closed = FaultInjectingStore::new();
    request_closed
        .fail_next_operation()
        .expect("schedule retained fault");
    request_closed.close_requests().expect("close requests");
    let request_error = request_closed
        .append_block_mutations(vec![mutation(1)])
        .await
        .expect_err("closed request side");
    assert!(matches!(request_error, ServerError::InvalidState(_)));
    assert_eq!(request_closed.scheduled_faults().expect("schedule"), 1);
    assert!(request_closed
        .snapshot()
        .expect("snapshot")
        .block_mutations()
        .is_empty());

    let response_closed = FaultInjectingStore::new();
    response_closed.close_responses().expect("close responses");
    let response_error = response_closed
        .append_block_mutations(vec![mutation(2)])
        .await
        .expect_err("closed response side");
    assert!(matches!(response_error, ServerError::InvalidState(_)));
    assert_eq!(
        response_closed
            .snapshot()
            .expect("snapshot")
            .block_mutations()
            .len(),
        1
    );
    assert!(response_closed
        .trace()
        .expect("trace")
        .iter()
        .any(|entry| entry.stage() == FaultStage::ResponseClosed));
}

#[tokio::test]
async fn fault_schedule_is_fifo_and_commit_faults_skip_reads() {
    let store = FaultInjectingStore::new();
    store
        .fail_next_before_commit()
        .expect("schedule write fault");

    assert!(store
        .load_chunk(chunk_key())
        .await
        .expect("read succeeds")
        .is_none());
    assert_eq!(store.scheduled_faults().expect("schedule"), 1);
    store
        .append_block_mutations(vec![mutation(1)])
        .await
        .expect_err("write consumes scheduled failure");
    assert_eq!(store.scheduled_faults().expect("schedule"), 0);
    store
        .append_block_mutations(vec![mutation(2)])
        .await
        .expect("fault is one-shot");
}

#[tokio::test]
async fn gates_handle_early_release_and_late_response_closure() {
    let early_store = FaultInjectingStore::new();
    let early_gate = early_store
        .hold_next_before_commit()
        .expect("schedule early-released gate");
    early_gate.release();
    early_store
        .append_block_mutations(vec![mutation(1)])
        .await
        .expect("early release is retained");
    early_gate.wait_until_reached().await;
    assert!(early_gate.is_reached());

    let late_store = Arc::new(FaultInjectingStore::new());
    let response_gate = late_store
        .hold_next_response()
        .expect("schedule response hold");
    let pending = {
        let store = Arc::clone(&late_store);
        tokio::spawn(async move { store.append_block_mutations(vec![mutation(2)]).await })
    };
    response_gate.wait_until_reached().await;
    assert_eq!(
        late_store
            .snapshot()
            .expect("committed snapshot")
            .block_mutations()
            .len(),
        1
    );
    late_store.close_responses().expect("close responses");
    response_gate.release();
    let error = pending
        .await
        .expect("task completes")
        .expect_err("late response closure is observed");
    assert!(matches!(error, ServerError::InvalidState(_)));
}

#[test]
fn fault_schedule_is_bounded() {
    let store = FaultInjectingStore::new();
    for _ in 0..MAX_FAULT_SCHEDULE {
        store.fail_next_operation().expect("within schedule bound");
    }
    let error = store
        .fail_next_operation()
        .expect_err("schedule must stay bounded");
    assert_eq!(error.capacity(), Some(MAX_FAULT_SCHEDULE));
}
