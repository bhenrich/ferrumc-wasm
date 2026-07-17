//! Deterministic scheduling internals for the non-authoritative multi-shard
//! shadow path.
//!
//! The scheduler is the simulation tick flow's "run active shards" seam. The
//! current shard still drains its admitted bounded inbox inside
//! [`SimShard::run_tick`], while storage completions, plugin tasks, cross-shard
//! delivery, dirty batching, and metrics do not yet have scheduler-owned
//! carriers. Those boundaries are deliberately not represented by fake no-op
//! hooks here: later packets add the real bounded values around this execution
//! seam.
//!
//! The default mode calls the existing single-shard primitive directly. The
//! non-default shadow mode builds a canonical worker-slot plan, transfers owned
//! shard batches to a fixed group of worker threads through capacity-one
//! channels, then restores ownership and canonical output order before the next
//! tick. Because a batch owns each shard value, plans reject duplicate ids, and
//! the scheduler waits for every batch to return, the same shard can never be
//! entered concurrently.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::{Arc, Barrier, Condvar, Mutex};

use ferrumc_core::Tick;

use crate::coordinator::TickCoordinator;
use crate::error::{SimError, SimResult};
use crate::message::{GameInput, GameOutput};
use crate::ownership::{ShardId, ShardLifecycle, ShardLifecycleState};
use crate::shard::SimShard;

/// The crate-internal rollout switch for scheduler execution.
///
/// The default stays on the existing inline single-shard path. Shadow workers
/// must be selected explicitly and are not wired into the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SchedulerMode {
    /// The authoritative, existing `SimShard::run_tick` path.
    #[default]
    AuthoritativeInline,
    /// The non-authoritative deterministic multi-shard worker pool.
    ShadowWorkers {
        /// Fixed number of persistent worker threads.
        worker_slots: NonZeroUsize,
    },
}

impl SchedulerMode {
    /// Builds the explicitly enabled worker mode.
    const fn shadow(worker_slots: NonZeroUsize) -> Self {
        Self::ShadowWorkers { worker_slots }
    }
}

/// A stable zero-based logical worker lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerSlot(usize);

impl WorkerSlot {
    /// Returns the zero-based lane index.
    const fn index(self) -> usize {
        self.0
    }
}

/// One canonical shard visit in a tick execution plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerVisit {
    shard: ShardId,
    /// `None` means the exact authoritative inline path; shadow visits always
    /// carry their stable logical worker assignment.
    slot: Option<WorkerSlot>,
}

/// A duplicate-free, canonically ordered plan for one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecutionPlan {
    visits: Vec<WorkerVisit>,
}

impl ExecutionPlan {
    /// Builds a plan independent of registration order.
    ///
    /// Duplicate ids are rejected before any tick or shard state advances.
    /// Canonical `ShardId` ordering remains the semantic execution and output
    /// order regardless of logical worker count.
    fn for_ids(
        mode: SchedulerMode,
        shard_ids: impl IntoIterator<Item = ShardId>,
    ) -> SimResult<Self> {
        let mut ordered = BTreeSet::new();
        for shard in shard_ids {
            if !ordered.insert(shard) {
                return Err(SimError::DuplicateShardDispatch { shard });
            }
        }

        if matches!(mode, SchedulerMode::AuthoritativeInline) && ordered.len() > 1 {
            return Err(SimError::MultipleScheduledShardsDisabled {
                scheduled: ordered.len(),
            });
        }

        let visits = ordered
            .into_iter()
            .enumerate()
            .map(|(ordinal, shard)| {
                let slot = match mode {
                    SchedulerMode::AuthoritativeInline => None,
                    SchedulerMode::ShadowWorkers { worker_slots } => {
                        Some(WorkerSlot(ordinal % worker_slots.get()))
                    }
                };
                WorkerVisit { shard, slot }
            })
            .collect();
        Ok(Self { visits })
    }

    /// Returns the canonical visits in semantic execution order.
    fn visits(&self) -> &[WorkerVisit] {
        &self.visits
    }
}

/// Deterministically forces one worker to publish its completion before another
/// in scheduler tests.
#[cfg(test)]
#[derive(Debug)]
struct TestCompletionOrder {
    first: ShardId,
    first_done: Mutex<bool>,
    changed: Condvar,
    observed: Mutex<Vec<ShardId>>,
}

#[cfg(test)]
impl TestCompletionOrder {
    /// Creates an empty completion trace whose first entry must be `first`.
    fn new(first: ShardId) -> Self {
        Self {
            first,
            first_done: Mutex::new(false),
            changed: Condvar::new(),
            observed: Mutex::new(Vec::new()),
        }
    }

    /// Records `shard`, blocking the non-first worker until the first finished.
    fn finish(&self, shard: ShardId) {
        if shard == self.first {
            self.observed
                .lock()
                .expect("completion trace lock")
                .push(shard);
            let mut first_done = self.first_done.lock().expect("completion gate lock");
            *first_done = true;
            self.changed.notify_all();
            return;
        }

        let mut first_done = self.first_done.lock().expect("completion gate lock");
        while !*first_done {
            first_done = self.changed.wait(first_done).expect("completion gate wait");
        }
        drop(first_done);
        self.observed
            .lock()
            .expect("completion trace lock")
            .push(shard);
    }

    /// Returns the observed worker completion order.
    fn observed(&self) -> Vec<ShardId> {
        self.observed.lock().expect("completion trace lock").clone()
    }
}

/// Test-only per-shard overlap and completion instrumentation.
#[cfg(test)]
#[derive(Debug)]
struct TestWorkerProbe {
    shard: ShardId,
    start: Arc<Barrier>,
    active: AtomicUsize,
    max_active: AtomicUsize,
    global_active: Arc<AtomicUsize>,
    global_max: Arc<AtomicUsize>,
    completion: Arc<TestCompletionOrder>,
    thread_ids: Mutex<Vec<thread::ThreadId>>,
}

#[cfg(test)]
impl TestWorkerProbe {
    /// Builds a probe sharing start/completion coordination with peer shards.
    fn new(
        shard: ShardId,
        start: Arc<Barrier>,
        global_active: Arc<AtomicUsize>,
        global_max: Arc<AtomicUsize>,
        completion: Arc<TestCompletionOrder>,
    ) -> Self {
        Self {
            shard,
            start,
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            global_active,
            global_max,
            completion,
            thread_ids: Mutex::new(Vec::new()),
        }
    }

    /// Marks this shard and the worker group in flight, then synchronizes starts.
    fn enter(self: &Arc<Self>) -> TestWorkerProbeGuard {
        self.thread_ids
            .lock()
            .expect("worker thread trace lock")
            .push(thread::current().id());
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        let global_active = self.global_active.fetch_add(1, Ordering::SeqCst) + 1;
        self.global_max.fetch_max(global_active, Ordering::SeqCst);
        self.start.wait();
        TestWorkerProbeGuard {
            probe: Arc::clone(self),
        }
    }

