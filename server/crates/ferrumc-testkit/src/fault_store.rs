//! Deterministic storage fault injection for durability tests.
//!
//! [`FaultInjectingStore`] implements [`WorldStore`] and [`PlayerStore`] entirely
//! in memory and exposes explicit FIFO cutpoints around each operation. Tests
//! can distinguish an attempted request from committed state, stop an operation
//! immediately before commit, hold either side of the commit boundary without a
//! sleep, and model a commit whose acknowledgement never reaches the caller.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use ferrumc_core::{PlayerId, Result as ServerResult, ServerError};
use ferrumc_storage::{
    BlockMutationLogRecord, ChunkKey, ChunkOverlayRecord, ChunkRecord, EntityKey, EntityRecord,
    InMemoryStore, JournalAppendReceipt, JournalBatchId, PlayerRecord, PlayerStore, StorageError,
    WorldStore, MAX_SAVE_BATCH,
};
use tokio::sync::{Mutex as AsyncMutex, Notify};

/// Maximum number of not-yet-consumed fault actions in one store.
///
/// The explicit bound prevents a test bug from building an unbounded control
/// queue. The operation trace and attempted-payload log are caller-driven
/// diagnostic histories for trusted test inputs; neither is a request queue.
pub const MAX_FAULT_SCHEDULE: usize = 64;

/// A storage operation recorded by [`FaultInjectingStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FaultOperation {
    /// Load one full chunk record.
    LoadChunk,
    /// Save one full chunk record.
    SaveChunk,
    /// Save a batch of full chunk records.
    SaveChunks,
    /// Delete one full chunk record.
    DeleteChunk,
    /// Load one chunk overlay record.
    LoadChunkOverlay,
    /// Save a batch of chunk overlay records.
    SaveChunkOverlays,
    /// Append a batch to the block-mutation journal.
    AppendBlockMutations,
    /// Idempotently append a tokenized batch to the block-mutation journal.
    AppendBlockMutationBatch,
    /// Load one entity record.
    LoadEntity,
    /// Save one entity record.
    SaveEntity,
    /// Save a batch of entity records.
    SaveEntities,
    /// Delete one entity record.
    DeleteEntity,
    /// Load one player record.
    LoadPlayer,
    /// Save one player record.
    SavePlayer,
    /// Delete one player record.
    DeletePlayer,
}

impl FaultOperation {
    /// Returns whether the operation can change committed state.
    fn is_mutating(self) -> bool {
        !matches!(
            self,
            Self::LoadChunk | Self::LoadChunkOverlay | Self::LoadEntity | Self::LoadPlayer
        )
    }
}

/// A deterministic stage in a fault-store operation trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FaultStage {
    /// The request was accepted and the operation began.
    Attempted,
    /// A generic injected failure stopped the operation before any effect.
    InjectedFailure,
    /// A write failed at the explicit point immediately before commit.
    BeforeCommitFailure,
    /// A write reached a deterministic hold immediately before commit.
    HeldBeforeCommit,
    /// The backend commit failed and changed no committed state.
    CommitFailure,
    /// A mutating operation changed committed state atomically.
    Committed,
    /// A tokenized journal retry returned an already-committed receipt.
    ReceiptReplayed,
    /// The operation committed or read its result, then held the response.
    HeldResponse,
    /// The operation committed, but its acknowledgement was deliberately lost.
    AcknowledgementLost,
    /// The closed request side rejected the operation before it was attempted.
    RequestClosed,
    /// The response side was closed after the operation executed.
    ResponseClosed,
    /// The caller received a successful response.
    Succeeded,
}

/// One immutable entry in the ordered fault-store trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultTraceEntry {
    sequence: u64,
    operation_id: u64,
    operation: FaultOperation,
    stage: FaultStage,
    item_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
enum FaultAttemptPayload {
    ChunkKey(ChunkKey),
    Chunks(Vec<(ChunkKey, ChunkRecord)>),
    Overlays(Vec<(ChunkKey, ChunkOverlayRecord)>),
    Mutations(Vec<BlockMutationLogRecord>),
    MutationBatch {
        batch_id: JournalBatchId,
        mutations: Vec<BlockMutationLogRecord>,
    },
    EntityKey(EntityKey),
    Entities(Vec<(EntityKey, EntityRecord)>),
    PlayerId(PlayerId),
    Players(Vec<(PlayerId, PlayerRecord)>),
}

impl FaultAttemptPayload {
    fn item_count(&self) -> usize {
        match self {
            Self::ChunkKey(_) | Self::EntityKey(_) | Self::PlayerId(_) => 1,
            Self::Chunks(records) => records.len(),
            Self::Overlays(records) => records.len(),
            Self::Mutations(records) => records.len(),
            Self::MutationBatch { mutations, .. } => mutations.len(),
            Self::Entities(records) => records.len(),
            Self::Players(records) => records.len(),
        }
    }
}

/// One accepted storage attempt, including its exact caller-supplied payload.
///
/// This record is captured before any injected failure or commit validation.
/// Use [`FaultInjectingStore::attempted_state`] to distinguish what callers
/// tried from the durable values returned by [`FaultInjectingStore::snapshot`].
#[derive(Debug, Clone, PartialEq)]
pub struct FaultStoreAttempt {
    operation_id: u64,
    operation: FaultOperation,
    payload: FaultAttemptPayload,
}

