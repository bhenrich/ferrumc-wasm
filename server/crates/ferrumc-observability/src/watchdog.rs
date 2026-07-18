//! Dedicated-thread detection for slow and non-returning plugin callbacks.
//!
//! The watchdog observes elapsed time only. It never interrupts a callback,
//! terminates its thread, unloads a library, or makes an in-process native
//! failure recoverable. A hard-threshold result tells the callback owner to
//! retire that thread if the call eventually returns. The watchdog also rejects
//! every later admission from that thread; physically ending it remains the
//! responsibility of the callback-thread owner.

use std::fmt;
use std::io::Write;
use std::marker::PhantomData;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use ferrumc_core::PluginId;
use serde::{Deserialize, Deserializer, Serialize};

use crate::RingBuffer;

/// Maximum number of callbacks monitored concurrently.
///
/// Admission beyond this cap fails explicitly, so a caller can refuse to run
/// unmonitored plugin code instead of silently losing watchdog coverage.
pub const ACTIVE_CALLBACK_CAPACITY: usize = 1_024;

/// Number of threshold diagnostics retained, newest replacing oldest.
pub const DIAGNOSTIC_HISTORY_CAPACITY: usize = 256;

/// Number of callback-thread identities retained after hard violations.
///
/// If this ledger fills, admission fails closed for every thread rather than
/// forgetting an identity and allowing a suspected callback thread to run
/// plugin code again.
pub const RETIRED_THREAD_CAPACITY: usize = 1_024;

const CRASH_REPORT_HISTORY_CAPACITY: usize = 64;
const MAX_PLUGIN_ID_BYTES: usize = 128;
const MAX_HOOK_BYTES: usize = 128;
const MAX_SHARD_LABEL_BYTES: usize = 64;
const MAX_THREAD_LABEL_BYTES: usize = 160;
const DEFAULT_SOFT_CALLBACK_MS: u64 = 50;
const DEFAULT_UNHEALTHY_CALLBACK_MS: u64 = 1_000;
const DEFAULT_HARD_CALLBACK_MS: u64 = 10_000;
const WATCHDOG_THREAD_NAME: &str = "ferrumc-plugin-watchdog";

/// Action requested after a callback crosses the hard watchdog threshold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WatchdogHardAction {
    /// Record and flush the crash report while leaving the process running.
    #[default]
    ReportOnly,
    /// Abort the process after recording and flushing the crash report.
    AbortProcess,
}

/// Invalid plugin-watchdog threshold configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatchdogConfigError {
    /// One threshold was configured as zero milliseconds.
    ZeroThreshold {
        /// Exact configuration key carrying zero.
        key: &'static str,
    },
    /// The three thresholds were not strictly increasing.
    ThresholdOrder {
        /// Configured soft threshold in milliseconds.
        soft_callback_ms: u64,
        /// Configured unhealthy threshold in milliseconds.
        unhealthy_callback_ms: u64,
        /// Configured hard threshold in milliseconds.
        hard_callback_ms: u64,
    },
}

impl fmt::Display for WatchdogConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroThreshold { key } => {
                write!(
                    formatter,
                    "plugins.watchdog.{key} must be greater than zero"
                )
            }
            Self::ThresholdOrder {
                soft_callback_ms,
                unhealthy_callback_ms,
                hard_callback_ms,
            } => write!(
                formatter,
                "plugins.watchdog thresholds must satisfy soft_callback_ms < \
                 unhealthy_callback_ms < hard_callback_ms (got {soft_callback_ms}, \
                 {unhealthy_callback_ms}, {hard_callback_ms})"
            ),
        }
    }
}

impl std::error::Error for WatchdogConfigError {}

/// Validated watchdog policy using the approved `plugins.watchdog` keys.
///
/// Deserialization accepts exactly `soft_callback_ms`,
/// `unhealthy_callback_ms`, `hard_callback_ms`, and `hard_action`; omitted
/// values use the documented defaults and unknown keys are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PluginWatchdogConfig {
    soft_callback_ms: u64,
    unhealthy_callback_ms: u64,
    hard_callback_ms: u64,
    hard_action: WatchdogHardAction,
}

impl PluginWatchdogConfig {
    /// Builds a validated watchdog policy from millisecond thresholds.
    ///
    /// # Errors
    ///
    /// Returns a typed error when any threshold is zero or the thresholds are
    /// not strictly increasing.
    pub const fn try_new(
        soft_callback_ms: u64,
        unhealthy_callback_ms: u64,
        hard_callback_ms: u64,
        hard_action: WatchdogHardAction,
    ) -> Result<Self, WatchdogConfigError> {
        if soft_callback_ms == 0 {
            return Err(WatchdogConfigError::ZeroThreshold {
                key: "soft_callback_ms",
            });
        }
        if unhealthy_callback_ms == 0 {
            return Err(WatchdogConfigError::ZeroThreshold {
                key: "unhealthy_callback_ms",
            });
        }
        if hard_callback_ms == 0 {
            return Err(WatchdogConfigError::ZeroThreshold {
                key: "hard_callback_ms",
            });
        }
        if !(soft_callback_ms < unhealthy_callback_ms && unhealthy_callback_ms < hard_callback_ms) {
            return Err(WatchdogConfigError::ThresholdOrder {
                soft_callback_ms,
                unhealthy_callback_ms,
                hard_callback_ms,
            });
        }
        Ok(Self {
            soft_callback_ms,
            unhealthy_callback_ms,
            hard_callback_ms,
            hard_action,
        })
    }

    /// Returns the soft warning threshold.
    pub const fn soft_callback(self) -> Duration {
        Duration::from_millis(self.soft_callback_ms)
    }

    /// Returns the threshold that fails readiness and health.
    pub const fn unhealthy_callback(self) -> Duration {
        Duration::from_millis(self.unhealthy_callback_ms)
    }

    /// Returns the hard crash-report threshold.
    pub const fn hard_callback(self) -> Duration {
        Duration::from_millis(self.hard_callback_ms)
    }

    /// Returns the action performed after a hard crash report is flushed.
    pub const fn hard_action(self) -> WatchdogHardAction {
        self.hard_action
    }
}

impl Default for PluginWatchdogConfig {
    fn default() -> Self {
        Self {
            soft_callback_ms: DEFAULT_SOFT_CALLBACK_MS,
            unhealthy_callback_ms: DEFAULT_UNHEALTHY_CALLBACK_MS,
            hard_callback_ms: DEFAULT_HARD_CALLBACK_MS,
            hard_action: WatchdogHardAction::ReportOnly,
        }
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PluginWatchdogConfigWire {
    soft_callback_ms: u64,
    unhealthy_callback_ms: u64,
    hard_callback_ms: u64,
    hard_action: WatchdogHardAction,
}

impl Default for PluginWatchdogConfigWire {
    fn default() -> Self {
        let defaults = PluginWatchdogConfig::default();
        Self {
            soft_callback_ms: defaults.soft_callback_ms,
            unhealthy_callback_ms: defaults.unhealthy_callback_ms,
            hard_callback_ms: defaults.hard_callback_ms,
            hard_action: defaults.hard_action,
        }
    }
}

impl<'de> Deserialize<'de> for PluginWatchdogConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PluginWatchdogConfigWire::deserialize(deserializer)?;
        Self::try_new(
            wire.soft_callback_ms,
            wire.unhealthy_callback_ms,
            wire.hard_callback_ms,
            wire.hard_action,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Invalid bounded label supplied for a watchdog callback.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatchdogLabelError {
    /// A required label was empty.
    Empty {
        /// Name of the empty field.
        field: &'static str,
    },
    /// A label exceeded its byte ceiling.
    TooLong {
        /// Name of the oversized field.
        field: &'static str,
        /// Supplied UTF-8 byte length.
        actual: usize,
        /// Maximum accepted UTF-8 byte length.
        maximum: usize,
    },
}

impl fmt::Display for WatchdogLabelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { field } => write!(formatter, "watchdog callback {field} is empty"),
            Self::TooLong {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "watchdog callback {field} is {actual} bytes; maximum is {maximum}"
            ),
        }
    }
}