    /// Forces and records the worker's completion order.
    fn after_run(&self) {
        self.completion.finish(self.shard);
    }

    /// Returns the maximum simultaneous entries observed for this shard.
    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    /// Returns the current number of entries, which must be zero after joining.
    fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// Returns whether `expected_runs` all used the same persistent thread.
    fn ran_on_one_persistent_thread(&self, expected_runs: usize) -> bool {
        let ids = self.thread_ids.lock().expect("worker thread trace lock");
        ids.len() == expected_runs
            && ids
                .first()
                .is_some_and(|first| ids.iter().all(|id| id == first))
    }
}

/// Clears a test probe's in-flight counters even if shard execution panics.
#[cfg(test)]
struct TestWorkerProbeGuard {
    probe: Arc<TestWorkerProbe>,
}

#[cfg(test)]
impl Drop for TestWorkerProbeGuard {
    fn drop(&mut self) {
        self.probe.active.fetch_sub(1, Ordering::SeqCst);
        self.probe.global_active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One scheduler-owned shard and its eligibility lifecycle.
#[derive(Debug)]
struct ScheduledShard {
    lifecycle: ShardLifecycle,
    shard: SimShard,
    #[cfg(test)]
    probe: Option<Arc<TestWorkerProbe>>,
    #[cfg(test)]
    panic_on_run: bool,
}

impl ScheduledShard {
    /// Couples a newly registered shard to its canonical identity.
    const fn new(shard_id: ShardId, shard: SimShard) -> Self {
        Self {
            lifecycle: ShardLifecycle::new(shard_id),
            shard,
            #[cfg(test)]
            probe: None,
            #[cfg(test)]
            panic_on_run: false,
        }
    }

    /// Runs the shard once while activating the test-only overlap probe.
    fn run_tick(&mut self) -> Vec<GameOutput> {
        #[cfg(test)]
        assert!(!self.panic_on_run, "injected shard worker panic");

        #[cfg(test)]
        let probe = self.probe.clone();
        #[cfg(test)]
        let _guard = probe.as_ref().map(TestWorkerProbe::enter);

        let outputs = self.shard.run_tick();

        #[cfg(test)]
        if let Some(probe) = probe {
            probe.after_run();
        }
        outputs
    }
}

/// One owned shard assigned to a persistent worker for a single tick.
#[derive(Debug)]
struct ShardWork {
    visit: WorkerVisit,
    scheduled: ScheduledShard,
}

impl ShardWork {
    /// Executes the one visit carried by this work item.
    fn run(mut self) -> CompletedShardWork {
        let outputs = self.scheduled.run_tick();
        CompletedShardWork {
            visit: self.visit,
            scheduled: self.scheduled,
            outputs,
        }
    }
}

/// A worker's completed shard plus the ownership returned to the scheduler.
#[derive(Debug)]
struct CompletedShardWork {
    visit: WorkerVisit,
    scheduled: ScheduledShard,
    outputs: Vec<GameOutput>,
}

/// Outputs from one shard, kept grouped under its canonical identity.
#[derive(Debug, Clone, PartialEq)]
struct ShardTickOutput {
    shard: ShardId,
    outputs: Vec<GameOutput>,
}

/// The result of one scheduler tick.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SchedulerTickOutcome {
    tick: Tick,
    visits: Vec<WorkerVisit>,
    shards: Vec<ShardTickOutput>,
}

impl SchedulerTickOutcome {
    /// Returns the globally completed tick.
    pub(crate) const fn tick(&self) -> Tick {
        self.tick
    }

    /// Iterates logical shard ids in canonical dispatch/publication order.
    pub(crate) fn visit_order(&self) -> impl Iterator<Item = ShardId> + '_ {
        self.visits.iter().map(|visit| visit.shard)
    }

    /// Iterates each shard's outputs in canonical shard order.
    pub(crate) fn shard_outputs(&self) -> impl Iterator<Item = (ShardId, &[GameOutput])> + '_ {
        self.shards
            .iter()
            .map(|shard| (shard.shard, shard.outputs.as_slice()))
    }
}

/// Returns whether a lifecycle still needs tick execution.
///
/// Draining shards reject new admissions but keep ticking work that was already
/// accepted before the transition.
const fn is_runnable(state: ShardLifecycleState) -> bool {
    matches!(
        state,
        ShardLifecycleState::Active | ShardLifecycleState::Draining
    )
}

/// One bounded command accepted by a persistent shard worker.
#[derive(Debug)]
enum ShardWorkerCommand {
    /// Run the owned shard batch for `tick`.
    Run {
        /// Global tick shared by every worker in this dispatch.
        tick: Tick,
        /// Shards owned exclusively by this worker until it returns them.
        work: Vec<ShardWork>,
    },
    /// Exit after all earlier capacity-one commands have completed.
    Shutdown,
}

/// A completed worker batch returned through its bounded result channel.
#[derive(Debug)]
struct ShardWorkerResult {
    tick: Tick,
    work: Vec<CompletedShardWork>,
}

/// Capacity one is a rendezvous with one tick in flight per worker.
///
/// The scheduler waits for every result before it can dispatch again, so a
/// larger queue would only permit accidental tick overlap.
const WORKER_CHANNEL_CAPACITY: usize = 1;

/// Defensive ceiling for the non-default internal worker pool.
///
/// The shadow seam is a correctness path, not an app configuration surface.
/// Capping it at 64 prevents a future caller from turning an unchecked integer
/// into an unbounded OS-thread allocation.
const MAX_SHARD_WORKERS: usize = 64;

/// One persistent worker thread and its capacity-one command/result endpoints.
#[derive(Debug)]
struct ShardWorkerHandle {
    slot: WorkerSlot,
    command_tx: SyncSender<ShardWorkerCommand>,
    result_rx: Receiver<ShardWorkerResult>,
    join: Option<JoinHandle<()>>,
}

/// Persistent worker pool used only by the non-default shadow scheduler.
#[derive(Debug)]
struct ShardWorkerPool {
    workers: Vec<ShardWorkerHandle>,
}

impl ShardWorkerPool {
    /// Spawns the fixed worker set before any shard can be registered.
    fn new(worker_slots: NonZeroUsize) -> SimResult<Self> {
        if worker_slots.get() > MAX_SHARD_WORKERS {
            return Err(SimError::TooManyShardWorkers {
                requested: worker_slots.get(),
                maximum: MAX_SHARD_WORKERS,
            });
        }
        let mut pool = Self {
            workers: Vec::with_capacity(worker_slots.get()),
        };
        for index in 0..worker_slots.get() {
            let slot = WorkerSlot(index);
            let (command_tx, command_rx) = mpsc::sync_channel(WORKER_CHANNEL_CAPACITY);
            let (result_tx, result_rx) = mpsc::sync_channel(WORKER_CHANNEL_CAPACITY);
            let name = format!("ferrumc-sim-worker-{index}");
            let join = match thread::Builder::new()
                .name(name)
                .spawn(move || shard_worker_loop(&command_rx, &result_tx))
            {
                Ok(join) => join,
                Err(source) => {
                    pool.shutdown();
                    return Err(SimError::ShardWorkerSpawnFailed {
                        slot: index,
                        kind: source.kind(),
                    });
                }
            };
            pool.workers.push(ShardWorkerHandle {
                slot,
                command_tx,
                result_rx,
                join: Some(join),
            });
        }
        Ok(pool)
    }

