//! Bounded deterministic durability soak/chaos scheduling.
//!
//! [`DurabilitySoakHarness`] combines the complete 23-case
//! [`DurabilityFaultBattery`](crate::DurabilityFaultBattery) with six fixed
//! connect/edit/persist/disconnect/restart cycles. The lifecycle schedule has
//! exactly [`DURABILITY_SOAK_STEPS`] logical steps, a capacity-one pending edit,
//! and no random source, clock, sleep, socket, or background task.
//! The integration regression runs the complete schedule under a paused Tokio
//! clock and fixed virtual deadline so a stalled fault cutpoint fails instead
//! of hanging the test process.
//!
//! A restart is modelled as dropping volatile connection state and re-reading
//! the durable player record from the same [`FaultInjectingStore`]. This is a
//! deterministic storage-contract soak, not a process-crash or durable-backend
//! reopen test.

use core::fmt;

use ferrumc_core::{GameMode, PlayerId, ServerError};
use ferrumc_storage::{
    BlockMutationLogRecord, JournalAppendReceipt, JournalBatchId, MutationActor, MutationLogCause,
    PlayerRecord, PlayerStore, SchemaVersion, StorageError, WorldStore,
};
use sha2::{Digest, Sha256};

use crate::durability_battery::{
    DurabilityBatteryError, DurabilityBatteryReport, DurabilityFaultBattery, DurabilityOutcome,
    DurabilityScenario, DurabilitySurface,
};
use crate::fault_store::{
    FaultInjectingStore, FaultOperation, FaultStage, FaultStoreControlError, FaultStoreSnapshot,
    FaultTraceEntry,
};

/// Number of connect/edit/persist/disconnect/restart cycles in the fixed soak.
pub const DURABILITY_SOAK_CYCLES: usize = 6;

/// Exact number of logical steps in a complete fixed soak run.
///
/// One step runs the reusable durability battery, followed by seven steps for
/// each of the six lifecycle cycles.
pub const DURABILITY_SOAK_STEPS: usize = 43;

/// Capacity of the soak's pending-edit slot.
///
/// A second edit cannot be accepted until the first has a durable journal
/// receipt. This is deliberately the smallest useful backpressure boundary.
pub const DURABILITY_SOAK_PENDING_CAPACITY: usize = 1;

/// Maximum retained fault-store trace entries in one soak report.
///
/// The current fixed schedule produces exactly 52 entries. The separate
/// ceiling makes a future accidental schedule expansion fail instead of
/// growing the report without review.
pub const MAX_DURABILITY_SOAK_STORE_TRACE: usize = 64;

const DURABILITY_BATTERY_CASES: usize = 23;
const EXPECTED_STORE_TRACE_ENTRIES: usize = 52;
const EXPECTED_ATTEMPTED_OPERATIONS: u64 = 20;
const EXPECTED_COMMITTED_OPERATIONS: u64 = 11;
const EXPECTED_SUCCESSFUL_RESPONSES: u64 = 16;
const DOMAIN: &[u8] = b"ferrumc.testkit.durability-soak.v1";

const SCHEDULE: [DurabilitySoakScenario; DURABILITY_SOAK_CYCLES] = [
    DurabilitySoakScenario::Clean,
    DurabilitySoakScenario::JournalBeforeCommitRetry,
    DurabilitySoakScenario::JournalAcknowledgementLossReplay,
    DurabilitySoakScenario::PlayerCommitError,
    DurabilitySoakScenario::PlayerAcknowledgementLoss,
    DurabilitySoakScenario::Clean,
];

const EXPECTED_RESTART_GENERATIONS: [usize; DURABILITY_SOAK_CYCLES] = [0, 1, 2, 2, 4, 5];

/// Fault pattern assigned to one fixed lifecycle cycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DurabilitySoakScenario {
    /// Journal and player writes both return success.
    Clean,
    /// The journal fails before commit, retains its pending edit, then retries.
    JournalBeforeCommitRetry,
    /// The journal commits without an acknowledgement, then replays its receipt.
    JournalAcknowledgementLossReplay,
    /// The journal commits, but the player-record commit fails.
    PlayerCommitError,
    /// Both writes commit, but the player-record acknowledgement is lost.
    PlayerAcknowledgementLoss,
}

/// One operation in the fixed logical schedule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DurabilitySoakAction {
    /// Run the reusable 23-case durability battery.
    RunBattery,
    /// Establish one logical connection.
    Connect,
    /// Fill the capacity-one pending-edit slot.
    QueueEdit,
    /// Make the cycle's first journal persistence attempt.
    PersistJournal,
    /// Confirm, retry, or replay the journal receipt.
    ResolveJournal,
    /// Persist the cycle's player record.
    PersistPlayer,
    /// Drop the logical connection after pending edits are durable.
    Disconnect,
    /// Clear volatile state and reload the durable player record.
    Restart,
}

/// Typed fault surfaced by a fixed soak step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DurabilitySoakFault {
    /// The journal write stopped before commit.
    JournalBeforeCommit,
    /// The journal committed but its first receipt response was lost.
    JournalAcknowledgementLost,
    /// The player record did not commit.
    PlayerCommit,
    /// The player record committed but its response was lost.
    PlayerAcknowledgementLost,
}

/// Typed result of one logical soak step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DurabilitySoakStepOutcome {
    /// The complete reusable battery returned the fixed case count.
    BatteryCompleted {
        /// Number of battery cases executed.
        cases: usize,
    },
    /// A logical connection was established.
    Connected,
    /// One edit occupied the bounded pending slot.
    EditQueued {
        /// Occupied pending slots after the edit.
        pending: usize,
        /// Fixed pending-slot capacity.
        capacity: usize,
    },
    /// A new journal batch committed and returned its receipt.
    JournalCommitted(JournalAppendReceipt),
    /// An already-returned receipt was confirmed in committed state.
    JournalConfirmed(JournalAppendReceipt),
    /// A retry returned the receipt for a batch whose first attempt failed.
    JournalRetried(JournalAppendReceipt),
    /// A same-token retry replayed the receipt hidden by acknowledgement loss.
    JournalReceiptReplayed(JournalAppendReceipt),
    /// A player record committed and returned success.
    PlayerCommitted {
        /// Deterministic player-record generation that committed.
        generation: usize,
    },
    /// An injected fault was observed as the expected typed result.
    Fault(DurabilitySoakFault),
    /// The logical connection was removed with no pending edit.
    Disconnected,
    /// Durable state was reloaded after a logical restart.
    Restarted {
        /// Player-record generation recovered from committed state.
        durable_player_generation: usize,
    },
}

/// One immutable entry in the bounded logical soak trace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurabilitySoakStep {
    index: usize,
    cycle: Option<usize>,
    scenario: Option<DurabilitySoakScenario>,
    action: DurabilitySoakAction,
    outcome: DurabilitySoakStepOutcome,
}

