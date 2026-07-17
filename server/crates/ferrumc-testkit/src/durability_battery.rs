//! Reusable deterministic storage-durability fault battery.
//!
//! [`DurabilityFaultBattery`] runs a fixed matrix against fresh
//! [`FaultInjectingStore`](crate::FaultInjectingStore) instances. A caller
//! supplies a stable seed and one representative mutation record; the seed is
//! used only as a namespace for fixture IDs and payloads. It never selects
//! cases, drives a random source, reads a clock, or sleeps.

use ferrumc_core::{DimensionId, EntityId, GameMode, PlayerId, ServerError, WorldId};
use ferrumc_storage::{
    BlockMutationLogRecord, EntityKey, EntityRecord, JournalAppendReceipt, JournalBatchId,
    PlayerRecord, PlayerStore, SchemaVersion, StorageError, WorldStore, MAX_SAVE_BATCH,
};

use crate::fault_store::{
    FaultInjectingStore, FaultStoreAttemptedState, FaultStoreControlError, FaultStoreSnapshot,
    FaultTraceEntry,
};

/// Storage contract surface exercised by one durability-battery case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DurabilitySurface {
    /// Single and batched entity writes through [`WorldStore`].
    World,
    /// Player-record writes through [`PlayerStore`].
    Player,
    /// Tokenized block-mutation journal appends through [`WorldStore`].
    Journal,
}

/// Deterministic fault or boundary exercised by one battery case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DurabilityScenario {
    /// The accepted write fails at the explicit pre-commit cutpoint.
    FailBeforeCommit,
    /// The backend reports a commit failure without changing durable state.
    CommitError,
    /// The write commits but its acknowledgement is lost.
    AckLoss,
    /// An accepted pre-commit-held write survives request-side closure, while a
    /// later request is rejected.
    RequestCloseWhileHeld,
    /// A committed response-held write observes response-side closure.
    ResponseCloseWhileHeld,
    /// A lost-ack journal append is retried with the same normalized payload.
    ReceiptReplay,
    /// A committed journal token is reused with a different normalized payload.
    PayloadMismatch,
    /// A zero-length batch is submitted.
    EmptyBatch,
    /// A batch of exactly [`MAX_SAVE_BATCH`] records is submitted.
    MaximumBatch,
    /// A batch of `MAX_SAVE_BATCH + 1` records is submitted.
    OversizedBatch,
}

/// Classified result delivered by one battery call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DurabilityOutcome {
    /// The caller received a successful response.
    Succeeded,
    /// A journal caller received the durable range assigned to its batch.
    Receipt(JournalAppendReceipt),
    /// The caller received a classified server error.
    Failed(ServerError),
}

impl DurabilityOutcome {
    fn from_result<T>(result: Result<T, ServerError>) -> Self {
        match result {
            Ok(_) => Self::Succeeded,
            Err(error) => Self::Failed(error),
        }
    }

    fn from_journal_result(result: Result<JournalAppendReceipt, ServerError>) -> Self {
        match result {
            Ok(receipt) => Self::Receipt(receipt),
            Err(error) => Self::Failed(error),
        }
    }

    /// Returns the journal receipt delivered by this outcome, if any.
    #[must_use]
    pub const fn receipt(&self) -> Option<JournalAppendReceipt> {
        match self {
            Self::Receipt(receipt) => Some(*receipt),
            Self::Succeeded | Self::Failed(_) => None,
        }
    }
}

/// Complete observation of one deterministic battery case.
///
/// The attempted payloads, committed snapshot, and trace are captured only
/// after every call in the case has completed. Each case owns a fresh store, so
/// these views cannot be contaminated by another case.
#[derive(Debug, Clone, PartialEq)]
pub struct DurabilityCaseReport {
    surface: DurabilitySurface,
    scenario: DurabilityScenario,
    outcome: DurabilityOutcome,
    outcomes: Vec<DurabilityOutcome>,
    attempted: FaultStoreAttemptedState,
    committed: FaultStoreSnapshot,
    trace: Vec<FaultTraceEntry>,
}