    /// Removes every planned shard from the scheduler and groups ownership by
    /// worker slot.
    fn take_work(
        &self,
        shards: &mut BTreeMap<ShardId, ScheduledShard>,
        plan: &ExecutionPlan,
    ) -> SimResult<Vec<Vec<ShardWork>>> {
        let planned_ids: Vec<_> = plan.visits().iter().map(|visit| visit.shard).collect();
        let runnable_ids: Vec<_> = shards
            .iter()
            .filter(|(_, scheduled)| is_runnable(scheduled.lifecycle.state()))
            .map(|(shard, _)| *shard)
            .collect();
        if planned_ids != runnable_ids {
            let shard = planned_ids
                .iter()
                .zip(&runnable_ids)
                .find_map(|(planned, actual)| (planned != actual).then_some(*planned))
                .or_else(|| planned_ids.last().copied())
                .or_else(|| runnable_ids.last().copied());
            if let Some(shard) = shard {
                return Err(SimError::InvalidSchedulerPlan { shard });
            }
        }

        let mut buckets: Vec<Vec<ShardWork>> =
            (0..self.workers.len()).map(|_| Vec::new()).collect();
        for visit in plan.visits().iter().copied() {
            let Some(slot) = visit.slot else {
                Self::restore_work(shards, buckets);
                return Err(SimError::InvalidSchedulerPlan { shard: visit.shard });
            };
            let Some(scheduled) = shards.remove(&visit.shard) else {
                Self::restore_work(shards, buckets);
                return Err(SimError::InvalidSchedulerPlan { shard: visit.shard });
            };
            let Some(bucket) = buckets.get_mut(slot.index()) else {
                shards.insert(visit.shard, scheduled);
                Self::restore_work(shards, buckets);
                return Err(SimError::InvalidSchedulerPlan { shard: visit.shard });
            };
            bucket.push(ShardWork { visit, scheduled });
        }
        Ok(buckets)
    }

    /// Restores an unexecuted batch to scheduler ownership.
    fn restore_work(shards: &mut BTreeMap<ShardId, ScheduledShard>, buckets: Vec<Vec<ShardWork>>) {
        for work in buckets.into_iter().flatten() {
            shards.insert(work.visit.shard, work.scheduled);
        }
    }

    /// Restores completed shards and appends their grouped outputs.
    fn restore_completed(
        shards: &mut BTreeMap<ShardId, ScheduledShard>,
        completed: Vec<CompletedShardWork>,
        outputs: &mut Vec<ShardTickOutput>,
    ) {
        for work in completed {
            outputs.push(ShardTickOutput {
                shard: work.visit.shard,
                outputs: work.outputs,
            });
            shards.insert(work.visit.shard, work.scheduled);
        }
    }

    /// Classifies and joins a worker whose channel disconnected.
    fn disconnected_error(&mut self, index: usize, tick: Tick) -> SimError {
        let worker = &mut self.workers[index];
        let panicked = worker.join.take().is_some_and(|join| join.join().is_err());
        if panicked {
            SimError::ShardWorkerPanicked {
                slot: worker.slot.index(),
                tick,
            }
        } else {
            SimError::ShardWorkerStopped {
                slot: worker.slot.index(),
                tick,
            }
        }
    }

    /// Dispatches one owned batch per nonempty worker slot and restores every
    /// returned shard before completing.
    fn execute(
        &mut self,
        coordinator: &mut TickCoordinator,
        shards: &mut BTreeMap<ShardId, ScheduledShard>,
        plan: ExecutionPlan,
    ) -> SimResult<SchedulerTickOutcome> {
        if plan.visits().is_empty() {
            let tick = coordinator.advance()?;
            return Ok(SchedulerTickOutcome {
                tick,
                visits: Vec::new(),
                shards: Vec::new(),
            });
        }

        let buckets = self.take_work(shards, &plan)?;
        let tick = match coordinator.advance() {
            Ok(tick) => tick,
            Err(error) => {
                Self::restore_work(shards, buckets);
                return Err(error);
            }
        };
        let mut sent = Vec::new();
        let mut failure = None;

        for (index, work) in buckets.into_iter().enumerate() {
            if work.is_empty() {
                continue;
            }
            if failure.is_some() {
                Self::restore_work(shards, vec![work]);
                continue;
            }
            let command = ShardWorkerCommand::Run { tick, work };
            if let Err(mpsc::SendError(command)) = self.workers[index].command_tx.send(command) {
                if let ShardWorkerCommand::Run { work, .. } = command {
                    Self::restore_work(shards, vec![work]);
                }
                failure = Some(self.disconnected_error(index, tick));
            } else {
                sent.push(index);
            }
        }

        let mut outputs = Vec::with_capacity(plan.visits().len());
        for index in sent {
            if let Ok(result) = self.workers[index].result_rx.recv() {
                if result.tick != tick && failure.is_none() {
                    failure = Some(SimError::ShardWorkerWrongTick {
                        slot: self.workers[index].slot.index(),
                        expected: tick,
                        actual: result.tick,
                    });
                }
                Self::restore_completed(shards, result.work, &mut outputs);
            } else {
                let error = self.disconnected_error(index, tick);
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }

        if let Some(error) = failure {
            return Err(error);
        }
        outputs.sort_unstable_by_key(|output| output.shard);
        Ok(SchedulerTickOutcome {
            tick,
            visits: plan.visits,
            shards: outputs,
        })
    }

    /// Requests clean shutdown and joins every worker that is still alive.
    fn shutdown(&mut self) {
        for worker in &self.workers {
            if worker.join.is_some() {
                // A disconnected receiver already terminated; Drop has no
                // caller to report that expected fail-stop state to.
                let _closed = worker.command_tx.send(ShardWorkerCommand::Shutdown);
            }
        }
        for worker in &mut self.workers {
            if let Some(join) = worker.join.take() {
                // Any panic was surfaced when the live scheduler observed its
                // disconnected result channel. During Drop there is no safe
                // recovery action beyond joining the thread.
                let _panicked = join.join().is_err();
            }
        }
    }
}

impl Drop for ShardWorkerPool {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Runs one persistent worker until its bounded command endpoint closes.
fn shard_worker_loop(
    command_rx: &Receiver<ShardWorkerCommand>,
    result_tx: &SyncSender<ShardWorkerResult>,
) {
    loop {
        let Ok(command) = command_rx.recv() else {
            return;
        };
        match command {
            ShardWorkerCommand::Run { tick, work } => {
                let work = work.into_iter().map(ShardWork::run).collect();
                if result_tx.send(ShardWorkerResult { tick, work }).is_err() {
                    return;
                }
            }
            ShardWorkerCommand::Shutdown => return,
        }
    }
}

/// Owns registered shards and drives them in a deterministic order.
///
/// This type is intentionally crate-private and has no application wiring. Its
/// immutable mode makes the authoritative-to-shadow transition impossible to
/// toggle accidentally at runtime.
#[derive(Debug)]
pub(crate) struct ShardScheduler {
    coordinator: TickCoordinator,
    mode: SchedulerMode,
    worker_pool: Option<ShardWorkerPool>,
    shards: BTreeMap<ShardId, ScheduledShard>,
    /// A worker failure after tick advance is fail-stop: retry could double-run
    /// peers whose state was already returned.
    poisoned_at: Option<Tick>,
}

impl ShardScheduler {
    /// Creates the default scheduler around exactly one active authoritative
    /// shard.
    pub(crate) fn authoritative(coordinator: TickCoordinator, shard: SimShard) -> SimResult<Self> {
        let mut scheduler = Self {
            coordinator,
            mode: SchedulerMode::default(),
            worker_pool: None,
            shards: BTreeMap::new(),
            poisoned_at: None,
        };
        let shard_id = scheduler.register(shard)?;
        scheduler.activate(shard_id)?;
        Ok(scheduler)
    }

