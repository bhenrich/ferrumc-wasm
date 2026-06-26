//! The plugin registry: registration, lifecycle, and panic-isolated dispatch.

use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Instant;

use ferrumc_command::CommandTree;
use ferrumc_core::PluginId;
use ferrumc_plugin_api::{
    CapabilityManifest, CommandRegistrar, CommandSink, EventContext, EventKind, EventRegistrar,
    PermissionApi, Plugin, PluginEvent, PluginMetadata, SetupContext, TeardownContext, WorldView,
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
}