impl FaultStoreAttempt {
    /// Returns the stable ID shared with this call's trace entries.
    #[must_use]
    pub fn operation_id(&self) -> u64 {
        self.operation_id
    }

    /// Returns the attempted storage operation.
    #[must_use]
    pub fn operation(&self) -> FaultOperation {
        self.operation
    }

    /// Returns the number of caller-supplied records in the attempt.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.payload.item_count()
    }

    /// Returns the requested chunk key for a chunk load, overlay load, or
    /// chunk delete.
    #[must_use]
    pub fn chunk_key(&self) -> Option<ChunkKey> {
        match self.payload {
            FaultAttemptPayload::ChunkKey(key) => Some(key),
            _ => None,
        }
    }

    /// Returns the caller-supplied chunk records for a single or batched save.
    #[must_use]
    pub fn chunks(&self) -> Option<&[(ChunkKey, ChunkRecord)]> {
        match &self.payload {
            FaultAttemptPayload::Chunks(records) => Some(records),
            _ => None,
        }
    }

    /// Returns the caller-supplied overlay records for an overlay save.
    #[must_use]
    pub fn chunk_overlays(&self) -> Option<&[(ChunkKey, ChunkOverlayRecord)]> {
        match &self.payload {
            FaultAttemptPayload::Overlays(records) => Some(records),
            _ => None,
        }
    }

    /// Returns the caller-supplied records for a mutation-journal append.
    ///
    /// These retain their provisional IDs. The committed snapshot instead
    /// exposes storage-assigned durable IDs, mirroring the real store contract.
    #[must_use]
    pub fn block_mutations(&self) -> Option<&[BlockMutationLogRecord]> {
        match &self.payload {
            FaultAttemptPayload::Mutations(records)
            | FaultAttemptPayload::MutationBatch {
                mutations: records, ..
            } => Some(records),
            _ => None,
        }
    }

    /// Returns the idempotency token for a tokenized journal append.
    #[must_use]
    pub fn journal_batch_id(&self) -> Option<JournalBatchId> {
        match self.payload {
            FaultAttemptPayload::MutationBatch { batch_id, .. } => Some(batch_id),
            _ => None,
        }
    }

    /// Returns the requested entity key for an entity load or delete.
    #[must_use]
    pub fn entity_key(&self) -> Option<EntityKey> {
        match self.payload {
            FaultAttemptPayload::EntityKey(key) => Some(key),
            _ => None,
        }
    }

    /// Returns the caller-supplied entity records for a single or batched save.
    #[must_use]
    pub fn entities(&self) -> Option<&[(EntityKey, EntityRecord)]> {
        match &self.payload {
            FaultAttemptPayload::Entities(records) => Some(records),
            _ => None,
        }
    }

    /// Returns the requested player ID for a player load or delete.
    #[must_use]
    pub fn player_id(&self) -> Option<PlayerId> {
        match self.payload {
            FaultAttemptPayload::PlayerId(id) => Some(id),
            _ => None,
        }
    }

    /// Returns the caller-supplied player record for a player save.
    #[must_use]
    pub fn players(&self) -> Option<&[(PlayerId, PlayerRecord)]> {
        match &self.payload {
            FaultAttemptPayload::Players(records) => Some(records),
            _ => None,
        }
    }
}

/// An immutable snapshot of all accepted calls, independent of commit outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct FaultStoreAttemptedState {
    operations: Vec<FaultStoreAttempt>,
}

impl FaultStoreAttemptedState {
    /// Returns accepted operations in deterministic call order.
    #[must_use]
    pub fn operations(&self) -> &[FaultStoreAttempt] {
        &self.operations
    }
}

impl FaultTraceEntry {
    /// Returns the monotonically increasing trace sequence number.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the stable ID shared by every stage of one operation.
    ///
    /// Use this to correlate interleaved trace entries from concurrent calls.
    #[must_use]
    pub fn operation_id(&self) -> u64 {
        self.operation_id
    }

    /// Returns the operation this entry describes.
    #[must_use]
    pub fn operation(&self) -> FaultOperation {
        self.operation
    }

    /// Returns the operation stage this entry records.
    #[must_use]
    pub fn stage(&self) -> FaultStage {
        self.stage
    }

    /// Returns the number of records carried by the operation.
    ///
    /// Single-record and load/delete operations report one; batch operations
    /// report their exact batch length, including zero.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.item_count
    }
}

/// A deterministic synchronization gate returned when a cutpoint is scheduled.
///
/// Wait for [`wait_until_reached`](Self::wait_until_reached) before inspecting
/// state at the cutpoint, then call [`release`](Self::release) to let the
/// operation continue. Releasing before the operation arrives is supported and
/// never loses the signal.
#[derive(Clone)]
pub struct FaultGate {
    inner: Arc<FaultGateInner>,
}

struct FaultGateInner {
    reached: AtomicBool,
    released: AtomicBool,
    reached_notify: Notify,
    released_notify: Notify,
}