impl std::error::Error for WatchdogLabelError {}

/// Bounded identity of one plugin callback invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogCallback {
    plugin_id: PluginId,
    hook: String,
    shard: String,
}

impl WatchdogCallback {
    /// Builds callback identity from a plugin ID, hook label, and shard label.
    ///
    /// The shard is a display label only; the caller should format its typed
    /// shard coordinate before crossing into this low-coupling telemetry crate.
    ///
    /// # Errors
    ///
    /// Returns a typed error for an empty or oversized label.
    pub fn new(
        plugin_id: PluginId,
        hook: impl Into<String>,
        shard: impl Into<String>,
    ) -> Result<Self, WatchdogLabelError> {
        validate_label("plugin_id", plugin_id.as_str(), MAX_PLUGIN_ID_BYTES)?;
        let hook = hook.into();
        validate_label("hook", &hook, MAX_HOOK_BYTES)?;
        let shard = shard.into();
        validate_label("shard", &shard, MAX_SHARD_LABEL_BYTES)?;
        Ok(Self {
            plugin_id,
            hook,
            shard,
        })
    }

    /// Returns the exact plugin identifier.
    pub const fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    /// Returns the bounded hook label.
    pub fn hook(&self) -> &str {
        &self.hook
    }

    /// Returns the bounded shard display label.
    pub fn shard(&self) -> &str {
        &self.shard
    }
}

fn validate_label(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), WatchdogLabelError> {
    if value.is_empty() {
        return Err(WatchdogLabelError::Empty { field });
    }
    if value.len() > maximum {
        return Err(WatchdogLabelError::TooLong {
            field,
            actual: value.len(),
            maximum,
        });
    }
    Ok(())
}

/// Process-local identifier for one monitored callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WatchdogCallId(u64);

impl WatchdogCallId {
    /// Returns the process-local numeric identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Threshold crossed by an active callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WatchdogThreshold {
    /// Callback exceeded the warning threshold.
    Soft,
    /// Callback exceeded the failed-health threshold.
    Unhealthy,
    /// Callback exceeded the hard crash-report threshold.
    Hard,
}

/// One attributed threshold transition retained by the watchdog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogDiagnostic {
    call_id: WatchdogCallId,
    callback: WatchdogCallback,
    callback_thread: String,
    threshold: WatchdogThreshold,
    elapsed: Duration,
}

impl WatchdogDiagnostic {
    /// Returns the monitored call identifier.
    pub const fn call_id(&self) -> WatchdogCallId {
        self.call_id
    }

    /// Returns the exact plugin identifier.
    pub const fn plugin_id(&self) -> &PluginId {
        self.callback.plugin_id()
    }

    /// Returns the callback hook label.
    pub fn hook(&self) -> &str {
        self.callback.hook()
    }

    /// Returns the callback shard label.
    pub fn shard(&self) -> &str {
        self.callback.shard()
    }

    /// Returns the captured callback-thread label.
    pub fn callback_thread(&self) -> &str {
        &self.callback_thread
    }

    /// Returns the threshold represented by this transition.
    pub const fn threshold(&self) -> WatchdogThreshold {
        self.threshold
    }

    /// Returns elapsed callback time at transition observation.
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

/// Best available hard-threshold crash report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogCrashReport {
    diagnostic: WatchdogDiagnostic,
    active_callbacks: usize,
}

impl WatchdogCrashReport {
    /// Returns the attributed hard-threshold diagnostic.
    pub const fn diagnostic(&self) -> &WatchdogDiagnostic {
        &self.diagnostic
    }

    /// Returns how many callbacks were active when this report was generated.
    pub const fn active_callbacks(&self) -> usize {
        self.active_callbacks
    }
}

/// Sink invoked synchronously by the dedicated watchdog thread at hard failure.
///
/// Implementations should keep both methods bounded and non-blocking. The
/// watchdog calls [`record_crash_report`](Self::record_crash_report), then
/// [`flush_logs`](Self::flush_logs), and only then performs the configured hard
/// action.
pub trait WatchdogReporter: Send + Sync + 'static {
    /// Records the best available hard-threshold crash report.
    fn record_crash_report(&self, report: &WatchdogCrashReport);

    /// Flushes the logging backend after the crash report is recorded.
    fn flush_logs(&self);
}

/// Reporter that emits structured tracing and flushes standard output/error.
#[derive(Debug, Default)]
pub struct TracingWatchdogReporter;

impl WatchdogReporter for TracingWatchdogReporter {
    fn record_crash_report(&self, report: &WatchdogCrashReport) {
        let diagnostic = report.diagnostic();
        tracing::error!(
            target: "ferrumc::observability::plugin_watchdog",
            plugin = %diagnostic.plugin_id(),
            hook = diagnostic.hook(),
            shard = diagnostic.shard(),
            callback_thread = diagnostic.callback_thread(),
            elapsed_ms = duration_millis(diagnostic.elapsed()),
            active_callbacks = report.active_callbacks(),
            "plugin callback crossed hard watchdog threshold"
        );
    }

    fn flush_logs(&self) {
        if let Err(error) = std::io::stdout().lock().flush() {
            tracing::error!(%error, "failed to flush stdout after watchdog report");
        }
        if let Err(error) = std::io::stderr().lock().flush() {
            tracing::error!(%error, "failed to flush stderr after watchdog report");
        }
    }
}

/// Failure to spawn the dedicated watchdog thread.
#[derive(Debug)]
pub struct WatchdogStartError {
    source: std::io::Error,
}

impl WatchdogStartError {
    /// Returns the underlying thread-spawn error.
    pub const fn source_error(&self) -> &std::io::Error {
        &self.source
    }
}

impl fmt::Display for WatchdogStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to spawn plugin watchdog thread: {}",
            self.source
        )
    }
}

impl std::error::Error for WatchdogStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Failure to admit a callback into the bounded active ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatchdogBeginError {
    /// The watchdog owner has stopped its worker.
    Stopped,
    /// The calling thread previously crossed a hard watchdog threshold.
    RetiredThread,
    /// The retired-thread ledger filled, so admission is globally fail-closed.
    RetirementLedgerFull {
        /// Maximum number of retired callback-thread identities retained.
        maximum: usize,
    },
    /// The bounded active-call ledger is full.
    LedgerFull {
        /// Maximum number of concurrently monitored callbacks.
        maximum: usize,
    },
    /// No fresh nonzero call identifier remains.
    CallIdExhausted,
}