impl DurabilityCaseReport {
    /// Returns the storage surface exercised by this case.
    #[must_use]
    pub fn surface(&self) -> DurabilitySurface {
        self.surface
    }

    /// Returns the fault or boundary exercised by this case.
    #[must_use]
    pub fn scenario(&self) -> DurabilityScenario {
        self.scenario
    }

    /// Returns every call result in execution order.
    ///
    /// Most cases contain one result. Receipt replay, payload mismatch, and
    /// request-close cases contain both the original and follow-up result.
    #[must_use]
    pub fn outcomes(&self) -> &[DurabilityOutcome] {
        &self.outcomes
    }

    /// Returns the final call result for this case.
    ///
    /// Every built-in case executes at least one call.
    #[must_use]
    pub fn outcome(&self) -> &DurabilityOutcome {
        &self.outcome
    }

    /// Returns exact accepted payloads, including attempts that did not commit.
    #[must_use]
    pub fn attempted(&self) -> &FaultStoreAttemptedState {
        &self.attempted
    }

    /// Returns the state and counters that actually committed.
    #[must_use]
    pub fn committed(&self) -> &FaultStoreSnapshot {
        &self.committed
    }

    /// Returns the globally ordered, operation-correlated trace for this case.
    #[must_use]
    pub fn trace(&self) -> &[FaultTraceEntry] {
        &self.trace
    }
}

/// Complete result of one seeded durability-battery run.
#[derive(Debug, Clone, PartialEq)]
pub struct DurabilityBatteryReport {
    seed: u64,
    cases: Vec<DurabilityCaseReport>,
}

impl DurabilityBatteryReport {
    /// Returns the deterministic fixture namespace used by this run.
    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Returns all case reports in the battery's fixed execution order.
    #[must_use]
    pub fn cases(&self) -> &[DurabilityCaseReport] {
        &self.cases
    }

    /// Finds the report for one surface/scenario pair.
    #[must_use]
    pub fn case(
        &self,
        surface: DurabilitySurface,
        scenario: DurabilityScenario,
    ) -> Option<&DurabilityCaseReport> {
        self.cases
            .iter()
            .find(|case| case.surface == surface && case.scenario == scenario)
    }
}

/// A failure while configuring or observing the durability battery.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DurabilityBatteryError {
    /// The underlying fault-store control surface failed.
    #[error(transparent)]
    Control(#[from] FaultStoreControlError),
    /// A deterministic fixture record violated a storage record bound.
    #[error(transparent)]
    Fixture(#[from] StorageError),
    /// A deterministic entity fixture index did not fit the typed entity ID.
    #[error("durability battery entity index {index} does not fit i32")]
    EntityIndexTooLarge {
        /// Index that could not be represented.
        index: usize,
    },
    /// An internal case definition did not execute a storage call.
    #[error("durability battery case {surface:?}/{scenario:?} produced no call outcome")]
    MissingOutcome {
        /// Surface whose case was malformed.
        surface: DurabilitySurface,
        /// Scenario whose case was malformed.
        scenario: DurabilityScenario,
    },
}

/// A fixed, reproducibly-seeded storage-durability fault battery.
///
/// `seed` namespaces fixture identities and bytes; it does not randomize case
/// selection or order. `mutation_template` supplies the project types needed by
/// journal cases without adding world/math dependencies to the testkit library.
/// The runner executes serially, uses a fresh store per case, and coordinates
/// held operations exclusively with [`crate::FaultGate`].
///
/// The report deliberately retains exact attempted payloads. Its allocation is
/// still fixed by the built-in 23-case matrix: no caller-supplied case list is
/// accepted, and the two oversized fixtures contain exactly
/// `MAX_SAVE_BATCH + 1` records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurabilityFaultBattery {
    seed: u64,
    mutation_template: BlockMutationLogRecord,
}