impl fmt::Debug for FaultGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FaultGate")
            .field("reached", &self.is_reached())
            .field("released", &self.is_released())
            .finish()
    }
}

impl FaultGate {
    fn new() -> Self {
        Self {
            inner: Arc::new(FaultGateInner {
                reached: AtomicBool::new(false),
                released: AtomicBool::new(false),
                reached_notify: Notify::new(),
                released_notify: Notify::new(),
            }),
        }
    }

    /// Waits until the scheduled operation reaches this cutpoint.
    pub async fn wait_until_reached(&self) {
        loop {
            if self.is_reached() {
                return;
            }
            let notified = self.inner.reached_notify.notified();
            if self.is_reached() {
                return;
            }
            notified.await;
        }
    }

    /// Releases the held operation. Repeated calls are harmless.
    pub fn release(&self) {
        if !self.inner.released.swap(true, Ordering::AcqRel) {
            self.inner.released_notify.notify_waiters();
        }
    }

    /// Returns whether the operation has reached this cutpoint.
    #[must_use]
    pub fn is_reached(&self) -> bool {
        self.inner.reached.load(Ordering::Acquire)
    }

    /// Returns whether this gate has been released.
    #[must_use]
    pub fn is_released(&self) -> bool {
        self.inner.released.load(Ordering::Acquire)
    }

    async fn reach_and_wait(&self) {
        if !self.inner.reached.swap(true, Ordering::AcqRel) {
            self.inner.reached_notify.notify_waiters();
        }
        loop {
            if self.is_released() {
                return;
            }
            let notified = self.inner.released_notify.notified();
            if self.is_released() {
                return;
            }
            notified.await;
        }
    }
}

/// A failure while configuring or inspecting a fault store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FaultStoreControlError {
    /// The bounded fault schedule already contains
    /// [`MAX_FAULT_SCHEDULE`] pending actions.
    #[error("fault schedule is full (capacity {MAX_FAULT_SCHEDULE})")]
    ScheduleFull,
    /// A prior panic poisoned the store's control/state mutex.
    #[error("fault store state lock is poisoned")]
    StatePoisoned,
}

impl FaultStoreControlError {
    /// Returns the exhausted capacity for a bounded-control error.
    #[must_use]
    pub fn capacity(&self) -> Option<usize> {
        match self {
            Self::ScheduleFull => Some(MAX_FAULT_SCHEDULE),
            Self::StatePoisoned => None,
        }
    }
}

/// A clone of the state that has actually committed in a fault store.
///
/// This snapshot intentionally excludes attempted-but-failed values. Pair it
/// with [`FaultInjectingStore::trace`] to compare attempts with durable facts.
#[derive(Debug, Clone, PartialEq)]
pub struct FaultStoreSnapshot {
    chunks: BTreeMap<ChunkKey, ChunkRecord>,
    overlays: BTreeMap<ChunkKey, ChunkOverlayRecord>,
    mutations: Vec<BlockMutationLogRecord>,
    journal_receipts: BTreeMap<JournalBatchId, JournalAppendReceipt>,
    entities: BTreeMap<EntityKey, EntityRecord>,
    players: BTreeMap<PlayerId, PlayerRecord>,
    attempted_operations: u64,
    committed_operations: u64,
    successful_responses: u64,
}

impl FaultStoreSnapshot {
    /// Returns the committed full chunk at `key`, if present.
    #[must_use]
    pub fn chunk(&self, key: ChunkKey) -> Option<&ChunkRecord> {
        self.chunks.get(&key)
    }

    /// Returns the committed overlay at `key`, if present.
    #[must_use]
    pub fn chunk_overlay(&self, key: ChunkKey) -> Option<&ChunkOverlayRecord> {
        self.overlays.get(&key)
    }

    /// Returns every committed block mutation in journal order.
    #[must_use]
    pub fn block_mutations(&self) -> &[BlockMutationLogRecord] {
        &self.mutations
    }

    /// Returns the committed receipt for `batch_id`, if that token has committed.
    #[must_use]
    pub fn journal_receipt(&self, batch_id: JournalBatchId) -> Option<JournalAppendReceipt> {
        self.journal_receipts.get(&batch_id).copied()
    }

    /// Returns the committed entity at `key`, if present.
    #[must_use]
    pub fn entity(&self, key: EntityKey) -> Option<&EntityRecord> {
        self.entities.get(&key)
    }

    /// Returns the committed player at `id`, if present.
    #[must_use]
    pub fn player(&self, id: PlayerId) -> Option<&PlayerRecord> {
        self.players.get(&id)
    }

    /// Returns the number of operations accepted by the request side.
    #[must_use]
    pub fn attempted_operations(&self) -> u64 {
        self.attempted_operations
    }

    /// Returns the number of mutating operations that committed.
    #[must_use]
    pub fn committed_operations(&self) -> u64 {
        self.committed_operations
    }

    /// Returns the number of successful responses delivered to callers.
    #[must_use]
    pub fn successful_responses(&self) -> u64 {
        self.successful_responses
    }
}

#[derive(Debug, Clone)]
enum ScheduledFault {
    FailOperation,
    FailBeforeCommit,
    CommitError,
    CommitThenLoseAck,
    HoldBeforeCommit(FaultGate),
    HoldResponse(FaultGate),
}

