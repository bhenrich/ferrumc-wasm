//! Bounded, per-plugin callback metrics.
//!
//! A callback owner submits exactly one [`PluginInvocationObservation`] after a
//! callback returns. Keeping elapsed time, budget classification, cooperative
//! panic status, and host-call error count in one observation prevents callers
//! from accidentally counting the overlapping failure views in a dispatch
//! report more than once.

use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ferrumc_core::PluginId;
use serde::de::{Error as DeserializeError, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{WatchdogSnapshot, ACTIVE_CALLBACK_CAPACITY};

/// Maximum number of stable plugin metric rows retained by one registry.
///
/// Rows are never evicted because eviction would make process-lifetime counters
/// disappear or decrease. Observations for a new plugin after this capacity is
/// reached fold into [`PluginMetricsSnapshot::overflowed_observations`].
pub const PLUGIN_METRIC_CAPACITY: usize = 256;

/// Maximum UTF-8 byte length accepted for a plugin metric label.
///
/// This matches the watchdog callback identity limit. Validation happens before
/// the identifier is cloned into the fixed-capacity table.
pub const PLUGIN_METRIC_ID_MAX_BYTES: usize = 128;

/// Invalid stable plugin identifier for the metric label surface.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginMetricLabelError {
    /// The plugin identifier was empty.
    EmptyPluginId,
    /// The plugin identifier exceeded [`PLUGIN_METRIC_ID_MAX_BYTES`].
    PluginIdTooLong {
        /// Actual UTF-8 byte length.
        actual: usize,
        /// Maximum accepted UTF-8 byte length.
        maximum: usize,
    },
}

impl fmt::Display for PluginMetricLabelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPluginId => formatter.write_str("plugin metric identifier is empty"),
            Self::PluginIdTooLong { actual, maximum } => write!(
                formatter,
                "plugin metric identifier is {actual} bytes; maximum is {maximum}"
            ),
        }
    }
}

impl std::error::Error for PluginMetricLabelError {}

/// One completed plugin callback's authoritative metric classification.
///
/// `host_call_errors` counts individual capability-facade calls that returned a
/// typed error. It does not count the callback status, a panic classification,
/// a budget classification, or a later command-buffer commit failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInvocationObservation {
    plugin_id: PluginId,
    elapsed: Duration,
    over_budget: bool,
    panic_status: bool,
    host_call_errors: u64,
}

impl PluginInvocationObservation {
    /// Builds a successful completed-callback observation.
    ///
    /// Use the builder methods to add classifications produced by that same
    /// callback. A callback that never returns has no completed invocation time;
    /// its current hard-stuck state is projected separately from the watchdog.
    ///
    /// # Errors
    ///
    /// Returns a typed error when `plugin_id` is empty or oversized.
    pub fn new(plugin_id: PluginId, elapsed: Duration) -> Result<Self, PluginMetricLabelError> {
        validate_plugin_id(&plugin_id)?;
        Ok(Self {
            plugin_id,
            elapsed,
            over_budget: false,
            panic_status: false,
            host_call_errors: 0,
        })
    }

    /// Classifies this callback as having exceeded its invocation budget.
    #[must_use]
    pub fn with_over_budget(mut self) -> Self {
        self.over_budget = true;
        self
    }

    /// Classifies this callback as returning the cooperative plugin-panic status.
    ///
    /// This represents one callback outcome even when a host report also exposes
    /// that outcome through panic, native-panic, and native-failure views.
    #[must_use]
    pub fn with_panic_status(mut self) -> Self {
        self.panic_status = true;
        self
    }

    /// Records how many host capability calls failed during this callback.
    #[must_use]
    pub fn with_host_call_errors(mut self, count: u64) -> Self {
        self.host_call_errors = count;
        self
    }

    /// Returns the stable plugin identifier.
    pub const fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    /// Returns the completed callback duration.
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// Returns whether this callback exceeded its invocation budget.
    pub const fn is_over_budget(&self) -> bool {
        self.over_budget
    }