    /// Creates an empty non-authoritative shadow scheduler.
    pub(crate) fn with_shadow_workers(
        coordinator: TickCoordinator,
        worker_slots: NonZeroUsize,
    ) -> SimResult<Self> {
        Ok(Self {
            coordinator,
            mode: SchedulerMode::shadow(worker_slots),
            worker_pool: Some(ShardWorkerPool::new(worker_slots)?),
            shards: BTreeMap::new(),
            poisoned_at: None,
        })
    }

    /// Rejects every mutating operation after a partially executed worker tick.
    fn ensure_healthy(&self) -> SimResult<()> {
        match self.poisoned_at {
            Some(tick) => Err(SimError::ShardSchedulerPoisoned { tick }),
            None => Ok(()),
        }
    }

    /// Registers a shard in the created state, deriving its id from the shard's
    /// own typed world, dimension, and position.
    pub(crate) fn register(&mut self, shard: SimShard) -> SimResult<ShardId> {
        self.ensure_healthy()?;
        let shard_id = ShardId::try_new(
            shard.loaded_chunks().world(),
            shard.loaded_chunks().dimension(),
            shard.shard_pos(),
        )?;
        if self.shards.contains_key(&shard_id) {
            return Err(SimError::OverlappingShardOwnership {
                region: shard_id.region(),
                shard: shard_id,
            });
        }
        self.shards
            .insert(shard_id, ScheduledShard::new(shard_id, shard));
        Ok(shard_id)
    }

    /// Makes a created shard eligible for tick planning.
    pub(crate) fn activate(&mut self, shard: ShardId) -> SimResult<()> {
        self.transition(shard, ShardLifecycleState::Active)
    }

    /// Applies one validated lifecycle transition.
    pub(crate) fn transition(
        &mut self,
        shard: ShardId,
        next: ShardLifecycleState,
    ) -> SimResult<()> {
        self.ensure_healthy()?;
        let current = self
            .shards
            .get(&shard)
            .ok_or(SimError::UnknownScheduledShard { shard })?
            .lifecycle
            .state();
        if current == ShardLifecycleState::Created
            && next == ShardLifecycleState::Active
            && matches!(self.mode, SchedulerMode::AuthoritativeInline)
        {
            let scheduled_count = self
                .shards
                .values()
                .filter(|scheduled| is_runnable(scheduled.lifecycle.state()))
                .count();
            if scheduled_count != 0 {
                return Err(SimError::MultipleScheduledShardsDisabled {
                    scheduled: scheduled_count + 1,
                });
            }
        }
        if current == ShardLifecycleState::Draining
            && next == ShardLifecycleState::Stopped
            && !self
                .shards
                .get(&shard)
                .ok_or(SimError::UnknownScheduledShard { shard })?
                .shard
                .is_tick_quiescent()
        {
            return Err(SimError::ShardDrainIncomplete { shard });
        }

        self.shards
            .get_mut(&shard)
            .ok_or(SimError::UnknownScheduledShard { shard })?
            .lifecycle
            .transition_to(next)
    }

    /// Enqueues a value into an active shard's existing bounded inbox.
    ///
    /// Created shards are not eligible yet; draining and stopped shards reject
    /// new work so admitted inputs cannot become permanently stranded.
    pub(crate) fn enqueue(&mut self, shard: ShardId, input: GameInput) -> SimResult<()> {
        self.ensure_healthy()?;
        let scheduled = self
            .shards
            .get_mut(&shard)
            .ok_or(SimError::UnknownScheduledShard { shard })?;
        let state = scheduled.lifecycle.state();
        if state != ShardLifecycleState::Active {
            return Err(SimError::ShardInputNotAccepted { shard, state });
        }
        scheduled.shard.enqueue(input)
    }

    /// Advances once and runs only active shards according to the validated
    /// plan.
    ///
    /// The plan is built before the coordinator advances, so a disabled
    /// multi-shard configuration or duplicate dispatch cannot drift the tick
    /// counter or drain any inbox. Tick overflow likewise occurs before the
    /// first shard is touched.
    pub(crate) fn tick(&mut self) -> SimResult<SchedulerTickOutcome> {
        self.ensure_healthy()?;
        let plan = ExecutionPlan::for_ids(
            self.mode,
            self.shards
                .iter()
                .filter(|(_, scheduled)| is_runnable(scheduled.lifecycle.state()))
                .map(|(shard, _)| *shard),
        )?;
        match self.mode {
            SchedulerMode::AuthoritativeInline => {
                Self::execute_inline(&mut self.coordinator, &mut self.shards, plan)
            }
            SchedulerMode::ShadowWorkers { .. } => {
                let result = self
                    .worker_pool
                    .as_mut()
                    .ok_or(SimError::ShardWorkerPoolUnavailable)?
                    .execute(&mut self.coordinator, &mut self.shards, plan);
                if matches!(
                    &result,
                    Err(SimError::ShardWorkerPanicked { .. }
                        | SimError::ShardWorkerStopped { .. }
                        | SimError::ShardWorkerWrongTick { .. })
                ) {
                    self.poisoned_at = Some(self.coordinator.current());
                }
                result
            }
        }
    }