impl fmt::Display for WatchdogBeginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped => formatter.write_str("plugin watchdog is stopped"),
            Self::RetiredThread => {
                formatter.write_str("plugin watchdog callback thread is retired")
            }
            Self::RetirementLedgerFull { maximum } => write!(
                formatter,
                "plugin watchdog retired-thread ledger is full (capacity {maximum})"
            ),
            Self::LedgerFull { maximum } => write!(
                formatter,
                "plugin watchdog active ledger is full (capacity {maximum})"
            ),
            Self::CallIdExhausted => {
                formatter.write_str("plugin watchdog call identifiers are exhausted")
            }
        }
    }
}

impl std::error::Error for WatchdogBeginError {}

/// Health/readiness projection of the active watchdog ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogHealth {
    /// No active callback has crossed the unhealthy threshold.
    Healthy,
    /// At least one active callback crossed the unhealthy threshold.
    Failed,
}

/// Current state of one active callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogActiveCall {
    call_id: WatchdogCallId,
    callback: WatchdogCallback,
    callback_thread: String,
    elapsed: Duration,
    threshold: Option<WatchdogThreshold>,
}

impl WatchdogActiveCall {
    /// Returns the active call identifier.
    pub const fn call_id(&self) -> WatchdogCallId {
        self.call_id
    }

    /// Returns the exact plugin identifier.
    pub const fn plugin_id(&self) -> &PluginId {
        self.callback.plugin_id()
    }

    /// Returns the callback hook label.
    pub fn hook(&self) -> &str {
        self.callback.hook()
    }

    /// Returns the callback shard label.
    pub fn shard(&self) -> &str {
        self.callback.shard()
    }

    /// Returns the captured callback-thread label.
    pub fn callback_thread(&self) -> &str {
        &self.callback_thread
    }

    /// Returns current elapsed callback time.
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// Returns the highest threshold observed for this active call.
    pub const fn threshold(&self) -> Option<WatchdogThreshold> {
        self.threshold
    }

    /// Returns whether the hard threshold has marked this callback hung.
    pub const fn is_hung(&self) -> bool {
        matches!(self.threshold, Some(WatchdogThreshold::Hard))
    }
}

/// Bounded point-in-time view of watchdog state.
#[derive(Debug, Clone)]
pub struct WatchdogSnapshot {
    health: WatchdogHealth,
    active: Vec<WatchdogActiveCall>,
    recent_diagnostics: Vec<WatchdogDiagnostic>,
    recent_crash_reports: Vec<WatchdogCrashReport>,
    soft_warning_count: u64,
    hard_report_count: u64,
}

impl WatchdogSnapshot {
    /// Returns the current health/readiness projection.
    pub const fn health(&self) -> WatchdogHealth {
        self.health
    }

    /// Returns active callbacks in call-ID order.
    pub fn active(&self) -> &[WatchdogActiveCall] {
        &self.active
    }

    /// Returns retained threshold transitions, oldest to newest.
    pub fn recent_diagnostics(&self) -> &[WatchdogDiagnostic] {
        &self.recent_diagnostics
    }

    /// Returns retained hard crash reports, oldest to newest.
    pub fn recent_crash_reports(&self) -> &[WatchdogCrashReport] {
        &self.recent_crash_reports
    }

    /// Returns the monotonic number of soft-threshold warnings.
    pub const fn soft_warning_count(&self) -> u64 {
        self.soft_warning_count
    }

    /// Returns the monotonic number of hard-threshold reports.
    pub const fn hard_report_count(&self) -> u64 {
        self.hard_report_count
    }

    /// Returns distinct plugins with at least one currently hard-stuck callback.
    ///
    /// The result is bounded by [`ACTIVE_CALLBACK_CAPACITY`] and preserves the
    /// first active call-ID order for each plugin.
    pub fn hung_plugins(&self) -> Vec<PluginId> {
        let mut plugins = Vec::new();
        for call in self.active.iter().filter(|call| call.is_hung()) {
            if !plugins.iter().any(|plugin| plugin == call.plugin_id()) {
                plugins.push(call.plugin_id().clone());
            }
        }
        plugins
    }
}

/// Whether the owner may reuse the thread after a monitored callback returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum WatchdogThreadDisposition {
    /// The originating thread has no recorded hard watchdog violation.
    Reusable,
    /// The originating thread is fail-closed; the owner must retire it.
    Retire,
}

trait WatchdogClock: Send + Sync + 'static {
    fn now(&self) -> Duration;

    fn uses_wall_clock_waits(&self) -> bool;
}

struct SystemWatchdogClock {
    origin: Instant,
}

impl SystemWatchdogClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl WatchdogClock for SystemWatchdogClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn uses_wall_clock_waits(&self) -> bool {
        true
    }
}

struct ActiveCall {
    call_id: WatchdogCallId,
    callback: WatchdogCallback,
    callback_thread_id: ThreadId,
    callback_thread: String,
    started_at: Duration,
    completed_at: Option<Duration>,
    soft_reported: bool,
    unhealthy_reported: bool,
    hard_reported: bool,
    processed_threshold: Option<WatchdogThreshold>,
}

impl ActiveCall {
    fn highest_threshold(&self) -> Option<WatchdogThreshold> {
        if self.hard_reported {
            Some(WatchdogThreshold::Hard)
        } else if self.unhealthy_reported {
            Some(WatchdogThreshold::Unhealthy)
        } else if self.soft_reported {
            Some(WatchdogThreshold::Soft)
        } else {
            None
        }
    }

    fn mark_processed(&mut self, threshold: WatchdogThreshold) {
        self.processed_threshold = Some(
            self.processed_threshold
                .map_or(threshold, |current| current.max(threshold)),
        );
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum TestWorkerGate {
    Open,
    PauseRequested,
    PauseObserved,
}

struct WatchdogState {
    accepting: bool,
    next_call_id: u64,
    active: Vec<ActiveCall>,
    diagnostics: RingBuffer<WatchdogDiagnostic, DIAGNOSTIC_HISTORY_CAPACITY>,
    crash_reports: RingBuffer<WatchdogCrashReport, CRASH_REPORT_HISTORY_CAPACITY>,
    retired_threads: Vec<ThreadId>,
    retirement_saturated: bool,
    soft_warning_count: u64,
    hard_report_count: u64,
    change_generation: u64,
    processed_change_generation: u64,
    #[cfg(test)]
    worker_gate: TestWorkerGate,
}

impl WatchdogState {
    fn new() -> Self {
        Self {
            accepting: true,
            next_call_id: 1,
            active: Vec::with_capacity(ACTIVE_CALLBACK_CAPACITY),
            diagnostics: RingBuffer::new(),
            crash_reports: RingBuffer::new(),
            retired_threads: Vec::with_capacity(RETIRED_THREAD_CAPACITY),
            retirement_saturated: false,
            soft_warning_count: 0,
            hard_report_count: 0,
            change_generation: 0,
            processed_change_generation: 0,
            #[cfg(test)]
            worker_gate: TestWorkerGate::Open,
        }
    }
}

struct WatchdogShared {
    config: PluginWatchdogConfig,
    clock: Arc<dyn WatchdogClock>,
    reporter: Arc<dyn WatchdogReporter>,
    state: Mutex<WatchdogState>,
    wake: Condvar,
}

/// Cloneable handle used by callback-owning threads.
#[derive(Clone)]
pub struct PluginWatchdogHandle {
    shared: Arc<WatchdogShared>,
}

impl fmt::Debug for PluginWatchdogHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginWatchdogHandle")
            .finish_non_exhaustive()
    }
}