impl DurabilityFaultBattery {
    /// Builds the fixed battery for `seed` and `mutation_template`.
    #[must_use]
    pub const fn new(seed: u64, mutation_template: BlockMutationLogRecord) -> Self {
        Self {
            seed,
            mutation_template,
        }
    }

    /// Runs the complete fixed case matrix and returns its exact observations.
    ///
    /// # Errors
    ///
    /// Returns [`DurabilityBatteryError::Control`] if fault scheduling or state
    /// inspection fails, [`DurabilityBatteryError::Fixture`] if a generated
    /// bounded record is rejected, or a typed internal-definition error if a
    /// fixture index or case outcome cannot be represented. Expected injected
    /// storage failures are retained as [`DurabilityOutcome::Failed`] values in
    /// the report and do not fail the battery runner.
    pub async fn run(&self) -> Result<DurabilityBatteryReport, DurabilityBatteryError> {
        let mut cases = Vec::with_capacity(23);
        self.run_world_fault_cases(&mut cases).await?;
        self.run_world_boundary_cases(&mut cases).await?;
        self.run_player_cases(&mut cases).await?;
        self.run_journal_fault_cases(&mut cases).await?;
        self.run_journal_close_and_boundary_cases(&mut cases)
            .await?;
        Ok(DurabilityBatteryReport {
            seed: self.seed,
            cases,
        })
    }

