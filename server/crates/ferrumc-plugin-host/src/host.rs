//! The plugin registry: registration, lifecycle, and panic-isolated dispatch.

use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Instant;

use ferrumc_command::CommandTree;
use ferrumc_core::{PluginId, TextComponent};
use ferrumc_plugin_api::{
    BlockBreakAttempt, BlockPlaceAttempt, Capability, CapabilityManifest, CommandRegistrar,
    CommandSink, EventContext, EventKind, EventRegistrar, PermissionApi, Plugin,
    PluginBlockDecision, PluginEvent, PluginMetadata, SetupContext, TeardownContext, WorldIntent,
    WorldView, MAX_EMITTED_INTENTS,
};

use crate::budget::CallBudget;
use crate::error::HostError;
use crate::state::{DisableReason, PluginState, PluginStats};
use crate::storage::{InMemoryPluginStorage, NamespacedStorage, PluginStorageBackend};

/// Default maximum number of registered plugins.
const DEFAULT_MAX_PLUGINS: usize = 256;

/// Tunable host policy.
///
/// Construct with [`HostConfig::new`] (or [`HostConfig::default`]) and the
/// `with_*` builder methods. Fields are private; the host reads them through
/// accessors.
#[derive(Debug, Clone, Copy)]
pub struct HostConfig {
    max_plugins: usize,
    call_budget: CallBudget,
    disable_on_overrun: bool,
}

impl HostConfig {
    /// Returns the default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the maximum number of registered plugins.
    #[must_use]
    pub const fn with_max_plugins(mut self, max: usize) -> Self {
        self.max_plugins = max;
        self
    }

    /// Sets the per-call time budget.
    #[must_use]
    pub const fn with_call_budget(mut self, budget: CallBudget) -> Self {
        self.call_budget = budget;
        self
    }

    /// Sets whether a plugin that overruns its budget on an event is disabled.
    #[must_use]
    pub const fn with_disable_on_overrun(mut self, disable: bool) -> Self {
        self.disable_on_overrun = disable;
        self
    }

    /// Returns the configured maximum number of plugins.
    pub const fn max_plugins(self) -> usize {
        self.max_plugins
    }

    /// Returns the configured per-call budget.
    pub const fn call_budget(self) -> CallBudget {
        self.call_budget
    }

    /// Returns whether budget overruns disable the plugin.
    pub const fn disable_on_overrun(self) -> bool {
        self.disable_on_overrun
    }
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            max_plugins: DEFAULT_MAX_PLUGINS,
            call_budget: CallBudget::default(),
            disable_on_overrun: false,
        }
    }
}

/// A registered plugin and the host's bookkeeping for it.
struct PluginSlot {
    id: PluginId,
    plugin: Box<dyn Plugin>,
    metadata: PluginMetadata,
    capabilities: CapabilityManifest,
    state: PluginState,
    subscriptions: BTreeSet<EventKind>,
    stats: PluginStats,
}

/// A summary of one [`PluginHost::dispatch_event`] call.
///
/// Reports how many enabled, subscribed plugins received the event, plus the
/// ids of any that panicked or overran their budget during it. Fields are
/// private; read them through the accessors.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DispatchReport {
    delivered: usize,
    panicked: Vec<PluginId>,
    budget_exceeded: Vec<PluginId>,
}

impl DispatchReport {
    /// Returns how many plugins successfully received the event.
    pub const fn delivered(&self) -> usize {
        self.delivered
    }

    /// Returns the ids of plugins that panicked (and were disabled).
    pub fn panicked(&self) -> &[PluginId] {
        &self.panicked
    }

    /// Returns the ids of plugins that exceeded their per-call budget.
    pub fn budget_exceeded(&self) -> &[PluginId] {
        &self.budget_exceeded
    }
}

/// A per-plugin tally of block-edit decisions, for observability.
///
/// One row per registered plugin: its display name plus the cumulative count of
/// edits it allowed, vetoed, rewrote, or panicked on across its lifetime. Read
/// through [`PluginHost::plugin_decision_reports`]. Fields are public because this
/// is an inert reporting DTO carrying no invariants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginDecisionReport {
    /// The plugin's display name.
    pub name: String,
    /// Edits the plugin allowed (or emitted intents alongside).
    pub allow: u64,
    /// Edits the plugin vetoed.
    pub deny: u64,
    /// Edits the plugin rewrote.
    pub replace: u64,
    /// Times the plugin panicked in a decision hook (each fails safe to a deny).
    pub panic: u64,
}

/// The combined verdict on a block edit after every consulted plugin has voted.
///
/// Produced by folding each plugin's [`PluginBlockDecision`] (see
/// [`PluginHost::dispatch_block_place_decision`] /
/// [`PluginHost::dispatch_block_break_decision`]). UNSTABLE / dev-only: part of
/// the in-development block-decision surface.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ResolvedDecision {
    /// No plugin objected: apply the edit as the player requested.
    Allow,
    /// At least one plugin denied the edit; nothing is mutated.
    Deny {
        /// The first denial message, if any, to show the acting player.
        message: Option<TextComponent>,
    },
    /// Apply the edit with a replacement block-state instead of the player's.
    Replace {
        /// The replacement registry block-state id.
        block_state_id: u32,
    },
}