impl PluginWatchdogHandle {
    /// Admits one callback into the bounded ledger and returns its guard.
    ///
    /// The caller should refuse to invoke the plugin when this returns an
    /// error, because the callback would otherwise run without watchdog
    /// coverage.
    pub fn begin_callback(
        &self,
        callback: WatchdogCallback,
    ) -> Result<WatchdogCallGuard, WatchdogBeginError> {
        let (callback_thread_id, callback_thread) = callback_thread_identity();
        let mut state = lock_state(&self.shared.state);
        if !state.accepting {
            return Err(WatchdogBeginError::Stopped);
        }
        if state.retirement_saturated {
            return Err(WatchdogBeginError::RetirementLedgerFull {
                maximum: RETIRED_THREAD_CAPACITY,
            });
        }
        if state.retired_threads.contains(&callback_thread_id) {
            return Err(WatchdogBeginError::RetiredThread);
        }
        if state.active.len() >= ACTIVE_CALLBACK_CAPACITY {
            return Err(WatchdogBeginError::LedgerFull {
                maximum: ACTIVE_CALLBACK_CAPACITY,
            });
        }
        let raw = state.next_call_id;
        let Some(next) = raw.checked_add(1) else {
            return Err(WatchdogBeginError::CallIdExhausted);
        };
        if raw == 0 {
            return Err(WatchdogBeginError::CallIdExhausted);
        }
        state.next_call_id = next;
        let call_id = WatchdogCallId(raw);
        state.active.push(ActiveCall {
            call_id,
            callback,
            callback_thread_id,
            callback_thread,
            started_at: self.shared.clock.now(),
            completed_at: None,
            soft_reported: false,
            unhealthy_reported: false,
            hard_reported: false,
            processed_threshold: None,
        });
        mark_changed(&mut state);
        self.shared.wake.notify_all();
        drop(state);
        Ok(WatchdogCallGuard {
            handle: self.clone(),
            call_id,
            finished: false,
            _not_send: PhantomData,
        })
    }

    /// Returns a bounded point-in-time snapshot.
    pub fn snapshot(&self) -> WatchdogSnapshot {
        let now = self.shared.clock.now();
        let state = lock_state(&self.shared.state);
        let active = state
            .active
            .iter()
            .map(|call| WatchdogActiveCall {
                call_id: call.call_id,
                callback: call.callback.clone(),
                callback_thread: call.callback_thread.clone(),
                elapsed: call
                    .completed_at
                    .unwrap_or(now)
                    .saturating_sub(call.started_at),
                threshold: call.highest_threshold(),
            })
            .collect::<Vec<_>>();
        let health = if active.iter().any(|call| {
            matches!(
                call.threshold(),
                Some(WatchdogThreshold::Unhealthy | WatchdogThreshold::Hard)
            )
        }) {
            WatchdogHealth::Failed
        } else {
            WatchdogHealth::Healthy
        };
        WatchdogSnapshot {
            health,
            active,
            recent_diagnostics: state.diagnostics.to_vec(),
            recent_crash_reports: state.crash_reports.to_vec(),
            soft_warning_count: state.soft_warning_count,
            hard_report_count: state.hard_report_count,
        }
    }

    fn complete(&self, call_id: WatchdogCallId) -> WatchdogThreadDisposition {
        let mut state = lock_state(&self.shared.state);
        let Some(initial_index) = active_call_index(&state, call_id) else {
            return WatchdogThreadDisposition::Reusable;
        };

        let completed_at = self.shared.clock.now();
        state.active[initial_index].completed_at = Some(completed_at);
        let elapsed = completed_at.saturating_sub(state.active[initial_index].started_at);
        let required_threshold = highest_crossed_threshold(self.shared.config, elapsed);

        if state.accepting
            && required_threshold.is_some()
            && !call_processed_through(&state, call_id, required_threshold)
        {
            let requested_change = mark_changed(&mut state);
            self.shared.wake.notify_all();
            while state.accepting
                && (state.processed_change_generation < requested_change
                    || !call_processed_through(&state, call_id, required_threshold))
            {
                state = wait_on_condvar(&self.shared.wake, state);
            }
        }

        let Some(index) = active_call_index(&state, call_id) else {
            return WatchdogThreadDisposition::Reusable;
        };
        let hard_elapsed = elapsed >= self.shared.config.hard_callback();
        if hard_elapsed
            && !state
                .retired_threads
                .contains(&state.active[index].callback_thread_id)
        {
            let callback_thread_id = state.active[index].callback_thread_id;
            retire_thread(&mut state, callback_thread_id);
        }
        let callback_thread_id = state.active[index].callback_thread_id;
        let retire = state.retirement_saturated
            || state.retired_threads.contains(&callback_thread_id)
            || state.active[index].hard_reported
            || hard_elapsed;
        state.active.remove(index);
        mark_changed(&mut state);
        self.shared.wake.notify_all();
        if retire {
            WatchdogThreadDisposition::Retire
        } else {
            WatchdogThreadDisposition::Reusable
        }
    }
}

/// RAII ledger entry for one active callback.
///
/// Call [`finish`](Self::finish) when the callback returns and obey the
/// resulting [`WatchdogThreadDisposition`]. Dropping without `finish` removes
/// the ledger entry, but cannot communicate a required hard-threshold thread
/// retirement to the owner. The guard is deliberately not `Send`: callback
/// completion must be observed on the same thread admitted by the watchdog.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<ferrumc_observability::WatchdogCallGuard>();
/// ```
#[must_use = "keep the guard alive for the complete callback and inspect finish()"]
pub struct WatchdogCallGuard {
    handle: PluginWatchdogHandle,
    call_id: WatchdogCallId,
    finished: bool,
    _not_send: PhantomData<Rc<()>>,
}

impl fmt::Debug for WatchdogCallGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WatchdogCallGuard")
            .field("call_id", &self.call_id)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl WatchdogCallGuard {
    /// Returns the monitored call identifier.
    pub const fn call_id(&self) -> WatchdogCallId {
        self.call_id
    }

    /// Completes monitoring and reports whether the callback thread is reusable.
    pub fn finish(mut self) -> WatchdogThreadDisposition {
        let disposition = self.handle.complete(self.call_id);
        self.finished = true;
        disposition
    }
}

impl Drop for WatchdogCallGuard {
    fn drop(&mut self) {
        if !self.finished {
            let disposition = self.handle.complete(self.call_id);
            if disposition == WatchdogThreadDisposition::Retire {
                tracing::error!(
                    call_id = self.call_id.get(),
                    "hard-stuck callback guard dropped without observing thread-retirement disposition"
                );
            }
        }
    }
}

