//! The plugin registry: registration, lifecycle, and catch-and-disable dispatch.

use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Instant;

use ferrumc_command::CommandTree;
use ferrumc_core::{DimensionId, PluginId, TextComponent, Tick, WorldId};
use ferrumc_math::ShardPos;
use ferrumc_plugin_abi::{
    FcResourceHandle, FcStatus, FC_CAPABILITY_DENIED, FC_ERROR, FC_OK, FC_PLUGIN_PANIC,
};
use ferrumc_plugin_api::{
    BlockBreakAttempt, BlockPlaceAttempt, Capability, CapabilityManifest, ChatAttempt,
    CommandRegistrar, CommandSink, EventContext, EventKind, EventRegistrar, IntentError,
    InteractAttempt, PermissionApi, Plugin, PluginBlockDecision, PluginEvent, PluginEventDecision,
    PluginMetadata, SetupContext, TeardownContext, WorldIntent, WorldView, MAX_EMITTED_INTENTS,
};
use ferrumc_plugin_loader::{
    ActivePlugin, CallbackError, LoadedPlugin, OwnedEvent, PluginCapability as NativeCapability,
};

use crate::budget::CallBudget;
use crate::error::{HostError, NativeLifecycleHook};
use crate::native_runtime::{
    encode_block_break_attempt, encode_block_place_attempt, encode_event,
    supported_native_capabilities, NativeCallbackServices, NativeCompletion, NativeDiagnostic,
    NativeEffect, NativeEventRoute,
};
use crate::state::{DisableReason, PluginState, PluginStats};
use crate::storage::{InMemoryPluginStorage, NamespacedStorage, PluginStorageBackend};

/// Default maximum number of registered plugins.
const DEFAULT_MAX_PLUGINS: usize = 256;

/// Stable host-authored context retained when a callback reports a panic.
const NATIVE_PANIC_DIAGNOSTIC: &str = "trusted native callback returned FC_PLUGIN_PANIC; staged commands were discarded and the plugin was disabled";

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

/// A validated trusted native plugin and the host bookkeeping around its
/// reusable factory and current instance.
struct TrustedNativeSlot {
    id: PluginId,
    factory: LoadedPlugin,
    active: Option<ActivePlugin>,
    metadata: PluginMetadata,
    capabilities: CapabilityManifest,
    state: PluginState,
    subscriptions: BTreeSet<EventKind>,
    stats: PluginStats,
    next_event_resource: u64,
}

/// Stable index into one of the host's two storage cohorts.
#[derive(Clone, Copy)]
enum RegisteredPlugin {
    Compiled(usize),
    TrustedNative(usize),
}

#[derive(Clone, Copy)]
enum NativeBlockAttempt<'a> {
    Place(&'a BlockPlaceAttempt),
    Break(&'a BlockBreakAttempt),
}

impl NativeBlockAttempt<'_> {
    const fn hook(self) -> EventKind {
        match self {
            Self::Place(_) => EventKind::BlockPlace,
            Self::Break(_) => EventKind::BlockBreak,
        }
    }

    const fn route(self) -> NativeEventRoute {
        match self {
            Self::Place(_) => NativeEventRoute::BlockPlaceDecision,
            Self::Break(_) => NativeEventRoute::BlockBreakDecision,
        }
    }

    fn encode(self, context: NativeEventContext, shard_resource: FcResourceHandle) -> OwnedEvent {
        match self {
            Self::Place(attempt) => {
                encode_block_place_attempt(attempt, context.tick().get(), shard_resource)
            }
            Self::Break(attempt) => {
                encode_block_break_attempt(attempt, context.tick().get(), shard_resource)
            }
        }
    }
}

/// One capability-gated host call denied during a trusted native callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCapabilityDenial {
    plugin_id: PluginId,
    capability: Capability,
}

impl NativeCapabilityDenial {
    /// Returns the plugin whose callback made the denied request.
    pub const fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    /// Returns the undeclared capability required by the request.
    pub const fn capability(&self) -> Capability {
        self.capability
    }
}

/// A cooperatively reported trusted-native panic attributed to its plugin and hook.
///
/// This record exists only when a callback returns `FC_PLUGIN_PANIC` normally
/// through the ABI. The host then discards that callback's staged commands and
/// disables the plugin. The failed instance is retired without another plugin
/// callback because its state may be inconsistent; plugin-owned resources tied
/// to that opaque handle may therefore remain until process exit. The host
/// refuses to re-enable this registration after that terminal disposition.
///
/// This handling cannot recover from `panic=abort`,
/// `std::process::abort`, segmentation faults, undefined behavior, deadlocks,
/// foreign exceptions, or malicious memory corruption. Those failures may
/// hang, corrupt, or terminate the process before any status returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePanicRecord {
    plugin_id: PluginId,
    hook: EventKind,
    diagnostic: String,
}

impl NativePanicRecord {
    /// Returns the plugin that cooperatively reported the panic.
    pub const fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    /// Returns the event hook being delivered.
    pub const fn hook(&self) -> EventKind {
        self.hook
    }

    /// Returns the final non-empty callback diagnostic, or the host-authored
    /// fail-stop fallback when the callback emitted none.
    pub fn diagnostic(&self) -> &str {
        &self.diagnostic
    }
}

/// Caller-supplied metadata used to deliver an event to trusted native plugins.
///
/// Simulation-owned callers may construct exact tick and shard metadata with
/// [`NativeEventContext::new`]. The current application dispatches gameplay
/// callbacks connection-side and off-tick; it uses
/// [`NativeEventContext::connection_side`] so the ABI receives documented
/// sentinel values instead of fabricated simulation identity. Exact simulation
/// contexts receive a fresh callback-scoped shard resource handle;
/// connection-side contexts carry an invalid handle as the ABI's
/// "shard unavailable" sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeEventContext {
    tick: Tick,
    world: WorldId,
    dimension: DimensionId,
    shard: ShardPos,
    connection_side: bool,
}

impl NativeEventContext {
    /// Creates caller-attested metadata for one logical shard's event.
    pub const fn new(tick: Tick, world: WorldId, dimension: DimensionId, shard: ShardPos) -> Self {
        Self {
            tick,
            world,
            dimension,
            shard,
            connection_side: false,
        }
    }

    /// Creates the documented metadata sentinels for connection-side dispatch.
    ///
    /// This path has no authoritative simulation tick or shard identity. Zero
    /// therefore means "unavailable", not tick zero or shard `(0, 0)`, and the
    /// ABI event carries [`FcResourceHandle::INVALID`] rather than inventing a
    /// live shard resource.
    pub const fn connection_side() -> Self {
        Self {
            tick: Tick::ZERO,
            world: WorldId::new(0),
            dimension: DimensionId::new(0),
            shard: ShardPos::new(0, 0),
            connection_side: true,
        }
    }

    /// Returns the caller-supplied tick or the connection-side zero sentinel.
    pub const fn tick(self) -> Tick {
        self.tick
    }

    /// Returns the caller-supplied world or its connection-side sentinel.
    pub const fn world(self) -> WorldId {
        self.world
    }

    /// Returns the caller-supplied dimension or its connection-side sentinel.
    pub const fn dimension(self) -> DimensionId {
        self.dimension
    }

    /// Returns the caller-supplied shard position or its connection-side sentinel.
    pub const fn shard(self) -> ShardPos {
        self.shard
    }

    pub(crate) const fn is_connection_side(self) -> bool {
        self.connection_side
    }
}

/// Why one trusted native event callback or its command commit did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NativeCallbackFailure {
    /// The caller used the context-free compatibility dispatch method.
    ///
    /// Native callbacks are not invoked because the host will not fabricate
    /// ABI tick or shard metadata.
    EventContextUnavailable,
    /// The plugin's callback-scoped resource generation cannot advance.
    ResourceHandleExhausted,
    /// The callback cooperatively returned a non-success ABI status.
    Status(FcStatus),
    /// The audited callback boundary rejected the invocation.
    Boundary(CallbackError),
    /// The simulation-provided command sink rejected a staged intent.
    ///
    /// Earlier intents from the same successful callback may already have been
    /// accepted by that sink; the rejected intent and remaining suffix are not
    /// submitted.
    CommandSink(IntentError),
}

/// A trusted native callback or commit failure attributed to its plugin and hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCallbackFailureRecord {
    plugin_id: PluginId,
    hook: EventKind,
    failure: NativeCallbackFailure,
}

impl NativeCallbackFailureRecord {
    /// Returns the plugin whose callback failed.
    pub const fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    /// Returns the event hook being delivered.
    pub const fn hook(&self) -> EventKind {
        self.hook
    }