impl ScheduledFault {
    fn applies_to(&self, operation: FaultOperation) -> bool {
        match self {
            Self::FailOperation | Self::HoldResponse(_) => true,
            Self::FailBeforeCommit
            | Self::CommitError
            | Self::CommitThenLoseAck
            | Self::HoldBeforeCommit(_) => operation.is_mutating(),
        }
    }
}

#[derive(Debug, Default)]
struct CommittedState {
    chunks: BTreeMap<ChunkKey, ChunkRecord>,
    overlays: BTreeMap<ChunkKey, ChunkOverlayRecord>,
    mutations: Vec<BlockMutationLogRecord>,
    last_mutation_id: Option<u64>,
    journal_receipts: BTreeMap<JournalBatchId, JournalAppendReceipt>,
    entities: BTreeMap<EntityKey, EntityRecord>,
    players: BTreeMap<PlayerId, PlayerRecord>,
}

#[derive(Debug, Default)]
struct Inner {
    committed: CommittedState,
    attempts: Vec<FaultStoreAttempt>,
    schedule: VecDeque<ScheduledFault>,
    trace: Vec<FaultTraceEntry>,
    next_sequence: u64,
    attempted_operations: u64,
    committed_operations: u64,
    successful_responses: u64,
    requests_closed: bool,
    responses_closed: bool,
}

/// A deterministic, in-memory [`WorldStore`] and [`PlayerStore`] with explicit
/// commit cutpoints.
///
/// Fault actions form a bounded FIFO schedule. Commit-specific actions wait at
/// the front until the next mutating operation; reads may pass without consuming
/// them. At most one scheduled action applies to an operation. The store uses no
/// wall clock, random source, socket, or background worker, so repeating the same
/// method/action order produces the same trace and committed snapshot.
#[derive(Debug, Default)]
pub struct FaultInjectingStore {
    inner: Mutex<Inner>,
    journal: InMemoryStore,
    journal_commit: AsyncMutex<()>,
}

impl FaultInjectingStore {
    /// Creates an empty store with open request/response sides and no faults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next operation fail before reading or committing anything.
    pub fn fail_next_operation(&self) -> Result<(), FaultStoreControlError> {
        self.schedule(ScheduledFault::FailOperation)
    }

    /// Makes the next mutating operation fail immediately before commit.
    pub fn fail_next_before_commit(&self) -> Result<(), FaultStoreControlError> {
        self.schedule(ScheduledFault::FailBeforeCommit)
    }

    /// Makes the next mutating operation return a commit error without changing
    /// committed state.
    pub fn return_next_commit_error(&self) -> Result<(), FaultStoreControlError> {
        self.schedule(ScheduledFault::CommitError)
    }

    /// Makes the next mutating operation commit, then return an error as though
    /// its acknowledgement was lost.
    pub fn lose_next_ack_after_commit(&self) -> Result<(), FaultStoreControlError> {
        self.schedule(ScheduledFault::CommitThenLoseAck)
    }

    /// Holds the next mutating operation immediately before commit.
    ///
    /// The returned gate is the deterministic latency control. Committed state
    /// remains unchanged until the gate is released.
    pub fn hold_next_before_commit(&self) -> Result<FaultGate, FaultStoreControlError> {
        let gate = FaultGate::new();
        self.schedule(ScheduledFault::HoldBeforeCommit(gate.clone()))?;
        Ok(gate)
    }

    /// Holds the next operation's response after its read or commit completes.
    ///
    /// A mutating operation is visible in [`snapshot`](Self::snapshot) once the
    /// gate is reached even though its caller is still awaiting a response.
    pub fn hold_next_response(&self) -> Result<FaultGate, FaultStoreControlError> {
        let gate = FaultGate::new();
        self.schedule(ScheduledFault::HoldResponse(gate.clone()))?;
        Ok(gate)
    }

    /// Permanently closes the request side. Later operations fail before the
    /// `Attempted` stage and cannot commit.
    pub fn close_requests(&self) -> Result<(), FaultStoreControlError> {
        self.lock_control()?.requests_closed = true;
        Ok(())
    }

    /// Permanently closes the response side. Later accepted writes still commit
    /// but return an error, modelling a lost response channel.
    pub fn close_responses(&self) -> Result<(), FaultStoreControlError> {
        self.lock_control()?.responses_closed = true;
        Ok(())
    }

    /// Returns a clone of committed state and operation counters.
    pub fn snapshot(&self) -> Result<FaultStoreSnapshot, FaultStoreControlError> {
        let inner = self.lock_control()?;
        Ok(FaultStoreSnapshot {
            chunks: inner.committed.chunks.clone(),
            overlays: inner.committed.overlays.clone(),
            mutations: inner.committed.mutations.clone(),
            journal_receipts: inner.committed.journal_receipts.clone(),
            entities: inner.committed.entities.clone(),
            players: inner.committed.players.clone(),
            attempted_operations: inner.attempted_operations,
            committed_operations: inner.committed_operations,
            successful_responses: inner.successful_responses,
        })
    }