    /// Returns whether this callback returned the cooperative panic status.
    pub const fn is_panic_status(&self) -> bool {
        self.panic_status
    }

    /// Returns the number of failed host capability calls in this callback.
    pub const fn host_call_errors(&self) -> u64 {
        self.host_call_errors
    }
}

/// Result of recording one valid plugin invocation observation.
///
/// Dropping an observation is telemetry-only and must never change callback,
/// gameplay, readiness, or access-control behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum PluginMetricRecordOutcome {
    /// The observation updated a retained plugin row.
    Recorded,
    /// The table was full and the new plugin folded into the overflow counter.
    DroppedCapacity,
}

/// Immutable, serializable metric values for one stable plugin identifier.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginMetricEntry {
    plugin_id: PluginId,
    invocation_count: u64,
    invocation_time_us_total: u64,
    invocation_time_us_last: u64,
    invocation_time_us_max: u64,
    invocation_time_us_avg: f64,
    over_budget_count: u64,
    panic_count: u64,
    hung: bool,
    host_call_error_count: u64,
}

impl PluginMetricEntry {
    /// Returns the stable plugin identifier used as the metric label.
    pub const fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    /// Returns the monotonic number of completed callback observations.
    pub const fn invocation_count(&self) -> u64 {
        self.invocation_count
    }

    /// Returns the saturating total callback duration in microseconds.
    pub const fn invocation_time_us_total(&self) -> u64 {
        self.invocation_time_us_total
    }

    /// Returns the most recently completed callback duration in microseconds.
    pub const fn invocation_time_us_last(&self) -> u64 {
        self.invocation_time_us_last
    }

    /// Returns the largest completed callback duration in microseconds.
    pub const fn invocation_time_us_max(&self) -> u64 {
        self.invocation_time_us_max
    }

    /// Returns total duration divided by completed invocation count.
    ///
    /// This is the exact arithmetic mean until either underlying `u64` summary
    /// saturates; after saturation it remains a bounded diagnostic quotient.
    pub const fn invocation_time_us_avg(&self) -> f64 {
        self.invocation_time_us_avg
    }

    /// Returns the monotonic count of over-budget callbacks.
    pub const fn over_budget_count(&self) -> u64 {
        self.over_budget_count
    }

    /// Returns the monotonic count of cooperative plugin-panic statuses.
    pub const fn panic_count(&self) -> u64 {
        self.panic_count
    }

    /// Returns whether the watchdog currently has a hard-stuck callback for this plugin.
    pub const fn is_hung(&self) -> bool {
        self.hung
    }

    /// Returns the monotonic number of failed host capability calls.
    pub const fn host_call_error_count(&self) -> u64 {
        self.host_call_error_count
    }
}

/// Bounded, deterministic snapshot of all retained per-plugin metrics.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PluginMetricsSnapshot {
    entries: Vec<PluginMetricEntry>,
    overflowed_observations: u64,
    untracked_hung_plugins: u64,
}

impl PluginMetricsSnapshot {
    /// Returns rows sorted lexicographically by stable plugin identifier.
    pub fn entries(&self) -> &[PluginMetricEntry] {
        &self.entries
    }

    /// Looks up one retained row by stable plugin identifier.
    pub fn entry(&self, plugin_id: &PluginId) -> Option<&PluginMetricEntry> {
        self.entries
            .iter()
            .find(|entry| entry.plugin_id() == plugin_id)
    }

    /// Returns the monotonic count of observations that could not be retained.
    ///
    /// This counts completed invocations for new plugins after capacity.
    pub const fn overflowed_observations(&self) -> u64 {
        self.overflowed_observations
    }

    /// Returns the current number of hard-stuck plugins that did not fit a row.
    ///
    /// This is a gauge recomputed by each authoritative watchdog projection, not
    /// a polling-frequency-dependent counter.
    pub const fn untracked_hung_plugins(&self) -> u64 {
        self.untracked_hung_plugins
    }
}