/// Owner of the dedicated plugin-watchdog thread.
pub struct PluginWatchdog {
    handle: PluginWatchdogHandle,
    worker: Option<JoinHandle<()>>,
}

impl fmt::Debug for PluginWatchdog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginWatchdog")
            .field("handle", &self.handle)
            .field("worker_running", &self.worker.is_some())
            .finish()
    }
}

impl PluginWatchdog {
    /// Starts a watchdog with the standard tracing/stdout/stderr reporter.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the dedicated thread cannot be spawned.
    pub fn start(config: PluginWatchdogConfig) -> Result<Self, WatchdogStartError> {
        Self::start_with_reporter(config, Arc::new(TracingWatchdogReporter))
    }

    /// Starts a watchdog with a caller-supplied crash-report and log-flush sink.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the dedicated thread cannot be spawned.
    pub fn start_with_reporter(
        config: PluginWatchdogConfig,
        reporter: Arc<dyn WatchdogReporter>,
    ) -> Result<Self, WatchdogStartError> {
        Self::start_with_clock(config, reporter, Arc::new(SystemWatchdogClock::new()))
    }

    fn start_with_clock(
        config: PluginWatchdogConfig,
        reporter: Arc<dyn WatchdogReporter>,
        clock: Arc<dyn WatchdogClock>,
    ) -> Result<Self, WatchdogStartError> {
        let shared = Arc::new(WatchdogShared {
            config,
            clock,
            reporter,
            state: Mutex::new(WatchdogState::new()),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name(WATCHDOG_THREAD_NAME.to_owned())
            .spawn(move || watchdog_worker(&worker_shared))
            .map_err(|source| WatchdogStartError { source })?;
        Ok(Self {
            handle: PluginWatchdogHandle { shared },
            worker: Some(worker),
        })
    }

    /// Returns a cloneable callback-side handle.
    pub fn handle(&self) -> PluginWatchdogHandle {
        self.handle.clone()
    }

    /// Returns a bounded point-in-time snapshot.
    pub fn snapshot(&self) -> WatchdogSnapshot {
        self.handle.snapshot()
    }
}

impl Drop for PluginWatchdog {
    fn drop(&mut self) {
        {
            let mut state = lock_state(&self.handle.shared.state);
            state.accepting = false;
            mark_changed(&mut state);
            self.handle.shared.wake.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::error!("plugin watchdog thread terminated unexpectedly");
            }
        }
    }
}

#[derive(Clone)]
enum WorkerEmission {
    Soft(WatchdogDiagnostic),
    Unhealthy(WatchdogDiagnostic),
    Hard(WatchdogCrashReport),
}

impl WorkerEmission {
    fn marker(&self) -> (WatchdogCallId, WatchdogThreshold) {
        match self {
            Self::Soft(diagnostic) => (diagnostic.call_id(), WatchdogThreshold::Soft),
            Self::Unhealthy(diagnostic) => (diagnostic.call_id(), WatchdogThreshold::Unhealthy),
            Self::Hard(report) => (report.diagnostic().call_id(), WatchdogThreshold::Hard),
        }
    }
}

fn watchdog_worker(shared: &WatchdogShared) {
    loop {
        let (emissions, wait_for, scanned_change) = {
            let mut state = lock_state(&shared.state);
            #[cfg(test)]
            while state.worker_gate != TestWorkerGate::Open && state.accepting {
                state.worker_gate = TestWorkerGate::PauseObserved;
                shared.wake.notify_all();
                state = wait_on_condvar(&shared.wake, state);
            }
            if !state.accepting {
                return;
            }
            let now = shared.clock.now();
            let emissions = evaluate_thresholds(&mut state, shared.config, now);
            let wait_for = next_wait(&state, shared.config, now);
            (emissions, wait_for, state.change_generation)
        };

        let mut processed = Vec::with_capacity(emissions.len());
        for emission in emissions {
            processed.push(emission.marker());
            emit_transition(shared, emission);
        }

        let mut state = lock_state(&shared.state);
        for (call_id, threshold) in processed {
            if let Some(index) = active_call_index(&state, call_id) {
                state.active[index].mark_processed(threshold);
            }
        }
        // A completed scan includes its external report and flush side effects,
        // so deterministic callers cannot observe the state before those land.
        state.processed_change_generation = state.processed_change_generation.max(scanned_change);
        shared.wake.notify_all();
        if !state.accepting {
            return;
        }
        if state.change_generation != scanned_change {
            continue;
        }
        state = wait_for_change(shared, state, wait_for);
        drop(state);
    }
}

fn evaluate_thresholds(
    state: &mut WatchdogState,
    config: PluginWatchdogConfig,
    now: Duration,
) -> Vec<WorkerEmission> {
    let active_callbacks = state.active.len();
    let mut emissions = Vec::with_capacity(active_callbacks.saturating_mul(3));
    let mut newly_retired = Vec::new();
    for call in &mut state.active {
        let observed_at = call.completed_at.unwrap_or(now);
        let elapsed = observed_at.saturating_sub(call.started_at);
        if !call.soft_reported && elapsed >= config.soft_callback() {
            call.soft_reported = true;
            let diagnostic = diagnostic_for(call, WatchdogThreshold::Soft, elapsed);
            state.soft_warning_count = state.soft_warning_count.saturating_add(1);
            state.diagnostics.push(diagnostic.clone());
            emissions.push(WorkerEmission::Soft(diagnostic));
        }
        if !call.unhealthy_reported && elapsed >= config.unhealthy_callback() {
            call.unhealthy_reported = true;
            let diagnostic = diagnostic_for(call, WatchdogThreshold::Unhealthy, elapsed);
            state.diagnostics.push(diagnostic.clone());
            emissions.push(WorkerEmission::Unhealthy(diagnostic));
        }
        if !call.hard_reported && elapsed >= config.hard_callback() {
            call.hard_reported = true;
            newly_retired.push(call.callback_thread_id);
            let diagnostic = diagnostic_for(call, WatchdogThreshold::Hard, elapsed);
            let report = WatchdogCrashReport {
                diagnostic: diagnostic.clone(),
                active_callbacks,
            };
            state.hard_report_count = state.hard_report_count.saturating_add(1);
            state.diagnostics.push(diagnostic);
            state.crash_reports.push(report.clone());
            emissions.push(WorkerEmission::Hard(report));
        }
    }
    for callback_thread_id in newly_retired {
        retire_thread(state, callback_thread_id);
    }
    emissions
}

fn diagnostic_for(
    call: &ActiveCall,
    threshold: WatchdogThreshold,
    elapsed: Duration,
) -> WatchdogDiagnostic {
    WatchdogDiagnostic {
        call_id: call.call_id,
        callback: call.callback.clone(),
        callback_thread: call.callback_thread.clone(),
        threshold,
        elapsed,
    }
}

fn next_wait(
    state: &WatchdogState,
    config: PluginWatchdogConfig,
    now: Duration,
) -> Option<Duration> {
    state
        .active
        .iter()
        .filter_map(|call| {
            if call.completed_at.is_some() {
                return None;
            }
            let threshold = if !call.soft_reported {
                config.soft_callback()
            } else if !call.unhealthy_reported {
                config.unhealthy_callback()
            } else if !call.hard_reported {
                config.hard_callback()
            } else {
                return None;
            };
            let deadline = call.started_at.saturating_add(threshold);
            Some(deadline.saturating_sub(now))
        })
        .min()
}

fn emit_transition(shared: &WatchdogShared, emission: WorkerEmission) {
    match emission {
        WorkerEmission::Soft(diagnostic) => {
            tracing::warn!(
                target: "ferrumc::observability::plugin_watchdog",
                plugin = %diagnostic.plugin_id(),
                hook = diagnostic.hook(),
                shard = diagnostic.shard(),
                callback_thread = diagnostic.callback_thread(),
                elapsed_ms = duration_millis(diagnostic.elapsed()),
                "plugin callback crossed soft watchdog threshold"
            );
        }
        WorkerEmission::Unhealthy(diagnostic) => {
            tracing::error!(
                target: "ferrumc::observability::plugin_watchdog",
                plugin = %diagnostic.plugin_id(),
                hook = diagnostic.hook(),
                shard = diagnostic.shard(),
                callback_thread = diagnostic.callback_thread(),
                elapsed_ms = duration_millis(diagnostic.elapsed()),
                "plugin callback crossed unhealthy watchdog threshold; health failed"
            );
        }
        WorkerEmission::Hard(report) => {
            if catch_unwind(AssertUnwindSafe(|| {
                shared.reporter.record_crash_report(&report);
            }))
            .is_err()
            {
                tracing::error!("plugin watchdog crash reporter panicked");
            }
            if catch_unwind(AssertUnwindSafe(|| shared.reporter.flush_logs())).is_err() {
                tracing::error!("plugin watchdog log flusher panicked");
            }
            perform_hard_action(shared.config.hard_action());
        }
    }
}

fn perform_hard_action(action: WatchdogHardAction) {
    match action {
        WatchdogHardAction::ReportOnly => {}
        WatchdogHardAction::AbortProcess => std::process::abort(),
    }
}

fn wait_for_change<'a>(
    shared: &'a WatchdogShared,
    state: MutexGuard<'a, WatchdogState>,
    wait_for: Option<Duration>,
) -> MutexGuard<'a, WatchdogState> {
    if !shared.clock.uses_wall_clock_waits() {
        return wait_on_condvar(&shared.wake, state);
    }
    match wait_for {
        Some(duration) => wait_timeout_on_condvar(&shared.wake, state, duration),
        None => wait_on_condvar(&shared.wake, state),
    }
}