impl DurabilitySoakStep {
    /// Returns the zero-based global logical step index.
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    /// Returns the zero-based lifecycle cycle, or `None` for the battery step.
    #[must_use]
    pub const fn cycle(&self) -> Option<usize> {
        self.cycle
    }

    /// Returns the cycle's fault pattern, or `None` for the battery step.
    #[must_use]
    pub const fn scenario(&self) -> Option<DurabilitySoakScenario> {
        self.scenario
    }

    /// Returns the scheduled operation.
    #[must_use]
    pub const fn action(&self) -> DurabilitySoakAction {
        self.action
    }

    /// Returns the typed result observed for the operation.
    #[must_use]
    pub const fn outcome(&self) -> DurabilitySoakStepOutcome {
        self.outcome
    }
}

/// Exact terminal state of a complete soak run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurabilitySoakEndState {
    player: PlayerId,
    connected: bool,
    pending_edits: usize,
    completed_cycles: usize,
    committed_mutations: usize,
    last_mutation_id: Option<u64>,
    durable_player_generation: usize,
    attempted_operations: u64,
    committed_operations: u64,
    successful_responses: u64,
    store_trace_entries: usize,
}

impl DurabilitySoakEndState {
    /// Returns the stable player identity used by all cycles.
    #[must_use]
    pub const fn player(&self) -> PlayerId {
        self.player
    }

    /// Returns whether a volatile connection remains after the final restart.
    #[must_use]
    pub const fn connected(&self) -> bool {
        self.connected
    }

    /// Returns edits still waiting for a durable receipt.
    #[must_use]
    pub const fn pending_edits(&self) -> usize {
        self.pending_edits
    }

    /// Returns the number of completed lifecycle cycles.
    #[must_use]
    pub const fn completed_cycles(&self) -> usize {
        self.completed_cycles
    }

    /// Returns the number of committed journal mutations.
    #[must_use]
    pub const fn committed_mutations(&self) -> usize {
        self.committed_mutations
    }

    /// Returns the final storage-assigned journal ID.
    #[must_use]
    pub const fn last_mutation_id(&self) -> Option<u64> {
        self.last_mutation_id
    }

    /// Returns the player-record generation recovered after the final restart.
    #[must_use]
    pub const fn durable_player_generation(&self) -> usize {
        self.durable_player_generation
    }

    /// Returns all requests accepted by the fault store, including reads.
    #[must_use]
    pub const fn attempted_operations(&self) -> u64 {
        self.attempted_operations
    }

    /// Returns mutating operations that changed committed state.
    #[must_use]
    pub const fn committed_operations(&self) -> u64 {
        self.committed_operations
    }

    /// Returns successful responses delivered to the harness.
    #[must_use]
    pub const fn successful_responses(&self) -> u64 {
        self.successful_responses
    }

    /// Returns entries retained from the correlated fault-store trace.
    #[must_use]
    pub const fn store_trace_entries(&self) -> usize {
        self.store_trace_entries
    }
}

/// Canonical SHA-256 digest of a deterministic durability soak report.
///
/// The digest uses a domain-separated, length-prefixed little-endian v1
/// grammar over battery classifications, logical steps, correlated store
/// trace, committed player/journal state, and terminal counters.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DurabilitySoakDigest([u8; 32]);

impl DurabilitySoakDigest {
    /// Returns the raw SHA-256 bytes.
    #[must_use]
    pub const fn bytes(self) -> [u8; 32] {
        self.0
    }

    /// Returns lowercase hexadecimal.
    #[must_use]
    pub fn as_hex(self) -> String {
        let mut output = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            let _ignored = write!(output, "{byte:02x}");
        }
        output
    }
}

impl fmt::Display for DurabilitySoakDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_hex())
    }
}

/// Complete report from one fixed deterministic soak run.
#[derive(Clone, Debug, PartialEq)]
pub struct DurabilitySoakReport {
    battery: DurabilityBatteryReport,
    steps: Vec<DurabilitySoakStep>,
    store_trace: Vec<FaultTraceEntry>,
    committed: FaultStoreSnapshot,
    end_state: DurabilitySoakEndState,
    digest: DurabilitySoakDigest,
}

impl DurabilitySoakReport {
    /// Returns the reusable battery report executed before lifecycle cycles.
    #[must_use]
    pub const fn battery(&self) -> &DurabilityBatteryReport {
        &self.battery
    }

    /// Returns exactly [`DURABILITY_SOAK_STEPS`] logical steps.
    #[must_use]
    pub fn steps(&self) -> &[DurabilitySoakStep] {
        &self.steps
    }

    /// Returns the bounded, operation-correlated store trace.
    #[must_use]
    pub fn store_trace(&self) -> &[FaultTraceEntry] {
        &self.store_trace
    }

    /// Returns the final committed fault-store snapshot.
    #[must_use]
    pub const fn committed(&self) -> &FaultStoreSnapshot {
        &self.committed
    }

    /// Returns the exact terminal lifecycle and counter state.
    #[must_use]
    pub const fn end_state(&self) -> DurabilitySoakEndState {
        self.end_state
    }

    /// Returns the canonical report digest.
    #[must_use]
    pub const fn digest(&self) -> DurabilitySoakDigest {
        self.digest
    }
}