/// The full result of a `before_*` block-decision dispatch: the combined
/// [`ResolvedDecision`], any [`WorldIntent`]s plugins asked to emit, and a
/// [`DispatchReport`] of which plugins panicked or overran their budget.
///
/// UNSTABLE / dev-only.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedBlockDecision {
    decision: ResolvedDecision,
    emitted: Vec<WorldIntent>,
    report: DispatchReport,
}

impl ResolvedBlockDecision {
    /// Returns the combined decision.
    pub const fn decision(&self) -> &ResolvedDecision {
        &self.decision
    }

    /// Returns the world-mutation intents plugins emitted (capped at
    /// [`MAX_EMITTED_INTENTS`]).
    pub fn emitted(&self) -> &[WorldIntent] {
        &self.emitted
    }

    /// Returns the dispatch report (panicked / budget-exceeded plugins).
    pub const fn report(&self) -> &DispatchReport {
        &self.report
    }

    /// Returns whether the edit was denied.
    pub const fn is_deny(&self) -> bool {
        matches!(self.decision, ResolvedDecision::Deny { .. })
    }

    /// Consumes the result, yielding the decision and emitted intents.
    pub fn into_parts(self) -> (ResolvedDecision, Vec<WorldIntent>) {
        (self.decision, self.emitted)
    }
}

/// An in-process plugin host: a registry that owns plugins and drives their
/// lifecycle with panic isolation and a per-call time budget.
///
/// The host loads [`Plugin`] trait objects directly (in-process; dynamic-library
/// loading is a later milestone), grants each a [`CapabilityManifest`], enables
/// them, and dispatches events to subscribers. Every call into plugin code is
/// wrapped so a panicking plugin is caught and disabled rather than crashing the
/// host. Plugin-registered commands are aggregated into a single
/// [`CommandTree`].
pub struct PluginHost {
    plugins: Vec<PluginSlot>,
    storage: Box<dyn PluginStorageBackend>,
    command_tree: CommandTree,
    config: HostConfig,
}

impl PluginHost {
    /// Creates a host backed by `storage` with default configuration.
    pub fn new(storage: Box<dyn PluginStorageBackend>) -> Self {
        Self::with_config(storage, HostConfig::default())
    }

    /// Creates a host backed by `storage` with the given `config`.
    pub fn with_config(storage: Box<dyn PluginStorageBackend>, config: HostConfig) -> Self {
        Self {
            plugins: Vec::new(),
            storage,
            command_tree: CommandTree::new(),
            config,
        }
    }

    /// Creates a host backed by a fresh [`InMemoryPluginStorage`].
    ///
    /// Convenience for tests and simple setups that do not need a custom
    /// storage backend.
    pub fn in_memory() -> Self {
        Self::new(Box::new(InMemoryPluginStorage::new()))
    }

    /// Registers `plugin`, granting it exactly the capabilities its metadata
    /// requests.
    ///
    /// Returns the plugin's id on success, or [`HostError::DuplicateId`] /
    /// [`HostError::CapacityExceeded`] / [`HostError::Panicked`] (if reading
    /// metadata panics).
    pub fn register(&mut self, plugin: Box<dyn Plugin>) -> Result<PluginId, HostError> {
        self.register_inner(plugin, None)
    }

    /// Registers `plugin`, granting it exactly `granted` capabilities regardless
    /// of what its metadata requests.
    ///
    /// Used to run a plugin with a restricted capability set.
    pub fn register_with_grants(
        &mut self,
        plugin: Box<dyn Plugin>,
        granted: CapabilityManifest,
    ) -> Result<PluginId, HostError> {
        self.register_inner(plugin, Some(granted))
    }

    fn register_inner(
        &mut self,
        plugin: Box<dyn Plugin>,
        granted: Option<CapabilityManifest>,
    ) -> Result<PluginId, HostError> {
        if self.plugins.len() >= self.config.max_plugins {
            return Err(HostError::CapacityExceeded {
                max: self.config.max_plugins,
            });
        }

        // Reading metadata calls into plugin code, so isolate it too. If it
        // panics we have no real id (reading it is what failed), so report the
        // panic against a placeholder id.
        let Ok(metadata) = catch_unwind(AssertUnwindSafe(|| plugin.metadata())) else {
            return Err(HostError::Panicked {
                id: PluginId::new("<unknown>"),
            });
        };

        let id = metadata.id().clone();
        if self.plugins.iter().any(|slot| slot.id == id) {
            return Err(HostError::DuplicateId(id));
        }

        let capabilities = granted.unwrap_or_else(|| metadata.requested_capabilities());
        self.plugins.push(PluginSlot {
            id: id.clone(),
            plugin,
            metadata,
            capabilities,
            state: PluginState::Registered,
            subscriptions: BTreeSet::new(),
            stats: PluginStats::default(),
        });
        Ok(id)
    }