    /// Executes the exact legacy single-shard path without a worker dispatch.
    fn execute_inline(
        coordinator: &mut TickCoordinator,
        shards: &mut BTreeMap<ShardId, ScheduledShard>,
        plan: ExecutionPlan,
    ) -> SimResult<SchedulerTickOutcome> {
        let mut runnable = shards
            .iter_mut()
            .filter(|(_, scheduled)| is_runnable(scheduled.lifecycle.state()));
        let scheduled = match plan.visits().first().copied() {
            Some(visit) => {
                if visit.slot.is_some() {
                    return Err(SimError::InvalidSchedulerPlan { shard: visit.shard });
                }
                let Some((actual_id, scheduled)) = runnable.next() else {
                    return Err(SimError::InvalidSchedulerPlan { shard: visit.shard });
                };
                if *actual_id != visit.shard {
                    return Err(SimError::InvalidSchedulerPlan { shard: visit.shard });
                }
                Some((visit.shard, scheduled))
            }
            None => None,
        };
        if let Some((shard, _)) = runnable.next() {
            return Err(SimError::InvalidSchedulerPlan { shard: *shard });
        }

        let tick = coordinator.advance()?;
        let mut outputs = Vec::with_capacity(usize::from(scheduled.is_some()));
        if let Some((shard, scheduled)) = scheduled {
            outputs.push(ShardTickOutput {
                shard,
                outputs: scheduled.run_tick(),
            });
        }
        Ok(SchedulerTickOutcome {
            tick,
            visits: plan.visits,
            shards: outputs,
        })
    }

    /// Returns the current global tick without advancing it.
    pub(crate) const fn current_tick(&self) -> Tick {
        self.coordinator.current()
    }

    /// Returns a registered shard for inspection.
    pub(crate) fn shard(&self, shard: ShardId) -> Option<&SimShard> {
        self.shards.get(&shard).map(|scheduled| &scheduled.shard)
    }

    /// Returns a registered shard's lifecycle state.
    pub(crate) fn lifecycle(&self, shard: ShardId) -> Option<ShardLifecycleState> {
        self.shards
            .get(&shard)
            .map(|scheduled| scheduled.lifecycle.state())
    }

    /// Installs deterministic overlap/completion instrumentation for a test.
    #[cfg(test)]
    fn set_probe(&mut self, shard: ShardId, probe: Arc<TestWorkerProbe>) -> SimResult<()> {
        self.ensure_healthy()?;
        self.shards
            .get_mut(&shard)
            .ok_or(SimError::UnknownScheduledShard { shard })?
            .probe = Some(probe);
        Ok(())
    }