/// Failure to execute or validate the fixed soak schedule.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DurabilitySoakError {
    /// The reusable durability battery could not be configured or observed.
    #[error(transparent)]
    Battery(#[from] DurabilityBatteryError),
    /// The fault-store control surface failed.
    #[error(transparent)]
    Control(#[from] FaultStoreControlError),
    /// A deterministic bounded player fixture was rejected.
    #[error(transparent)]
    Fixture(#[from] StorageError),
    /// The reusable battery returned a different matrix size.
    #[error("durability battery returned {actual} cases, expected {expected}")]
    BatteryCaseCount {
        /// Actual case count.
        actual: usize,
        /// Required fixed case count.
        expected: usize,
    },
    /// The logical trace tried to exceed its fixed step ceiling.
    #[error("durability soak step {attempted} exceeds the fixed maximum of {maximum}")]
    StepLimitExceeded {
        /// Step index that would have been appended.
        attempted: usize,
        /// Fixed trace capacity.
        maximum: usize,
    },
    /// The store trace exceeded its separately declared ceiling.
    #[error("durability soak store trace has {actual} entries, above the maximum of {maximum}")]
    StoreTraceLimitExceeded {
        /// Observed entry count.
        actual: usize,
        /// Declared ceiling.
        maximum: usize,
    },
    /// A lifecycle operation contradicted the connection state machine.
    #[error("cycle {cycle} cannot run {action:?} in the current connection state")]
    ConnectionState {
        /// Zero-based cycle.
        cycle: usize,
        /// Rejected action.
        action: DurabilitySoakAction,
    },
    /// An edit was queued while the capacity-one pending slot was occupied.
    #[error("cycle {cycle} exceeded the pending-edit capacity of {capacity}")]
    PendingEditCapacity {
        /// Zero-based cycle.
        cycle: usize,
        /// Fixed pending-edit capacity.
        capacity: usize,
    },
    /// Persistence ran without a queued edit.
    #[error("cycle {cycle} tried to persist without a pending edit")]
    MissingPendingEdit {
        /// Zero-based cycle.
        cycle: usize,
    },
    /// A scheduled fault unexpectedly returned success.
    #[error("cycle {cycle} did not surface the scheduled {fault:?} fault")]
    ExpectedFaultNotObserved {
        /// Zero-based cycle.
        cycle: usize,
        /// Fault that should have been observed.
        fault: DurabilitySoakFault,
    },
    /// A store call returned an error other than the scheduled typed result.
    #[error("cycle {cycle} {action:?} returned an unexpected store error: {source}")]
    UnexpectedStoreError {
        /// Zero-based cycle.
        cycle: usize,
        /// Operation that failed.
        action: DurabilitySoakAction,
        /// Unexpected classified server error.
        #[source]
        source: ServerError,
    },
    /// A returned or committed journal receipt did not match the fixed cycle.
    #[error("cycle {cycle} returned an invalid journal receipt: {receipt:?}")]
    InvalidReceipt {
        /// Zero-based cycle.
        cycle: usize,
        /// Receipt that failed validation.
        receipt: JournalAppendReceipt,
    },
    /// A committed journal receipt was absent during confirmation/replay.
    #[error("cycle {cycle} has no committed receipt for its batch")]
    MissingReceipt {
        /// Zero-based cycle.
        cycle: usize,
    },
    /// A restart found no durable player record.
    #[error("cycle {cycle} restart found no durable player record")]
    MissingPlayerRecord {
        /// Zero-based cycle.
        cycle: usize,
    },
    /// A restart found a malformed deterministic player fixture.
    #[error("cycle {cycle} restart found an invalid deterministic player record")]
    InvalidPlayerRecord {
        /// Zero-based cycle.
        cycle: usize,
    },
    /// A restart recovered a different player generation.
    #[error(
        "cycle {cycle} recovered player generation {actual}, expected committed generation {expected}"
    )]
    UnexpectedPlayerGeneration {
        /// Zero-based cycle.
        cycle: usize,
        /// Recovered generation.
        actual: usize,
        /// Expected durable generation.
        expected: usize,
    },
    /// The fixed schedule ended with an unexpected exact value.
    #[error("durability soak terminal invariant failed: {aspect}")]
    TerminalStateMismatch {
        /// Exact terminal aspect that differed.
        aspect: &'static str,
    },
    /// A canonical vector or byte field length did not fit the v1 `u64` grammar.
    #[error("canonical durability soak field {field} does not fit u64")]
    CanonicalLengthOverflow {
        /// Field whose length could not be represented.
        field: &'static str,
    },
    /// A future non-exhaustive enum value has no canonical v1 tag.
    #[error("{resource} has no canonical durability-soak-v1 encoding")]
    UnsupportedCanonicalValue {
        /// Value class missing a v1 assignment.
        resource: &'static str,
    },
}

/// Fixed, reproducibly seeded durability soak/chaos harness.
///
/// The seed namespaces player and journal identities; it does not choose
/// scenarios or operation order. The supplied mutation is copied once per
/// cycle with a deterministic tick/provisional-ID offset. Every run executes
/// the same 43-step schedule and returns all injected faults as
/// [`DurabilitySoakStepOutcome::Fault`] values rather than aborting the run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurabilitySoakHarness {
    seed: u64,
    mutation_template: BlockMutationLogRecord,
}

impl DurabilitySoakHarness {
    /// Builds the fixed soak for a deterministic fixture namespace.
    #[must_use]
    pub const fn new(seed: u64, mutation_template: BlockMutationLogRecord) -> Self {
        Self {
            seed,
            mutation_template,
        }
    }

    /// Runs the reusable battery and all six lifecycle cycles serially.
    ///
    /// # Errors
    ///
    /// Returns a classified error if fixture construction, fault scheduling,
    /// the underlying store, a schedule bound, a receipt, a restart oracle, or
    /// the canonical digest contract differs from the fixed definition.
    pub async fn run(&self) -> Result<DurabilitySoakReport, DurabilitySoakError> {
        let battery = DurabilityFaultBattery::new(self.seed, self.mutation_template)
            .run()
            .await?;
        if battery.cases().len() != DURABILITY_BATTERY_CASES {
            return Err(DurabilitySoakError::BatteryCaseCount {
                actual: battery.cases().len(),
                expected: DURABILITY_BATTERY_CASES,
            });
        }

        let store = FaultInjectingStore::new();
        let player = self.player();
        let mut state = RunState::new();
        let mut steps = Vec::with_capacity(DURABILITY_SOAK_STEPS);
        push_step(
            &mut steps,
            None,
            None,
            DurabilitySoakAction::RunBattery,
            DurabilitySoakStepOutcome::BatteryCompleted {
                cases: battery.cases().len(),
            },
        )?;

        for (cycle, scenario) in SCHEDULE.into_iter().enumerate() {
            self.run_cycle(cycle, scenario, &store, player, &mut state, &mut steps)
                .await?;
        }

        let committed = store.snapshot()?;
        let store_trace = store.trace()?;
        let scheduled_faults = store.scheduled_faults()?;
        validate_terminal(
            self,
            &state,
            &steps,
            &store_trace,
            &committed,
            scheduled_faults,
            player,
        )?;
        let durable_player_generation =
            state
                .durable_player_generation
                .ok_or(DurabilitySoakError::TerminalStateMismatch {
                    aspect: "final durable player generation",
                })?;
        let end_state = DurabilitySoakEndState {
            player,
            connected: state.connected,
            pending_edits: usize::from(state.pending.is_some()),
            completed_cycles: state.completed_cycles,
            committed_mutations: committed.block_mutations().len(),
            last_mutation_id: committed
                .block_mutations()
                .last()
                .map(BlockMutationLogRecord::id),
            durable_player_generation,
            attempted_operations: committed.attempted_operations(),
            committed_operations: committed.committed_operations(),
            successful_responses: committed.successful_responses(),
            store_trace_entries: store_trace.len(),
        };
        let digest = canonical_digest(
            self.seed,
            &battery,
            &steps,
            &store_trace,
            &committed,
            end_state,
        )?;
        Ok(DurabilitySoakReport {
            battery,
            steps,
            store_trace,
            committed,
            end_state,
            digest,
        })
    }