fn lock_state(mutex: &Mutex<WatchdogState>) -> MutexGuard<'_, WatchdogState> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn wait_on_condvar<'a>(
    condvar: &Condvar,
    state: MutexGuard<'a, WatchdogState>,
) -> MutexGuard<'a, WatchdogState> {
    condvar.wait(state).unwrap_or_else(PoisonError::into_inner)
}

fn wait_timeout_on_condvar<'a>(
    condvar: &Condvar,
    state: MutexGuard<'a, WatchdogState>,
    duration: Duration,
) -> MutexGuard<'a, WatchdogState> {
    match condvar.wait_timeout(state, duration) {
        Ok((state, _timeout)) => state,
        Err(poisoned) => poisoned.into_inner().0,
    }
}

fn active_call_index(state: &WatchdogState, call_id: WatchdogCallId) -> Option<usize> {
    state.active.iter().position(|call| call.call_id == call_id)
}

fn call_processed_through(
    state: &WatchdogState,
    call_id: WatchdogCallId,
    required: Option<WatchdogThreshold>,
) -> bool {
    let Some(required) = required else {
        return true;
    };
    active_call_index(state, call_id).is_none_or(|index| {
        state.active[index]
            .processed_threshold
            .is_some_and(|processed| processed >= required)
    })
}

fn highest_crossed_threshold(
    config: PluginWatchdogConfig,
    elapsed: Duration,
) -> Option<WatchdogThreshold> {
    if elapsed >= config.hard_callback() {
        Some(WatchdogThreshold::Hard)
    } else if elapsed >= config.unhealthy_callback() {
        Some(WatchdogThreshold::Unhealthy)
    } else if elapsed >= config.soft_callback() {
        Some(WatchdogThreshold::Soft)
    } else {
        None
    }
}

fn retire_thread(state: &mut WatchdogState, callback_thread_id: ThreadId) {
    if state.retirement_saturated || state.retired_threads.contains(&callback_thread_id) {
        return;
    }
    if state.retired_threads.len() == RETIRED_THREAD_CAPACITY {
        state.retirement_saturated = true;
        return;
    }
    state.retired_threads.push(callback_thread_id);
}

fn mark_changed(state: &mut WatchdogState) -> u64 {
    state.change_generation = state.change_generation.saturating_add(1);
    state.change_generation
}

fn callback_thread_identity() -> (ThreadId, String) {
    let current = thread::current();
    let id = current.id();
    let name = current.name().unwrap_or("unnamed");
    let label = truncate_utf8(format!("{name}:{id:?}"), MAX_THREAD_LABEL_BYTES);
    (id, label)
}