    /// Returns exact accepted call payloads, including failed operations.
    ///
    /// Request-side-closed calls are excluded because the store rejects them
    /// before acceptance. This state is independent from [`snapshot`](Self::snapshot):
    /// an operation remains attempted even when no corresponding value commits.
    pub fn attempted_state(&self) -> Result<FaultStoreAttemptedState, FaultStoreControlError> {
        Ok(FaultStoreAttemptedState {
            operations: self.lock_control()?.attempts.clone(),
        })
    }

    /// Returns the complete ordered operation trace.
    pub fn trace(&self) -> Result<Vec<FaultTraceEntry>, FaultStoreControlError> {
        Ok(self.lock_control()?.trace.clone())
    }

    /// Returns the number of scheduled actions not yet consumed.
    pub fn scheduled_faults(&self) -> Result<usize, FaultStoreControlError> {
        Ok(self.lock_control()?.schedule.len())
    }

    fn schedule(&self, fault: ScheduledFault) -> Result<(), FaultStoreControlError> {
        let mut inner = self.lock_control()?;
        if inner.schedule.len() >= MAX_FAULT_SCHEDULE {
            return Err(FaultStoreControlError::ScheduleFull);
        }
        inner.schedule.push_back(fault);
        Ok(())
    }

    fn lock_control(&self) -> Result<MutexGuard<'_, Inner>, FaultStoreControlError> {
        self.inner
            .lock()
            .map_err(|_| FaultStoreControlError::StatePoisoned)
    }

    fn lock_server(&self) -> ServerResult<MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| ServerError::internal("fault store state lock is poisoned"))
    }

    fn push_trace(
        inner: &mut Inner,
        operation_id: u64,
        operation: FaultOperation,
        stage: FaultStage,
        item_count: usize,
    ) {
        let sequence = inner.next_sequence;
        inner.next_sequence = inner.next_sequence.saturating_add(1);
        inner.trace.push(FaultTraceEntry {
            sequence,
            operation_id,
            operation,
            stage,
            item_count,
        });
    }

    fn begin_operation(
        &self,
        operation: FaultOperation,
        payload: FaultAttemptPayload,
    ) -> ServerResult<(u64, Option<ScheduledFault>)> {
        let item_count = payload.item_count();
        let mut inner = self.lock_server()?;
        let operation_id = inner.next_sequence;
        if inner.requests_closed {
            Self::push_trace(
                &mut inner,
                operation_id,
                operation,
                FaultStage::RequestClosed,
                item_count,
            );
            return Err(ServerError::invalid_state(
                "fault store request side is closed",
            ));
        }
        inner.attempted_operations = inner.attempted_operations.saturating_add(1);
        Self::push_trace(
            &mut inner,
            operation_id,
            operation,
            FaultStage::Attempted,
            item_count,
        );
        inner.attempts.push(FaultStoreAttempt {
            operation_id,
            operation,
            payload,
        });
        let applies = inner
            .schedule
            .front()
            .is_some_and(|fault| fault.applies_to(operation));
        if applies {
            Ok((operation_id, inner.schedule.pop_front()))
        } else {
            Ok((operation_id, None))
        }
    }

    fn record_stage(
        &self,
        operation_id: u64,
        operation: FaultOperation,
        stage: FaultStage,
        item_count: usize,
    ) -> ServerResult<()> {
        let mut inner = self.lock_server()?;
        Self::push_trace(&mut inner, operation_id, operation, stage, item_count);
        Ok(())
    }

    async fn apply_before_commit(
        &self,
        operation_id: u64,
        operation: FaultOperation,
        item_count: usize,
        fault: Option<&ScheduledFault>,
    ) -> ServerResult<()> {
        match fault {
            Some(ScheduledFault::FailOperation) => {
                self.record_stage(
                    operation_id,
                    operation,
                    FaultStage::InjectedFailure,
                    item_count,
                )?;
                Err(ServerError::internal(
                    "fault store injected operation failure",
                ))
            }
            Some(ScheduledFault::FailBeforeCommit) => {
                self.record_stage(
                    operation_id,
                    operation,
                    FaultStage::BeforeCommitFailure,
                    item_count,
                )?;
                Err(ServerError::internal(
                    "fault store injected failure before commit",
                ))
            }
            Some(ScheduledFault::CommitError) => {
                self.record_stage(
                    operation_id,
                    operation,
                    FaultStage::CommitFailure,
                    item_count,
                )?;
                Err(ServerError::internal("fault store injected commit failure"))
            }
            Some(ScheduledFault::HoldBeforeCommit(gate)) => {
                self.record_stage(
                    operation_id,
                    operation,
                    FaultStage::HeldBeforeCommit,
                    item_count,
                )?;
                gate.reach_and_wait().await;
                Ok(())
            }
            Some(ScheduledFault::CommitThenLoseAck | ScheduledFault::HoldResponse(_)) | None => {
                Ok(())
            }
        }
    }

    fn apply_before_read(
        &self,
        operation_id: u64,
        operation: FaultOperation,
        item_count: usize,
        fault: Option<&ScheduledFault>,
    ) -> ServerResult<()> {
        if matches!(fault, Some(ScheduledFault::FailOperation)) {
            self.record_stage(
                operation_id,
                operation,
                FaultStage::InjectedFailure,
                item_count,
            )?;
            Err(ServerError::internal(
                "fault store injected operation failure",
            ))
        } else {
            Ok(())
        }
    }

    async fn finish_response<T>(
        &self,
        operation_id: u64,
        operation: FaultOperation,
        item_count: usize,
        fault: Option<&ScheduledFault>,
        result: T,
    ) -> ServerResult<T> {
        if matches!(fault, Some(ScheduledFault::CommitThenLoseAck)) {
            self.record_stage(
                operation_id,
                operation,
                FaultStage::AcknowledgementLost,
                item_count,
            )?;
            return Err(ServerError::internal(
                "fault store committed but lost acknowledgement",
            ));
        }
        if let Some(ScheduledFault::HoldResponse(gate)) = fault {
            self.record_stage(
                operation_id,
                operation,
                FaultStage::HeldResponse,
                item_count,
            )?;
            gate.reach_and_wait().await;
        }

        let mut inner = self.lock_server()?;
        if inner.responses_closed {
            Self::push_trace(
                &mut inner,
                operation_id,
                operation,
                FaultStage::ResponseClosed,
                item_count,
            );
            return Err(ServerError::invalid_state(
                "fault store response side is closed",
            ));
        }
        inner.successful_responses = inner.successful_responses.saturating_add(1);
        Self::push_trace(
            &mut inner,
            operation_id,
            operation,
            FaultStage::Succeeded,
            item_count,
        );
        Ok(result)
    }

    async fn execute_read<T, F>(
        &self,
        operation: FaultOperation,
        payload: FaultAttemptPayload,
        read: F,
    ) -> ServerResult<T>
    where
        T: Send,
        F: FnOnce(&CommittedState) -> ServerResult<T> + Send,
    {
        let item_count = payload.item_count();
        let (operation_id, fault) = self.begin_operation(operation, payload)?;
        self.apply_before_read(operation_id, operation, item_count, fault.as_ref())?;
        let result = {
            let inner = self.lock_server()?;
            read(&inner.committed)?
        };
        self.finish_response(operation_id, operation, item_count, fault.as_ref(), result)
            .await
    }

    async fn execute_write<T, F>(
        &self,
        operation: FaultOperation,
        payload: FaultAttemptPayload,
        commit: F,
    ) -> ServerResult<T>
    where
        T: Send,
        F: FnOnce(&mut CommittedState) -> ServerResult<T> + Send,
    {
        let item_count = payload.item_count();
        let (operation_id, fault) = self.begin_operation(operation, payload)?;
        self.apply_before_commit(operation_id, operation, item_count, fault.as_ref())
            .await?;
        let result = {
            let mut inner = self.lock_server()?;
            match commit(&mut inner.committed) {
                Ok(result) => {
                    inner.committed_operations = inner.committed_operations.saturating_add(1);
                    Self::push_trace(
                        &mut inner,
                        operation_id,
                        operation,
                        FaultStage::Committed,
                        item_count,
                    );
                    Ok(result)
                }
                Err(error) => {
                    Self::push_trace(
                        &mut inner,
                        operation_id,
                        operation,
                        FaultStage::CommitFailure,
                        item_count,
                    );
                    Err(error)
                }
            }
        }?;
        self.finish_response(operation_id, operation, item_count, fault.as_ref(), result)
            .await
    }

    async fn execute_journal_append(
        &self,
        mutations: Vec<BlockMutationLogRecord>,
    ) -> ServerResult<()> {
        let operation = FaultOperation::AppendBlockMutations;
        let payload = FaultAttemptPayload::Mutations(mutations.clone());
        let item_count = payload.item_count();
        let (operation_id, fault) = self.begin_operation(operation, payload)?;

        // Both journal APIs share this authority so their storage-owned IDs
        // cannot overlap when a test mixes legacy and tokenized appends.
        let commit_guard = self.journal_commit.lock().await;
        self.apply_before_commit(operation_id, operation, item_count, fault.as_ref())
            .await?;
        if let Err(error) = self.journal.append_block_mutations(mutations.clone()).await {
            self.record_stage(
                operation_id,
                operation,
                FaultStage::CommitFailure,
                item_count,
            )?;
            return Err(error);
        }
        {
            let mut inner = self.lock_server()?;
            append_mutations(&mut inner.committed, mutations)?;
            inner.committed_operations = inner.committed_operations.saturating_add(1);
            Self::push_trace(
                &mut inner,
                operation_id,
                operation,
                FaultStage::Committed,
                item_count,
            );
        }
        drop(commit_guard);

        self.finish_response(operation_id, operation, item_count, fault.as_ref(), ())
            .await
    }

    async fn execute_journal_batch(
        &self,
        batch_id: JournalBatchId,
        mutations: Vec<BlockMutationLogRecord>,
    ) -> ServerResult<JournalAppendReceipt> {
        let operation = FaultOperation::AppendBlockMutationBatch;
        let payload = FaultAttemptPayload::MutationBatch {
            batch_id,
            mutations: mutations.clone(),
        };
        let item_count = payload.item_count();
        let (operation_id, fault) = self.begin_operation(operation, payload)?;

        let commit_guard = self.journal_commit.lock().await;
        self.apply_before_commit(operation_id, operation, item_count, fault.as_ref())
            .await?;
        let receipt = match self
            .journal
            .append_block_mutation_batch(batch_id, mutations.clone())
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                self.record_stage(
                    operation_id,
                    operation,
                    FaultStage::CommitFailure,
                    item_count,
                )?;
                return Err(error);
            }
        };
        if receipt.batch_id() != batch_id {
            self.record_stage(
                operation_id,
                operation,
                FaultStage::CommitFailure,
                item_count,
            )?;
            return Err(ServerError::internal(
                "fault store journal authority returned a receipt for another batch",
            ));
        }
        let normalized = normalize_mutations(receipt, &mutations)?;
        {
            let mut inner = self.lock_server()?;
            let stage = match inner.committed.journal_receipts.get(&batch_id) {
                Some(committed) if *committed == receipt => FaultStage::ReceiptReplayed,
                Some(_) => {
                    Self::push_trace(
                        &mut inner,
                        operation_id,
                        operation,
                        FaultStage::CommitFailure,
                        item_count,
                    );
                    return Err(ServerError::internal(
                        "fault store journal authority returned a different receipt",
                    ));
                }
                None => {
                    inner.committed.mutations.extend(normalized);
                    inner.committed.last_mutation_id =
                        receipt.last_id().or(inner.committed.last_mutation_id);
                    inner.committed.journal_receipts.insert(batch_id, receipt);
                    inner.committed_operations = inner.committed_operations.saturating_add(1);
                    FaultStage::Committed
                }
            };
            Self::push_trace(&mut inner, operation_id, operation, stage, item_count);
        }
        drop(commit_guard);

        self.finish_response(operation_id, operation, item_count, fault.as_ref(), receipt)
            .await
    }
}