    async fn run_cycle(
        &self,
        cycle: usize,
        scenario: DurabilitySoakScenario,
        store: &FaultInjectingStore,
        player: PlayerId,
        state: &mut RunState,
        steps: &mut Vec<DurabilitySoakStep>,
    ) -> Result<(), DurabilitySoakError> {
        connect(cycle, scenario, state, steps)?;
        let mutation = self.mutation(cycle)?;
        queue_edit(cycle, scenario, mutation, state, steps)?;
        let batch_id = self.batch_id(cycle)?;

        let first = self
            .persist_journal(cycle, scenario, store, batch_id, mutation, state)
            .await?;
        push_step(
            steps,
            Some(cycle),
            Some(scenario),
            DurabilitySoakAction::PersistJournal,
            first,
        )?;

        let resolved = self
            .resolve_journal(cycle, scenario, store, batch_id, state)
            .await?;
        push_step(
            steps,
            Some(cycle),
            Some(scenario),
            DurabilitySoakAction::ResolveJournal,
            resolved,
        )?;

        let player_outcome = self.persist_player(cycle, scenario, store, player).await?;
        push_step(
            steps,
            Some(cycle),
            Some(scenario),
            DurabilitySoakAction::PersistPlayer,
            player_outcome,
        )?;

        disconnect(cycle, scenario, state, steps)?;
        self.restart(cycle, scenario, store, player, state, steps)
            .await
    }

    async fn persist_journal(
        &self,
        cycle: usize,
        scenario: DurabilitySoakScenario,
        store: &FaultInjectingStore,
        batch_id: JournalBatchId,
        mutation: BlockMutationLogRecord,
        state: &mut RunState,
    ) -> Result<DurabilitySoakStepOutcome, DurabilitySoakError> {
        match scenario {
            DurabilitySoakScenario::JournalBeforeCommitRetry => {
                store.fail_next_before_commit()?;
                observe_fault(
                    store
                        .append_block_mutation_batch(batch_id, vec![mutation])
                        .await,
                    cycle,
                    DurabilitySoakAction::PersistJournal,
                    DurabilitySoakFault::JournalBeforeCommit,
                    "fault store injected failure before commit",
                )
            }
            DurabilitySoakScenario::JournalAcknowledgementLossReplay => {
                store.lose_next_ack_after_commit()?;
                let outcome = observe_fault(
                    store
                        .append_block_mutation_batch(batch_id, vec![mutation])
                        .await,
                    cycle,
                    DurabilitySoakAction::PersistJournal,
                    DurabilitySoakFault::JournalAcknowledgementLost,
                    "fault store committed but lost acknowledgement",
                )?;
                let receipt = committed_receipt(store, cycle, batch_id)?;
                validate_receipt(cycle, batch_id, receipt)?;
                state.receipts[cycle] = Some(receipt);
                Ok(outcome)
            }
            DurabilitySoakScenario::Clean
            | DurabilitySoakScenario::PlayerCommitError
            | DurabilitySoakScenario::PlayerAcknowledgementLoss => {
                let receipt = clean_store_result(
                    store
                        .append_block_mutation_batch(batch_id, vec![mutation])
                        .await,
                    cycle,
                    DurabilitySoakAction::PersistJournal,
                )?;
                validate_receipt(cycle, batch_id, receipt)?;
                state.receipts[cycle] = Some(receipt);
                state.pending = None;
                Ok(DurabilitySoakStepOutcome::JournalCommitted(receipt))
            }
        }
    }

    async fn resolve_journal(
        &self,
        cycle: usize,
        scenario: DurabilitySoakScenario,
        store: &FaultInjectingStore,
        batch_id: JournalBatchId,
        state: &mut RunState,
    ) -> Result<DurabilitySoakStepOutcome, DurabilitySoakError> {
        match scenario {
            DurabilitySoakScenario::JournalBeforeCommitRetry
            | DurabilitySoakScenario::JournalAcknowledgementLossReplay => {
                let pending = state
                    .pending
                    .ok_or(DurabilitySoakError::MissingPendingEdit { cycle })?;
                let receipt = clean_store_result(
                    store
                        .append_block_mutation_batch(batch_id, vec![pending])
                        .await,
                    cycle,
                    DurabilitySoakAction::ResolveJournal,
                )?;
                validate_receipt(cycle, batch_id, receipt)?;
                let outcome = if scenario == DurabilitySoakScenario::JournalBeforeCommitRetry {
                    state.receipts[cycle] = Some(receipt);
                    DurabilitySoakStepOutcome::JournalRetried(receipt)
                } else {
                    if state.receipts[cycle] != Some(receipt) {
                        return Err(DurabilitySoakError::InvalidReceipt { cycle, receipt });
                    }
                    DurabilitySoakStepOutcome::JournalReceiptReplayed(receipt)
                };
                state.pending = None;
                Ok(outcome)
            }
            DurabilitySoakScenario::Clean
            | DurabilitySoakScenario::PlayerCommitError
            | DurabilitySoakScenario::PlayerAcknowledgementLoss => {
                let receipt = committed_receipt(store, cycle, batch_id)?;
                validate_receipt(cycle, batch_id, receipt)?;
                if state.receipts[cycle] != Some(receipt) {
                    return Err(DurabilitySoakError::InvalidReceipt { cycle, receipt });
                }
                Ok(DurabilitySoakStepOutcome::JournalConfirmed(receipt))
            }
        }
    }

    async fn persist_player(
        &self,
        cycle: usize,
        scenario: DurabilitySoakScenario,
        store: &FaultInjectingStore,
        player: PlayerId,
    ) -> Result<DurabilitySoakStepOutcome, DurabilitySoakError> {
        let record = self.player_record(cycle)?;
        match scenario {
            DurabilitySoakScenario::PlayerCommitError => {
                store.return_next_commit_error()?;
                observe_fault(
                    store.save_player(player, record).await,
                    cycle,
                    DurabilitySoakAction::PersistPlayer,
                    DurabilitySoakFault::PlayerCommit,
                    "fault store injected commit failure",
                )
            }
            DurabilitySoakScenario::PlayerAcknowledgementLoss => {
                store.lose_next_ack_after_commit()?;
                observe_fault(
                    store.save_player(player, record).await,
                    cycle,
                    DurabilitySoakAction::PersistPlayer,
                    DurabilitySoakFault::PlayerAcknowledgementLost,
                    "fault store committed but lost acknowledgement",
                )
            }
            DurabilitySoakScenario::Clean
            | DurabilitySoakScenario::JournalBeforeCommitRetry
            | DurabilitySoakScenario::JournalAcknowledgementLossReplay => {
                clean_store_result(
                    store.save_player(player, record).await,
                    cycle,
                    DurabilitySoakAction::PersistPlayer,
                )?;
                Ok(DurabilitySoakStepOutcome::PlayerCommitted { generation: cycle })
            }
        }
    }