impl<'de> Deserialize<'de> for PluginMetricsSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PluginMetricsSnapshotWire::deserialize(deserializer)?;
        if wire.untracked_hung_plugins > ACTIVE_CALLBACK_CAPACITY as u64 {
            return Err(D::Error::custom(format_args!(
                "untracked hung-plugin gauge exceeds watchdog capacity {ACTIVE_CALLBACK_CAPACITY}"
            )));
        }
        let mut entries = Vec::with_capacity(wire.entries.0.len());
        for entry in wire.entries.0 {
            entries.push(entry.into_entry::<D::Error>()?);
        }
        entries.sort_unstable_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
        if entries
            .windows(2)
            .any(|pair| pair[0].plugin_id == pair[1].plugin_id)
        {
            return Err(D::Error::custom(
                "plugin metric snapshot contains a duplicate plugin identifier",
            ));
        }
        if wire.untracked_hung_plugins != 0 && entries.len() != PLUGIN_METRIC_CAPACITY {
            return Err(D::Error::custom(
                "untracked hung plugins require a full plugin metric table",
            ));
        }
        let tracked_hung = entries.iter().filter(|entry| entry.hung).count() as u64;
        if tracked_hung.saturating_add(wire.untracked_hung_plugins)
            > ACTIVE_CALLBACK_CAPACITY as u64
        {
            return Err(D::Error::custom(
                "tracked and untracked hung plugins exceed watchdog capacity",
            ));
        }
        Ok(Self {
            entries,
            overflowed_observations: wire.overflowed_observations,
            untracked_hung_plugins: wire.untracked_hung_plugins,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginMetricsSnapshotWire {
    entries: BoundedPluginMetricEntries,
    overflowed_observations: u64,
    #[serde(default)]
    untracked_hung_plugins: u64,
}

struct BoundedPluginMetricEntries(Vec<PluginMetricEntryWire>);

impl<'de> Deserialize<'de> for BoundedPluginMetricEntries {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_seq(BoundedPluginMetricEntriesVisitor)
    }
}

struct BoundedPluginMetricEntriesVisitor;

impl<'de> Visitor<'de> for BoundedPluginMetricEntriesVisitor {
    type Value = BoundedPluginMetricEntries;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "at most {PLUGIN_METRIC_CAPACITY} plugin metric entries"
        )
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let hinted = sequence
            .size_hint()
            .unwrap_or(0)
            .min(PLUGIN_METRIC_CAPACITY);
        let mut entries = Vec::with_capacity(hinted);
        while entries.len() < PLUGIN_METRIC_CAPACITY {
            match sequence.next_element()? {
                Some(entry) => entries.push(entry),
                None => return Ok(BoundedPluginMetricEntries(entries)),
            }
        }
        if sequence.next_element::<IgnoredAny>()?.is_some() {
            return Err(A::Error::custom(format_args!(
                "plugin metric snapshot exceeds capacity {PLUGIN_METRIC_CAPACITY}"
            )));
        }
        Ok(BoundedPluginMetricEntries(entries))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginMetricEntryWire {
    plugin_id: BoundedPluginId,
    invocation_count: u64,
    invocation_time_us_total: u64,
    invocation_time_us_last: u64,
    invocation_time_us_max: u64,
    invocation_time_us_avg: f64,
    over_budget_count: u64,
    panic_count: u64,
    hung: bool,
    host_call_error_count: u64,
}

impl PluginMetricEntryWire {
    fn into_entry<E>(self) -> Result<PluginMetricEntry, E>
    where
        E: DeserializeError,
    {
        if self.invocation_count == 0 {
            if !self.hung
                || self.invocation_time_us_total != 0
                || self.invocation_time_us_last != 0
                || self.invocation_time_us_max != 0
                || self.over_budget_count != 0
                || self.panic_count != 0
                || self.host_call_error_count != 0
            {
                return Err(E::custom(
                    "zero-invocation plugin metric row is not an active hung placeholder",
                ));
            }
        } else if self.invocation_time_us_last > self.invocation_time_us_max
            || self.invocation_time_us_max > self.invocation_time_us_total
            || self.over_budget_count > self.invocation_count
            || self.panic_count > self.invocation_count
        {
            return Err(E::custom(
                "plugin metric row contains inconsistent callback counters",
            ));
        }

        let expected_avg = if self.invocation_count == 0 {
            0.0
        } else {
            self.invocation_time_us_total as f64 / self.invocation_count as f64
        };
        if self.invocation_time_us_avg.to_bits() != expected_avg.to_bits() {
            return Err(E::custom(
                "plugin metric row contains an inconsistent invocation average",
            ));
        }

        Ok(PluginMetricEntry {
            plugin_id: self.plugin_id.0,
            invocation_count: self.invocation_count,
            invocation_time_us_total: self.invocation_time_us_total,
            invocation_time_us_last: self.invocation_time_us_last,
            invocation_time_us_max: self.invocation_time_us_max,
            invocation_time_us_avg: expected_avg,
            over_budget_count: self.over_budget_count,
            panic_count: self.panic_count,
            hung: self.hung,
            host_call_error_count: self.host_call_error_count,
        })
    }
}

struct BoundedPluginId(PluginId);

impl<'de> Deserialize<'de> for BoundedPluginId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(BoundedPluginIdVisitor)
    }
}