    /// Enables the plugin with id `id`, running its
    /// [`on_enable`](Plugin::on_enable) hook.
    ///
    /// On success the plugin's subscriptions are recorded and its registered
    /// commands are merged into the host's command tree. If the hook returns an
    /// error the plugin is left disabled and [`HostError::PluginFailed`] is
    /// returned; if it panics the plugin is disabled and [`HostError::Panicked`]
    /// is returned. Either way the host keeps running.
    pub fn enable(&mut self, id: &PluginId) -> Result<(), HostError> {
        let Self {
            plugins,
            storage,
            command_tree,
            config,
        } = self;

        let slot = plugins
            .iter_mut()
            .find(|slot| &slot.id == id)
            .ok_or_else(|| HostError::UnknownPlugin(id.clone()))?;

        if slot.state.is_enabled() {
            return Err(HostError::AlreadyEnabled(id.clone()));
        }

        let mut events = EventRegistrar::new();
        let mut commands = CommandRegistrar::new();
        let namespaced = NamespacedStorage::new(storage.as_ref(), slot.id.clone());
        let mut ctx = SetupContext::new(slot.capabilities, &mut events, &mut commands, &namespaced);

        let start = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| slot.plugin.on_enable(&mut ctx)));
        let elapsed = start.elapsed();

        match outcome {
            Ok(Ok(())) => {
                slot.subscriptions = events.subscriptions().collect();
                slot.state = PluginState::Enabled;
                for command in commands.into_commands() {
                    command_tree.register(command);
                }
                if config.call_budget().is_exceeded(elapsed) {
                    slot.stats.budget_overruns += 1;
                    tracing::warn!(
                        plugin = %slot.id,
                        ?elapsed,
                        "plugin exceeded its time budget during on_enable"
                    );
                }
                Ok(())
            }
            Ok(Err(source)) => {
                slot.state = PluginState::Disabled(DisableReason::EnableFailed);
                Err(HostError::PluginFailed {
                    id: id.clone(),
                    source,
                })
            }
            Err(_) => {
                slot.stats.panics += 1;
                slot.state = PluginState::Disabled(DisableReason::Panicked);
                tracing::warn!(plugin = %slot.id, "plugin panicked during on_enable; disabled");
                Err(HostError::Panicked { id: id.clone() })
            }
        }
    }

    /// Disables the enabled plugin with id `id`, running its
    /// [`on_disable`](Plugin::on_disable) hook.
    ///
    /// A panic in the hook is caught and ignored (the plugin is being disabled
    /// regardless). Returns [`HostError::NotEnabled`] if the plugin is not
    /// currently enabled.
    pub fn disable(&mut self, id: &PluginId) -> Result<(), HostError> {
        let Self {
            plugins, storage, ..
        } = self;

        let slot = plugins
            .iter_mut()
            .find(|slot| &slot.id == id)
            .ok_or_else(|| HostError::UnknownPlugin(id.clone()))?;

        if !slot.state.is_enabled() {
            return Err(HostError::NotEnabled(id.clone()));
        }

        let namespaced = NamespacedStorage::new(storage.as_ref(), slot.id.clone());
        let mut ctx = TeardownContext::new(slot.capabilities, &namespaced);
        if catch_unwind(AssertUnwindSafe(|| slot.plugin.on_disable(&mut ctx))).is_err() {
            slot.stats.panics += 1;
            tracing::warn!(plugin = %slot.id, "plugin panicked during on_disable; ignored");
        }
        slot.state = PluginState::Disabled(DisableReason::Manual);
        Ok(())
    }

    /// Dispatches `event` to every enabled plugin subscribed to its kind, with
    /// panic isolation and per-call budgeting.
    ///
    /// The world, sink, and permission facades are injected by the caller (the
    /// simulation layer); the host supplies each plugin its own namespaced
    /// storage. A plugin that panics is disabled and recorded in the returned
    /// [`DispatchReport`]; the host (and the remaining plugins) keep running. A
    /// plugin that overruns the call budget is recorded, and disabled too if
    /// [`HostConfig::disable_on_overrun`] is set.
    pub fn dispatch_event(
        &mut self,
        event: &PluginEvent,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> DispatchReport {
        let kind = event.kind();
        let Self {
            plugins,
            storage,
            config,
            ..
        } = self;
        let budget = config.call_budget();
        let disable_on_overrun = config.disable_on_overrun();

        let mut report = DispatchReport::default();
        for slot in plugins.iter_mut() {
            if !slot.state.is_enabled() || !slot.subscriptions.contains(&kind) {
                continue;
            }

            let namespaced = NamespacedStorage::new(storage.as_ref(), slot.id.clone());
            let mut ctx = EventContext::new(
                slot.capabilities,
                world,
                &mut *sink,
                permissions,
                &namespaced,
            );

            let start = Instant::now();
            let outcome = catch_unwind(AssertUnwindSafe(|| slot.plugin.on_event(event, &mut ctx)));
            let elapsed = start.elapsed();

            if outcome.is_ok() {
                report.delivered += 1;
                if budget.is_exceeded(elapsed) {
                    slot.stats.budget_overruns += 1;
                    report.budget_exceeded.push(slot.id.clone());
                    tracing::warn!(
                        plugin = %slot.id,
                        ?elapsed,
                        "plugin exceeded its time budget during on_event"
                    );
                    if disable_on_overrun {
                        slot.state = PluginState::Disabled(DisableReason::BudgetExceeded);
                    }
                }
            } else {
                slot.stats.panics += 1;
                slot.state = PluginState::Disabled(DisableReason::Panicked);
                report.panicked.push(slot.id.clone());
                tracing::warn!(plugin = %slot.id, "plugin panicked during on_event; disabled");
            }
        }
        report
    }

    /// Consults every enabled, [`VetoBlockEdits`](Capability::VetoBlockEdits)-capable
    /// plugin about a pending block *placement* and folds their decisions into one
    /// [`ResolvedBlockDecision`].
    ///
    /// See [`PluginHost::dispatch_block_break_decision`] for the shared semantics
    /// (precedence, isolation, fail-safe); this is the placement-side entry point.
    pub fn dispatch_block_place_decision(
        &mut self,
        attempt: &BlockPlaceAttempt,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> ResolvedBlockDecision {
        self.fold_before(world, sink, permissions, |plugin, ctx| {
            plugin.before_block_place(attempt, ctx)
        })
    }

    /// Consults every enabled, [`VetoBlockEdits`](Capability::VetoBlockEdits)-capable
    /// plugin about a pending block *break* and folds their decisions into one
    /// [`ResolvedBlockDecision`].
    ///
    /// # Precedence (deterministic, fail-safe)
    ///
    /// Plugins are consulted in registration order:
    ///
    /// 1. The first [`Deny`](PluginBlockDecision::Deny) is *absorbing*: it wins,
    ///    the remaining plugins are skipped, and its message (the first non-`None`)
    ///    is carried. A security action beats a convenience rewrite.
    /// 2. Otherwise decisions fold: a [`Replace`](PluginBlockDecision::Replace) is
    ///    last-writer-wins, and [`EmitIntents`](PluginBlockDecision::EmitIntents)
    ///    vectors concatenate (capped at [`MAX_EMITTED_INTENTS`]).
    /// 3. If nobody objects, the result is [`Allow`](ResolvedDecision::Allow).
    ///
    /// # Isolation and fail-safe
    ///
    /// Each hook is wrapped in [`catch_unwind`], exactly like
    /// [`dispatch_event`](Self::dispatch_event). A plugin that *panics* is disabled
    /// ([`DisableReason::Panicked`]), counted in [`PluginStats`], logged, and — the
    /// fail-safe — treated as a [`Deny`](ResolvedDecision::Deny) so a broken plugin
    /// can never silently let a destructive edit through. Per-call budgets apply as
    /// for event dispatch. None of this runs inside the simulation tick, so the
    /// server and tick are unaffected.
    pub fn dispatch_block_break_decision(
        &mut self,
        attempt: &BlockBreakAttempt,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> ResolvedBlockDecision {
        self.fold_before(world, sink, permissions, |plugin, ctx| {
            plugin.before_block_break(attempt, ctx)
        })
    }

    /// The shared fold driving both `before_block_*` decision paths.
    ///
    /// `call` invokes the appropriate hook on one plugin; everything else (the
    /// capability gate, panic isolation, budgeting, and the precedence fold) is
    /// identical for placement and break.
    fn fold_before<F>(
        &mut self,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
        mut call: F,
    ) -> ResolvedBlockDecision
    where
        F: FnMut(&mut dyn Plugin, &mut EventContext<'_>) -> PluginBlockDecision,
    {
        let Self {
            plugins,
            storage,
            config,
            ..
        } = self;
        let budget = config.call_budget();
        let disable_on_overrun = config.disable_on_overrun();

        let mut decision = ResolvedDecision::Allow;
        let mut emitted: Vec<WorldIntent> = Vec::new();
        let mut report = DispatchReport::default();

        for slot in plugins.iter_mut() {
            // A Deny is absorbing: once denied, stop consulting plugins.
            if matches!(decision, ResolvedDecision::Deny { .. }) {
                break;
            }
            if !slot.state.is_enabled() || !slot.capabilities.grants(Capability::VetoBlockEdits) {
                continue;
            }

            let namespaced = NamespacedStorage::new(storage.as_ref(), slot.id.clone());
            let mut ctx = EventContext::new(
                slot.capabilities,
                world,
                &mut *sink,
                permissions,
                &namespaced,
            );

            let start = Instant::now();
            let outcome = catch_unwind(AssertUnwindSafe(|| call(&mut *slot.plugin, &mut ctx)));
            let elapsed = start.elapsed();

            let Ok(plugin_decision) = outcome else {
                // Fail-safe: a panicking plugin denies the edit and is disabled.
                slot.stats.panics += 1;
                slot.state = PluginState::Disabled(DisableReason::Panicked);
                report.panicked.push(slot.id.clone());
                tracing::warn!(
                    plugin = %slot.id,
                    "plugin panicked during a before_block_* decision; disabled and treated as Deny"
                );
                decision = ResolvedDecision::Deny { message: None };
                continue;
            };

            report.delivered += 1;
            if budget.is_exceeded(elapsed) {
                slot.stats.budget_overruns += 1;
                report.budget_exceeded.push(slot.id.clone());
                tracing::warn!(
                    plugin = %slot.id,
                    ?elapsed,
                    "plugin exceeded its time budget during a before_block_* decision"
                );
                if disable_on_overrun {
                    slot.state = PluginState::Disabled(DisableReason::BudgetExceeded);
                }
            }

            match plugin_decision {
                PluginBlockDecision::Deny { message } => {
                    slot.stats.deny = slot.stats.deny.saturating_add(1);
                    decision = ResolvedDecision::Deny { message };
                }
                PluginBlockDecision::Replace { block_state_id } => {
                    slot.stats.replace = slot.stats.replace.saturating_add(1);
                    decision = ResolvedDecision::Replace { block_state_id };
                }
                PluginBlockDecision::EmitIntents(intents) => {
                    // Emitting intents lets the original edit proceed, so it counts
                    // as an allow for the per-plugin decision tally.
                    slot.stats.allow = slot.stats.allow.saturating_add(1);
                    for intent in intents {
                        if emitted.len() >= MAX_EMITTED_INTENTS {
                            tracing::warn!(
                                plugin = %slot.id,
                                cap = MAX_EMITTED_INTENTS,
                                "plugin exceeded the emitted-intent cap; dropping the rest"
                            );
                            break;
                        }
                        emitted.push(intent);
                    }
                }
                // `PluginBlockDecision::Allow` (no-op) and — since the enum is
                // `#[non_exhaustive]` — any unknown future variant leave the fold
                // unchanged, and count as an allow (the edit was not vetoed).
                _ => {
                    slot.stats.allow = slot.stats.allow.saturating_add(1);
                }
            }
        }

        // A resolved Deny prevents the edit entirely. Any intents an earlier plugin
        // emitted via `EmitIntents` were predicated on the edit proceeding (the
        // variant means "proceed with the original edit and *additionally* submit
        // these"), so a later, absorbing Deny must drop them — otherwise a denied
        // edit would still execute another plugin's `SetBlock` and mutate the world,
        // breaking the boundary's "a Deny truly prevents the mutation" guarantee.
        if matches!(decision, ResolvedDecision::Deny { .. }) {
            emitted.clear();
        }

        ResolvedBlockDecision {
            decision,
            emitted,
            report,
        }
    }

    /// Returns the number of registered plugins.
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Returns whether no plugins are registered.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Returns the lifecycle state of the plugin with id `id`, if registered.
    pub fn state(&self, id: &PluginId) -> Option<PluginState> {
        self.slot(id).map(|slot| slot.state)
    }

    /// Returns whether the plugin with id `id` is registered and enabled.
    pub fn is_enabled(&self, id: &PluginId) -> bool {
        self.state(id).is_some_and(PluginState::is_enabled)
    }

    /// Returns the capabilities granted to the plugin with id `id`.
    pub fn capabilities(&self, id: &PluginId) -> Option<CapabilityManifest> {
        self.slot(id).map(|slot| slot.capabilities)
    }

    /// Returns the metadata of the plugin with id `id`, if registered.
    pub fn metadata(&self, id: &PluginId) -> Option<&PluginMetadata> {
        self.slot(id).map(|slot| &slot.metadata)
    }

    /// Returns the accumulated statistics for the plugin with id `id`.
    pub fn stats(&self, id: &PluginId) -> Option<PluginStats> {
        self.slot(id).map(|slot| slot.stats)
    }

    /// Returns a per-plugin block-edit decision report for every registered
    /// plugin, in registration order.
    ///
    /// A cheap, bounded read (one row per plugin, capped at
    /// [`HostConfig::max_plugins`]) the host or its embedder can fold into a
    /// metrics snapshot each tick without touching the decision hot path.
    pub fn plugin_decision_reports(&self) -> Vec<PluginDecisionReport> {
        self.plugins
            .iter()
            .map(|slot| PluginDecisionReport {
                name: slot.metadata.name().to_string(),
                allow: slot.stats.allow(),
                deny: slot.stats.deny(),
                replace: slot.stats.replace(),
                panic: u64::from(slot.stats.panics()),
            })
            .collect()
    }

    /// Returns whether the plugin with id `id` is subscribed to `kind`.
    pub fn is_subscribed(&self, id: &PluginId, kind: EventKind) -> bool {
        self.slot(id)
            .is_some_and(|slot| slot.subscriptions.contains(&kind))
    }

    /// Returns the aggregated command tree of all enabled plugins' commands.
    pub const fn command_tree(&self) -> &CommandTree {
        &self.command_tree
    }

    /// Returns the host configuration.
    pub const fn config(&self) -> HostConfig {
        self.config
    }

    fn slot(&self, id: &PluginId) -> Option<&PluginSlot> {
        self.plugins.iter().find(|slot| &slot.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_plugin_api::Version;

    /// A trivial plugin used to exercise registry bookkeeping.
    struct NoopPlugin {
        id: &'static str,
    }

    impl Plugin for NoopPlugin {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata::new(
                PluginId::new(self.id),
                self.id,
                Version::new(0, 1, 0),
                CapabilityManifest::empty(),
            )
        }
    }

    #[test]
    fn register_rejects_duplicate_ids() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(Box::new(NoopPlugin { id: "dup" }))
            .expect("first registers");
        assert_eq!(id, PluginId::new("dup"));
        let err = host
            .register(Box::new(NoopPlugin { id: "dup" }))
            .expect_err("second is a duplicate");
        assert_eq!(err, HostError::DuplicateId(PluginId::new("dup")));
        assert_eq!(host.len(), 1);
    }

    #[test]
    fn register_respects_capacity() {
        let config = HostConfig::new().with_max_plugins(1);
        let mut host = PluginHost::with_config(Box::new(InMemoryPluginStorage::new()), config);
        host.register(Box::new(NoopPlugin { id: "a" }))
            .expect("first fits");
        let err = host
            .register(Box::new(NoopPlugin { id: "b" }))
            .expect_err("second overflows");
        assert_eq!(err, HostError::CapacityExceeded { max: 1 });
    }

    #[test]
    fn enable_then_disable_tracks_state() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(Box::new(NoopPlugin { id: "p" }))
            .expect("registers");
        assert_eq!(host.state(&id), Some(PluginState::Registered));

        host.enable(&id).expect("enables");
        assert!(host.is_enabled(&id));
        assert_eq!(
            host.enable(&id),
            Err(HostError::AlreadyEnabled(id.clone())),
            "double enable is rejected"
        );

        host.disable(&id).expect("disables");
        assert_eq!(
            host.state(&id),
            Some(PluginState::Disabled(DisableReason::Manual))
        );
        assert_eq!(host.disable(&id), Err(HostError::NotEnabled(id)));
    }

    #[test]
    fn operations_on_unknown_plugin_fail() {
        let mut host = PluginHost::in_memory();
        let missing = PluginId::new("ghost");
        assert_eq!(
            host.enable(&missing),
            Err(HostError::UnknownPlugin(missing.clone()))
        );
        assert_eq!(
            host.disable(&missing),
            Err(HostError::UnknownPlugin(missing.clone()))
        );
        assert_eq!(host.state(&missing), None);
    }

    // --- Block-decision surface ----------------------------------------------

    use ferrumc_core::{DimensionId, PlayerId, TextComponent};
    use ferrumc_math::{BlockPos, ChunkPos, Vec3};
    use ferrumc_permission::{PermissionNode, Resolution};
    use ferrumc_plugin_api::{
        BlockBreakAttempt, BlockPlaceAttempt, IntentError, PluginBlockDecision, PluginError,
        WorldIntent,
    };

    /// A plugin that returns a fixed decision from both `before_*` hooks.
    struct DecisionPlugin {
        id: &'static str,
        decision: PluginBlockDecision,
    }

    impl DecisionPlugin {
        fn boxed(id: &'static str, decision: PluginBlockDecision) -> Box<dyn Plugin> {
            Box::new(Self { id, decision })
        }
    }

    impl Plugin for DecisionPlugin {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata::new(
                PluginId::new(self.id),
                self.id,
                Version::new(0, 1, 0),
                CapabilityManifest::empty().with(Capability::VetoBlockEdits),
            )
        }
        fn before_block_place(
            &mut self,
            _ev: &BlockPlaceAttempt,
            _ctx: &mut EventContext<'_>,
        ) -> PluginBlockDecision {
            self.decision.clone()
        }
        fn before_block_break(
            &mut self,
            _ev: &BlockBreakAttempt,
            _ctx: &mut EventContext<'_>,
        ) -> PluginBlockDecision {
            self.decision.clone()
        }
    }

    /// A plugin that panics inside its decision hooks.
    struct PanicDecisionPlugin {
        id: &'static str,
    }

    impl Plugin for PanicDecisionPlugin {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata::new(
                PluginId::new(self.id),
                self.id,
                Version::new(0, 1, 0),
                CapabilityManifest::empty().with(Capability::VetoBlockEdits),
            )
        }
        fn before_block_place(
            &mut self,
            _ev: &BlockPlaceAttempt,
            _ctx: &mut EventContext<'_>,
        ) -> PluginBlockDecision {
            panic!("boom in before_block_place");
        }
        fn before_block_break(
            &mut self,
            _ev: &BlockBreakAttempt,
            _ctx: &mut EventContext<'_>,
        ) -> PluginBlockDecision {
            panic!("boom in before_block_break");
        }
    }

    /// A plugin with no `VetoBlockEdits` capability; it must never be consulted.
    struct PassivePlugin {
        id: &'static str,
    }

    impl Plugin for PassivePlugin {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata::new(
                PluginId::new(self.id),
                self.id,
                Version::new(0, 1, 0),
                CapabilityManifest::empty().with(Capability::ReceiveEvents),
            )
        }
        fn before_block_place(
            &mut self,
            _ev: &BlockPlaceAttempt,
            _ctx: &mut EventContext<'_>,
        ) -> PluginBlockDecision {
            panic!("a passive plugin must never have its decision hook called");
        }
    }

    /// A recording plugin used to prove an after-* notification is delivered.
    struct AfterRecorder {
        id: &'static str,
    }

    impl Plugin for AfterRecorder {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata::new(
                PluginId::new(self.id),
                self.id,
                Version::new(0, 1, 0),
                CapabilityManifest::empty()
                    .with(Capability::ReceiveEvents)
                    .with(Capability::SubmitIntents),
            )
        }
        fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
            ctx.events()?.subscribe(EventKind::AfterBlockPlace);
            Ok(())
        }
        fn on_event(&mut self, event: &PluginEvent, ctx: &mut EventContext<'_>) {
            if let PluginEvent::AfterBlockPlace { player, .. } = event {
                if let Ok(sink) = ctx.sink() {
                    let _ = sink.submit(WorldIntent::Message {
                        player: *player,
                        message: TextComponent::text("placed"),
                    });
                }
            }
        }
    }

    struct NullWorld;
    impl WorldView for NullWorld {
        fn dimension(&self) -> DimensionId {
            DimensionId::new(0)
        }
        fn is_chunk_loaded(&self, _chunk: ChunkPos) -> bool {
            false
        }
        fn block_state_id(&self, _pos: BlockPos) -> Option<u32> {
            None
        }
        fn player_position(&self, _player: PlayerId) -> Option<Vec3> {
            None
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        intents: Vec<WorldIntent>,
    }
    impl CommandSink for RecordingSink {
        fn submit(&mut self, intent: WorldIntent) -> Result<(), IntentError> {
            self.intents.push(intent);
            Ok(())
        }
    }

    struct NullPermissions;
    impl PermissionApi for NullPermissions {
        fn has_permission(&self, _player: PlayerId, _node: &PermissionNode) -> bool {
            false
        }
        fn resolve(&self, _player: PlayerId, _node: &PermissionNode) -> Resolution {
            Resolution::Unset
        }
    }

    fn enable_all(host: &mut PluginHost, ids: &[&PluginId]) {
        for id in ids {
            host.enable(id).expect("enables");
        }
    }

    fn break_attempt() -> BlockBreakAttempt {
        BlockBreakAttempt::new(PlayerId::offline("Steve"), BlockPos::new(0, 64, 0))
    }

    fn place_attempt(state: u32) -> BlockPlaceAttempt {
        BlockPlaceAttempt::new(PlayerId::offline("Steve"), BlockPos::new(0, 64, 0), state)
    }

    #[test]
    fn deny_stops_the_edit() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(DecisionPlugin::boxed(
                "denier",
                PluginBlockDecision::Deny {
                    message: Some(TextComponent::text("no")),
                },
            ))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_block_break_decision(
            &break_attempt(),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert_eq!(
            resolved.decision(),
            &ResolvedDecision::Deny {
                message: Some(TextComponent::text("no")),
            }
        );
        assert!(resolved.is_deny());
    }

    #[test]
    fn replace_swaps_the_block_state() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(DecisionPlugin::boxed(
                "replacer",
                PluginBlockDecision::Replace { block_state_id: 42 },
            ))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_block_place_decision(
            &place_attempt(1),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert_eq!(
            resolved.decision(),
            &ResolvedDecision::Replace { block_state_id: 42 }
        );
    }

    #[test]
    fn deny_is_absorbing_and_short_circuits_a_later_replace() {
        let mut host = PluginHost::in_memory();
        // Registration order: denier first, then a replacer that must never run.
        let denier = host
            .register(DecisionPlugin::boxed(
                "denier",
                PluginBlockDecision::Deny {
                    message: Some(TextComponent::text("blocked")),
                },
            ))
            .expect("registers denier");
        let replacer = host
            .register(DecisionPlugin::boxed(
                "replacer",
                PluginBlockDecision::Replace { block_state_id: 7 },
            ))
            .expect("registers replacer");
        enable_all(&mut host, &[&denier, &replacer]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_block_place_decision(
            &place_attempt(1),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert!(
            resolved.is_deny(),
            "the first Deny wins over a later Replace"
        );
        // Only the denier was consulted; the replacer was short-circuited.
        assert_eq!(resolved.report().delivered(), 1);
    }

    #[test]
    fn replace_is_last_writer_wins() {
        let mut host = PluginHost::in_memory();
        let first = host
            .register(DecisionPlugin::boxed(
                "first",
                PluginBlockDecision::Replace { block_state_id: 1 },
            ))
            .expect("registers first");
        let second = host
            .register(DecisionPlugin::boxed(
                "second",
                PluginBlockDecision::Replace { block_state_id: 2 },
            ))
            .expect("registers second");
        enable_all(&mut host, &[&first, &second]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_block_place_decision(
            &place_attempt(99),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert_eq!(
            resolved.decision(),
            &ResolvedDecision::Replace { block_state_id: 2 },
            "the last Replace wins"
        );
    }

    #[test]
    fn emit_intents_concatenate_and_are_capped() {
        let mut host = PluginHost::in_memory();
        let many: Vec<WorldIntent> = (0..(MAX_EMITTED_INTENTS + 10))
            .map(|i| WorldIntent::SetBlock {
                pos: BlockPos::new(i32::try_from(i).expect("test index fits i32"), 64, 0),
                block_state_id: 1,
            })
            .collect();
        let id = host
            .register(DecisionPlugin::boxed(
                "emitter",
                PluginBlockDecision::EmitIntents(many),
            ))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_block_break_decision(
            &break_attempt(),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert_eq!(resolved.decision(), &ResolvedDecision::Allow);
        assert_eq!(
            resolved.emitted().len(),
            MAX_EMITTED_INTENTS,
            "the emitted-intent vector is capped"
        );
    }

    #[test]
    fn deny_after_an_earlier_emit_drops_the_emitted_intents() {
        let mut host = PluginHost::in_memory();
        // Registration order: an emitter first, then a denier. The absorbing Deny
        // wins, and the earlier plugin's emitted intents must not survive it — else
        // a denied edit could still mutate the world via the emitted `SetBlock`.
        let emitter = host
            .register(DecisionPlugin::boxed(
                "emitter",
                PluginBlockDecision::EmitIntents(vec![WorldIntent::SetBlock {
                    pos: BlockPos::new(0, 64, 0),
                    block_state_id: 1,
                }]),
            ))
            .expect("registers emitter");
        let denier = host
            .register(DecisionPlugin::boxed(
                "denier",
                PluginBlockDecision::Deny { message: None },
            ))
            .expect("registers denier");
        enable_all(&mut host, &[&emitter, &denier]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_block_break_decision(
            &break_attempt(),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert!(resolved.is_deny(), "the later Deny wins");
        assert!(
            resolved.emitted().is_empty(),
            "a Deny drops earlier plugins' emitted intents"
        );
    }

    #[test]
    fn panicking_plugin_is_contained_and_fails_safe_to_deny() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(Box::new(PanicDecisionPlugin { id: "boom" }))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_block_break_decision(
            &break_attempt(),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        // The host stays up, the plugin is disabled, and the edit fails safe.
        assert_eq!(
            resolved.decision(),
            &ResolvedDecision::Deny { message: None }
        );
        assert_eq!(resolved.report().panicked(), std::slice::from_ref(&id));
        assert_eq!(
            host.state(&id),
            Some(PluginState::Disabled(DisableReason::Panicked))
        );
        assert_eq!(host.stats(&id).map(PluginStats::panics), Some(1));
    }

    #[test]
    fn plugin_without_capability_is_not_consulted() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(Box::new(PassivePlugin { id: "passive" }))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        // PassivePlugin panics if consulted; an Allow result proves it was skipped.
        let resolved = host.dispatch_block_place_decision(
            &place_attempt(1),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert_eq!(resolved.decision(), &ResolvedDecision::Allow);
        assert_eq!(resolved.report().delivered(), 0);
    }

    #[test]
    fn decision_reports_tally_allow_deny_replace_panic_per_plugin() {
        let mut host = PluginHost::in_memory();
        let allower = host
            .register(DecisionPlugin::boxed("allower", PluginBlockDecision::Allow))
            .expect("registers allower");
        let replacer = host
            .register(DecisionPlugin::boxed(
                "replacer",
                PluginBlockDecision::Replace { block_state_id: 9 },
            ))
            .expect("registers replacer");
        let boom = host
            .register(Box::new(PanicDecisionPlugin { id: "boom" }))
            .expect("registers boom");
        enable_all(&mut host, &[&allower, &replacer, &boom]);

        let mut sink = RecordingSink::default();
        // Two placements. Registration order is allower, replacer, boom; only a
        // Deny is absorbing, so a Replace never short-circuits the panicking plugin.
        // Round 1: allower allows, replacer rewrites, boom panics (disabled after).
        // Round 2: boom is disabled and skipped; allower and replacer run again.
        for _ in 0..2 {
            let _ = host.dispatch_block_place_decision(
                &place_attempt(1),
                &NullWorld,
                &mut sink,
                &NullPermissions,
            );
        }

        let reports = host.plugin_decision_reports();
        let by_name: std::collections::BTreeMap<&str, &PluginDecisionReport> =
            reports.iter().map(|r| (r.name.as_str(), r)).collect();
        assert_eq!(by_name["allower"].allow, 2);
        assert_eq!(by_name["replacer"].replace, 2);
        // boom panics once then is disabled, contributing a single panic and no deny.
        assert_eq!(by_name["boom"].panic, 1);
        assert_eq!(by_name["boom"].deny, 0);
    }

    #[test]
    fn after_block_place_notification_is_delivered() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(Box::new(AfterRecorder { id: "after" }))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        let report = host.dispatch_event(
            &PluginEvent::AfterBlockPlace {
                player: PlayerId::offline("Steve"),
                pos: BlockPos::new(0, 64, 0),
                block_state_id: 1,
            },
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert_eq!(report.delivered(), 1);
        assert_eq!(sink.intents.len(), 1, "after_* fired and emitted a message");
    }
}