fn validate_batch(len: usize) -> ServerResult<()> {
    if len > MAX_SAVE_BATCH {
        return Err(StorageError::BatchTooLarge {
            len,
            max: MAX_SAVE_BATCH,
        }
        .into());
    }
    Ok(())
}

fn append_mutations(
    state: &mut CommittedState,
    mutations: Vec<BlockMutationLogRecord>,
) -> ServerResult<()> {
    validate_batch(mutations.len())?;
    if mutations.is_empty() {
        return Ok(());
    }
    let first_id = match state.last_mutation_id {
        Some(last_id) => last_id.checked_add(1).ok_or_else(|| {
            ServerError::from(StorageError::JournalSequenceExhausted {
                last_id,
                requested: mutations.len(),
            })
        })?,
        None => 0,
    };
    let additional = u64::try_from(mutations.len() - 1).map_err(|_| {
        ServerError::from(StorageError::JournalSequenceExhausted {
            last_id: state.last_mutation_id.unwrap_or(0),
            requested: mutations.len(),
        })
    })?;
    let final_id = first_id.checked_add(additional).ok_or_else(|| {
        ServerError::from(StorageError::JournalSequenceExhausted {
            last_id: state.last_mutation_id.unwrap_or(0),
            requested: mutations.len(),
        })
    })?;
    state.mutations.extend(
        mutations
            .into_iter()
            .zip(first_id..=final_id)
            .map(|(record, id)| {
                BlockMutationLogRecord::new(
                    record.schema_version(),
                    id,
                    record.tick(),
                    record.actor(),
                    record.pos(),
                    record.old_state(),
                    record.new_state(),
                    record.cause(),
                )
            }),
    );
    state.last_mutation_id = Some(final_id);
    Ok(())
}