struct BoundedPluginIdVisitor;

impl Visitor<'_> for BoundedPluginIdVisitor {
    type Value = BoundedPluginId;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "a non-empty plugin identifier of at most {PLUGIN_METRIC_ID_MAX_BYTES} bytes"
        )
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: DeserializeError,
    {
        validate_plugin_id_value(value)?;
        Ok(BoundedPluginId(PluginId::new(value)))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: DeserializeError,
    {
        validate_plugin_id_value(&value)?;
        Ok(BoundedPluginId(PluginId::new(value)))
    }
}

fn validate_plugin_id_value<E>(value: &str) -> Result<(), E>
where
    E: DeserializeError,
{
    if value.is_empty() {
        return Err(E::custom(PluginMetricLabelError::EmptyPluginId));
    }
    if value.len() > PLUGIN_METRIC_ID_MAX_BYTES {
        return Err(E::custom(PluginMetricLabelError::PluginIdTooLong {
            actual: value.len(),
            maximum: PLUGIN_METRIC_ID_MAX_BYTES,
        }));
    }
    Ok(())
}

#[derive(Debug)]
struct PluginMetricRow {
    plugin_id: PluginId,
    invocation_count: u64,
    invocation_time_us_total: u64,
    invocation_time_us_last: u64,
    invocation_time_us_max: u64,
    over_budget_count: u64,
    panic_count: u64,
    hung: bool,
    host_call_error_count: u64,
}

impl PluginMetricRow {
    fn from_observation(observation: PluginInvocationObservation) -> Self {
        let elapsed_us = duration_micros(observation.elapsed);
        Self {
            plugin_id: observation.plugin_id,
            invocation_count: 1,
            invocation_time_us_total: elapsed_us,
            invocation_time_us_last: elapsed_us,
            invocation_time_us_max: elapsed_us,
            over_budget_count: u64::from(observation.over_budget),
            panic_count: u64::from(observation.panic_status),
            hung: false,
            host_call_error_count: observation.host_call_errors,
        }
    }

    fn hung(plugin_id: PluginId) -> Self {
        Self {
            plugin_id,
            invocation_count: 0,
            invocation_time_us_total: 0,
            invocation_time_us_last: 0,
            invocation_time_us_max: 0,
            over_budget_count: 0,
            panic_count: 0,
            hung: true,
            host_call_error_count: 0,
        }
    }