fn truncate_utf8(mut value: String, maximum: usize) -> String {
    if value.len() <= maximum {
        return value;
    }
    let mut boundary = maximum;
    while !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    value.truncate(boundary);
    value
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[derive(Default)]
    struct ManualClock {
        nanos: AtomicU64,
    }

    impl ManualClock {
        fn set(&self, now: Duration) {
            let nanos = u64::try_from(now.as_nanos()).unwrap_or(u64::MAX);
            self.nanos.store(nanos, Ordering::Release);
        }
    }

    impl WatchdogClock for ManualClock {
        fn now(&self) -> Duration {
            Duration::from_nanos(self.nanos.load(Ordering::Acquire))
        }

        fn uses_wall_clock_waits(&self) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct RecordingReporter {
        calls: Mutex<Vec<ReporterCall>>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum ReporterCall {
        Report {
            plugin: PluginId,
            hook: String,
            shard: String,
            worker: String,
        },
        Flush {
            worker: String,
        },
    }

    impl WatchdogReporter for RecordingReporter {
        fn record_crash_report(&self, report: &WatchdogCrashReport) {
            let worker = thread::current().name().unwrap_or("unnamed").to_owned();
            let diagnostic = report.diagnostic();
            lock_reporter(&self.calls).push(ReporterCall::Report {
                plugin: diagnostic.plugin_id().clone(),
                hook: diagnostic.hook().to_owned(),
                shard: diagnostic.shard().to_owned(),
                worker,
            });
        }

        fn flush_logs(&self) {
            let worker = thread::current().name().unwrap_or("unnamed").to_owned();
            lock_reporter(&self.calls).push(ReporterCall::Flush { worker });
        }
    }

    fn lock_reporter(mutex: &Mutex<Vec<ReporterCall>>) -> MutexGuard<'_, Vec<ReporterCall>> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn callback(id: &str) -> WatchdogCallback {
        WatchdogCallback::new(PluginId::new(id), "on_event", "0,-1").expect("bounded test callback")
    }

    fn start_manual(
        config: PluginWatchdogConfig,
    ) -> (PluginWatchdog, Arc<ManualClock>, Arc<RecordingReporter>) {
        let clock = Arc::new(ManualClock::default());
        let reporter = Arc::new(RecordingReporter::default());
        let watchdog = PluginWatchdog::start_with_clock(config, reporter.clone(), clock.clone())
            .expect("spawn manual watchdog");
        (watchdog, clock, reporter)
    }

    fn drive_and_wait(watchdog: &PluginWatchdog, clock: &ManualClock, now: Duration) {
        let shared = &watchdog.handle.shared;
        let mut state = lock_state(&shared.state);
        clock.set(now);
        let requested_change = mark_changed(&mut state);
        shared.wake.notify_all();
        while state.processed_change_generation < requested_change {
            state = wait_on_condvar(&shared.wake, state);
        }
    }

    fn pause_worker_scans(watchdog: &PluginWatchdog) {
        let shared = &watchdog.handle.shared;
        let mut state = lock_state(&shared.state);
        state.worker_gate = TestWorkerGate::PauseRequested;
        mark_changed(&mut state);
        shared.wake.notify_all();
        while state.worker_gate != TestWorkerGate::PauseObserved {
            state = wait_on_condvar(&shared.wake, state);
        }
    }

    #[test]
    fn config_uses_exact_keys_defaults_and_validation() {
        let default: PluginWatchdogConfig = serde_json::from_str("{}").expect("default config");
        assert_eq!(default, PluginWatchdogConfig::default());

        let configured: PluginWatchdogConfig = serde_json::from_str(
            r#"{
                "soft_callback_ms": 5,
                "unhealthy_callback_ms": 10,
                "hard_callback_ms": 20,
                "hard_action": "abort-process"
            }"#,
        )
        .expect("exact keys parse");
        assert_eq!(configured.soft_callback(), Duration::from_millis(5));
        assert_eq!(configured.hard_action(), WatchdogHardAction::AbortProcess);
        assert!(serde_json::from_str::<PluginWatchdogConfig>(
            r#"{"soft_callback_ms": 10, "unhealthy_callback_ms": 10}"#
        )
        .is_err());
        assert!(
            serde_json::from_str::<PluginWatchdogConfig>(r#"{"unknown_callback_ms": 10}"#).is_err()
        );
    }

    #[test]
    fn approved_nested_toml_surface_round_trips_exactly() {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Root {
            plugins: Plugins,
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Plugins {
            watchdog: PluginWatchdogConfig,
        }

        let root: Root = toml::from_str(
            r#"
                [plugins.watchdog]
                soft_callback_ms = 50
                unhealthy_callback_ms = 1000
                hard_callback_ms = 10000
                hard_action = "report-only"
            "#,
        )
        .expect("approved watchdog table");
        assert_eq!(root.plugins.watchdog, PluginWatchdogConfig::default());
    }

    #[test]
    fn callback_labels_are_bounded() {
        assert_eq!(
            WatchdogCallback::new(PluginId::new(""), "hook", "0,0"),
            Err(WatchdogLabelError::Empty { field: "plugin_id" })
        );
        assert!(matches!(
            WatchdogCallback::new(
                PluginId::new("p".repeat(MAX_PLUGIN_ID_BYTES + 1)),
                "hook",
                "0,0"
            ),
            Err(WatchdogLabelError::TooLong {
                field: "plugin_id",
                maximum: MAX_PLUGIN_ID_BYTES,
                ..
            })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn paused_time_crosses_each_threshold_once_on_real_worker() {
        let (watchdog, clock, reporter) = start_manual(PluginWatchdogConfig::default());
        let guard = watchdog
            .handle()
            .begin_callback(callback("fixture"))
            .expect("admit callback");

        tokio::time::advance(Duration::from_millis(49)).await;
        drive_and_wait(&watchdog, &clock, Duration::from_millis(49));
        let before = watchdog.snapshot();
        assert_eq!(before.health(), WatchdogHealth::Healthy);
        assert_eq!(before.soft_warning_count(), 0);
        assert!(before.recent_diagnostics().is_empty());

        tokio::time::advance(Duration::from_millis(1)).await;
        drive_and_wait(&watchdog, &clock, Duration::from_millis(50));
        let soft = watchdog.snapshot();
        assert_eq!(soft.health(), WatchdogHealth::Healthy);
        assert_eq!(soft.soft_warning_count(), 1);
        assert_eq!(soft.recent_diagnostics().len(), 1);
        assert_eq!(
            soft.recent_diagnostics()[0].threshold(),
            WatchdogThreshold::Soft
        );

        tokio::time::advance(Duration::from_millis(950)).await;
        drive_and_wait(&watchdog, &clock, Duration::from_secs(1));
        let unhealthy = watchdog.snapshot();
        assert_eq!(unhealthy.health(), WatchdogHealth::Failed);
        assert_eq!(unhealthy.recent_diagnostics().len(), 2);
        let unhealthy_diagnostic = &unhealthy.recent_diagnostics()[1];
        assert_eq!(
            unhealthy_diagnostic.threshold(),
            WatchdogThreshold::Unhealthy
        );
        assert_eq!(unhealthy_diagnostic.plugin_id(), &PluginId::new("fixture"));
        assert_eq!(unhealthy_diagnostic.hook(), "on_event");
        assert_eq!(unhealthy_diagnostic.shard(), "0,-1");
        assert!(!unhealthy_diagnostic.callback_thread().is_empty());
        assert_eq!(unhealthy_diagnostic.elapsed(), Duration::from_secs(1));

        tokio::time::advance(Duration::from_secs(9)).await;
        drive_and_wait(&watchdog, &clock, Duration::from_secs(10));
        let hard = watchdog.snapshot();
        assert_eq!(hard.health(), WatchdogHealth::Failed);
        assert_eq!(hard.soft_warning_count(), 1);
        assert_eq!(hard.hard_report_count(), 1);
        assert_eq!(hard.recent_diagnostics().len(), 3);
        assert_eq!(hard.recent_crash_reports().len(), 1);
        assert_eq!(hard.hung_plugins(), vec![PluginId::new("fixture")]);
        assert_eq!(
            lock_reporter(&reporter.calls).as_slice(),
            [
                ReporterCall::Report {
                    plugin: PluginId::new("fixture"),
                    hook: "on_event".to_owned(),
                    shard: "0,-1".to_owned(),
                    worker: WATCHDOG_THREAD_NAME.to_owned(),
                },
                ReporterCall::Flush {
                    worker: WATCHDOG_THREAD_NAME.to_owned(),
                },
            ]
        );

        tokio::time::advance(Duration::from_secs(5)).await;
        drive_and_wait(&watchdog, &clock, Duration::from_secs(15));
        let repeated = watchdog.snapshot();
        assert_eq!(repeated.soft_warning_count(), 1);
        assert_eq!(repeated.hard_report_count(), 1);
        assert_eq!(repeated.recent_diagnostics().len(), 3);
        assert_eq!(lock_reporter(&reporter.calls).len(), 2);

        assert_eq!(guard.finish(), WatchdogThreadDisposition::Retire);
        let recovered = watchdog.snapshot();
        assert_eq!(recovered.health(), WatchdogHealth::Healthy);
        assert!(recovered.active().is_empty());
        assert_eq!(recovered.recent_diagnostics().len(), 3);
    }

    #[test]
    fn completion_forces_pending_hard_scan_and_retires_origin_thread() {
        let (watchdog, clock, reporter) = start_manual(PluginWatchdogConfig::default());
        let handle = watchdog.handle();
        pause_worker_scans(&watchdog);
        let guard = handle
            .begin_callback(callback("completion-race"))
            .expect("admit callback");
        let call_id = guard.call_id();

        // Pin the hostile interleaving: the callback completes at the hard
        // boundary before the worker is permitted to inspect the ledger.
        clock.set(Duration::from_secs(10));
        let release_handle = handle.clone();
        let releaser = thread::spawn(move || {
            let shared = &release_handle.shared;
            let mut state = lock_state(&shared.state);
            while active_call_index(&state, call_id)
                .is_some_and(|index| state.active[index].completed_at.is_none())
            {
                state = wait_on_condvar(&shared.wake, state);
            }
            state.worker_gate = TestWorkerGate::Open;
            shared.wake.notify_all();
        });
        assert_eq!(guard.finish(), WatchdogThreadDisposition::Retire);
        releaser.join().expect("release worker scan");

        let snapshot = watchdog.snapshot();
        assert_eq!(snapshot.soft_warning_count(), 1);
        assert_eq!(snapshot.hard_report_count(), 1);
        assert_eq!(snapshot.recent_diagnostics().len(), 3);
        assert_eq!(snapshot.recent_crash_reports().len(), 1);
        assert_eq!(lock_reporter(&reporter.calls).len(), 2);
        assert!(matches!(
            handle.begin_callback(callback("must-not-reuse")),
            Err(WatchdogBeginError::RetiredThread)
        ));
    }

    #[test]
    fn completion_elapsed_is_sampled_at_return_boundary() {
        let (watchdog, clock, _reporter) = start_manual(PluginWatchdogConfig::default());
        let guard = watchdog
            .handle()
            .begin_callback(callback("finish-boundary"))
            .expect("admit callback");

        clock.set(Duration::from_millis(50));
        assert_eq!(guard.finish(), WatchdogThreadDisposition::Reusable);
        let snapshot = watchdog.snapshot();
        assert_eq!(snapshot.soft_warning_count(), 1);
        assert_eq!(snapshot.hard_report_count(), 0);
        assert_eq!(snapshot.recent_diagnostics().len(), 1);
        assert_eq!(
            snapshot.recent_diagnostics()[0].elapsed(),
            Duration::from_millis(50)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn one_completion_cannot_clear_another_hard_call() {
        let (watchdog, clock, _reporter) = start_manual(PluginWatchdogConfig::default());
        let first = watchdog
            .handle()
            .begin_callback(callback("same-plugin"))
            .expect("first call");
        let second = watchdog
            .handle()
            .begin_callback(callback("same-plugin"))
            .expect("second call");

        tokio::time::advance(Duration::from_secs(10)).await;
        drive_and_wait(&watchdog, &clock, Duration::from_secs(10));
        assert_eq!(
            watchdog.snapshot().hung_plugins(),
            vec![PluginId::new("same-plugin")]
        );
        assert_eq!(first.finish(), WatchdogThreadDisposition::Retire);
        assert_eq!(watchdog.snapshot().health(), WatchdogHealth::Failed);
        assert_eq!(
            watchdog.snapshot().hung_plugins(),
            vec![PluginId::new("same-plugin")]
        );
        assert_eq!(second.finish(), WatchdogThreadDisposition::Retire);
        assert_eq!(watchdog.snapshot().health(), WatchdogHealth::Healthy);
    }

    #[test]
    fn full_ledger_rejects_instead_of_running_unmonitored() {
        let (watchdog, _clock, _reporter) = start_manual(PluginWatchdogConfig::default());
        let handle = watchdog.handle();
        let mut guards = Vec::with_capacity(ACTIVE_CALLBACK_CAPACITY);
        for index in 0..ACTIVE_CALLBACK_CAPACITY {
            guards.push(
                handle
                    .begin_callback(callback(&format!("plugin-{index}")))
                    .expect("within active capacity"),
            );
        }
        assert!(matches!(
            handle.begin_callback(callback("overflow")),
            Err(WatchdogBeginError::LedgerFull {
                maximum: ACTIVE_CALLBACK_CAPACITY,
            })
        ));
        drop(guards);
    }

    #[test]
    fn saturated_retirement_ledger_fails_all_admission_closed() {
        let (watchdog, _clock, _reporter) = start_manual(PluginWatchdogConfig::default());
        {
            let mut state = lock_state(&watchdog.handle.shared.state);
            state.retirement_saturated = true;
        }
        assert!(matches!(
            watchdog
                .handle()
                .begin_callback(callback("retirement-overflow")),
            Err(WatchdogBeginError::RetirementLedgerFull {
                maximum: RETIRED_THREAD_CAPACITY,
            })
        ));
    }

    #[test]
    fn diagnostic_and_crash_history_are_bounded() {
        let mut diagnostics = RingBuffer::<WatchdogDiagnostic, DIAGNOSTIC_HISTORY_CAPACITY>::new();
        let sample = WatchdogDiagnostic {
            call_id: WatchdogCallId(1),
            callback: callback("fixture"),
            callback_thread: "thread".to_owned(),
            threshold: WatchdogThreshold::Soft,
            elapsed: Duration::from_millis(50),
        };
        for _ in 0..(DIAGNOSTIC_HISTORY_CAPACITY + 10) {
            diagnostics.push(sample.clone());
        }
        assert_eq!(diagnostics.len(), DIAGNOSTIC_HISTORY_CAPACITY);
    }

    #[test]
    fn live_crash_report_history_evicts_the_oldest_entry() {
        let (watchdog, clock, reporter) = start_manual(PluginWatchdogConfig::default());
        let handle = watchdog.handle();
        let mut guards = Vec::with_capacity(CRASH_REPORT_HISTORY_CAPACITY + 1);
        for index in 0..=CRASH_REPORT_HISTORY_CAPACITY {
            guards.push(
                handle
                    .begin_callback(callback(&format!("crash-{index}")))
                    .expect("admit bounded crash fixture"),
            );
        }

        drive_and_wait(&watchdog, &clock, Duration::from_secs(10));
        let snapshot = watchdog.snapshot();
        assert_eq!(
            snapshot.recent_crash_reports().len(),
            CRASH_REPORT_HISTORY_CAPACITY
        );
        assert_eq!(
            snapshot.recent_crash_reports()[0]
                .diagnostic()
                .call_id()
                .get(),
            2
        );
        assert_eq!(
            lock_reporter(&reporter.calls).len(),
            (CRASH_REPORT_HISTORY_CAPACITY + 1) * 2
        );

        for guard in guards {
            assert_eq!(guard.finish(), WatchdogThreadDisposition::Retire);
        }
    }
}