fn normalize_mutations(
    receipt: JournalAppendReceipt,
    mutations: &[BlockMutationLogRecord],
) -> ServerResult<Vec<BlockMutationLogRecord>> {
    if receipt.len() != mutations.len() {
        return Err(ServerError::internal(
            "fault store journal authority returned a mismatched receipt length",
        ));
    }
    if mutations.is_empty() {
        if receipt.first_id().is_none() && receipt.last_id().is_none() {
            return Ok(Vec::new());
        }
        return Err(ServerError::internal(
            "fault store journal authority returned a range for an empty receipt",
        ));
    }
    let (Some(first_id), Some(last_id)) = (receipt.first_id(), receipt.last_id()) else {
        return Err(ServerError::internal(
            "fault store journal authority omitted a non-empty receipt range",
        ));
    };
    let final_offset = u64::try_from(mutations.len() - 1)
        .map_err(|_| ServerError::internal("fault store journal receipt length did not fit u64"))?;
    if first_id.checked_add(final_offset) != Some(last_id) {
        return Err(ServerError::internal(
            "fault store journal authority returned an inconsistent receipt range",
        ));
    }

    mutations
        .iter()
        .enumerate()
        .map(|(offset, record)| {
            let offset = u64::try_from(offset).map_err(|_| {
                ServerError::internal("fault store journal receipt offset did not fit u64")
            })?;
            let id = first_id.checked_add(offset).ok_or_else(|| {
                ServerError::internal("fault store journal receipt range overflowed")
            })?;
            Ok(BlockMutationLogRecord::new(
                record.schema_version(),
                id,
                record.tick(),
                record.actor(),
                record.pos(),
                record.old_state(),
                record.new_state(),
                record.cause(),
            ))
        })
        .collect()
}