    fn record(&mut self, observation: &PluginInvocationObservation) {
        let elapsed_us = duration_micros(observation.elapsed);
        self.invocation_count = self.invocation_count.saturating_add(1);
        self.invocation_time_us_total = self.invocation_time_us_total.saturating_add(elapsed_us);
        self.invocation_time_us_last = elapsed_us;
        self.invocation_time_us_max = self.invocation_time_us_max.max(elapsed_us);
        if observation.over_budget {
            self.over_budget_count = self.over_budget_count.saturating_add(1);
        }
        if observation.panic_status {
            self.panic_count = self.panic_count.saturating_add(1);
        }
        self.host_call_error_count = self
            .host_call_error_count
            .saturating_add(observation.host_call_errors);
    }

    fn snapshot(&self) -> PluginMetricEntry {
        PluginMetricEntry {
            plugin_id: self.plugin_id.clone(),
            invocation_count: self.invocation_count,
            invocation_time_us_total: self.invocation_time_us_total,
            invocation_time_us_last: self.invocation_time_us_last,
            invocation_time_us_max: self.invocation_time_us_max,
            invocation_time_us_avg: if self.invocation_count == 0 {
                0.0
            } else {
                self.invocation_time_us_total as f64 / self.invocation_count as f64
            },
            over_budget_count: self.over_budget_count,
            panic_count: self.panic_count,
            hung: self.hung,
            host_call_error_count: self.host_call_error_count,
        }
    }
}

#[derive(Debug)]
struct PluginMetricTable {
    rows: [Option<PluginMetricRow>; PLUGIN_METRIC_CAPACITY],
    len: usize,
    overflowed_observations: u64,
    untracked_hung_plugins: u64,
}

impl PluginMetricTable {
    fn new() -> Self {
        Self {
            rows: std::array::from_fn(|_| None),
            len: 0,
            overflowed_observations: 0,
            untracked_hung_plugins: 0,
        }
    }

    fn record_drop(&mut self) {
        self.overflowed_observations = self.overflowed_observations.saturating_add(1);
    }

    fn remove_inactive_placeholders(&mut self) {
        let previous_len = self.len;
        let mut retained = 0;
        for index in 0..previous_len {
            let keep = self.rows[index]
                .as_ref()
                .is_some_and(|row| row.invocation_count != 0 || row.hung);
            if keep {
                if retained != index {
                    self.rows[retained] = self.rows[index].take();
                }
                retained += 1;
            }
        }
        for index in retained..previous_len {
            self.rows[index] = None;
        }
        self.len = retained;
    }
}

/// Fixed-capacity metric table owned by [`crate::CounterRegistry`].
#[derive(Debug)]
pub(crate) struct PluginMetricRegistry {
    table: Mutex<PluginMetricTable>,
}

impl PluginMetricRegistry {
    pub(crate) fn new() -> Self {
        Self {
            table: Mutex::new(PluginMetricTable::new()),
        }
    }

    pub(crate) fn record(
        &self,
        observation: PluginInvocationObservation,
    ) -> PluginMetricRecordOutcome {
        let mut table = lock_table(&self.table);
        let len = table.len;
        for row in table.rows.iter_mut().take(len).flatten() {
            if row.plugin_id == observation.plugin_id {
                row.record(&observation);
                return PluginMetricRecordOutcome::Recorded;
            }
        }

        if len == PLUGIN_METRIC_CAPACITY {
            table.record_drop();
            return PluginMetricRecordOutcome::DroppedCapacity;
        }

        table.rows[len] = Some(PluginMetricRow::from_observation(observation));
        table.len = len + 1;
        PluginMetricRecordOutcome::Recorded
    }