    async fn restart(
        &self,
        cycle: usize,
        scenario: DurabilitySoakScenario,
        store: &FaultInjectingStore,
        player: PlayerId,
        state: &mut RunState,
        steps: &mut Vec<DurabilitySoakStep>,
    ) -> Result<(), DurabilitySoakError> {
        let loaded = clean_store_result(
            store.load_player(player).await,
            cycle,
            DurabilitySoakAction::Restart,
        )?
        .ok_or(DurabilitySoakError::MissingPlayerRecord { cycle })?;
        let generation = self.player_generation(cycle, &loaded)?;
        let expected = EXPECTED_RESTART_GENERATIONS[cycle];
        if generation != expected {
            return Err(DurabilitySoakError::UnexpectedPlayerGeneration {
                cycle,
                actual: generation,
                expected,
            });
        }
        state.completed_cycles = state.completed_cycles.saturating_add(1);
        state.durable_player_generation = Some(generation);
        push_step(
            steps,
            Some(cycle),
            Some(scenario),
            DurabilitySoakAction::Restart,
            DurabilitySoakStepOutcome::Restarted {
                durable_player_generation: generation,
            },
        )
    }

    fn player(&self) -> PlayerId {
        PlayerId::offline(&format!("durability-soak-{:016x}", self.seed))
    }

    fn mutation(&self, cycle: usize) -> Result<BlockMutationLogRecord, DurabilitySoakError> {
        let offset =
            u64::try_from(cycle).map_err(|_| DurabilitySoakError::CanonicalLengthOverflow {
                field: "cycle mutation offset",
            })?;
        Ok(BlockMutationLogRecord::new(
            self.mutation_template.schema_version(),
            self.mutation_template.id().wrapping_add(offset),
            self.mutation_template.tick().wrapping_add(offset),
            self.mutation_template.actor(),
            self.mutation_template.pos(),
            self.mutation_template.old_state(),
            self.mutation_template.new_state(),
            self.mutation_template.cause(),
        ))
    }

    fn batch_id(&self, cycle: usize) -> Result<JournalBatchId, DurabilitySoakError> {
        let discriminator = u64::try_from(cycle)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(DurabilitySoakError::CanonicalLengthOverflow {
                field: "cycle batch discriminator",
            })?;
        let mut bytes = [0_u8; 16];
        bytes[..8].copy_from_slice(&self.seed.to_be_bytes());
        bytes[8..].copy_from_slice(&discriminator.to_be_bytes());
        Ok(JournalBatchId::from_bytes(bytes))
    }

    fn player_record(&self, cycle: usize) -> Result<PlayerRecord, DurabilitySoakError> {
        let generation =
            u8::try_from(cycle).map_err(|_| DurabilitySoakError::CanonicalLengthOverflow {
                field: "player generation",
            })?;
        let mut data = self.seed.to_be_bytes().to_vec();
        data.push(generation);
        Ok(PlayerRecord::new(
            SchemaVersion::new(1),
            game_mode(cycle),
            data,
        )?)
    }

    fn player_generation(
        &self,
        cycle: usize,
        record: &PlayerRecord,
    ) -> Result<usize, DurabilitySoakError> {
        let data = record.data();
        let Some((&generation, seed_bytes)) = data.split_last() else {
            return Err(DurabilitySoakError::InvalidPlayerRecord { cycle });
        };
        if seed_bytes != self.seed.to_be_bytes()
            || record.schema_version() != SchemaVersion::new(1)
            || record.game_mode() != game_mode(usize::from(generation))
        {
            return Err(DurabilitySoakError::InvalidPlayerRecord { cycle });
        }
        Ok(usize::from(generation))
    }
}

#[derive(Debug)]
struct RunState {
    connected: bool,
    pending: Option<BlockMutationLogRecord>,
    receipts: [Option<JournalAppendReceipt>; DURABILITY_SOAK_CYCLES],
    completed_cycles: usize,
    durable_player_generation: Option<usize>,
}

impl RunState {
    const fn new() -> Self {
        Self {
            connected: false,
            pending: None,
            receipts: [None; DURABILITY_SOAK_CYCLES],
            completed_cycles: 0,
            durable_player_generation: None,
        }
    }
}

fn connect(
    cycle: usize,
    scenario: DurabilitySoakScenario,
    state: &mut RunState,
    steps: &mut Vec<DurabilitySoakStep>,
) -> Result<(), DurabilitySoakError> {
    if state.connected {
        return Err(DurabilitySoakError::ConnectionState {
            cycle,
            action: DurabilitySoakAction::Connect,
        });
    }
    state.connected = true;
    push_step(
        steps,
        Some(cycle),
        Some(scenario),
        DurabilitySoakAction::Connect,
        DurabilitySoakStepOutcome::Connected,
    )
}

fn queue_edit(
    cycle: usize,
    scenario: DurabilitySoakScenario,
    mutation: BlockMutationLogRecord,
    state: &mut RunState,
    steps: &mut Vec<DurabilitySoakStep>,
) -> Result<(), DurabilitySoakError> {
    if !state.connected {
        return Err(DurabilitySoakError::ConnectionState {
            cycle,
            action: DurabilitySoakAction::QueueEdit,
        });
    }
    if state.pending.is_some() {
        return Err(DurabilitySoakError::PendingEditCapacity {
            cycle,
            capacity: DURABILITY_SOAK_PENDING_CAPACITY,
        });
    }
    state.pending = Some(mutation);
    push_step(
        steps,
        Some(cycle),
        Some(scenario),
        DurabilitySoakAction::QueueEdit,
        DurabilitySoakStepOutcome::EditQueued {
            pending: 1,
            capacity: DURABILITY_SOAK_PENDING_CAPACITY,
        },
    )
}

fn disconnect(
    cycle: usize,
    scenario: DurabilitySoakScenario,
    state: &mut RunState,
    steps: &mut Vec<DurabilitySoakStep>,
) -> Result<(), DurabilitySoakError> {
    if !state.connected || state.pending.is_some() {
        return Err(DurabilitySoakError::ConnectionState {
            cycle,
            action: DurabilitySoakAction::Disconnect,
        });
    }
    state.connected = false;
    push_step(
        steps,
        Some(cycle),
        Some(scenario),
        DurabilitySoakAction::Disconnect,
        DurabilitySoakStepOutcome::Disconnected,
    )
}

fn push_step(
    steps: &mut Vec<DurabilitySoakStep>,
    cycle: Option<usize>,
    scenario: Option<DurabilitySoakScenario>,
    action: DurabilitySoakAction,
    outcome: DurabilitySoakStepOutcome,
) -> Result<(), DurabilitySoakError> {
    if steps.len() >= DURABILITY_SOAK_STEPS {
        return Err(DurabilitySoakError::StepLimitExceeded {
            attempted: steps.len(),
            maximum: DURABILITY_SOAK_STEPS,
        });
    }
    steps.push(DurabilitySoakStep {
        index: steps.len(),
        cycle,
        scenario,
        action,
        outcome,
    });
    Ok(())
}