#[async_trait]
impl WorldStore for FaultInjectingStore {
    async fn load_chunk(&self, key: ChunkKey) -> ServerResult<Option<ChunkRecord>> {
        self.execute_read(
            FaultOperation::LoadChunk,
            FaultAttemptPayload::ChunkKey(key),
            move |state| Ok(state.chunks.get(&key).cloned()),
        )
        .await
    }

    async fn save_chunk(&self, key: ChunkKey, record: ChunkRecord) -> ServerResult<()> {
        let payload = FaultAttemptPayload::Chunks(vec![(key, record.clone())]);
        self.execute_write(FaultOperation::SaveChunk, payload, move |state| {
            state.chunks.insert(key, record);
            Ok(())
        })
        .await
    }

    async fn save_chunks(&self, chunks: Vec<(ChunkKey, ChunkRecord)>) -> ServerResult<()> {
        let payload = FaultAttemptPayload::Chunks(chunks.clone());
        self.execute_write(FaultOperation::SaveChunks, payload, move |state| {
            validate_batch(chunks.len())?;
            state.chunks.extend(chunks);
            Ok(())
        })
        .await
    }

    async fn delete_chunk(&self, key: ChunkKey) -> ServerResult<bool> {
        self.execute_write(
            FaultOperation::DeleteChunk,
            FaultAttemptPayload::ChunkKey(key),
            move |state| Ok(state.chunks.remove(&key).is_some()),
        )
        .await
    }

    async fn load_chunk_overlay(&self, key: ChunkKey) -> ServerResult<Option<ChunkOverlayRecord>> {
        self.execute_read(
            FaultOperation::LoadChunkOverlay,
            FaultAttemptPayload::ChunkKey(key),
            move |state| Ok(state.overlays.get(&key).cloned()),
        )
        .await
    }

    async fn save_chunk_overlays(
        &self,
        overlays: Vec<(ChunkKey, ChunkOverlayRecord)>,
    ) -> ServerResult<()> {
        let payload = FaultAttemptPayload::Overlays(overlays.clone());
        self.execute_write(FaultOperation::SaveChunkOverlays, payload, move |state| {
            validate_batch(overlays.len())?;
            state.overlays.extend(overlays);
            Ok(())
        })
        .await
    }

    async fn append_block_mutations(
        &self,
        mutations: Vec<BlockMutationLogRecord>,
    ) -> ServerResult<()> {
        self.execute_journal_append(mutations).await
    }

    async fn append_block_mutation_batch(
        &self,
        batch_id: JournalBatchId,
        mutations: Vec<BlockMutationLogRecord>,
    ) -> ServerResult<JournalAppendReceipt> {
        self.execute_journal_batch(batch_id, mutations).await
    }

    async fn load_entity(&self, key: EntityKey) -> ServerResult<Option<EntityRecord>> {
        self.execute_read(
            FaultOperation::LoadEntity,
            FaultAttemptPayload::EntityKey(key),
            move |state| Ok(state.entities.get(&key).cloned()),
        )
        .await
    }

    async fn save_entity(&self, key: EntityKey, record: EntityRecord) -> ServerResult<()> {
        let payload = FaultAttemptPayload::Entities(vec![(key, record.clone())]);
        self.execute_write(FaultOperation::SaveEntity, payload, move |state| {
            state.entities.insert(key, record);
            Ok(())
        })
        .await
    }

    async fn save_entities(&self, entities: Vec<(EntityKey, EntityRecord)>) -> ServerResult<()> {
        let payload = FaultAttemptPayload::Entities(entities.clone());
        self.execute_write(FaultOperation::SaveEntities, payload, move |state| {
            validate_batch(entities.len())?;
            state.entities.extend(entities);
            Ok(())
        })
        .await
    }

    async fn delete_entity(&self, key: EntityKey) -> ServerResult<bool> {
        self.execute_write(
            FaultOperation::DeleteEntity,
            FaultAttemptPayload::EntityKey(key),
            move |state| Ok(state.entities.remove(&key).is_some()),
        )
        .await
    }
}

#[async_trait]
impl PlayerStore for FaultInjectingStore {
    async fn load_player(&self, id: PlayerId) -> ServerResult<Option<PlayerRecord>> {
        self.execute_read(
            FaultOperation::LoadPlayer,
            FaultAttemptPayload::PlayerId(id),
            move |state| Ok(state.players.get(&id).cloned()),
        )
        .await
    }

    async fn save_player(&self, id: PlayerId, record: PlayerRecord) -> ServerResult<()> {
        let payload = FaultAttemptPayload::Players(vec![(id, record.clone())]);
        self.execute_write(FaultOperation::SavePlayer, payload, move |state| {
            state.players.insert(id, record);
            Ok(())
        })
        .await
    }

    async fn delete_player(&self, id: PlayerId) -> ServerResult<bool> {
        self.execute_write(
            FaultOperation::DeletePlayer,
            FaultAttemptPayload::PlayerId(id),
            move |state| Ok(state.players.remove(&id).is_some()),
        )
        .await
    }
}