    pub(crate) fn sync_hung_from_watchdog(&self, snapshot: &WatchdogSnapshot) {
        let hung_plugins = snapshot.hung_plugins();
        let mut table = lock_table(&self.table);
        let len = table.len;
        for row in table.rows.iter_mut().take(len).flatten() {
            row.hung = hung_plugins
                .iter()
                .any(|plugin_id| plugin_id == &row.plugin_id);
        }
        table.remove_inactive_placeholders();
        table.untracked_hung_plugins = 0;

        for plugin_id in hung_plugins {
            let len = table.len;
            let mut found = false;
            for row in table.rows.iter_mut().take(len).flatten() {
                if row.plugin_id == plugin_id {
                    row.hung = true;
                    found = true;
                    break;
                }
            }
            if found {
                continue;
            }
            if len == PLUGIN_METRIC_CAPACITY {
                table.untracked_hung_plugins = table.untracked_hung_plugins.saturating_add(1);
                continue;
            }
            table.rows[len] = Some(PluginMetricRow::hung(plugin_id));
            table.len = len + 1;
        }
    }

    pub(crate) fn snapshot(&self) -> PluginMetricsSnapshot {
        let table = lock_table(&self.table);
        let mut entries: Vec<_> = table
            .rows
            .iter()
            .take(table.len)
            .flatten()
            .map(PluginMetricRow::snapshot)
            .collect();
        entries.sort_unstable_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
        PluginMetricsSnapshot {
            entries,
            overflowed_observations: table.overflowed_observations,
            untracked_hung_plugins: table.untracked_hung_plugins,
        }
    }
}

fn validate_plugin_id(plugin_id: &PluginId) -> Result<(), PluginMetricLabelError> {
    let value = plugin_id.as_str();
    if value.is_empty() {
        return Err(PluginMetricLabelError::EmptyPluginId);
    }
    if value.len() > PLUGIN_METRIC_ID_MAX_BYTES {
        return Err(PluginMetricLabelError::PluginIdTooLong {
            actual: value.len(),
            maximum: PLUGIN_METRIC_ID_MAX_BYTES,
        });
    }
    Ok(())
}