    /// Makes one test worker panic after taking ownership of its shard.
    #[cfg(test)]
    fn set_panic_on_run(&mut self, shard: ShardId) -> SimResult<()> {
        self.ensure_healthy()?;
        self.shards
            .get_mut(&shard)
            .ok_or(SimError::UnknownScheduledShard { shard })?
            .panic_on_run = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    use ferrumc_core::{DimensionId, GameMode, PlayerId, Tick, WorldId};
    use ferrumc_math::{BlockPos, ShardPos, Vec3};

    use super::{
        ExecutionPlan, SchedulerMode, ShardScheduler, TestCompletionOrder, TestWorkerProbe,
        MAX_SHARD_WORKERS,
    };
    use crate::{
        GameInput, GameOutput, MutationCause, ShardId, ShardLifecycleState, SignFace, SimError,
        SimShard, TickCoordinator, TickRate,
    };

    fn logical_id(world: WorldId, dimension: DimensionId, position: ShardPos) -> ShardId {
        ShardId::try_new(world, dimension, position).expect("valid test shard position")
    }

    fn append_len(bytes: &mut Vec<u8>, len: usize) {
        let len = u64::try_from(len).expect("test value fits u64");
        bytes.extend_from_slice(&len.to_be_bytes());
    }

    fn append_player(bytes: &mut Vec<u8>, player: PlayerId) {
        bytes.extend_from_slice(player.as_uuid().as_bytes());
    }

    fn append_position(bytes: &mut Vec<u8>, position: Vec3) {
        bytes.extend_from_slice(&position.x.to_bits().to_be_bytes());
        bytes.extend_from_slice(&position.y.to_bits().to_be_bytes());
        bytes.extend_from_slice(&position.z.to_bits().to_be_bytes());
    }

    fn append_block_position(bytes: &mut Vec<u8>, position: BlockPos) {
        bytes.extend_from_slice(&position.x().to_be_bytes());
        bytes.extend_from_slice(&position.y().to_be_bytes());
        bytes.extend_from_slice(&position.z().to_be_bytes());
    }

    fn append_string(bytes: &mut Vec<u8>, value: &str) {
        append_len(bytes, value.len());
        bytes.extend_from_slice(value.as_bytes());
    }

    fn append_sign_face(bytes: &mut Vec<u8>, face: &SignFace) {
        for line in face.lines() {
            append_string(bytes, line);
        }
        append_string(bytes, face.color());
        bytes.push(u8::from(face.has_glowing_text()));
    }

    fn append_mutation_cause(bytes: &mut Vec<u8>, cause: MutationCause) {
        match cause {
            MutationCause::PlayerCreative { player } => {
                bytes.push(0);
                append_player(bytes, player);
            }
            MutationCause::Command => bytes.push(1),
            MutationCause::Plugin => bytes.push(2),
            MutationCause::Test => bytes.push(3),
        }
    }

    /// Encodes every observable output field explicitly, including float bit
    /// patterns, so parity does not depend on `Debug` formatting or Rust memory
    /// layout.
    fn canonical_output_bytes(outputs: &[GameOutput]) -> Vec<u8> {
        let mut bytes = Vec::new();
        append_len(&mut bytes, outputs.len());
        for output in outputs {
            match output {
                GameOutput::PlayerSpawned { player, position } => {
                    bytes.push(0);
                    append_player(&mut bytes, *player);
                    append_position(&mut bytes, *position);
                }
                GameOutput::PlayerMoved {
                    player,
                    position,
                    yaw,
                    pitch,
                    position_changed,
                } => {
                    bytes.push(1);
                    append_player(&mut bytes, *player);
                    append_position(&mut bytes, *position);
                    bytes.extend_from_slice(&yaw.to_bits().to_be_bytes());
                    bytes.extend_from_slice(&pitch.to_bits().to_be_bytes());
                    bytes.push(u8::from(*position_changed));
                }
                GameOutput::PlayerPositionCorrected { player, position } => {
                    bytes.push(2);
                    append_player(&mut bytes, *player);
                    append_position(&mut bytes, *position);
                }
                GameOutput::PlayerDespawned { player } => {
                    bytes.push(3);
                    append_player(&mut bytes, *player);
                }
                GameOutput::BlockChanged {
                    position,
                    state,
                    sequence,
                    cause,
                } => {
                    bytes.push(4);
                    append_block_position(&mut bytes, *position);
                    bytes.extend_from_slice(&state.as_u32().to_be_bytes());
                    bytes.extend_from_slice(&sequence.to_be_bytes());
                    append_mutation_cause(&mut bytes, *cause);
                }
                GameOutput::BlockChangeRejected {
                    player,
                    position,
                    sequence,
                    requested_state,
                    authoritative_state,
                } => {
                    bytes.push(5);
                    append_player(&mut bytes, *player);
                    append_block_position(&mut bytes, *position);
                    bytes.extend_from_slice(&sequence.to_be_bytes());
                    bytes.extend_from_slice(&requested_state.as_u32().to_be_bytes());
                    bytes.extend_from_slice(&authoritative_state.as_u32().to_be_bytes());
                }
                GameOutput::SignUpdated { position, sign } => {
                    bytes.push(6);
                    append_block_position(&mut bytes, *position);
                    bytes.extend_from_slice(&sign.kind().block_entity_type().to_be_bytes());
                    bytes.push(u8::from(sign.is_waxed()));
                    append_sign_face(&mut bytes, sign.front());
                    append_sign_face(&mut bytes, sign.back());
                }
                GameOutput::OpenSignEditor { player, position } => {
                    bytes.push(7);
                    append_player(&mut bytes, *player);
                    append_block_position(&mut bytes, *position);
                }
            }
        }
        bytes
    }

    fn assert_same_observable_shard_state(left: &SimShard, right: &SimShard, player: PlayerId) {
        assert_eq!(format!("{left:#?}"), format!("{right:#?}"));
        assert_eq!(left.shard_pos(), right.shard_pos());
        assert_eq!(left.inbox_len(), right.inbox_len());
        assert_eq!(left.player_count(), right.player_count());
        assert_eq!(left.player_position(player), right.player_position(player));
        assert_eq!(
            left.player_game_mode(player),
            right.player_game_mode(player)
        );
        assert_eq!(
            left.loaded_chunks().loaded_count(),
            right.loaded_chunks().loaded_count()
        );
        assert_eq!(left.has_pending_mutations(), right.has_pending_mutations());
    }

    #[test]
    fn switch_off_tick_output_is_byte_identical_to_legacy_path() {
        let player = PlayerId::offline("scheduler-legacy");
        let mut shard = SimShard::new(ShardPos::new(0, 0));
        shard
            .enqueue(GameInput::PlayerJoin {
                player,
                position: Vec3::new(4.0, 70.0, 8.0),
            })
            .expect("test inbox has capacity");
        shard
            .enqueue(GameInput::PlayerMove {
                player,
                position: Some(Vec3::new(5.0, 70.5, 9.0)),
                yaw: Some(90.0),
                pitch: Some(-12.0),
            })
            .expect("test inbox has capacity");

        let shard_id = logical_id(WorldId::new(0), DimensionId::new(0), shard.shard_pos());
        let mut legacy_shard = shard.clone();
        let mut legacy_coordinator = TickCoordinator::new(TickRate::VANILLA);
        let legacy_tick = legacy_coordinator.advance().expect("tick advances");
        let legacy_outputs = legacy_shard.run_tick();

        assert_eq!(SchedulerMode::default(), SchedulerMode::AuthoritativeInline);
        let mut scheduler =
            ShardScheduler::authoritative(TickCoordinator::new(TickRate::VANILLA), shard)
                .expect("valid authoritative shard");
        let scheduler_tick = scheduler.tick().expect("scheduled tick");

        assert_eq!(scheduler_tick.tick, legacy_tick);
        assert_eq!(scheduler_tick.visits.len(), 1);
        assert_eq!(scheduler_tick.visits[0].shard, shard_id);
        assert_eq!(scheduler_tick.visits[0].slot, None);
        assert_eq!(scheduler_tick.shards.len(), 1);
        assert_eq!(scheduler_tick.shards[0].shard, shard_id);
        assert_eq!(scheduler_tick.shards[0].outputs, legacy_outputs);
        assert_eq!(
            canonical_output_bytes(&scheduler_tick.shards[0].outputs),
            canonical_output_bytes(&legacy_outputs)
        );
        assert_same_observable_shard_state(
            scheduler.shard(shard_id).expect("registered shard"),
            &legacy_shard,
            player,
        );
    }

    #[test]
    fn switch_on_one_shard_is_identical_to_switch_off() {
        let player = PlayerId::offline("scheduler-shadow");
        let shard = SimShard::new(ShardPos::new(-2, 3));
        let shard_id = logical_id(WorldId::new(0), DimensionId::new(0), shard.shard_pos());
        let mut inline =
            ShardScheduler::authoritative(TickCoordinator::new(TickRate::VANILLA), shard.clone())
                .expect("valid authoritative shard");
        let mut shadow = ShardScheduler::with_shadow_workers(
            TickCoordinator::new(TickRate::VANILLA),
            NonZeroUsize::new(3).expect("nonzero worker slots"),
        )
        .expect("worker pool");
        assert_eq!(shadow.register(shard).expect("register"), shard_id);
        shadow.activate(shard_id).expect("activate");

        let input_schedule = [
            vec![GameInput::PlayerJoin {
                player,
                position: Vec3::new(-17.0, 64.0, 25.0),
            }],
            vec![
                GameInput::PlayerMove {
                    player,
                    position: Some(Vec3::new(-16.5, 65.0, 26.0)),
                    yaw: Some(135.0),
                    pitch: Some(5.0),
                },
                GameInput::SetGameMode {
                    player,
                    mode: GameMode::Creative,
                },
            ],
            vec![GameInput::PlayerLeave { player }],
        ];

        for inputs in input_schedule {
            for input in inputs {
                inline
                    .enqueue(shard_id, input.clone())
                    .expect("inline inbox");
                shadow.enqueue(shard_id, input).expect("shadow inbox");
            }

            let inline_tick = inline.tick().expect("inline tick");
            let shadow_tick = shadow.tick().expect("shadow tick");
            assert_eq!(shadow_tick.tick, inline_tick.tick);
            assert_eq!(inline_tick.visits[0].slot, None);
            assert_eq!(
                shadow_tick.visits[0].slot.map(super::WorkerSlot::index),
                Some(0)
            );
            assert_eq!(
                canonical_output_bytes(&shadow_tick.shards[0].outputs),
                canonical_output_bytes(&inline_tick.shards[0].outputs)
            );
            assert_eq!(shadow_tick.shards[0].outputs, inline_tick.shards[0].outputs);
            assert_same_observable_shard_state(
                shadow.shard(shard_id).expect("shadow shard"),
                inline.shard(shard_id).expect("inline shard"),
                player,
            );
        }
    }

    fn scheduler_with_mixed_lifecycles(
        worker_slots: NonZeroUsize,
    ) -> (ShardScheduler, Vec<ShardId>) {
        let entries = [
            (
                WorldId::new(1),
                DimensionId::new(0),
                ShardPos::new(-4, 7),
                ShardLifecycleState::Active,
            ),
            (
                WorldId::new(0),
                DimensionId::new(1),
                ShardPos::new(8, -3),
                ShardLifecycleState::Created,
            ),
            (
                WorldId::new(0),
                DimensionId::new(0),
                ShardPos::new(9, 2),
                ShardLifecycleState::Active,
            ),
            (
                WorldId::new(0),
                DimensionId::new(0),
                ShardPos::new(-9, 5),
                ShardLifecycleState::Stopped,
            ),
            (
                WorldId::new(0),
                DimensionId::new(0),
                ShardPos::new(0, 0),
                ShardLifecycleState::Active,
            ),
            (
                WorldId::new(0),
                DimensionId::new(2),
                ShardPos::new(1, 1),
                ShardLifecycleState::Draining,
            ),
        ];
        let mut scheduler = ShardScheduler::with_shadow_workers(
            TickCoordinator::new(TickRate::VANILLA),
            worker_slots,
        )
        .expect("worker pool");
        let mut expected_active = Vec::new();

        for (world, dimension, position, target) in entries {
            let shard_id = scheduler
                .register(SimShard::in_dimension(position, world, dimension))
                .expect("unique valid shard");
            match target {
                ShardLifecycleState::Created => {}
                ShardLifecycleState::Active => {
                    scheduler.activate(shard_id).expect("activate");
                    expected_active.push(shard_id);
                }
                ShardLifecycleState::Draining => {
                    scheduler.activate(shard_id).expect("activate");
                    scheduler
                        .transition(shard_id, ShardLifecycleState::Draining)
                        .expect("drain");
                    expected_active.push(shard_id);
                }
                ShardLifecycleState::Stopped => {
                    scheduler.activate(shard_id).expect("activate");
                    scheduler
                        .transition(shard_id, ShardLifecycleState::Draining)
                        .expect("drain");
                    scheduler
                        .transition(shard_id, ShardLifecycleState::Stopped)
                        .expect("stop");
                }
            }
        }
        expected_active.sort_unstable();
        (scheduler, expected_active)
    }

    #[test]
    fn active_shards_visit_in_canonical_id_order() {
        for worker_slots in [1, 2, 3] {
            let worker_slots = NonZeroUsize::new(worker_slots).expect("nonzero");
            let (mut scheduler, expected) = scheduler_with_mixed_lifecycles(worker_slots);
            let outcome = scheduler.tick().expect("shadow tick");

            let visited: Vec<_> = outcome.visits.iter().map(|visit| visit.shard).collect();
            let output_order: Vec<_> = outcome.shards.iter().map(|output| output.shard).collect();
            let assigned_slots: Vec<_> = outcome
                .visits
                .iter()
                .map(|visit| visit.slot.expect("shadow dispatch").index())
                .collect();
            let expected_slots: Vec<_> = (0..expected.len())
                .map(|ordinal| ordinal % worker_slots.get())
                .collect();

            assert_eq!(visited, expected);
            assert_eq!(output_order, expected);
            assert_eq!(assigned_slots, expected_slots);
        }
    }

    #[test]
    fn same_shard_is_never_concurrently_scheduled() {
        let worker_slots = NonZeroUsize::new(2).expect("nonzero");
        let first = logical_id(WorldId::new(0), DimensionId::new(0), ShardPos::new(0, 0));
        let second = logical_id(WorldId::new(0), DimensionId::new(0), ShardPos::new(1, 0));

        let duplicate = ExecutionPlan::for_ids(SchedulerMode::shadow(worker_slots), [first, first])
            .expect_err("a shard may appear only once in a tick plan");
        assert_eq!(duplicate, SimError::DuplicateShardDispatch { shard: first });

        let plan = ExecutionPlan::for_ids(SchedulerMode::shadow(worker_slots), [second, first])
            .expect("unique shards");
        let planned: Vec<_> = plan.visits().iter().map(|visit| visit.shard).collect();
        assert_eq!(planned, vec![first, second]);

        let mut scheduler = ShardScheduler::with_shadow_workers(
            TickCoordinator::new(TickRate::VANILLA),
            worker_slots,
        )
        .expect("worker pool");
        let first_player = PlayerId::offline("worker-first");
        let second_player = PlayerId::offline("worker-second");
        for (position, player) in [
            (ShardPos::new(0, 0), first_player),
            (ShardPos::new(1, 0), second_player),
        ] {
            let id = scheduler
                .register(SimShard::new(position))
                .expect("register");
            scheduler.activate(id).expect("activate");
            scheduler
                .enqueue(
                    id,
                    GameInput::PlayerJoin {
                        player,
                        position: Vec3::new(f64::from(position.x()), 64.0, 0.0),
                    },
                )
                .expect("active shard inbox");
        }

        let start = Arc::new(Barrier::new(2));
        let global_active = Arc::new(AtomicUsize::new(0));
        let global_max = Arc::new(AtomicUsize::new(0));
        // Force the higher id to complete first so collation cannot
        // accidentally inherit worker completion order.
        let completion = Arc::new(TestCompletionOrder::new(second));
        let first_probe = Arc::new(TestWorkerProbe::new(
            first,
            Arc::clone(&start),
            Arc::clone(&global_active),
            Arc::clone(&global_max),
            Arc::clone(&completion),
        ));
        let second_probe = Arc::new(TestWorkerProbe::new(
            second,
            start,
            Arc::clone(&global_active),
            Arc::clone(&global_max),
            Arc::clone(&completion),
        ));
        scheduler
            .set_probe(first, Arc::clone(&first_probe))
            .expect("first probe");
        scheduler
            .set_probe(second, Arc::clone(&second_probe))
            .expect("second probe");

        let outcome = scheduler.tick().expect("parallel shadow tick");
        let visited: Vec<_> = outcome.visits.iter().map(|visit| visit.shard).collect();
        let output_order: Vec<_> = outcome.shards.iter().map(|output| output.shard).collect();
        let unique: BTreeSet<_> = visited.iter().copied().collect();

        assert_eq!(outcome.tick, Tick::new(1));
        assert_eq!(completion.observed(), vec![second, first]);
        assert_eq!(visited, vec![first, second]);
        assert_eq!(output_order, vec![first, second]);
        assert_eq!(unique.len(), visited.len());
        assert_eq!(global_max.load(Ordering::SeqCst), 2);
        assert_eq!(global_active.load(Ordering::SeqCst), 0);
        assert_eq!(first_probe.max_active(), 1);
        assert_eq!(second_probe.max_active(), 1);
        assert_eq!(first_probe.active(), 0);
        assert_eq!(second_probe.active(), 0);
        assert!(matches!(
            outcome.shards[0].outputs.as_slice(),
            [GameOutput::PlayerSpawned { player, .. }] if *player == first_player
        ));
        assert!(matches!(
            outcome.shards[1].outputs.as_slice(),
            [GameOutput::PlayerSpawned { player, .. }] if *player == second_player
        ));

        // A second dispatch reuses the same fixed threads; the start barrier is
        // reusable and again proves the two distinct shards overlap.
        let second_tick = scheduler.tick().expect("second parallel shadow tick");
        assert_eq!(second_tick.tick, Tick::new(2));
        assert!(first_probe.ran_on_one_persistent_thread(2));
        assert!(second_probe.ran_on_one_persistent_thread(2));
        assert_eq!(first_probe.max_active(), 1);
        assert_eq!(second_probe.max_active(), 1);
    }

    #[test]
    fn authoritative_mode_rejects_a_second_active_shard_before_tick() {
        let first_shard = SimShard::new(ShardPos::new(0, 0));
        let first = logical_id(
            WorldId::new(0),
            DimensionId::new(0),
            first_shard.shard_pos(),
        );
        let mut scheduler =
            ShardScheduler::authoritative(TickCoordinator::new(TickRate::VANILLA), first_shard)
                .expect("authoritative shard");
        let mut second_shard = SimShard::new(ShardPos::new(1, 0));
        second_shard
            .enqueue(GameInput::PlayerJoin {
                player: PlayerId::offline("not-admitted"),
                position: Vec3::ZERO,
            })
            .expect("staged shard inbox");
        let second = scheduler
            .register(second_shard)
            .expect("created shard may be staged");

        assert_eq!(
            scheduler.activate(second),
            Err(SimError::MultipleScheduledShardsDisabled { scheduled: 2 })
        );
        assert_eq!(scheduler.current_tick(), Tick::ZERO);
        assert_eq!(scheduler.shard(first).expect("first").player_count(), 0);
        assert_eq!(scheduler.shard(second).expect("second").inbox_len(), 1);
        assert_eq!(
            scheduler.lifecycle(second).expect("second lifecycle"),
            ShardLifecycleState::Created
        );
    }

    #[test]
    fn draining_shard_finishes_admitted_work_and_rejects_new_input() {
        let player = PlayerId::offline("draining");
        let mut scheduler = ShardScheduler::with_shadow_workers(
            TickCoordinator::new(TickRate::VANILLA),
            NonZeroUsize::MIN,
        )
        .expect("worker pool");
        let shard = scheduler
            .register(SimShard::new(ShardPos::new(0, 0)))
            .expect("register");
        scheduler.activate(shard).expect("activate");
        scheduler
            .enqueue(
                shard,
                GameInput::PlayerJoin {
                    player,
                    position: Vec3::ZERO,
                },
            )
            .expect("admit before drain");
        scheduler
            .transition(shard, ShardLifecycleState::Draining)
            .expect("begin draining");

        assert_eq!(
            scheduler.transition(shard, ShardLifecycleState::Stopped),
            Err(SimError::ShardDrainIncomplete { shard })
        );
        assert_eq!(
            scheduler.enqueue(shard, GameInput::PlayerLeave { player }),
            Err(SimError::ShardInputNotAccepted {
                shard,
                state: ShardLifecycleState::Draining,
            })
        );
        let outcome = scheduler.tick().expect("draining tick");
        assert_eq!(outcome.visits[0].shard, shard);
        assert_eq!(
            outcome.shards[0].outputs,
            vec![GameOutput::PlayerSpawned {
                player,
                position: Vec3::ZERO,
            }]
        );
        assert_eq!(scheduler.shard(shard).expect("shard").inbox_len(), 0);
        scheduler
            .transition(shard, ShardLifecycleState::Stopped)
            .expect("quiescent shard stops");
        assert_eq!(
            scheduler.lifecycle(shard),
            Some(ShardLifecycleState::Stopped)
        );
    }

    #[test]
    fn worker_panic_poison_is_fail_stop_and_cannot_be_retried() {
        let player = PlayerId::offline("worker-panic");
        let mut scheduler = ShardScheduler::with_shadow_workers(
            TickCoordinator::new(TickRate::VANILLA),
            NonZeroUsize::MIN,
        )
        .expect("worker pool");
        let shard = scheduler
            .register(SimShard::new(ShardPos::new(0, 0)))
            .expect("register");
        scheduler.activate(shard).expect("activate");
        scheduler
            .enqueue(
                shard,
                GameInput::PlayerJoin {
                    player,
                    position: Vec3::ZERO,
                },
            )
            .expect("inbox");
        scheduler.set_panic_on_run(shard).expect("panic injection");

        assert_eq!(
            scheduler.tick(),
            Err(SimError::ShardWorkerPanicked {
                slot: 0,
                tick: Tick::new(1),
            })
        );
        assert_eq!(scheduler.current_tick(), Tick::new(1));
        assert_eq!(
            scheduler.tick(),
            Err(SimError::ShardSchedulerPoisoned { tick: Tick::new(1) })
        );
        assert_eq!(
            scheduler.enqueue(shard, GameInput::PlayerLeave { player }),
            Err(SimError::ShardSchedulerPoisoned { tick: Tick::new(1) })
        );
    }

    #[test]
    fn worker_pool_rejects_an_unbounded_thread_count() {
        let requested = MAX_SHARD_WORKERS + 1;
        let result = ShardScheduler::with_shadow_workers(
            TickCoordinator::new(TickRate::VANILLA),
            NonZeroUsize::new(requested).expect("nonzero"),
        );
        assert!(matches!(
            result,
            Err(SimError::TooManyShardWorkers {
                requested: actual,
                maximum: MAX_SHARD_WORKERS,
            }) if actual == requested
        ));
    }

    #[test]
    fn tick_overflow_leaves_all_shard_inboxes_untouched() {
        let mut scheduler = ShardScheduler::with_shadow_workers(
            TickCoordinator::resuming_at(TickRate::VANILLA, Tick::new(u64::MAX)),
            NonZeroUsize::new(2).expect("nonzero"),
        )
        .expect("worker pool");
        let mut ids = Vec::new();
        for x in [0, 1] {
            let id = scheduler
                .register(SimShard::new(ShardPos::new(x, 0)))
                .expect("register");
            scheduler.activate(id).expect("activate");
            scheduler
                .enqueue(
                    id,
                    GameInput::PlayerJoin {
                        player: PlayerId::offline(&format!("overflow-{x}")),
                        position: Vec3::ZERO,
                    },
                )
                .expect("inbox");
            ids.push(id);
        }

        assert!(matches!(scheduler.tick(), Err(SimError::TickOverflow)));
        assert_eq!(scheduler.current_tick(), Tick::new(u64::MAX));
        for id in ids {
            let shard = scheduler.shard(id).expect("registered");
            assert_eq!(shard.inbox_len(), 1);
            assert_eq!(shard.player_count(), 0);
        }
    }
}