    async fn run_world_fault_cases(
        &self,
        cases: &mut Vec<DurabilityCaseReport>,
    ) -> Result<(), DurabilityBatteryError> {
        let store = FaultInjectingStore::new();
        store.fail_next_before_commit()?;
        let (key, record) = self.entity_fixture(1)?;
        let result = store.save_entity(key, record).await;
        cases.push(Self::capture(
            DurabilitySurface::World,
            DurabilityScenario::FailBeforeCommit,
            &store,
            vec![DurabilityOutcome::from_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        store.return_next_commit_error()?;
        let (key, record) = self.entity_fixture(2)?;
        let result = store.save_entity(key, record).await;
        cases.push(Self::capture(
            DurabilitySurface::World,
            DurabilityScenario::CommitError,
            &store,
            vec![DurabilityOutcome::from_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        store.lose_next_ack_after_commit()?;
        let (key, record) = self.entity_fixture(3)?;
        let result = store.save_entity(key, record).await;
        cases.push(Self::capture(
            DurabilitySurface::World,
            DurabilityScenario::AckLoss,
            &store,
            vec![DurabilityOutcome::from_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        let gate = store.hold_next_before_commit()?;
        let (key, record) = self.entity_fixture(4)?;
        let operation = store.save_entity(key, record);
        let controller = async {
            gate.wait_until_reached().await;
            let close = store.close_requests();
            gate.release();
            close
        };
        let (accepted, close) = tokio::join!(operation, controller);
        close?;
        let (later_key, later_record) = self.entity_fixture(5)?;
        let later = store.save_entity(later_key, later_record).await;
        cases.push(Self::capture(
            DurabilitySurface::World,
            DurabilityScenario::RequestCloseWhileHeld,
            &store,
            vec![
                DurabilityOutcome::from_result(accepted),
                DurabilityOutcome::from_result(later),
            ],
        )?);

        let store = FaultInjectingStore::new();
        let gate = store.hold_next_response()?;
        let (key, record) = self.entity_fixture(6)?;
        let operation = store.save_entity(key, record);
        let controller = async {
            gate.wait_until_reached().await;
            let close = store.close_responses();
            gate.release();
            close
        };
        let (result, close) = tokio::join!(operation, controller);
        close?;
        cases.push(Self::capture(
            DurabilitySurface::World,
            DurabilityScenario::ResponseCloseWhileHeld,
            &store,
            vec![DurabilityOutcome::from_result(result)],
        )?);
        Ok(())
    }

    async fn run_world_boundary_cases(
        &self,
        cases: &mut Vec<DurabilityCaseReport>,
    ) -> Result<(), DurabilityBatteryError> {
        let store = FaultInjectingStore::new();
        let result = store.save_entities(Vec::new()).await;
        cases.push(Self::capture(
            DurabilitySurface::World,
            DurabilityScenario::EmptyBatch,
            &store,
            vec![DurabilityOutcome::from_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        let result = store
            .save_entities(self.entity_batch(MAX_SAVE_BATCH, 7)?)
            .await;
        cases.push(Self::capture(
            DurabilitySurface::World,
            DurabilityScenario::MaximumBatch,
            &store,
            vec![DurabilityOutcome::from_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        let result = store
            .save_entities(self.entity_batch(MAX_SAVE_BATCH + 1, 8)?)
            .await;
        cases.push(Self::capture(
            DurabilitySurface::World,
            DurabilityScenario::OversizedBatch,
            &store,
            vec![DurabilityOutcome::from_result(result)],
        )?);
        Ok(())
    }

    async fn run_player_cases(
        &self,
        cases: &mut Vec<DurabilityCaseReport>,
    ) -> Result<(), DurabilityBatteryError> {
        let player = self.player_id();

        let store = FaultInjectingStore::new();
        store.fail_next_before_commit()?;
        let result = store.save_player(player, self.player_record(1)?).await;
        cases.push(Self::capture(
            DurabilitySurface::Player,
            DurabilityScenario::FailBeforeCommit,
            &store,
            vec![DurabilityOutcome::from_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        store.return_next_commit_error()?;
        let result = store.save_player(player, self.player_record(2)?).await;
        cases.push(Self::capture(
            DurabilitySurface::Player,
            DurabilityScenario::CommitError,
            &store,
            vec![DurabilityOutcome::from_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        store.lose_next_ack_after_commit()?;
        let result = store.save_player(player, self.player_record(3)?).await;
        cases.push(Self::capture(
            DurabilitySurface::Player,
            DurabilityScenario::AckLoss,
            &store,
            vec![DurabilityOutcome::from_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        let gate = store.hold_next_before_commit()?;
        let operation = store.save_player(player, self.player_record(4)?);
        let controller = async {
            gate.wait_until_reached().await;
            let close = store.close_requests();
            gate.release();
            close
        };
        let (accepted, close) = tokio::join!(operation, controller);
        close?;
        let later = store.save_player(player, self.player_record(5)?).await;
        cases.push(Self::capture(
            DurabilitySurface::Player,
            DurabilityScenario::RequestCloseWhileHeld,
            &store,
            vec![
                DurabilityOutcome::from_result(accepted),
                DurabilityOutcome::from_result(later),
            ],
        )?);

        let store = FaultInjectingStore::new();
        let gate = store.hold_next_response()?;
        let operation = store.save_player(player, self.player_record(6)?);
        let controller = async {
            gate.wait_until_reached().await;
            let close = store.close_responses();
            gate.release();
            close
        };
        let (result, close) = tokio::join!(operation, controller);
        close?;
        cases.push(Self::capture(
            DurabilitySurface::Player,
            DurabilityScenario::ResponseCloseWhileHeld,
            &store,
            vec![DurabilityOutcome::from_result(result)],
        )?);
        Ok(())
    }

    async fn run_journal_fault_cases(
        &self,
        cases: &mut Vec<DurabilityCaseReport>,
    ) -> Result<(), DurabilityBatteryError> {
        let store = FaultInjectingStore::new();
        store.fail_next_before_commit()?;
        let result = store
            .append_block_mutation_batch(self.batch_id(1), vec![self.mutation_template])
            .await;
        cases.push(Self::capture(
            DurabilitySurface::Journal,
            DurabilityScenario::FailBeforeCommit,
            &store,
            vec![DurabilityOutcome::from_journal_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        store.return_next_commit_error()?;
        let result = store
            .append_block_mutation_batch(self.batch_id(2), vec![self.mutation_template])
            .await;
        cases.push(Self::capture(
            DurabilitySurface::Journal,
            DurabilityScenario::CommitError,
            &store,
            vec![DurabilityOutcome::from_journal_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        store.lose_next_ack_after_commit()?;
        let result = store
            .append_block_mutation_batch(self.batch_id(3), vec![self.mutation_template])
            .await;
        cases.push(Self::capture(
            DurabilitySurface::Journal,
            DurabilityScenario::AckLoss,
            &store,
            vec![DurabilityOutcome::from_journal_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        let batch_id = self.batch_id(4);
        store.lose_next_ack_after_commit()?;
        let first = store
            .append_block_mutation_batch(batch_id, vec![self.mutation_template])
            .await;
        let replay = store
            .append_block_mutation_batch(batch_id, vec![self.mutation_with_id(1)])
            .await;
        cases.push(Self::capture(
            DurabilitySurface::Journal,
            DurabilityScenario::ReceiptReplay,
            &store,
            vec![
                DurabilityOutcome::from_journal_result(first),
                DurabilityOutcome::from_journal_result(replay),
            ],
        )?);

        let store = FaultInjectingStore::new();
        let batch_id = self.batch_id(5);
        let first = store
            .append_block_mutation_batch(batch_id, vec![self.mutation_template])
            .await;
        let mismatch = store
            .append_block_mutation_batch(batch_id, vec![self.conflicting_mutation()])
            .await;
        cases.push(Self::capture(
            DurabilitySurface::Journal,
            DurabilityScenario::PayloadMismatch,
            &store,
            vec![
                DurabilityOutcome::from_journal_result(first),
                DurabilityOutcome::from_journal_result(mismatch),
            ],
        )?);
        Ok(())
    }

    async fn run_journal_close_and_boundary_cases(
        &self,
        cases: &mut Vec<DurabilityCaseReport>,
    ) -> Result<(), DurabilityBatteryError> {
        let store = FaultInjectingStore::new();
        let gate = store.hold_next_before_commit()?;
        let operation =
            store.append_block_mutation_batch(self.batch_id(6), vec![self.mutation_template]);
        let controller = async {
            gate.wait_until_reached().await;
            let close = store.close_requests();
            gate.release();
            close
        };
        let (accepted, close) = tokio::join!(operation, controller);
        close?;
        let later = store
            .append_block_mutation_batch(self.batch_id(7), vec![self.mutation_template])
            .await;
        cases.push(Self::capture(
            DurabilitySurface::Journal,
            DurabilityScenario::RequestCloseWhileHeld,
            &store,
            vec![
                DurabilityOutcome::from_journal_result(accepted),
                DurabilityOutcome::from_journal_result(later),
            ],
        )?);

        let store = FaultInjectingStore::new();
        let gate = store.hold_next_response()?;
        let operation =
            store.append_block_mutation_batch(self.batch_id(8), vec![self.mutation_template]);
        let controller = async {
            gate.wait_until_reached().await;
            let close = store.close_responses();
            gate.release();
            close
        };
        let (result, close) = tokio::join!(operation, controller);
        close?;
        cases.push(Self::capture(
            DurabilitySurface::Journal,
            DurabilityScenario::ResponseCloseWhileHeld,
            &store,
            vec![DurabilityOutcome::from_journal_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        let result = store
            .append_block_mutation_batch(self.batch_id(9), Vec::new())
            .await;
        cases.push(Self::capture(
            DurabilitySurface::Journal,
            DurabilityScenario::EmptyBatch,
            &store,
            vec![DurabilityOutcome::from_journal_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        let result = store
            .append_block_mutation_batch(
                self.batch_id(10),
                vec![self.mutation_template; MAX_SAVE_BATCH],
            )
            .await;
        cases.push(Self::capture(
            DurabilitySurface::Journal,
            DurabilityScenario::MaximumBatch,
            &store,
            vec![DurabilityOutcome::from_journal_result(result)],
        )?);

        let store = FaultInjectingStore::new();
        let result = store
            .append_block_mutation_batch(
                self.batch_id(11),
                vec![self.mutation_template; MAX_SAVE_BATCH + 1],
            )
            .await;
        cases.push(Self::capture(
            DurabilitySurface::Journal,
            DurabilityScenario::OversizedBatch,
            &store,
            vec![DurabilityOutcome::from_journal_result(result)],
        )?);
        Ok(())
    }

    fn capture(
        surface: DurabilitySurface,
        scenario: DurabilityScenario,
        store: &FaultInjectingStore,
        outcomes: Vec<DurabilityOutcome>,
    ) -> Result<DurabilityCaseReport, DurabilityBatteryError> {
        let outcome = outcomes
            .last()
            .cloned()
            .ok_or(DurabilityBatteryError::MissingOutcome { surface, scenario })?;
        Ok(DurabilityCaseReport {
            surface,
            scenario,
            outcome,
            outcomes,
            attempted: store.attempted_state()?,
            committed: store.snapshot()?,
            trace: store.trace()?,
        })
    }

    fn entity_fixture(
        &self,
        index: usize,
    ) -> Result<(EntityKey, EntityRecord), DurabilityBatteryError> {
        let entity = i32::try_from(index)
            .map_err(|_| DurabilityBatteryError::EntityIndexTooLarge { index })?;
        let seed = self.seed.to_be_bytes();
        let world = WorldId::new(u32::from_be_bytes([seed[0], seed[1], seed[2], seed[3]]));
        let dimension = DimensionId::new(u32::from_be_bytes([seed[4], seed[5], seed[6], seed[7]]));
        let mut payload = seed.to_vec();
        payload.extend_from_slice(&entity.to_be_bytes());
        let record = EntityRecord::new(SchemaVersion::new(1), payload)?;
        Ok((
            EntityKey::new(world, dimension, EntityId::new(entity)),
            record,
        ))
    }

    fn entity_batch(
        &self,
        len: usize,
        namespace: usize,
    ) -> Result<Vec<(EntityKey, EntityRecord)>, DurabilityBatteryError> {
        let offset = namespace.saturating_mul(MAX_SAVE_BATCH + 1);
        (0..len)
            .map(|index| self.entity_fixture(offset.saturating_add(index)))
            .collect()
    }

    fn player_id(&self) -> PlayerId {
        PlayerId::offline(&format!("durability-battery-{:016x}", self.seed))
    }

    fn player_record(&self, discriminator: u8) -> Result<PlayerRecord, StorageError> {
        let mut payload = self.seed.to_be_bytes().to_vec();
        payload.push(discriminator);
        PlayerRecord::new(SchemaVersion::new(1), GameMode::Survival, payload)
    }

    fn batch_id(&self, discriminator: u64) -> JournalBatchId {
        let mut bytes = [0_u8; 16];
        bytes[..8].copy_from_slice(&self.seed.to_be_bytes());
        bytes[8..].copy_from_slice(&discriminator.to_be_bytes());
        JournalBatchId::from_bytes(bytes)
    }

    fn mutation_with_id(&self, id: u64) -> BlockMutationLogRecord {
        BlockMutationLogRecord::new(
            self.mutation_template.schema_version(),
            id,
            self.mutation_template.tick(),
            self.mutation_template.actor(),
            self.mutation_template.pos(),
            self.mutation_template.old_state(),
            self.mutation_template.new_state(),
            self.mutation_template.cause(),
        )
    }

    fn conflicting_mutation(&self) -> BlockMutationLogRecord {
        BlockMutationLogRecord::new(
            self.mutation_template.schema_version(),
            self.mutation_template.id(),
            self.mutation_template.tick().wrapping_add(1),
            self.mutation_template.actor(),
            self.mutation_template.pos(),
            self.mutation_template.old_state(),
            self.mutation_template.new_state(),
            self.mutation_template.cause(),
        )
    }
}