fn duration_micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn lock_table(table: &Mutex<PluginMetricTable>) -> MutexGuard<'_, PluginMetricTable> {
    table.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    fn observation(plugin_id: &str, elapsed_us: u64) -> PluginInvocationObservation {
        PluginInvocationObservation::new(
            PluginId::new(plugin_id),
            Duration::from_micros(elapsed_us),
        )
        .expect("valid test observation")
    }

    fn serialized_entry(plugin_id: &str) -> serde_json::Value {
        serde_json::json!({
            "plugin_id": plugin_id,
            "invocation_count": 1,
            "invocation_time_us_total": 2,
            "invocation_time_us_last": 2,
            "invocation_time_us_max": 2,
            "invocation_time_us_avg": 2.0,
            "over_budget_count": 0,
            "panic_count": 0,
            "hung": false,
            "host_call_error_count": 0
        })
    }

    #[test]
    fn invalid_labels_are_rejected_before_table_insertion() {
        assert_eq!(
            PluginInvocationObservation::new(PluginId::new(""), Duration::ZERO),
            Err(PluginMetricLabelError::EmptyPluginId)
        );
        let oversized = "x".repeat(PLUGIN_METRIC_ID_MAX_BYTES + 1);
        assert_eq!(
            PluginInvocationObservation::new(PluginId::new(oversized), Duration::ZERO),
            Err(PluginMetricLabelError::PluginIdTooLong {
                actual: PLUGIN_METRIC_ID_MAX_BYTES + 1,
                maximum: PLUGIN_METRIC_ID_MAX_BYTES,
            })
        );
    }

    #[test]
    fn capacity_drops_new_plugins_without_evicting_or_freezing_existing_rows() {
        let registry = PluginMetricRegistry::new();
        for index in 0..PLUGIN_METRIC_CAPACITY {
            assert_eq!(
                registry.record(observation(&format!("plugin-{index:03}"), 1)),
                PluginMetricRecordOutcome::Recorded
            );
        }
        assert_eq!(
            registry.record(observation("overflow", 1)),
            PluginMetricRecordOutcome::DroppedCapacity
        );
        assert_eq!(
            registry.record(observation("plugin-000", 2)),
            PluginMetricRecordOutcome::Recorded
        );

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.entries().len(), PLUGIN_METRIC_CAPACITY);
        assert_eq!(snapshot.overflowed_observations(), 1);
        assert!(snapshot.entry(&PluginId::new("overflow")).is_none());
        assert_eq!(
            snapshot
                .entry(&PluginId::new("plugin-000"))
                .expect("retained row")
                .invocation_count(),
            2
        );
    }

    #[test]
    fn concurrent_first_insert_and_updates_have_exact_totals() {
        const THREADS: usize = 8;
        const OBSERVATIONS_PER_THREAD: usize = 500;

        let registry = Arc::new(PluginMetricRegistry::new());
        let mut workers = Vec::new();
        for _ in 0..THREADS {
            let registry = Arc::clone(&registry);
            workers.push(thread::spawn(move || {
                for _ in 0..OBSERVATIONS_PER_THREAD {
                    assert_eq!(
                        registry.record(observation("shared", 2)),
                        PluginMetricRecordOutcome::Recorded
                    );
                }
            }));
        }
        for worker in workers {
            worker.join().expect("metric worker");
        }

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.entries().len(), 1);
        let entry = snapshot
            .entry(&PluginId::new("shared"))
            .expect("shared row");
        let expected = (THREADS * OBSERVATIONS_PER_THREAD) as u64;
        assert_eq!(entry.invocation_count(), expected);
        assert_eq!(entry.invocation_time_us_total(), expected * 2);
    }

    #[test]
    fn counters_and_duration_conversion_saturate() {
        let mut row = PluginMetricRow {
            plugin_id: PluginId::new("saturation"),
            invocation_count: u64::MAX - 1,
            invocation_time_us_total: u64::MAX - 1,
            invocation_time_us_last: 1,
            invocation_time_us_max: 1,
            over_budget_count: u64::MAX - 1,
            panic_count: u64::MAX - 1,
            hung: false,
            host_call_error_count: u64::MAX - 1,
        };
        let extreme = PluginInvocationObservation::new(PluginId::new("saturation"), Duration::MAX)
            .expect("valid")
            .with_over_budget()
            .with_panic_status()
            .with_host_call_errors(u64::MAX);
        row.record(&extreme);
        row.record(&extreme);

        let entry = row.snapshot();
        assert_eq!(entry.invocation_count(), u64::MAX);
        assert_eq!(entry.invocation_time_us_total(), u64::MAX);
        assert_eq!(entry.invocation_time_us_last(), u64::MAX);
        assert_eq!(entry.invocation_time_us_max(), u64::MAX);
        assert_eq!(entry.over_budget_count(), u64::MAX);
        assert_eq!(entry.panic_count(), u64::MAX);
        assert_eq!(entry.host_call_error_count(), u64::MAX);
    }

    #[test]
    fn inactive_hung_only_rows_are_reclaimed_without_evicting_counter_rows() {
        let mut table = PluginMetricTable::new();
        table.rows[0] = Some(PluginMetricRow::hung(PluginId::new("transient")));
        table.rows[1] = Some(PluginMetricRow::from_observation(observation(
            "completed",
            1,
        )));
        table.len = 2;
        for row in table.rows.iter_mut().take(table.len).flatten() {
            row.hung = false;
        }

        table.remove_inactive_placeholders();

        assert_eq!(table.len, 1);
        let retained = table.rows[0].as_ref().expect("counter row retained");
        assert_eq!(retained.plugin_id, PluginId::new("completed"));
        assert_eq!(retained.invocation_count, 1);
    }

    #[test]
    fn snapshot_order_is_deterministic_by_stable_plugin_id() {
        let registry = PluginMetricRegistry::new();
        assert_eq!(
            registry.record(observation("zeta", 1)),
            PluginMetricRecordOutcome::Recorded
        );
        assert_eq!(
            registry.record(observation("alpha", 1)),
            PluginMetricRecordOutcome::Recorded
        );

        let snapshot = registry.snapshot();
        let ids: Vec<_> = snapshot
            .entries()
            .iter()
            .map(|entry| entry.plugin_id().as_str())
            .collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
    }

    #[test]
    fn deserialization_enforces_snapshot_bounds_and_normalizes_order() {
        let valid = serde_json::json!({
            "entries": [serialized_entry("zeta"), serialized_entry("alpha")],
            "overflowed_observations": 3
        });
        let snapshot: PluginMetricsSnapshot =
            serde_json::from_value(valid).expect("valid bounded snapshot");
        let ids: Vec<_> = snapshot
            .entries()
            .iter()
            .map(|entry| entry.plugin_id().as_str())
            .collect();
        assert_eq!(ids, vec!["alpha", "zeta"]);
        assert_eq!(snapshot.overflowed_observations(), 3);

        let duplicate = serde_json::json!({
            "entries": [serialized_entry("same"), serialized_entry("same")],
            "overflowed_observations": 0
        });
        assert!(serde_json::from_value::<PluginMetricsSnapshot>(duplicate).is_err());

        let empty = serde_json::json!({
            "entries": [serialized_entry("")],
            "overflowed_observations": 0
        });
        assert!(serde_json::from_value::<PluginMetricsSnapshot>(empty).is_err());

        let oversized_id = "x".repeat(PLUGIN_METRIC_ID_MAX_BYTES + 1);
        let oversized_label = serde_json::json!({
            "entries": [serialized_entry(&oversized_id)],
            "overflowed_observations": 0
        });
        assert!(serde_json::from_value::<PluginMetricsSnapshot>(oversized_label).is_err());

        let entries: Vec<_> = (0..=PLUGIN_METRIC_CAPACITY)
            .map(|index| serialized_entry(&format!("plugin-{index:03}")))
            .collect();
        let oversized_table = serde_json::json!({
            "entries": entries,
            "overflowed_observations": 0
        });
        assert!(serde_json::from_value::<PluginMetricsSnapshot>(oversized_table).is_err());

        let mut inconsistent = serialized_entry("inconsistent");
        inconsistent["panic_count"] = serde_json::json!(2);
        let inconsistent = serde_json::json!({
            "entries": [inconsistent],
            "overflowed_observations": 0
        });
        assert!(serde_json::from_value::<PluginMetricsSnapshot>(inconsistent).is_err());

        let inactive_placeholder = serde_json::json!({
            "entries": [{
                "plugin_id": "inactive",
                "invocation_count": 0,
                "invocation_time_us_total": 0,
                "invocation_time_us_last": 0,
                "invocation_time_us_max": 0,
                "invocation_time_us_avg": 0.0,
                "over_budget_count": 0,
                "panic_count": 0,
                "hung": false,
                "host_call_error_count": 0
            }],
            "overflowed_observations": 0
        });
        assert!(serde_json::from_value::<PluginMetricsSnapshot>(inactive_placeholder).is_err());

        let untracked_with_capacity = serde_json::json!({
            "entries": [],
            "overflowed_observations": 0,
            "untracked_hung_plugins": 1
        });
        assert!(serde_json::from_value::<PluginMetricsSnapshot>(untracked_with_capacity).is_err());

        let tracked_hung: Vec<_> = (0..PLUGIN_METRIC_CAPACITY)
            .map(|index| {
                let mut entry = serialized_entry(&format!("hung-{index:03}"));
                entry["hung"] = serde_json::json!(true);
                entry
            })
            .collect();
        let too_many_hung = serde_json::json!({
            "entries": tracked_hung,
            "overflowed_observations": 0,
            "untracked_hung_plugins":
                ACTIVE_CALLBACK_CAPACITY - PLUGIN_METRIC_CAPACITY + 1
        });
        assert!(serde_json::from_value::<PluginMetricsSnapshot>(too_many_hung).is_err());
    }
}