fn clean_store_result<T>(
    result: Result<T, ServerError>,
    cycle: usize,
    action: DurabilitySoakAction,
) -> Result<T, DurabilitySoakError> {
    result.map_err(|source| DurabilitySoakError::UnexpectedStoreError {
        cycle,
        action,
        source,
    })
}

fn observe_fault<T>(
    result: Result<T, ServerError>,
    cycle: usize,
    action: DurabilitySoakAction,
    fault: DurabilitySoakFault,
    expected_context: &str,
) -> Result<DurabilitySoakStepOutcome, DurabilitySoakError> {
    match result {
        Err(ServerError::Internal { context }) if context == expected_context => {
            Ok(DurabilitySoakStepOutcome::Fault(fault))
        }
        Err(source) => Err(DurabilitySoakError::UnexpectedStoreError {
            cycle,
            action,
            source,
        }),
        Ok(_) => Err(DurabilitySoakError::ExpectedFaultNotObserved { cycle, fault }),
    }
}

fn committed_receipt(
    store: &FaultInjectingStore,
    cycle: usize,
    batch_id: JournalBatchId,
) -> Result<JournalAppendReceipt, DurabilitySoakError> {
    store
        .snapshot()?
        .journal_receipt(batch_id)
        .ok_or(DurabilitySoakError::MissingReceipt { cycle })
}

fn validate_receipt(
    cycle: usize,
    batch_id: JournalBatchId,
    receipt: JournalAppendReceipt,
) -> Result<(), DurabilitySoakError> {
    let expected =
        u64::try_from(cycle).map_err(|_| DurabilitySoakError::CanonicalLengthOverflow {
            field: "receipt sequence",
        })?;
    if receipt.batch_id() != batch_id
        || receipt.first_id() != Some(expected)
        || receipt.last_id() != Some(expected)
        || receipt.len() != 1
    {
        return Err(DurabilitySoakError::InvalidReceipt { cycle, receipt });
    }
    Ok(())
}