    /// Returns the typed callback failure.
    pub const fn failure(&self) -> &NativeCallbackFailure {
        &self.failure
    }
}

/// A summary of one [`PluginHost::dispatch_event`] call.
///
/// Reports how many enabled, subscribed plugins received the event, plus any
/// panic, budget, capability, callback, or command-commit outcomes. Fields are
/// private; read them through the accessors.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DispatchReport {
    delivered: usize,
    panicked: Vec<PluginId>,
    budget_exceeded: Vec<PluginId>,
    native_capability_denials: Vec<NativeCapabilityDenial>,
    native_panics: Vec<NativePanicRecord>,
    native_failures: Vec<NativeCallbackFailureRecord>,
}

impl DispatchReport {
    /// Returns how many plugins completed their event callback successfully.
    ///
    /// For a trusted native callback this counts an `FC_OK` callback even when
    /// the caller-owned command sink later rejects one of its staged intents;
    /// inspect [`DispatchReport::native_failures`] for commit failures.
    pub const fn delivered(&self) -> usize {
        self.delivered
    }

    /// Returns the ids of plugins disabled after a panic outcome.
    ///
    /// Compiled-in plugins appear here when their Rust callback unwinds.
    /// Trusted native plugins appear only when their callback cooperatively
    /// returns `FC_PLUGIN_PANIC`.
    pub fn panicked(&self) -> &[PluginId] {
        &self.panicked
    }

    /// Returns the ids of plugins that exceeded their per-call budget.
    pub fn budget_exceeded(&self) -> &[PluginId] {
        &self.budget_exceeded
    }

    /// Returns the first undeclared-capability denial from each native callback.
    ///
    /// Every denied host call still receives `FC_CAPABILITY_DENIED`; this
    /// bounded report retains one representative denial per callback.
    pub fn native_capability_denials(&self) -> &[NativeCapabilityDenial] {
        &self.native_capability_denials
    }

    /// Returns trusted-native panic records with plugin, hook, and diagnostic.
    ///
    /// This is the detailed subset also represented by
    /// [`DispatchReport::panicked`] and
    /// [`NativeCallbackFailure::Status`] in
    /// [`DispatchReport::native_failures`]; consumers should not sum those
    /// surfaces as independent panic events.
    pub fn native_panics(&self) -> &[NativePanicRecord] {
        &self.native_panics
    }

    /// Returns trusted native delivery, callback, boundary, and command-commit
    /// failures.
    pub fn native_failures(&self) -> &[NativeCallbackFailureRecord] {
        &self.native_failures
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

/// The combined verdict on a vetoable player event (a chat message or an
/// interaction) after every consulted plugin has voted.
///
/// Produced by folding each plugin's [`PluginEventDecision`] (see
/// [`PluginHost::dispatch_chat_decision`] /
/// [`PluginHost::dispatch_interact_decision`]). The event-side counterpart of
/// [`ResolvedBlockDecision`] — there is no `emitted` field because the event
/// hooks carry no inline-intent variant; intents a plugin submits ride the
/// [`CommandSink`] the caller passes in. UNSTABLE / dev-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEventOutcome {
    decision: PluginEventDecision,
    report: DispatchReport,
}

impl ResolvedEventOutcome {
    /// Returns the combined decision.
    pub const fn decision(&self) -> &PluginEventDecision {
        &self.decision
    }

    /// Returns the dispatch report (panicked / budget-exceeded plugins).
    pub const fn report(&self) -> &DispatchReport {
        &self.report
    }

    /// Returns whether the event was denied.
    pub const fn is_deny(&self) -> bool {
        self.decision.is_deny()
    }

    /// Consumes the outcome, yielding the combined decision.
    pub fn into_decision(self) -> PluginEventDecision {
        self.decision
    }
}

/// An in-process plugin host: a registry that owns plugins and drives their
/// lifecycle with catch-and-disable panic handling and a per-call time budget.
///
/// The host owns compiled-in [`Plugin`] trait objects and validated trusted
/// native factories. Compiled-in calls use `catch_unwind`; trusted native
/// callbacks use ABI statuses and a transactional bounded command stage.
/// A normally returned `FC_PLUGIN_PANIC` triggers fail-stop handling for that
/// native instance; process-ending and non-returning failures cannot be
/// recovered here. See [`NativePanicRecord`] for the exact boundary.
/// Plugin-registered commands are aggregated into a single [`CommandTree`].
pub struct PluginHost {
    plugins: Vec<PluginSlot>,
    native_plugins: Vec<TrustedNativeSlot>,
    registration_order: Vec<RegisteredPlugin>,
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
            native_plugins: Vec::new(),
            registration_order: Vec::new(),
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
        if self.len() >= self.config.max_plugins {
            return Err(HostError::CapacityExceeded {
                max: self.config.max_plugins,
            });
        }

        // Reading metadata calls into plugin code, so protect this boundary too. If it
        // panics we have no real id (reading it is what failed), so report the
        // panic against a placeholder id.
        let Ok(metadata) = catch_unwind(AssertUnwindSafe(|| plugin.metadata())) else {
            return Err(HostError::Panicked {
                id: PluginId::new("<unknown>"),
            });
        };

        let id = metadata.id().clone();
        if self.has_plugin(&id) {
            return Err(HostError::DuplicateId(id));
        }

        let capabilities = granted.unwrap_or_else(|| metadata.requested_capabilities());
        let index = self.plugins.len();
        self.plugins.push(PluginSlot {
            id: id.clone(),
            plugin,
            metadata,
            capabilities,
            state: PluginState::Registered,
            subscriptions: BTreeSet::new(),
            stats: PluginStats::default(),
        });
        self.registration_order
            .push(RegisteredPlugin::Compiled(index));
        Ok(id)
    }

    /// Registers one fully validated trusted native plugin factory.
    ///
    /// Dispatch preserves the host's global registration order. Registering a
    /// [`ferrumc_plugin_loader::LoadedPlugins`] set in its iterator order also
    /// preserves the loader's deterministic plugin-id order. This host currently
    /// resolves event subscriptions, vetoable block-place/break callbacks, and
    /// the `MESSAGE` and `TELEPORT` subset of intent submission. A manifest
    /// requesting another facade is rejected before initialization; `SET_BLOCK`
    /// remains unavailable until a live dimension-resource facade is wired.
    pub fn register_trusted_native(&mut self, plugin: LoadedPlugin) -> Result<PluginId, HostError> {
        if self.len() >= self.config.max_plugins {
            return Err(HostError::CapacityExceeded {
                max: self.config.max_plugins,
            });
        }

        let manifest = plugin.manifest();
        let id = PluginId::new(manifest.id());
        if self.has_plugin(&id) {
            return Err(HostError::DuplicateId(id));
        }

        let mut capabilities = CapabilityManifest::empty();
        for native_capability in manifest.capabilities().iter() {
            let capability = map_native_capability(native_capability);
            if !supported_native_capabilities().grants(capability) {
                return Err(HostError::UnsupportedNativeCapability { id, capability });
            }
            capabilities = capabilities.with(capability);
        }

        let metadata = PluginMetadata::new(
            id.clone(),
            manifest.name(),
            manifest.version().clone(),
            capabilities,
        );
        let index = self.native_plugins.len();
        self.native_plugins.push(TrustedNativeSlot {
            id: id.clone(),
            factory: plugin,
            active: None,
            metadata,
            capabilities,
            state: PluginState::Registered,
            subscriptions: BTreeSet::new(),
            stats: PluginStats::default(),
            next_event_resource: 1,
        });
        self.registration_order
            .push(RegisteredPlugin::TrustedNative(index));
        Ok(id)
    }

