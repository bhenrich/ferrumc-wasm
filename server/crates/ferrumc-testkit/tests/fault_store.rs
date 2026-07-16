use std::sync::Arc;

use ferrumc_core::{DimensionId, ServerError, WorldId};
use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_storage::{
    BlockMutationLogRecord, ChunkKey, MutationActor, MutationLogCause, SchemaVersion, WorldStore,
};
use ferrumc_testkit::{
    FaultInjectingStore, FaultStage, FaultStoreAttempt, FaultTraceEntry, MAX_FAULT_SCHEDULE,
};
use ferrumc_world::BlockStateId;

fn mutation(tick: u64) -> BlockMutationLogRecord {
    BlockMutationLogRecord::new(
        SchemaVersion::new(1),
        99,
        tick,
        MutationActor::System,
        BlockPos::new(1, 64, 1),
        BlockStateId::new(1),
        BlockStateId::new(2),
        MutationLogCause::Test,
    )
}

fn chunk_key() -> ChunkKey {
    ChunkKey::new(WorldId::new(1), DimensionId::new(2), ChunkPos::new(3, 4))
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