fn game_mode(generation: usize) -> GameMode {
    match generation % 4 {
        0 => GameMode::Survival,
        1 => GameMode::Creative,
        2 => GameMode::Adventure,
        _ => GameMode::Spectator,
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_terminal(
    harness: &DurabilitySoakHarness,
    state: &RunState,
    steps: &[DurabilitySoakStep],
    store_trace: &[FaultTraceEntry],
    committed: &FaultStoreSnapshot,
    scheduled_faults: usize,
    player: PlayerId,
) -> Result<(), DurabilitySoakError> {
    if steps.len() != DURABILITY_SOAK_STEPS {
        return Err(DurabilitySoakError::TerminalStateMismatch {
            aspect: "logical step count",
        });
    }
    if store_trace.len() > MAX_DURABILITY_SOAK_STORE_TRACE {
        return Err(DurabilitySoakError::StoreTraceLimitExceeded {
            actual: store_trace.len(),
            maximum: MAX_DURABILITY_SOAK_STORE_TRACE,
        });
    }
    if store_trace.len() != EXPECTED_STORE_TRACE_ENTRIES {
        return Err(DurabilitySoakError::TerminalStateMismatch {
            aspect: "store trace entry count",
        });
    }
    if state.connected || state.pending.is_some() {
        return Err(DurabilitySoakError::TerminalStateMismatch {
            aspect: "volatile connection and pending-edit state",
        });
    }
    if state.completed_cycles != DURABILITY_SOAK_CYCLES
        || state.durable_player_generation != Some(DURABILITY_SOAK_CYCLES - 1)
    {
        return Err(DurabilitySoakError::TerminalStateMismatch {
            aspect: "completed cycle and player generation",
        });
    }
    if state.receipts.iter().any(Option::is_none) || scheduled_faults != 0 {
        return Err(DurabilitySoakError::TerminalStateMismatch {
            aspect: "receipt and fault schedule exhaustion",
        });
    }
    if committed.attempted_operations() != EXPECTED_ATTEMPTED_OPERATIONS
        || committed.committed_operations() != EXPECTED_COMMITTED_OPERATIONS
        || committed.successful_responses() != EXPECTED_SUCCESSFUL_RESPONSES
    {
        return Err(DurabilitySoakError::TerminalStateMismatch {
            aspect: "store operation counters",
        });
    }
    if committed.block_mutations().len() != DURABILITY_SOAK_CYCLES {
        return Err(DurabilitySoakError::TerminalStateMismatch {
            aspect: "committed mutation count",
        });
    }
    for (cycle, mutation) in committed.block_mutations().iter().enumerate() {
        let expected = harness.mutation(cycle)?;
        let sequence =
            u64::try_from(cycle).map_err(|_| DurabilitySoakError::CanonicalLengthOverflow {
                field: "terminal mutation sequence",
            })?;
        if mutation.id() != sequence
            || mutation.schema_version() != expected.schema_version()
            || mutation.tick() != expected.tick()
            || mutation.actor() != expected.actor()
            || mutation.pos() != expected.pos()
            || mutation.old_state() != expected.old_state()
            || mutation.new_state() != expected.new_state()
            || mutation.cause() != expected.cause()
        {
            return Err(DurabilitySoakError::TerminalStateMismatch {
                aspect: "committed mutation sequence",
            });
        }
    }
    let expected_player = harness.player_record(DURABILITY_SOAK_CYCLES - 1)?;
    if committed.player(player) != Some(&expected_player) {
        return Err(DurabilitySoakError::TerminalStateMismatch {
            aspect: "final committed player record",
        });
    }
    if !store_trace
        .windows(2)
        .all(|pair| pair[0].sequence() < pair[1].sequence())
    {
        return Err(DurabilitySoakError::TerminalStateMismatch {
            aspect: "strictly ordered store trace",
        });
    }
    Ok(())
}

fn canonical_digest(
    seed: u64,
    battery: &DurabilityBatteryReport,
    steps: &[DurabilitySoakStep],
    store_trace: &[FaultTraceEntry],
    committed: &FaultStoreSnapshot,
    end_state: DurabilitySoakEndState,
) -> Result<DurabilitySoakDigest, DurabilitySoakError> {
    let mut writer = CanonicalWriter::new();
    writer.field(DOMAIN, "domain")?;
    writer.u64(seed);
    writer.battery(battery)?;
    writer.steps(steps)?;
    writer.trace(store_trace, "store trace")?;
    writer.snapshot(committed, end_state.player)?;
    writer.end_state(end_state)?;
    Ok(DurabilitySoakDigest(writer.finish()))
}

struct CanonicalWriter {
    hasher: Sha256,
}

impl CanonicalWriter {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> [u8; 32] {
        self.hasher.finalize().into()
    }

    fn byte(&mut self, value: u8) {
        self.hasher.update([value]);
    }

    fn u32(&mut self, value: u32) {
        self.hasher.update(value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.hasher.update(value.to_le_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.hasher.update(value.to_le_bytes());
    }

    fn usize(&mut self, value: usize, field: &'static str) -> Result<(), DurabilitySoakError> {
        self.u64(
            u64::try_from(value)
                .map_err(|_| DurabilitySoakError::CanonicalLengthOverflow { field })?,
        );
        Ok(())
    }

    fn field(&mut self, value: &[u8], field: &'static str) -> Result<(), DurabilitySoakError> {
        self.usize(value.len(), field)?;
        self.hasher.update(value);
        Ok(())
    }

    fn optional_usize(
        &mut self,
        value: Option<usize>,
        field: &'static str,
    ) -> Result<(), DurabilitySoakError> {
        match value {
            Some(value) => {
                self.byte(1);
                self.usize(value, field)?;
            }
            None => self.byte(0),
        }
        Ok(())
    }

    fn optional_u64(&mut self, value: Option<u64>) {
        match value {
            Some(value) => {
                self.byte(1);
                self.u64(value);
            }
            None => self.byte(0),
        }
    }

    fn battery(&mut self, report: &DurabilityBatteryReport) -> Result<(), DurabilitySoakError> {
        self.byte(1);
        self.u64(report.seed());
        self.usize(report.cases().len(), "battery cases")?;
        for case in report.cases() {
            self.surface(case.surface());
            self.battery_scenario(case.scenario());
            self.usize(case.outcomes().len(), "battery outcomes")?;
            for outcome in case.outcomes() {
                self.battery_outcome(outcome)?;
            }
            self.usize(
                case.attempted().operations().len(),
                "battery attempted operations",
            )?;
            self.u64(case.committed().attempted_operations());
            self.u64(case.committed().committed_operations());
            self.u64(case.committed().successful_responses());
            self.trace(case.trace(), "battery trace")?;
        }
        Ok(())
    }

    fn steps(&mut self, steps: &[DurabilitySoakStep]) -> Result<(), DurabilitySoakError> {
        self.byte(2);
        self.usize(steps.len(), "logical steps")?;
        for step in steps {
            self.usize(step.index, "step index")?;
            self.optional_usize(step.cycle, "step cycle")?;
            match step.scenario {
                Some(scenario) => {
                    self.byte(1);
                    self.soak_scenario(scenario);
                }
                None => self.byte(0),
            }
            self.action(step.action);
            self.step_outcome(step.outcome)?;
        }
        Ok(())
    }

    fn trace(
        &mut self,
        trace: &[FaultTraceEntry],
        field: &'static str,
    ) -> Result<(), DurabilitySoakError> {
        self.usize(trace.len(), field)?;
        for entry in trace {
            self.u64(entry.sequence());
            self.u64(entry.operation_id());
            self.operation(entry.operation());
            self.stage(entry.stage());
            self.usize(entry.item_count(), "trace item count")?;
        }
        Ok(())
    }

    fn snapshot(
        &mut self,
        snapshot: &FaultStoreSnapshot,
        player: PlayerId,
    ) -> Result<(), DurabilitySoakError> {
        self.byte(3);
        self.usize(snapshot.block_mutations().len(), "committed mutation count")?;
        for mutation in snapshot.block_mutations() {
            self.mutation(mutation)?;
        }
        match snapshot.player(player) {
            Some(record) => {
                self.byte(1);
                self.player(player);
                self.u32(record.schema_version().get());
                self.byte(record.game_mode().as_id());
                self.field(record.data(), "player data")?;
            }
            None => self.byte(0),
        }
        self.u64(snapshot.attempted_operations());
        self.u64(snapshot.committed_operations());
        self.u64(snapshot.successful_responses());
        Ok(())
    }

    fn end_state(&mut self, state: DurabilitySoakEndState) -> Result<(), DurabilitySoakError> {
        self.byte(4);
        self.player(state.player);
        self.byte(u8::from(state.connected));
        self.usize(state.pending_edits, "end pending edits")?;
        self.usize(state.completed_cycles, "end completed cycles")?;
        self.usize(state.committed_mutations, "end committed mutations")?;
        self.optional_u64(state.last_mutation_id);
        self.usize(state.durable_player_generation, "end player generation")?;
        self.u64(state.attempted_operations);
        self.u64(state.committed_operations);
        self.u64(state.successful_responses);
        self.usize(state.store_trace_entries, "end store trace entries")
    }

    fn mutation(&mut self, mutation: &BlockMutationLogRecord) -> Result<(), DurabilitySoakError> {
        self.u32(mutation.schema_version().get());
        self.u64(mutation.id());
        self.u64(mutation.tick());
        match mutation.actor() {
            MutationActor::Player(player) => {
                self.byte(1);
                self.player(player);
            }
            MutationActor::System => self.byte(2),
            _ => {
                return Err(DurabilitySoakError::UnsupportedCanonicalValue {
                    resource: "mutation actor",
                })
            }
        }
        let pos = mutation.pos();
        self.i32(pos.x());
        self.i32(pos.y());
        self.i32(pos.z());
        self.u32(mutation.old_state().as_u32());
        self.u32(mutation.new_state().as_u32());
        let cause = match mutation.cause() {
            MutationLogCause::PlayerCreative => 1,
            MutationLogCause::Command => 2,
            MutationLogCause::Plugin => 3,
            MutationLogCause::Test => 4,
            _ => {
                return Err(DurabilitySoakError::UnsupportedCanonicalValue {
                    resource: "mutation cause",
                })
            }
        };
        self.byte(cause);
        Ok(())
    }

    fn player(&mut self, player: PlayerId) {
        self.hasher.update(player.as_uuid().into_bytes());
    }

    fn receipt(&mut self, receipt: JournalAppendReceipt) -> Result<(), DurabilitySoakError> {
        self.hasher.update(receipt.batch_id().into_bytes());
        self.optional_u64(receipt.first_id());
        self.optional_u64(receipt.last_id());
        self.usize(receipt.len(), "receipt length")
    }

    fn server_error(&mut self, error: &ServerError) -> Result<(), DurabilitySoakError> {
        match error {
            ServerError::NotFound(message) => {
                self.byte(1);
                self.field(message.as_bytes(), "server error")?;
            }
            ServerError::InvalidState(message) => {
                self.byte(2);
                self.field(message.as_bytes(), "server error")?;
            }
            ServerError::Capacity(message) => {
                self.byte(3);
                self.field(message.as_bytes(), "server error")?;
            }
            ServerError::Config(message) => {
                self.byte(4);
                self.field(message.as_bytes(), "server error")?;
            }
            ServerError::Unsupported(message) => {
                self.byte(5);
                self.field(message.as_bytes(), "server error")?;
            }
            ServerError::Internal { context } => {
                self.byte(6);
                self.field(context.as_bytes(), "server error")?;
            }
            _ => {
                return Err(DurabilitySoakError::UnsupportedCanonicalValue {
                    resource: "server error",
                })
            }
        }
        Ok(())
    }

    fn battery_outcome(&mut self, outcome: &DurabilityOutcome) -> Result<(), DurabilitySoakError> {
        match outcome {
            DurabilityOutcome::Succeeded => self.byte(1),
            DurabilityOutcome::Receipt(receipt) => {
                self.byte(2);
                self.receipt(*receipt)?;
            }
            DurabilityOutcome::Failed(error) => {
                self.byte(3);
                self.server_error(error)?;
            }
        }
        Ok(())
    }

    fn step_outcome(
        &mut self,
        outcome: DurabilitySoakStepOutcome,
    ) -> Result<(), DurabilitySoakError> {
        match outcome {
            DurabilitySoakStepOutcome::BatteryCompleted { cases } => {
                self.byte(1);
                self.usize(cases, "step battery cases")?;
            }
            DurabilitySoakStepOutcome::Connected => self.byte(2),
            DurabilitySoakStepOutcome::EditQueued { pending, capacity } => {
                self.byte(3);
                self.usize(pending, "step pending edits")?;
                self.usize(capacity, "step pending capacity")?;
            }
            DurabilitySoakStepOutcome::JournalCommitted(receipt) => {
                self.byte(4);
                self.receipt(receipt)?;
            }
            DurabilitySoakStepOutcome::JournalConfirmed(receipt) => {
                self.byte(5);
                self.receipt(receipt)?;
            }
            DurabilitySoakStepOutcome::JournalRetried(receipt) => {
                self.byte(6);
                self.receipt(receipt)?;
            }
            DurabilitySoakStepOutcome::JournalReceiptReplayed(receipt) => {
                self.byte(7);
                self.receipt(receipt)?;
            }
            DurabilitySoakStepOutcome::PlayerCommitted { generation } => {
                self.byte(8);
                self.usize(generation, "step player generation")?;
            }
            DurabilitySoakStepOutcome::Fault(fault) => {
                self.byte(9);
                self.soak_fault(fault);
            }
            DurabilitySoakStepOutcome::Disconnected => self.byte(10),
            DurabilitySoakStepOutcome::Restarted {
                durable_player_generation,
            } => {
                self.byte(11);
                self.usize(durable_player_generation, "step restart generation")?;
            }
        }
        Ok(())
    }

    fn surface(&mut self, surface: DurabilitySurface) {
        let tag = match surface {
            DurabilitySurface::World => 1,
            DurabilitySurface::Player => 2,
            DurabilitySurface::Journal => 3,
        };
        self.byte(tag);
    }

    fn battery_scenario(&mut self, scenario: DurabilityScenario) {
        let tag = match scenario {
            DurabilityScenario::FailBeforeCommit => 1,
            DurabilityScenario::CommitError => 2,
            DurabilityScenario::AckLoss => 3,
            DurabilityScenario::RequestCloseWhileHeld => 4,
            DurabilityScenario::ResponseCloseWhileHeld => 5,
            DurabilityScenario::ReceiptReplay => 6,
            DurabilityScenario::PayloadMismatch => 7,
            DurabilityScenario::EmptyBatch => 8,
            DurabilityScenario::MaximumBatch => 9,
            DurabilityScenario::OversizedBatch => 10,
        };
        self.byte(tag);
    }

    fn soak_scenario(&mut self, scenario: DurabilitySoakScenario) {
        let tag = match scenario {
            DurabilitySoakScenario::Clean => 1,
            DurabilitySoakScenario::JournalBeforeCommitRetry => 2,
            DurabilitySoakScenario::JournalAcknowledgementLossReplay => 3,
            DurabilitySoakScenario::PlayerCommitError => 4,
            DurabilitySoakScenario::PlayerAcknowledgementLoss => 5,
        };
        self.byte(tag);
    }

    fn action(&mut self, action: DurabilitySoakAction) {
        let tag = match action {
            DurabilitySoakAction::RunBattery => 1,
            DurabilitySoakAction::Connect => 2,
            DurabilitySoakAction::QueueEdit => 3,
            DurabilitySoakAction::PersistJournal => 4,
            DurabilitySoakAction::ResolveJournal => 5,
            DurabilitySoakAction::PersistPlayer => 6,
            DurabilitySoakAction::Disconnect => 7,
            DurabilitySoakAction::Restart => 8,
        };
        self.byte(tag);
    }

    fn soak_fault(&mut self, fault: DurabilitySoakFault) {
        self.byte(match fault {
            DurabilitySoakFault::JournalBeforeCommit => 1,
            DurabilitySoakFault::JournalAcknowledgementLost => 2,
            DurabilitySoakFault::PlayerCommit => 3,
            DurabilitySoakFault::PlayerAcknowledgementLost => 4,
        });
    }

    fn operation(&mut self, operation: FaultOperation) {
        let tag = match operation {
            FaultOperation::LoadChunk => 1,
            FaultOperation::SaveChunk => 2,
            FaultOperation::SaveChunks => 3,
            FaultOperation::DeleteChunk => 4,
            FaultOperation::LoadChunkOverlay => 5,
            FaultOperation::SaveChunkOverlays => 6,
            FaultOperation::AppendBlockMutations => 7,
            FaultOperation::AppendBlockMutationBatch => 8,
            FaultOperation::LoadEntity => 9,
            FaultOperation::SaveEntity => 10,
            FaultOperation::SaveEntities => 11,
            FaultOperation::DeleteEntity => 12,
            FaultOperation::LoadPlayer => 13,
            FaultOperation::SavePlayer => 14,
            FaultOperation::DeletePlayer => 15,
        };
        self.byte(tag);
    }

    fn stage(&mut self, stage: FaultStage) {
        let tag = match stage {
            FaultStage::Attempted => 1,
            FaultStage::InjectedFailure => 2,
            FaultStage::BeforeCommitFailure => 3,
            FaultStage::HeldBeforeCommit => 4,
            FaultStage::CommitFailure => 5,
            FaultStage::Committed => 6,
            FaultStage::ReceiptReplayed => 7,
            FaultStage::HeldResponse => 8,
            FaultStage::AcknowledgementLost => 9,
            FaultStage::RequestClosed => 10,
            FaultStage::ResponseClosed => 11,
            FaultStage::Succeeded => 12,
        };
        self.byte(tag);
    }
}