    /// Enables the plugin with id `id`, running its
    /// [`on_enable`](Plugin::on_enable) hook.
    ///
    /// On success the plugin's subscriptions are recorded and its registered
    /// commands are merged into the host's command tree. If the hook returns an
    /// error the plugin is left disabled and [`HostError::PluginFailed`] is
    /// returned. If a compiled-in hook unwinds, that plugin is disabled and
    /// [`HostError::Panicked`] is returned; a later enable attempt returns
    /// [`HostError::PanicDisabled`] without invoking that retained instance
    /// again. A trusted native initialization failure reported through the ABI
    /// returns [`HostError::NativeLifecycle`]; process-aborting native failures
    /// cannot be recovered here. A native registration previously disabled by
    /// `FC_PLUGIN_PANIC` returns [`HostError::NativePanicDisabled`] instead of
    /// allocating another instance.
    pub fn enable(&mut self, id: &PluginId) -> Result<(), HostError> {
        if self.native_plugins.iter().any(|slot| &slot.id == id) {
            return self.enable_trusted_native(id);
        }

        let Self {
            plugins,
            storage,
            command_tree,
            config,
            ..
        } = self;

        let slot = plugins
            .iter_mut()
            .find(|slot| &slot.id == id)
            .ok_or_else(|| HostError::UnknownPlugin(id.clone()))?;

        if slot.state.is_enabled() {
            return Err(HostError::AlreadyEnabled(id.clone()));
        }
        if slot.state == PluginState::Disabled(DisableReason::Panicked) {
            return Err(HostError::PanicDisabled(id.clone()));
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
    /// An unwind from a compiled-in hook leaves the registration terminally
    /// disabled and returns [`HostError::Panicked`]. Returns
    /// [`HostError::NotEnabled`] if the plugin is not currently enabled. A
    /// trusted native shutdown failure reported through the ABI leaves the
    /// plugin disabled and returns [`HostError::NativeLifecycle`].
    pub fn disable(&mut self, id: &PluginId) -> Result<(), HostError> {
        if self.native_plugins.iter().any(|slot| &slot.id == id) {
            return self.disable_trusted_native(id);
        }

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
        if catch_unwind(AssertUnwindSafe(|| slot.plugin.on_disable(&mut ctx))).is_ok() {
            slot.state = PluginState::Disabled(DisableReason::Manual);
            Ok(())
        } else {
            slot.stats.panics += 1;
            slot.state = PluginState::Disabled(DisableReason::Panicked);
            tracing::warn!(plugin = %slot.id, "plugin panicked during on_disable; disabled");
            Err(HostError::Panicked { id: id.clone() })
        }
    }

    fn enable_trusted_native(&mut self, id: &PluginId) -> Result<(), HostError> {
        let config = self.config;
        let slot = self
            .native_plugins
            .iter_mut()
            .find(|slot| &slot.id == id)
            .ok_or_else(|| HostError::UnknownPlugin(id.clone()))?;
        if slot.state.is_enabled() {
            return Err(HostError::AlreadyEnabled(id.clone()));
        }
        if slot.state == PluginState::Disabled(DisableReason::Panicked) {
            return Err(HostError::NativePanicDisabled(id.clone()));
        }
        shutdown_native_instance(slot);
        slot.subscriptions.clear();

        let mut services = NativeCallbackServices::for_initialization(slot.capabilities);
        let start = Instant::now();
        let outcome = slot.factory.initialize(&mut services);
        let elapsed = start.elapsed();

        let active = match outcome {
            Ok(active) => active,
            Err(source) => {
                let status = callback_error_status(&source);
                let completion = services.complete(status);
                log_native_diagnostics(&slot.id, completion.diagnostics());
                slot.state = PluginState::Disabled(DisableReason::EnableFailed);
                return Err(HostError::NativeLifecycle {
                    id: id.clone(),
                    hook: NativeLifecycleHook::Initialize,
                    source,
                });
            }
        };

        let completion = services.complete(FC_OK);
        log_native_diagnostics(&slot.id, completion.diagnostics());
        if !completion.is_committed() {
            let status = completion_failure_status(&completion);
            shutdown_after_failed_enable(active, slot.capabilities);
            slot.state = PluginState::Disabled(DisableReason::EnableFailed);
            return Err(HostError::NativeLifecycle {
                id: id.clone(),
                hook: NativeLifecycleHook::Initialize,
                source: CallbackError::Status(status),
            });
        }

        let mut subscriptions = BTreeSet::new();
        for effect in completion.into_effects() {
            match effect {
                NativeEffect::Subscribe(kind) => {
                    subscriptions.insert(kind);
                }
                NativeEffect::Intent(_) | NativeEffect::BlockDecision(_) => {
                    shutdown_after_failed_enable(active, slot.capabilities);
                    slot.state = PluginState::Disabled(DisableReason::EnableFailed);
                    return Err(HostError::NativeLifecycle {
                        id: id.clone(),
                        hook: NativeLifecycleHook::Initialize,
                        source: CallbackError::Status(FC_ERROR),
                    });
                }
            }
        }

        slot.active = Some(active);
        slot.subscriptions = subscriptions;
        slot.state = PluginState::Enabled;
        if config.call_budget().is_exceeded(elapsed) {
            slot.stats.budget_overruns += 1;
            tracing::warn!(
                plugin = %slot.id,
                ?elapsed,
                "trusted native plugin exceeded its time budget during initialization"
            );
        }
        Ok(())
    }

    fn disable_trusted_native(&mut self, id: &PluginId) -> Result<(), HostError> {
        let slot = self
            .native_plugins
            .iter_mut()
            .find(|slot| &slot.id == id)
            .ok_or_else(|| HostError::UnknownPlugin(id.clone()))?;
        if !slot.state.is_enabled() {
            return Err(HostError::NotEnabled(id.clone()));
        }

        let Some(active) = slot.active.take() else {
            slot.state = PluginState::Disabled(DisableReason::Manual);
            slot.subscriptions.clear();
            return Err(HostError::NotEnabled(id.clone()));
        };
        let mut services = NativeCallbackServices::for_shutdown(slot.capabilities);
        let outcome = active.shutdown(&mut services);
        slot.state = PluginState::Disabled(DisableReason::Manual);
        slot.subscriptions.clear();

        match outcome {
            Ok(status) => {
                let completion = services.complete(status);
                log_native_diagnostics(&slot.id, completion.diagnostics());
                if status == FC_OK && completion.is_committed() {
                    Ok(())
                } else {
                    Err(HostError::NativeLifecycle {
                        id: id.clone(),
                        hook: NativeLifecycleHook::Shutdown,
                        source: CallbackError::Status(completion_failure_status(&completion)),
                    })
                }
            }
            Err(source) => {
                let status = callback_error_status(&source);
                let completion = services.complete(status);
                log_native_diagnostics(&slot.id, completion.diagnostics());
                Err(HostError::NativeLifecycle {
                    id: id.clone(),
                    hook: NativeLifecycleHook::Shutdown,
                    source,
                })
            }
        }
    }

    /// Dispatches `event` to enabled compiled-in plugins subscribed to its kind.
    ///
    /// The world, sink, and permission facades are injected by the caller; the
    /// host supplies each plugin its own namespaced storage. An unwinding
    /// compiled-in plugin is disabled and recorded in the returned
    /// [`DispatchReport`]. An eligible trusted native plugin is not invoked:
    /// its report contains
    /// [`NativeCallbackFailure::EventContextUnavailable`] because this
    /// compatibility method has no caller-supplied ABI metadata. Use
    /// [`PluginHost::dispatch_event_with_native_context`] with exact simulation
    /// metadata or [`NativeEventContext::connection_side`] to drive both
    /// packaging representations.
    pub fn dispatch_event(
        &mut self,
        event: &PluginEvent,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> DispatchReport {
        self.dispatch_event_inner(event, None, world, sink, permissions)
    }

    /// Dispatches `event` to compiled-in and trusted native subscribers in one
    /// deterministic registration order.
    ///
    /// `native_context` supplies exact caller-attested simulation metadata or
    /// documented connection-side sentinels for the ABI envelope. For exact
    /// simulation metadata, the host mints a fresh shard resource for each
    /// native callback. Connection-side callbacks pass
    /// `FcResourceHandle::INVALID` as the metadata-unavailable sentinel. A
    /// successful callback's staged `MESSAGE` or `TELEPORT` intents are
    /// submitted to the same caller-owned bounded `sink` used by compiled-in
    /// plugins. Callback failures or capability denials discard the native
    /// stage before the sink is touched.
    pub fn dispatch_event_with_native_context(
        &mut self,
        event: &PluginEvent,
        native_context: NativeEventContext,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> DispatchReport {
        self.dispatch_event_inner(event, Some(native_context), world, sink, permissions)
    }

    fn dispatch_event_inner(
        &mut self,
        event: &PluginEvent,
        native_context: Option<NativeEventContext>,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> DispatchReport {
        let kind = event.kind();
        let Self {
            plugins,
            native_plugins,
            registration_order,
            storage,
            config,
            ..
        } = self;
        let budget = config.call_budget();
        let disable_on_overrun = config.disable_on_overrun();

        let mut report = DispatchReport::default();
        for registered in registration_order.iter().copied() {
            match registered {
                RegisteredPlugin::Compiled(index) => {
                    let Some(slot) = plugins.get_mut(index) else {
                        continue;
                    };
                    dispatch_compiled_event(
                        slot,
                        event,
                        kind,
                        world,
                        sink,
                        permissions,
                        storage.as_ref(),
                        budget,
                        disable_on_overrun,
                        &mut report,
                    );
                }
                RegisteredPlugin::TrustedNative(index) => {
                    let Some(slot) = native_plugins.get_mut(index) else {
                        continue;
                    };
                    if !slot.state.is_enabled() || !slot.subscriptions.contains(&kind) {
                        continue;
                    }
                    let Some(native_context) = native_context else {
                        report.native_failures.push(NativeCallbackFailureRecord {
                            plugin_id: slot.id.clone(),
                            hook: kind,
                            failure: NativeCallbackFailure::EventContextUnavailable,
                        });
                        continue;
                    };
                    let Some(shard_resource) = event_resource_for_context(slot, native_context)
                    else {
                        report.native_failures.push(NativeCallbackFailureRecord {
                            plugin_id: slot.id.clone(),
                            hook: kind,
                            failure: NativeCallbackFailure::ResourceHandleExhausted,
                        });
                        continue;
                    };
                    let Ok(encoded_event) =
                        encode_event(event, native_context.tick().get(), shard_resource)
                    else {
                        continue;
                    };
                    dispatch_native_event(
                        slot,
                        &encoded_event,
                        native_context,
                        kind,
                        sink,
                        budget,
                        disable_on_overrun,
                        &mut report,
                    );
                }
            }
        }
        report
    }

    /// Consults every enabled, [`VetoBlockEdits`](Capability::VetoBlockEdits)-capable
    /// plugin about a pending block *placement* and folds their decisions into one
    /// [`ResolvedBlockDecision`].
    ///
    /// See [`PluginHost::dispatch_block_break_decision`] for the shared semantics
    /// (precedence, catch-and-disable handling, fail-safe); this is the
    /// placement-side entry point. This compatibility method has no native ABI
    /// metadata: an enabled trusted-native veto plugin therefore fails the edit
    /// closed with [`NativeCallbackFailure::EventContextUnavailable`]. Use
    /// [`PluginHost::dispatch_block_place_decision_with_native_context`] to
    /// invoke both packaging representations.
    pub fn dispatch_block_place_decision(
        &mut self,
        attempt: &BlockPlaceAttempt,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> ResolvedBlockDecision {
        self.fold_before(
            NativeBlockAttempt::Place(attempt),
            None,
            world,
            sink,
            permissions,
            |plugin, ctx| plugin.before_block_place(attempt, ctx),
        )
    }

    /// Consults compiled-in and trusted native plugins about a pending block
    /// placement using caller-supplied native callback metadata.
    ///
    /// The decision precedence and fail-closed behavior are identical to
    /// [`PluginHost::dispatch_block_place_decision`]. The current application
    /// supplies [`NativeEventContext::connection_side`] because this dispatch
    /// runs connection-side and off-tick.
    pub fn dispatch_block_place_decision_with_native_context(
        &mut self,
        attempt: &BlockPlaceAttempt,
        native_context: NativeEventContext,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> ResolvedBlockDecision {
        self.fold_before(
            NativeBlockAttempt::Place(attempt),
            Some(native_context),
            world,
            sink,
            permissions,
            |plugin, ctx| plugin.before_block_place(attempt, ctx),
        )
    }

    /// Consults every enabled, [`VetoBlockEdits`](Capability::VetoBlockEdits)-capable
    /// plugin about a pending block *break* and folds their decisions into one
    /// [`ResolvedBlockDecision`].
    ///
    /// This compatibility method has no native ABI metadata: an enabled
    /// trusted-native veto plugin therefore fails the edit closed with
    /// [`NativeCallbackFailure::EventContextUnavailable`]. Use
    /// [`PluginHost::dispatch_block_break_decision_with_native_context`] to
    /// invoke both packaging representations.
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
    /// # Catch-and-disable and fail-safe
    ///
    /// A compiled-in hook is wrapped in [`catch_unwind`], exactly like
    /// [`dispatch_event`](Self::dispatch_event). A compiled-in panic disables
    /// that plugin. A trusted-native callback uses its ABI status; a returned
    /// `FC_PLUGIN_PANIC` disables that plugin, while another callback failure
    /// leaves it enabled. Every failure is treated as a
    /// [`Deny`](ResolvedDecision::Deny), so a broken plugin cannot silently let
    /// a destructive edit through. Per-call budgets apply as for event
    /// dispatch. Callers must reject the attempted edit.
    pub fn dispatch_block_break_decision(
        &mut self,
        attempt: &BlockBreakAttempt,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> ResolvedBlockDecision {
        self.fold_before(
            NativeBlockAttempt::Break(attempt),
            None,
            world,
            sink,
            permissions,
            |plugin, ctx| plugin.before_block_break(attempt, ctx),
        )
    }

    /// Consults compiled-in and trusted native plugins about a pending block
    /// break using caller-supplied native callback metadata.
    ///
    /// The decision precedence and fail-closed behavior are identical to
    /// [`PluginHost::dispatch_block_break_decision`]. The current application
    /// supplies [`NativeEventContext::connection_side`] because this dispatch
    /// runs connection-side and off-tick.
    pub fn dispatch_block_break_decision_with_native_context(
        &mut self,
        attempt: &BlockBreakAttempt,
        native_context: NativeEventContext,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> ResolvedBlockDecision {
        self.fold_before(
            NativeBlockAttempt::Break(attempt),
            Some(native_context),
            world,
            sink,
            permissions,
            |plugin, ctx| plugin.before_block_break(attempt, ctx),
        )
    }

    /// The shared fold driving both `before_block_*` decision paths.
    ///
    /// `call` invokes the appropriate hook on one plugin; everything else (the
    /// capability gate, catch-and-disable handling, budgeting, and the precedence
    /// fold) is identical for placement and break.
    #[allow(clippy::too_many_lines)]
    fn fold_before<F>(
        &mut self,
        native_attempt: NativeBlockAttempt<'_>,
        native_context: Option<NativeEventContext>,
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
            native_plugins,
            registration_order,
            storage,
            config,
            ..
        } = self;
        let budget = config.call_budget();
        let disable_on_overrun = config.disable_on_overrun();

        let mut decision = ResolvedDecision::Allow;
        let mut emitted: Vec<WorldIntent> = Vec::new();
        let mut report = DispatchReport::default();

        for registered in registration_order.iter().copied() {
            // A Deny is absorbing: once denied, stop consulting plugins.
            if matches!(decision, ResolvedDecision::Deny { .. }) {
                break;
            }
            match registered {
                RegisteredPlugin::Compiled(index) => {
                    let Some(slot) = plugins.get_mut(index) else {
                        continue;
                    };
                    if !slot.state.is_enabled()
                        || !slot.capabilities.grants(Capability::VetoBlockEdits)
                    {
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
                    let outcome =
                        catch_unwind(AssertUnwindSafe(|| call(&mut *slot.plugin, &mut ctx)));
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

                    fold_compiled_block_decision(
                        slot,
                        plugin_decision,
                        &mut decision,
                        &mut emitted,
                    );
                }
                RegisteredPlugin::TrustedNative(index) => {
                    let Some(slot) = native_plugins.get_mut(index) else {
                        continue;
                    };
                    if !slot.state.is_enabled()
                        || !slot.capabilities.grants(Capability::VetoBlockEdits)
                    {
                        continue;
                    }
                    let hook = native_attempt.hook();
                    let Some(native_context) = native_context else {
                        report.native_failures.push(NativeCallbackFailureRecord {
                            plugin_id: slot.id.clone(),
                            hook,
                            failure: NativeCallbackFailure::EventContextUnavailable,
                        });
                        decision = ResolvedDecision::Deny { message: None };
                        continue;
                    };
                    let Some(shard_resource) = event_resource_for_context(slot, native_context)
                    else {
                        report.native_failures.push(NativeCallbackFailureRecord {
                            plugin_id: slot.id.clone(),
                            hook,
                            failure: NativeCallbackFailure::ResourceHandleExhausted,
                        });
                        decision = ResolvedDecision::Deny { message: None };
                        continue;
                    };
                    let encoded_event = native_attempt.encode(native_context, shard_resource);
                    let Some(plugin_decision) = dispatch_native_block_decision(
                        slot,
                        &encoded_event,
                        native_context,
                        native_attempt.route(),
                        hook,
                        sink,
                        budget,
                        disable_on_overrun,
                        &mut report,
                    ) else {
                        decision = ResolvedDecision::Deny { message: None };
                        continue;
                    };
                    fold_native_block_decision(slot, plugin_decision, &mut decision);
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

    /// Consults every enabled, [`VetoEvents`](Capability::VetoEvents)-capable plugin
    /// about a pending *chat message* and folds their decisions into one
    /// [`ResolvedEventOutcome`].
    ///
    /// See [`PluginHost::dispatch_interact_decision`] for the shared semantics
    /// (precedence, catch-and-disable handling, fail-safe); this is the chat-side
    /// entry point. Any intents a plugin submits during the hook are pushed to
    /// `sink`.
    pub fn dispatch_chat_decision(
        &mut self,
        attempt: &ChatAttempt,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> ResolvedEventOutcome {
        self.fold_event_decision(world, sink, permissions, |plugin, ctx| {
            plugin.before_chat(attempt, ctx)
        })
    }

    /// Consults every enabled, [`VetoEvents`](Capability::VetoEvents)-capable plugin
    /// about a pending *interaction* and folds their decisions into one
    /// [`ResolvedEventOutcome`].
    ///
    /// # Precedence (deterministic, fail-safe)
    ///
    /// Plugins are consulted in registration order; the first
    /// [`Deny`](PluginEventDecision::Deny) is *absorbing*: it wins, the remaining
    /// plugins are skipped, and its message (the first non-`None`) is carried. If
    /// nobody objects the result is [`Allow`](PluginEventDecision::Allow).
    ///
    /// # Catch-and-disable and fail-safe
    ///
    /// Each hook is wrapped in [`catch_unwind`], exactly like
    /// [`dispatch_event`](Self::dispatch_event). A plugin that *panics* is disabled,
    /// counted in [`PluginStats`], logged, and — the fail-safe — treated as a
    /// [`Deny`](PluginEventDecision::Deny). Per-call budgets apply as for event
    /// dispatch. None of this runs inside the simulation tick.
    pub fn dispatch_interact_decision(
        &mut self,
        attempt: &InteractAttempt,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
    ) -> ResolvedEventOutcome {
        self.fold_event_decision(world, sink, permissions, |plugin, ctx| {
            plugin.before_interact(attempt, ctx)
        })
    }

    /// The shared fold driving both `before_chat` / `before_interact` decision
    /// paths.
    ///
    /// `call` invokes the appropriate hook on one plugin; everything else (the
    /// [`VetoEvents`](Capability::VetoEvents) gate, catch-and-disable handling,
    /// budgeting, and the absorbing-`Deny` precedence) is identical for chat and
    /// interact.
    fn fold_event_decision<F>(
        &mut self,
        world: &dyn WorldView,
        sink: &mut dyn CommandSink,
        permissions: &dyn PermissionApi,
        mut call: F,
    ) -> ResolvedEventOutcome
    where
        F: FnMut(&mut dyn Plugin, &mut EventContext<'_>) -> PluginEventDecision,
    {
        let Self {
            plugins,
            storage,
            config,
            ..
        } = self;
        let budget = config.call_budget();
        let disable_on_overrun = config.disable_on_overrun();

        let mut decision = PluginEventDecision::Allow;
        let mut report = DispatchReport::default();

        for slot in plugins.iter_mut() {
            // A Deny is absorbing: once denied, stop consulting plugins.
            if decision.is_deny() {
                break;
            }
            if !slot.state.is_enabled() || !slot.capabilities.grants(Capability::VetoEvents) {
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
                // Fail-safe: a panicking plugin denies the event and is disabled.
                slot.stats.panics += 1;
                slot.state = PluginState::Disabled(DisableReason::Panicked);
                report.panicked.push(slot.id.clone());
                tracing::warn!(
                    plugin = %slot.id,
                    "plugin panicked during a before_* event decision; disabled and treated as Deny"
                );
                decision = PluginEventDecision::Deny { message: None };
                continue;
            };

            report.delivered += 1;
            if budget.is_exceeded(elapsed) {
                slot.stats.budget_overruns += 1;
                report.budget_exceeded.push(slot.id.clone());
                tracing::warn!(
                    plugin = %slot.id,
                    ?elapsed,
                    "plugin exceeded its time budget during a before_* event decision"
                );
                if disable_on_overrun {
                    slot.state = PluginState::Disabled(DisableReason::BudgetExceeded);
                }
            }

            // Tally this plugin's own contribution (mirroring the per-plugin
            // attribution in `fold_before`), then fold it: a Deny is absorbing
            // and recorded; Allow (and, since the enum is `#[non_exhaustive]`,
            // any unknown future variant) leaves the fold unchanged.
            match plugin_decision {
                PluginEventDecision::Deny { message } => {
                    slot.stats.deny = slot.stats.deny.saturating_add(1);
                    decision = PluginEventDecision::Deny { message };
                }
                // `PluginEventDecision::Allow` (no-op) and any future no-veto
                // variant count as an allow (the event was not vetoed).
                _ => {
                    slot.stats.allow = slot.stats.allow.saturating_add(1);
                }
            }
        }

        ResolvedEventOutcome { decision, report }
    }

    /// Returns the number of registered plugins.
    pub fn len(&self) -> usize {
        self.plugins.len() + self.native_plugins.len()
    }

    /// Returns whether no plugins are registered.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty() && self.native_plugins.is_empty()
    }

    /// Returns the lifecycle state of the plugin with id `id`, if registered.
    pub fn state(&self, id: &PluginId) -> Option<PluginState> {
        self.slot(id)
            .map(|slot| slot.state)
            .or_else(|| self.native_slot(id).map(|slot| slot.state))
    }

    /// Returns whether the plugin with id `id` is registered and enabled.
    pub fn is_enabled(&self, id: &PluginId) -> bool {
        self.state(id).is_some_and(PluginState::is_enabled)
    }

    /// Returns the capabilities granted to the plugin with id `id`.
    pub fn capabilities(&self, id: &PluginId) -> Option<CapabilityManifest> {
        self.slot(id)
            .map(|slot| slot.capabilities)
            .or_else(|| self.native_slot(id).map(|slot| slot.capabilities))
    }

    /// Returns the metadata of the plugin with id `id`, if registered.
    pub fn metadata(&self, id: &PluginId) -> Option<&PluginMetadata> {
        self.slot(id)
            .map(|slot| &slot.metadata)
            .or_else(|| self.native_slot(id).map(|slot| &slot.metadata))
    }

    /// Returns the accumulated statistics for the plugin with id `id`.
    pub fn stats(&self, id: &PluginId) -> Option<PluginStats> {
        self.slot(id)
            .map(|slot| slot.stats)
            .or_else(|| self.native_slot(id).map(|slot| slot.stats))
    }

    /// Returns a per-plugin block-edit decision report for every registered
    /// plugin, in registration order.
    ///
    /// A cheap, bounded read (one row per plugin, capped at
    /// [`HostConfig::max_plugins`]) the host or its embedder can fold into a
    /// metrics snapshot each tick without touching the decision hot path.
    pub fn plugin_decision_reports(&self) -> Vec<PluginDecisionReport> {
        self.registration_order
            .iter()
            .filter_map(|registered| match registered {
                RegisteredPlugin::Compiled(index) => self
                    .plugins
                    .get(*index)
                    .map(|slot| (&slot.metadata, slot.stats)),
                RegisteredPlugin::TrustedNative(index) => self
                    .native_plugins
                    .get(*index)
                    .map(|slot| (&slot.metadata, slot.stats)),
            })
            .map(|(metadata, stats)| PluginDecisionReport {
                name: metadata.name().to_string(),
                allow: stats.allow(),
                deny: stats.deny(),
                replace: stats.replace(),
                panic: u64::from(stats.panics()),
            })
            .collect()
    }

    /// Returns whether the plugin with id `id` is subscribed to `kind`.
    pub fn is_subscribed(&self, id: &PluginId, kind: EventKind) -> bool {
        self.slot(id)
            .is_some_and(|slot| slot.subscriptions.contains(&kind))
            || self
                .native_slot(id)
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

    fn native_slot(&self, id: &PluginId) -> Option<&TrustedNativeSlot> {
        self.native_plugins.iter().find(|slot| &slot.id == id)
    }

    fn has_plugin(&self, id: &PluginId) -> bool {
        self.slot(id).is_some() || self.native_slot(id).is_some()
    }
}

fn fold_compiled_block_decision(
    slot: &mut PluginSlot,
    plugin_decision: PluginBlockDecision,
    decision: &mut ResolvedDecision,
    emitted: &mut Vec<WorldIntent>,
) {
    match plugin_decision {
        PluginBlockDecision::Deny { message } => {
            slot.stats.deny = slot.stats.deny.saturating_add(1);
            *decision = ResolvedDecision::Deny { message };
        }
        PluginBlockDecision::Replace { block_state_id } => {
            slot.stats.replace = slot.stats.replace.saturating_add(1);
            *decision = ResolvedDecision::Replace { block_state_id };
        }
        PluginBlockDecision::EmitIntents(intents) => {
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
        _ => {
            slot.stats.allow = slot.stats.allow.saturating_add(1);
        }
    }
}

fn fold_native_block_decision(
    slot: &mut TrustedNativeSlot,
    plugin_decision: PluginBlockDecision,
    decision: &mut ResolvedDecision,
) {
    match plugin_decision {
        PluginBlockDecision::Deny { message } => {
            slot.stats.deny = slot.stats.deny.saturating_add(1);
            *decision = ResolvedDecision::Deny { message };
        }
        PluginBlockDecision::Replace { block_state_id } => {
            slot.stats.replace = slot.stats.replace.saturating_add(1);
            *decision = ResolvedDecision::Replace { block_state_id };
        }
        _ => {
            slot.stats.allow = slot.stats.allow.saturating_add(1);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_compiled_event(
    slot: &mut PluginSlot,
    event: &PluginEvent,
    kind: EventKind,
    world: &dyn WorldView,
    sink: &mut dyn CommandSink,
    permissions: &dyn PermissionApi,
    storage: &dyn PluginStorageBackend,
    budget: CallBudget,
    disable_on_overrun: bool,
    report: &mut DispatchReport,
) {
    if !slot.state.is_enabled() || !slot.subscriptions.contains(&kind) {
        return;
    }

    let namespaced = NamespacedStorage::new(storage, slot.id.clone());
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

#[allow(clippy::too_many_arguments)]
fn dispatch_native_block_decision(
    slot: &mut TrustedNativeSlot,
    encoded_event: &OwnedEvent,
    event_context: NativeEventContext,
    event_route: NativeEventRoute,
    hook: EventKind,
    sink: &mut dyn CommandSink,
    budget: CallBudget,
    disable_on_overrun: bool,
    report: &mut DispatchReport,
) -> Option<PluginBlockDecision> {
    let Some(active) = slot.active.as_mut() else {
        report.native_failures.push(NativeCallbackFailureRecord {
            plugin_id: slot.id.clone(),
            hook,
            failure: NativeCallbackFailure::Status(FC_ERROR),
        });
        return None;
    };

    let mut services = NativeCallbackServices::for_block_decision(
        slot.capabilities,
        encoded_event.shard(),
        event_context,
        event_route,
    );
    let start = Instant::now();
    let outcome = active.on_event(encoded_event, &mut services);
    let elapsed = start.elapsed();
    let (completion, boundary_error) = match outcome {
        Ok(status) => (services.complete(status), None),
        Err(error) => (services.complete(FC_ERROR), Some(error)),
    };
    let cooperatively_panicked =
        boundary_error.is_none() && completion.callback_status() == FC_PLUGIN_PANIC;
    let panic_diagnostic = if cooperatively_panicked {
        native_panic_diagnostic(&completion)
    } else {
        String::new()
    };
    let over_budget = budget.is_exceeded(elapsed);
    let disposition =
        native_post_call_disposition(cooperatively_panicked, over_budget, disable_on_overrun);

    let decision =
        record_native_block_completion(slot, hook, completion, boundary_error, sink, report);
    if disposition.over_budget {
        slot.stats.budget_overruns += 1;
        report.budget_exceeded.push(slot.id.clone());
        tracing::warn!(
            plugin = %slot.id,
            ?elapsed,
            "trusted native plugin exceeded its time budget during block decision dispatch"
        );
    }
    match disposition.action {
        NativePostCallAction::KeepEnabled => {}
        NativePostCallAction::DisableForBudget => {
            shutdown_native_instance(slot);
            slot.subscriptions.clear();
            slot.state = PluginState::Disabled(DisableReason::BudgetExceeded);
        }
        NativePostCallAction::DisableForPanic => {
            disable_after_native_panic(slot, hook, panic_diagnostic, report);
        }
    }
    decision
}

fn record_native_block_completion(
    slot: &TrustedNativeSlot,
    hook: EventKind,
    completion: NativeCompletion,
    boundary_error: Option<CallbackError>,
    sink: &mut dyn CommandSink,
    report: &mut DispatchReport,
) -> Option<PluginBlockDecision> {
    log_native_diagnostics(&slot.id, completion.diagnostics());
    if let Some(capability) = completion.capability_denial() {
        report
            .native_capability_denials
            .push(NativeCapabilityDenial {
                plugin_id: slot.id.clone(),
                capability,
            });
    }

    if let Some(error) = boundary_error {
        report.native_failures.push(NativeCallbackFailureRecord {
            plugin_id: slot.id.clone(),
            hook,
            failure: NativeCallbackFailure::Boundary(error),
        });
        return None;
    }
    if !completion.is_committed() {
        report.native_failures.push(NativeCallbackFailureRecord {
            plugin_id: slot.id.clone(),
            hook,
            failure: NativeCallbackFailure::Status(completion_failure_status(&completion)),
        });
        return None;
    }

    report.delivered += 1;
    let mut decision = None;
    for effect in completion.into_effects() {
        match effect {
            NativeEffect::Intent(intent) => {
                if let Err(error) = sink.submit(intent) {
                    report.native_failures.push(NativeCallbackFailureRecord {
                        plugin_id: slot.id.clone(),
                        hook,
                        failure: NativeCallbackFailure::CommandSink(error),
                    });
                    return None;
                }
            }
            NativeEffect::BlockDecision(value) => {
                decision = Some(value);
            }
            NativeEffect::Subscribe(_) => {
                report.native_failures.push(NativeCallbackFailureRecord {
                    plugin_id: slot.id.clone(),
                    hook,
                    failure: NativeCallbackFailure::Status(FC_ERROR),
                });
                return None;
            }
        }
    }
    decision
}

#[allow(clippy::too_many_arguments)]
fn dispatch_native_event(
    slot: &mut TrustedNativeSlot,
    encoded_event: &OwnedEvent,
    event_context: NativeEventContext,
    kind: EventKind,
    sink: &mut dyn CommandSink,
    budget: CallBudget,
    disable_on_overrun: bool,
    report: &mut DispatchReport,
) {
    if !slot.state.is_enabled() || !slot.subscriptions.contains(&kind) {
        return;
    }
    let Some(active) = slot.active.as_mut() else {
        return;
    };

    let mut services =
        NativeCallbackServices::for_event(slot.capabilities, encoded_event.shard(), event_context);
    let start = Instant::now();
    let outcome = active.on_event(encoded_event, &mut services);
    let elapsed = start.elapsed();
    let (completion, boundary_error) = match outcome {
        Ok(status) => (services.complete(status), None),
        Err(error) => (services.complete(FC_ERROR), Some(error)),
    };
    let cooperatively_panicked =
        boundary_error.is_none() && completion.callback_status() == FC_PLUGIN_PANIC;
    let panic_diagnostic = if cooperatively_panicked {
        native_panic_diagnostic(&completion)
    } else {
        String::new()
    };
    let over_budget = budget.is_exceeded(elapsed);
    let disposition =
        native_post_call_disposition(cooperatively_panicked, over_budget, disable_on_overrun);

    record_native_completion(slot, kind, completion, boundary_error, sink, report);
    if disposition.over_budget {
        slot.stats.budget_overruns += 1;
        report.budget_exceeded.push(slot.id.clone());
        tracing::warn!(
            plugin = %slot.id,
            ?elapsed,
            "trusted native plugin exceeded its time budget during event dispatch"
        );
    }
    match disposition.action {
        NativePostCallAction::KeepEnabled => {}
        NativePostCallAction::DisableForBudget => {
            shutdown_native_instance(slot);
            slot.subscriptions.clear();
            slot.state = PluginState::Disabled(DisableReason::BudgetExceeded);
        }
        NativePostCallAction::DisableForPanic => {
            disable_after_native_panic(slot, kind, panic_diagnostic, report);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativePostCallAction {
    KeepEnabled,
    DisableForBudget,
    DisableForPanic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativePostCallDisposition {
    over_budget: bool,
    action: NativePostCallAction,
}

const fn native_post_call_disposition(
    cooperatively_panicked: bool,
    over_budget: bool,
    disable_on_overrun: bool,
) -> NativePostCallDisposition {
    let action = if cooperatively_panicked {
        NativePostCallAction::DisableForPanic
    } else if over_budget && disable_on_overrun {
        NativePostCallAction::DisableForBudget
    } else {
        NativePostCallAction::KeepEnabled
    };
    NativePostCallDisposition {
        over_budget,
        action,
    }
}

fn next_event_resource(slot: &mut TrustedNativeSlot) -> Option<FcResourceHandle> {
    let raw = slot.next_event_resource;
    let next = raw.checked_add(1)?;
    slot.next_event_resource = next;
    Some(FcResourceHandle::from_raw(raw))
}

fn event_resource_for_context(
    slot: &mut TrustedNativeSlot,
    context: NativeEventContext,
) -> Option<FcResourceHandle> {
    if context.is_connection_side() {
        Some(FcResourceHandle::INVALID)
    } else {
        next_event_resource(slot)
    }
}

fn record_native_completion(
    slot: &TrustedNativeSlot,
    kind: EventKind,
    completion: NativeCompletion,
    boundary_error: Option<CallbackError>,
    sink: &mut dyn CommandSink,
    report: &mut DispatchReport,
) {
    log_native_diagnostics(&slot.id, completion.diagnostics());
    if let Some(capability) = completion.capability_denial() {
        report
            .native_capability_denials
            .push(NativeCapabilityDenial {
                plugin_id: slot.id.clone(),
                capability,
            });
    }

    if let Some(error) = boundary_error {
        report.native_failures.push(NativeCallbackFailureRecord {
            plugin_id: slot.id.clone(),
            hook: kind,
            failure: NativeCallbackFailure::Boundary(error),
        });
        return;
    }
    if !completion.is_committed() {
        report.native_failures.push(NativeCallbackFailureRecord {
            plugin_id: slot.id.clone(),
            hook: kind,
            failure: NativeCallbackFailure::Status(completion_failure_status(&completion)),
        });
        return;
    }

    report.delivered += 1;
    commit_native_effects(slot, kind, completion.into_effects(), sink, report);
}

fn commit_native_effects(
    slot: &TrustedNativeSlot,
    kind: EventKind,
    effects: Vec<NativeEffect>,
    sink: &mut dyn CommandSink,
    report: &mut DispatchReport,
) {
    for effect in effects {
        let result = match effect {
            NativeEffect::Intent(intent) => sink.submit(intent),
            NativeEffect::Subscribe(_) | NativeEffect::BlockDecision(_) => {
                Err(IntentError::rejected(
                    "non-intent native effect cannot be committed as an event intent",
                ))
            }
        };
        if let Err(error) = result {
            report.native_failures.push(NativeCallbackFailureRecord {
                plugin_id: slot.id.clone(),
                hook: kind,
                failure: NativeCallbackFailure::CommandSink(error),
            });
            break;
        }
    }
}

fn disable_after_native_panic(
    slot: &mut TrustedNativeSlot,
    kind: EventKind,
    diagnostic: String,
    report: &mut DispatchReport,
) {
    // An unwind may have left plugin-owned state inconsistent. Retire the
    // opaque instance without invoking another plugin callback. The library
    // stays resident, and the host rejects re-enable so this retirement can
    // happen at most once for the registration.
    let _retired = slot.active.take();
    slot.subscriptions.clear();
    slot.state = PluginState::Disabled(DisableReason::Panicked);
    slot.stats.panics = slot.stats.panics.saturating_add(1);
    report.panicked.push(slot.id.clone());
    tracing::warn!(
        plugin = %slot.id,
        hook = ?kind,
        diagnostic = %diagnostic,
        "trusted native plugin cooperatively reported a panic; disabled"
    );
    report.native_panics.push(NativePanicRecord {
        plugin_id: slot.id.clone(),
        hook: kind,
        diagnostic,
    });
}

fn native_panic_diagnostic(completion: &NativeCompletion) -> String {
    completion
        .diagnostics()
        .iter()
        .rev()
        .find(|diagnostic| !diagnostic.message().is_empty())
        .map_or_else(
            || NATIVE_PANIC_DIAGNOSTIC.to_owned(),
            |diagnostic| diagnostic.message().to_owned(),
        )
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        for slot in &mut self.native_plugins {
            let Some(active) = slot.active.take() else {
                continue;
            };
            let mut services = NativeCallbackServices::for_shutdown(slot.capabilities);
            let status = match active.shutdown(&mut services) {
                Ok(status) => status,
                Err(error) => callback_error_status(&error),
            };
            let completion = services.complete(status);
            log_native_diagnostics(&slot.id, completion.diagnostics());
        }
    }
}

fn callback_error_status(error: &CallbackError) -> FcStatus {
    match error {
        CallbackError::Status(status) => *status,
        _ => FC_ERROR,
    }
}

fn completion_failure_status(completion: &NativeCompletion) -> FcStatus {
    if completion.callback_status() != FC_OK {
        completion.callback_status()
    } else if completion.capability_denial().is_some() {
        FC_CAPABILITY_DENIED
    } else {
        completion
            .first_error()
            .map_or(FC_ERROR, crate::native_runtime::NativeServiceError::status)
    }
}

fn log_native_diagnostics(id: &PluginId, diagnostics: &[NativeDiagnostic]) {
    for diagnostic in diagnostics {
        tracing::debug!(
            plugin = %id,
            abi_level = diagnostic.level(),
            message = diagnostic.message(),
            "trusted native plugin diagnostic"
        );
    }
}

fn shutdown_after_failed_enable(active: ActivePlugin, capabilities: CapabilityManifest) {
    let mut services = NativeCallbackServices::for_shutdown(capabilities);
    let status = match active.shutdown(&mut services) {
        Ok(status) => status,
        Err(error) => callback_error_status(&error),
    };
    let _completion = services.complete(status);
}

fn shutdown_native_instance(slot: &mut TrustedNativeSlot) {
    let Some(active) = slot.active.take() else {
        return;
    };
    let mut services = NativeCallbackServices::for_shutdown(slot.capabilities);
    let status = match active.shutdown(&mut services) {
        Ok(status) => status,
        Err(error) => callback_error_status(&error),
    };
    let completion = services.complete(status);
    log_native_diagnostics(&slot.id, completion.diagnostics());
}

fn map_native_capability(capability: NativeCapability) -> Capability {
    match capability {
        NativeCapability::ReadWorld => Capability::ReadWorld,
        NativeCapability::SubmitIntents => Capability::SubmitIntents,
        NativeCapability::RegisterCommands => Capability::RegisterCommands,
        NativeCapability::ReceiveEvents => Capability::ReceiveEvents,
        NativeCapability::ReadPermissions => Capability::ReadPermissions,
        NativeCapability::Storage => Capability::Storage,
        NativeCapability::VetoBlockEdits => Capability::VetoBlockEdits,
        NativeCapability::VetoEvents => Capability::VetoEvents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_plugin_abi::{FC_DIAGNOSTIC_ERROR, FC_DIAGNOSTIC_INFO};
    use ferrumc_plugin_api::Version;
    use ferrumc_plugin_loader::HostServices;

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
    fn native_panic_record_prefers_the_final_nonempty_callback_diagnostic() {
        let mut services = NativeCallbackServices::for_shutdown(CapabilityManifest::empty());
        assert_eq!(
            services.diagnostic(FC_DIAGNOSTIC_INFO, "earlier context".to_owned()),
            FC_OK
        );
        assert_eq!(
            services.diagnostic(FC_DIAGNOSTIC_INFO, String::new()),
            FC_OK
        );
        assert_eq!(
            services.diagnostic(FC_DIAGNOSTIC_ERROR, "panic detail".to_owned()),
            FC_OK
        );

        let completion = services.complete(FC_PLUGIN_PANIC);
        assert_eq!(native_panic_diagnostic(&completion), "panic detail");
    }

    #[test]
    fn native_panic_disposition_wins_over_budget_disabling() {
        assert_eq!(
            native_post_call_disposition(true, true, true),
            NativePostCallDisposition {
                over_budget: true,
                action: NativePostCallAction::DisableForPanic,
            },
            "the overrun remains observable without replacing panic disposition"
        );
        assert_eq!(
            native_post_call_disposition(false, true, true).action,
            NativePostCallAction::DisableForBudget,
        );
        assert_eq!(
            native_post_call_disposition(false, true, false).action,
            NativePostCallAction::KeepEnabled,
        );
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
        BlockBreakAttempt, BlockPlaceAttempt, ChatAttempt, IntentError, InteractAttempt,
        InteractHand, InteractTarget, PluginBlockDecision, PluginError, PluginEventDecision,
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

    /// A plugin that returns a fixed decision from the chat / interact hooks and
    /// records that each was called.
    struct EventDecisionPlugin {
        id: &'static str,
        decision: PluginEventDecision,
    }

    impl EventDecisionPlugin {
        fn boxed(id: &'static str, decision: PluginEventDecision) -> Box<dyn Plugin> {
            Box::new(Self { id, decision })
        }
    }

    impl Plugin for EventDecisionPlugin {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata::new(
                PluginId::new(self.id),
                self.id,
                Version::new(0, 1, 0),
                CapabilityManifest::empty().with(Capability::VetoEvents),
            )
        }
        fn before_chat(
            &mut self,
            _ev: &ChatAttempt,
            _ctx: &mut EventContext<'_>,
        ) -> PluginEventDecision {
            self.decision.clone()
        }
        fn before_interact(
            &mut self,
            _ev: &InteractAttempt,
            _ctx: &mut EventContext<'_>,
        ) -> PluginEventDecision {
            self.decision.clone()
        }
    }

    /// A plugin with no `VetoEvents` capability; its event hooks must never run.
    struct PassiveEventPlugin {
        id: &'static str,
    }

    impl Plugin for PassiveEventPlugin {
        fn metadata(&self) -> PluginMetadata {
            PluginMetadata::new(
                PluginId::new(self.id),
                self.id,
                Version::new(0, 1, 0),
                CapabilityManifest::empty().with(Capability::ReceiveEvents),
            )
        }
        fn before_chat(
            &mut self,
            _ev: &ChatAttempt,
            _ctx: &mut EventContext<'_>,
        ) -> PluginEventDecision {
            panic!("a plugin without VetoEvents must never have its chat hook called");
        }
    }

    /// A recorder that proves a `PlayerMove` notification is delivered.
    struct MoveRecorder {
        id: &'static str,
    }

    impl Plugin for MoveRecorder {
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
            ctx.events()?.subscribe(EventKind::PlayerMove);
            Ok(())
        }
        fn on_event(&mut self, event: &PluginEvent, ctx: &mut EventContext<'_>) {
            if let PluginEvent::PlayerMove { player, .. } = event {
                if let Ok(sink) = ctx.sink() {
                    let _ = sink.submit(WorldIntent::Message {
                        player: *player,
                        message: TextComponent::text("moved"),
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
    fn panicking_plugin_is_disabled_and_fails_safe_to_deny() {
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

    // --- Event-decision surface (chat / interact / move) ----------------------

    fn chat_attempt(message: &str) -> ChatAttempt {
        ChatAttempt::new(PlayerId::offline("Steve"), message)
    }

    fn interact_attempt() -> InteractAttempt {
        InteractAttempt::new(
            PlayerId::offline("Steve"),
            InteractHand::Main,
            InteractTarget::Block {
                pos: BlockPos::new(0, 64, 0),
                face: ferrumc_math::Direction::Up,
            },
        )
    }

    #[test]
    fn chat_deny_drops_the_message() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(EventDecisionPlugin::boxed(
                "filter",
                PluginEventDecision::Deny {
                    message: Some(TextComponent::text("watch it")),
                },
            ))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_chat_decision(
            &chat_attempt("hi"),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert!(resolved.is_deny(), "a Deny drops the chat line");
        assert_eq!(
            resolved.decision(),
            &PluginEventDecision::Deny {
                message: Some(TextComponent::text("watch it")),
            }
        );
    }

    #[test]
    fn chat_allow_passes_through() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(EventDecisionPlugin::boxed(
                "filter",
                PluginEventDecision::Allow,
            ))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_chat_decision(
            &chat_attempt("clean"),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert_eq!(resolved.decision(), &PluginEventDecision::Allow);
        assert_eq!(resolved.report().delivered(), 1, "the hook ran");
    }

    #[test]
    fn event_decisions_are_tallied_per_plugin() {
        let mut host = PluginHost::in_memory();
        // Registration order: allower first, then denier. Only a Deny is
        // absorbing, so the allower runs and tallies an allow before the
        // denier runs, tallies a deny, and short-circuits the rest. This proves
        // each plugin's OWN contribution is recorded, not just the folded result.
        let allower = host
            .register(EventDecisionPlugin::boxed(
                "allower",
                PluginEventDecision::Allow,
            ))
            .expect("registers allower");
        let denier = host
            .register(EventDecisionPlugin::boxed(
                "denier",
                PluginEventDecision::Deny {
                    message: Some(TextComponent::text("nope")),
                },
            ))
            .expect("registers denier");
        enable_all(&mut host, &[&allower, &denier]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_chat_decision(
            &chat_attempt("hi"),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert!(resolved.is_deny(), "the denier vetoes the message");

        let allower_stats = host.stats(&allower).expect("allower registered");
        assert_eq!(allower_stats.allow(), 1, "the allower tallied an allow");
        assert_eq!(allower_stats.deny(), 0);
        let denier_stats = host.stats(&denier).expect("denier registered");
        assert_eq!(denier_stats.deny(), 1, "the denier tallied a deny");
        assert_eq!(denier_stats.allow(), 0);
    }

    #[test]
    fn interact_is_delivered_and_can_deny() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(EventDecisionPlugin::boxed(
                "gate",
                PluginEventDecision::Deny { message: None },
            ))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_interact_decision(
            &interact_attempt(),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert!(resolved.is_deny(), "the interaction was vetoed");
        assert_eq!(resolved.report().delivered(), 1);
    }

    #[test]
    fn first_deny_is_absorbing_for_events() {
        let mut host = PluginHost::in_memory();
        // Registration order: a denier first, then an allower that must never run.
        let denier = host
            .register(EventDecisionPlugin::boxed(
                "denier",
                PluginEventDecision::Deny { message: None },
            ))
            .expect("registers denier");
        let allower = host
            .register(EventDecisionPlugin::boxed(
                "allower",
                PluginEventDecision::Allow,
            ))
            .expect("registers allower");
        enable_all(&mut host, &[&denier, &allower]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_chat_decision(
            &chat_attempt("x"),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert!(resolved.is_deny());
        assert_eq!(
            resolved.report().delivered(),
            1,
            "the absorbing Deny short-circuits the later plugin"
        );
    }

    #[test]
    fn plugin_without_veto_events_is_not_consulted() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(Box::new(PassiveEventPlugin { id: "passive" }))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        // PassiveEventPlugin panics if consulted; an Allow proves it was skipped.
        let resolved = host.dispatch_chat_decision(
            &chat_attempt("x"),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert_eq!(resolved.decision(), &PluginEventDecision::Allow);
        assert_eq!(resolved.report().delivered(), 0);
    }

    #[test]
    fn player_move_notification_is_delivered() {
        let mut host = PluginHost::in_memory();
        let id = host
            .register(Box::new(MoveRecorder { id: "mover" }))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        let report = host.dispatch_event(
            &PluginEvent::PlayerMove {
                player: PlayerId::offline("Steve"),
                from: BlockPos::new(0, 64, 0),
                to: BlockPos::new(1, 64, 0),
            },
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert_eq!(report.delivered(), 1);
        assert_eq!(
            sink.intents.len(),
            1,
            "PlayerMove fired and emitted a message"
        );
    }

    #[test]
    fn event_decision_panic_disables_plugin_and_fails_safe_to_deny() {
        // A plugin that panics in before_chat must be disabled and treated as Deny.
        struct PanicChat {
            id: &'static str,
        }
        impl Plugin for PanicChat {
            fn metadata(&self) -> PluginMetadata {
                PluginMetadata::new(
                    PluginId::new(self.id),
                    self.id,
                    Version::new(0, 1, 0),
                    CapabilityManifest::empty().with(Capability::VetoEvents),
                )
            }
            fn before_chat(
                &mut self,
                _ev: &ChatAttempt,
                _ctx: &mut EventContext<'_>,
            ) -> PluginEventDecision {
                panic!("boom in before_chat");
            }
        }

        let mut host = PluginHost::in_memory();
        let id = host
            .register(Box::new(PanicChat { id: "boom" }))
            .expect("registers");
        enable_all(&mut host, &[&id]);

        let mut sink = RecordingSink::default();
        let resolved = host.dispatch_chat_decision(
            &chat_attempt("x"),
            &NullWorld,
            &mut sink,
            &NullPermissions,
        );
        assert!(resolved.is_deny(), "a panicking filter fails safe to Deny");
        assert_eq!(resolved.report().panicked(), std::slice::from_ref(&id));
        assert_eq!(
            host.state(&id),
            Some(PluginState::Disabled(DisableReason::Panicked))
        );
    }
}
