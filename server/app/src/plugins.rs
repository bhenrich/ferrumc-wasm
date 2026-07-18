//! Plugin bring-up and the per-connection play policy.
//!
//! At startup the application builds one long-lived [`PluginHost`]
//! ([`build_play_policy`]). Depending on configuration, that host registers the
//! built-in [`SpawnProtectPlugin`], [`BlockRulesPlugin`], and [`GreeterPlugin`],
//! then loads, registers, and enables every strict trusted-native bundle from
//! the configured plugins directory. The host is wrapped in a
//! [`BlockEventDispatcher`] every connection shares.
//!
//! The resulting [`PlayPolicy`] bundles the rest of what a connection consults
//! during play: the per-player bypass permissions, the command tree, and the
//! spawn position; the [`BlockEventDispatcher`] carries the plugin event
//! decisions (block edits, chat, and interactions).
//!
//! ## Where block edits are decided
//!
//! Block-edit enforcement is the plugins' `before_block_place` /
//! `before_block_break` decision hooks, consulted by the
//! [`BlockEventDispatcher`] at the *intent boundary* on the connection task —
//! before the edit reaches the simulation, and never inside the deterministic,
//! plugin-free tick. The hooks run under a [`std::sync::Mutex`] with no lock held
//! across an `.await`. An unwinding built-in hook is caught, disabled, and fails
//! the block decision closed. Trusted-native code is not panic-contained:
//! cooperative SDK panic statuses are handled fail-stop, but an abort, hang, or
//! memory-safety failure can still terminate or compromise the process.
//!
//! Trusted-native callbacks receive connection-side metadata explicitly marked
//! as sentinel context: it identifies this off-tick boundary without pretending
//! to be an authoritative simulation tick or logical shard.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use anyhow::Context;
use ferrumc_core::{DimensionId, PlayerId};
use ferrumc_math::{BlockPos, ChunkPos, Vec3, WorldIntent};
use ferrumc_observability::{PluginDecisionSnapshot, PluginDecisions};
use ferrumc_permission::{Grant, PermissionNode, Resolution, Subject};
use ferrumc_plugin_api::{
    BlockBreakAttempt, BlockPlaceAttempt, ChatAttempt, CommandSink, EventKind, IntentError,
    InteractAttempt, PermissionApi, PluginEvent, PluginEventDecision, WorldView,
    MAX_EMITTED_INTENTS,
};
use ferrumc_plugin_block_rules::BlockRulesPlugin;
use ferrumc_plugin_greeter::GreeterPlugin;
use ferrumc_plugin_host::{
    DispatchReport, InMemoryPluginStorage, NativeCapabilityDenial, NativeEventContext, PluginHost,
    PluginStorageBackend, ResolvedDecision, ResolvedEventOutcome,
};
use ferrumc_plugin_loader::{PluginCapabilities, PluginCapability, PluginLoader};
use ferrumc_plugin_spawn_protect::{bypass_node, SpawnProtect, SpawnProtectPlugin, CONFIG_KEY};

use crate::command::build_command_tree_with_limits;
use crate::config::AppConfig;

/// Permission *level* granted to a configured operator.
///
/// Mirrors vanilla's top operator tier (level 4): it satisfies every operator
/// gate, including [`GAMEMODE_LEVEL`](crate::command::GAMEMODE_LEVEL). Only the
/// players named in [`AppConfig::ops`] act at this level; everyone else acts at
/// the configured [`AppConfig::default_permission_level`] (0 by default), so the
/// gate is meaningful instead of granting every connection operator rights.
pub(crate) const OPERATOR_PERMISSION_LEVEL: u8 = 4;

/// Trusted-native capabilities the production connection boundary implements.
///
/// This is deliberately narrower than the ABI vocabulary. In particular, the
/// app does not advertise authoritative world reads, permission reads, storage,
/// command registration, or non-block vetoes until those facades are wired.
const NATIVE_PLUGIN_CAPABILITIES: PluginCapabilities = PluginCapabilities::empty()
    .with(PluginCapability::ReceiveEvents)
    .with(PluginCapability::SubmitIntents)
    .with(PluginCapability::VetoBlockEdits);

/// A read-only registry mapping players to their permission [`Subject`].
///
/// Built once at startup from the configured bypass list; queried per block edit
/// (through the [`PermissionFacade`] the spawn-protect plugin consults for the
/// bypass node) and per command (for node-string checks).
#[derive(Debug, Default)]
pub(crate) struct PermissionRegistry {
    subjects: BTreeMap<PlayerId, Subject>,
}

impl PermissionRegistry {
    /// Builds a registry granting the spawn-protection bypass node to every
    /// player named in `bypass_names`.
    ///
    /// # Errors
    ///
    /// Returns an error only if the bypass permission constant cannot be parsed
    /// (it always can in shipped builds).
    fn from_bypass_names(bypass_names: &[String]) -> anyhow::Result<Self> {
        let bypass = bypass_node()?;
        let mut subjects = BTreeMap::new();
        for name in bypass_names {
            let mut subject = Subject::new();
            subject.add_grant(Grant::allow(bypass.clone()));
            subjects.insert(PlayerId::offline(name), subject);
        }
        Ok(Self { subjects })
    }

    /// Returns whether `player` is granted the permission `node` (a node string,
    /// as a command node declares). A malformed node string is treated as denied.
    pub(crate) fn is_allowed(&self, player: PlayerId, node: &str) -> bool {
        let Ok(node) = PermissionNode::parse(node) else {
            return false;
        };
        self.subjects
            .get(&player)
            .is_some_and(|subject| subject.has_permission(&node))
    }

    /// Returns whether `player` is granted the typed permission `node`, treating an
    /// unset node as denied (closed by default).
    ///
    /// The typed counterpart of [`is_allowed`](Self::is_allowed), used by the
    /// [`PermissionFacade`] the plugin host queries during a block decision.
    fn has_node(&self, player: PlayerId, node: &PermissionNode) -> bool {
        self.subjects
            .get(&player)
            .is_some_and(|subject| subject.has_permission(node))
    }

    /// Resolves `node` for `player` to a tri-state [`Resolution`] (distinguishing
    /// an explicit deny from an unset node).
    fn resolve_node(&self, player: PlayerId, node: &PermissionNode) -> Resolution {
        self.subjects
            .get(&player)
            .map_or(Resolution::Unset, |subject| subject.resolve(node))
    }
}

/// A [`PermissionApi`] facade over a [`PermissionRegistry`].
///
/// The plugin host's block-decision call needs a `&dyn PermissionApi`; this thin
/// adapter borrows the registry for the duration of one synchronous dispatch (it
/// is never held across an `.await`). The registry is read-only, so the facade
/// only ever *answers* permission queries — a plugin can never grant or revoke.
pub(crate) struct PermissionFacade<'a> {
    registry: &'a PermissionRegistry,
}

impl<'a> PermissionFacade<'a> {
    /// Wraps `registry` as a permission facade.
    pub(crate) fn new(registry: &'a PermissionRegistry) -> Self {
        Self { registry }
    }
}

impl PermissionApi for PermissionFacade<'_> {
    fn has_permission(&self, player: PlayerId, node: &PermissionNode) -> bool {
        self.registry.has_node(player, node)
    }

    fn resolve(&self, player: PlayerId, node: &PermissionNode) -> Resolution {
        self.registry.resolve_node(player, node)
    }
}

/// The [`WorldView`] handed to plugins during a connection-side decision.
///
/// The decision runs on the connection task, which is forbidden from reading
/// authoritative world state (only the simulation owns it). So plugins consulted
/// at the intent boundary see a view that reports nothing *loaded*: they decide
/// from the incoming attempt (position, state, actor) and permissions, not from
/// live block reads.
///
/// The one piece of real state the connection *does* know is the acting player's
/// own position (the connection mirrors every reported move), so
/// [`player_position`](WorldView::player_position) returns it for that player.
/// Authoritative block reads ([`is_chunk_loaded`](WorldView::is_chunk_loaded) /
/// [`block_state_id`](WorldView::block_state_id)) remain `None`/`false`: serving
/// them needs a sim query channel routed off the connection task, a documented
/// future milestone.
struct ConnectionWorldView {
    /// The acting player whose position this view can answer.
    player: PlayerId,
    /// The acting player's last reported position, if the connection knows it.
    position: Option<Vec3>,
}

impl ConnectionWorldView {
    /// Builds a view bound to `player`, optionally carrying their last reported
    /// `position`. Block decisions pass `None` (they do not consult position);
    /// chat/interact/move dispatch passes the connection's last known position.
    fn new(player: PlayerId, position: Option<Vec3>) -> Self {
        Self { player, position }
    }
}

impl WorldView for ConnectionWorldView {
    fn dimension(&self) -> DimensionId {
        DimensionId::new(0)
    }
    fn is_chunk_loaded(&self, _chunk: ChunkPos) -> bool {
        // Authoritative residency is owned by the simulation; the connection task
        // cannot read it. Future milestone: route through a sim query channel.
        false
    }
    fn block_state_id(&self, _pos: BlockPos) -> Option<u32> {
        // See `is_chunk_loaded`: authoritative block reads are a future milestone.
        None
    }
    fn player_position(&self, player: PlayerId) -> Option<Vec3> {
        // The connection knows only the acting player's own position.
        if player == self.player {
            self.position
        } else {
            None
        }
    }
}

/// A bounded [`CommandSink`] that collects the intents a plugin submits during a
/// single dispatch.
///
/// Capped at [`MAX_EMITTED_INTENTS`]: once full, further [`submit`](CommandSink::submit)
/// calls return [`IntentError::QueueFull`] rather than growing without bound, so a
/// misbehaving plugin cannot flood the simulation from one event. The collected
/// intents are drained by the dispatcher and routed by the connection.
struct CollectingSink {
    intents: Vec<WorldIntent>,
}

impl CollectingSink {
    fn new() -> Self {
        Self {
            intents: Vec::new(),
        }
    }

    fn into_intents(self) -> Vec<WorldIntent> {
        self.intents
    }
}

impl CommandSink for CollectingSink {
    fn submit(&mut self, intent: WorldIntent) -> Result<(), IntentError> {
        if self.intents.len() >= MAX_EMITTED_INTENTS {
            return Err(IntentError::QueueFull);
        }
        self.intents.push(intent);
        Ok(())
    }
}

/// The long-lived plugin host wrapped for shared, off-tick plugin-event dispatch.
///
/// Despite the name (kept for the block-edit path it grew from) this is the
/// connection's single entry point to the plugin host for *every* off-tick event:
/// the `before_block_*` decisions, the `after_block_*` notifications, and the chat
/// / interact / move events this milestone adds. All run synchronously under the
/// mutex, with no lock held across an `.await`.
///
/// Shared behind an [`Arc`](std::sync::Arc) by every connection task (it lives on
/// [`ConnContext`](crate::connection::ConnContext)). Connection tasks run
/// concurrently, but [`PluginHost`] is `Send` and not `Sync` (plugins are called
/// one at a time), so it is guarded by a [`std::sync::Mutex`]. The plugin calls
/// are synchronous: the lock is acquired, the decision is folded, and the lock
/// is released *before* any async I/O (acks, resyncs, system-chat, intent
/// routing) happens, so the guard is never held across an `.await`. Built-in
/// plugin unwinds are caught and disable that plugin; trusted native callbacks
/// retain the process-level failure limits documented by [`PluginHost`]. This is
/// a per-edit serialization point, not a broad lock over world state: it guards
/// only the plugin registry, which owns no world.
pub(crate) struct BlockEventDispatcher {
    host: Mutex<PluginHost>,
    /// Native failure warnings already emitted, keyed by stable plugin and hook.
    ///
    /// The set is bounded by the host's plugin limit multiplied by the finite
    /// [`EventKind`] vocabulary. It prevents a client-triggerable failing
    /// callback from amplifying the same warning on every event.
    native_failure_warnings: Mutex<BTreeSet<(ferrumc_core::PluginId, EventKind)>>,
}

impl BlockEventDispatcher {
    /// Wraps an enabled [`PluginHost`] for shared dispatch.
    pub(crate) fn new(host: PluginHost) -> Self {
        Self {
            host: Mutex::new(host),
            native_failure_warnings: Mutex::new(BTreeSet::new()),
        }
    }

    /// Snapshots each plugin's cumulative event-decision counts — across block
    /// edits, chat, and interactions — for the live `ServerSnapshot`.
    ///
    /// Locks the host briefly to copy the per-plugin
    /// [`PluginDecisionReport`](ferrumc_plugin_host::PluginDecisionReport) tally
    /// and maps it onto the observability vocabulary. Cheap and bounded (one row
    /// per registered plugin); the driver calls it once per tick, contending with
    /// the connection tasks' decision dispatch only for the brief copy, never
    /// across an `.await`.
    pub(crate) fn decision_snapshots(&self) -> Vec<PluginDecisionSnapshot> {
        self.lock()
            .plugin_decision_reports()
            .into_iter()
            .map(|report| PluginDecisionSnapshot {
                plugin_name: report.name,
                decisions: PluginDecisions {
                    allow: report.allow,
                    deny: report.deny,
                    replace: report.replace,
                    panic: report.panic,
                },
            })
            .collect()
    }

    /// Locks the host, recovering the guard even if a previous holder panicked.
    ///
    /// Built-in plugin unwinds are caught inside the host
    /// ([`std::panic::catch_unwind`]), so those callbacks do not poison this
    /// mutex. Recovering from any other Rust-side poison keeps a stray panic from
    /// wedging later dispatch; it does not contain native process failures.
    fn lock(&self) -> std::sync::MutexGuard<'_, PluginHost> {
        self.host
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Consults the loaded plugins about a pending block *placement*, returning the
    /// combined [`ResolvedDecision`] and any world-mutation intents to route.
    ///
    /// Runs the synchronous host dispatch under the lock and returns owned data,
    /// so the caller does its async routing after the lock is released. An
    /// unwinding built-in voter is caught, disabled, and fails the decision
    /// closed; trusted-native callbacks retain their documented process-level
    /// failure limits.
    pub(crate) fn before_block_place(
        &self,
        attempt: &BlockPlaceAttempt,
        permissions: &dyn PermissionApi,
    ) -> (ResolvedDecision, Vec<WorldIntent>) {
        let world = ConnectionWorldView::new(attempt.player(), None);
        let mut sink = CollectingSink::new();
        let resolved = self
            .lock()
            .dispatch_block_place_decision_with_native_context(
                attempt,
                NativeEventContext::connection_side(),
                &world,
                &mut sink,
                permissions,
            );
        self.record_native_dispatch_failures(resolved.report());
        let (decision, mut emitted) = resolved.into_parts();
        // A Deny prevents the edit, so intents conditioned on it proceeding must not
        // run: the host already drops the folded `EmitIntents` on a Deny, and the
        // intents a plugin pushed to the sink are likewise dropped here. On
        // Allow/Replace the edit proceeds and the sink intents route alongside it.
        if !matches!(decision, ResolvedDecision::Deny { .. }) {
            emitted.extend(sink.into_intents());
        }
        (decision, emitted)
    }

    /// Consults the loaded plugins about a pending block *break*. See
    /// [`before_block_place`](Self::before_block_place).
    pub(crate) fn before_block_break(
        &self,
        attempt: &BlockBreakAttempt,
        permissions: &dyn PermissionApi,
    ) -> (ResolvedDecision, Vec<WorldIntent>) {
        let world = ConnectionWorldView::new(attempt.player(), None);
        let mut sink = CollectingSink::new();
        let resolved = self
            .lock()
            .dispatch_block_break_decision_with_native_context(
                attempt,
                NativeEventContext::connection_side(),
                &world,
                &mut sink,
                permissions,
            );
        self.record_native_dispatch_failures(resolved.report());
        let (decision, mut emitted) = resolved.into_parts();
        // A Deny prevents the edit; drop both intent channels (see
        // [`before_block_place`](Self::before_block_place)).
        if !matches!(decision, ResolvedDecision::Deny { .. }) {
            emitted.extend(sink.into_intents());
        }
        (decision, emitted)
    }

    /// Fires the [`PluginEvent::AfterBlockPlace`] notification to subscribers,
    /// returning any intents they emitted (e.g. messages) for the caller to route.
    ///
    /// "After" means accepted at the intent boundary and routed to the simulation
    /// (the `before_*` decision allowed/replaced the edit and the command was
    /// sent), not tick-confirmed — the simulation may still reject the edit for
    /// reach or chunk residency. A tick-confirmed variant is a documented
    /// follow-up.
    pub(crate) fn after_block_place(
        &self,
        player: PlayerId,
        pos: BlockPos,
        block_state_id: u32,
        permissions: &dyn PermissionApi,
    ) -> Vec<WorldIntent> {
        self.dispatch_after(
            player,
            &PluginEvent::AfterBlockPlace {
                player,
                pos,
                block_state_id,
            },
            permissions,
        )
    }

    /// Fires the [`PluginEvent::AfterBlockBreak`] notification. See
    /// [`after_block_place`](Self::after_block_place).
    pub(crate) fn after_block_break(
        &self,
        player: PlayerId,
        pos: BlockPos,
        permissions: &dyn PermissionApi,
    ) -> Vec<WorldIntent> {
        self.dispatch_after(
            player,
            &PluginEvent::AfterBlockBreak { player, pos },
            permissions,
        )
    }

    /// Consults the loaded plugins about a pending *chat message* (before it is
    /// broadcast), returning the combined [`PluginEventDecision`] and any intents
    /// subscribers emitted to route.
    ///
    /// `position` is the sender's last known position (the connection mirrors it),
    /// surfaced to plugins through [`ConnectionWorldView`]. Runs the synchronous
    /// built-in decision dispatch under the lock and returns owned data, so the
    /// caller does its async routing after the lock is released. On a `Deny` the
    /// intents are dropped (the dropped message must not still trigger side effects).
    pub(crate) fn before_chat(
        &self,
        attempt: &ChatAttempt,
        position: Option<Vec3>,
        permissions: &dyn PermissionApi,
    ) -> (PluginEventDecision, Vec<WorldIntent>) {
        let world = ConnectionWorldView::new(attempt.player(), position);
        let mut sink = CollectingSink::new();
        let resolved = self
            .lock()
            .dispatch_chat_decision(attempt, &world, &mut sink, permissions);
        Self::finish_event_decision(resolved, sink)
    }

    /// Consults the loaded plugins about a pending *interaction* (a right-click).
    /// See [`before_chat`](Self::before_chat) for the shared semantics.
    pub(crate) fn before_interact(
        &self,
        attempt: &InteractAttempt,
        position: Option<Vec3>,
        permissions: &dyn PermissionApi,
    ) -> (PluginEventDecision, Vec<WorldIntent>) {
        let world = ConnectionWorldView::new(attempt.player(), position);
        let mut sink = CollectingSink::new();
        let resolved =
            self.lock()
                .dispatch_interact_decision(attempt, &world, &mut sink, permissions);
        Self::finish_event_decision(resolved, sink)
    }

    /// Folds a resolved event decision and its sink into the decision plus the
    /// intents to route: a `Deny` drops the intents (the dropped event must not
    /// still execute a plugin's side effect), an `Allow` keeps them.
    fn finish_event_decision(
        resolved: ResolvedEventOutcome,
        sink: CollectingSink,
    ) -> (PluginEventDecision, Vec<WorldIntent>) {
        let decision = resolved.into_decision();
        let emitted = if decision.is_deny() {
            Vec::new()
        } else {
            sink.into_intents()
        };
        (decision, emitted)
    }

    /// Fires the observe-only [`PluginEvent::PlayerMove`] notification to
    /// subscribers, returning any intents they emitted for the caller to route.
    ///
    /// `position` is the player's new position (the move that triggered this),
    /// surfaced through [`ConnectionWorldView`]. Movement cannot be vetoed through
    /// this surface; the connection throttles the call to one per block crossing.
    pub(crate) fn player_move(
        &self,
        player: PlayerId,
        from: BlockPos,
        to: BlockPos,
        position: Option<Vec3>,
        permissions: &dyn PermissionApi,
    ) -> Vec<WorldIntent> {
        self.dispatch_notification(
            player,
            position,
            &PluginEvent::PlayerMove { player, from, to },
            permissions,
        )
    }

    /// Shared body of the `after_*` notifications: dispatch the event with no
    /// position context (a block edit carries its own position) and drain the
    /// intents subscribers emitted.
    fn dispatch_after(
        &self,
        player: PlayerId,
        event: &PluginEvent,
        permissions: &dyn PermissionApi,
    ) -> Vec<WorldIntent> {
        self.dispatch_notification(player, None, event, permissions)
    }

    /// Shared body of every observe-only notification: build the connection world
    /// view for `player` (carrying `position` when known), dispatch `event`, and
    /// drain the intents subscribers emitted.
    fn dispatch_notification(
        &self,
        player: PlayerId,
        position: Option<Vec3>,
        event: &PluginEvent,
        permissions: &dyn PermissionApi,
    ) -> Vec<WorldIntent> {
        let world = ConnectionWorldView::new(player, position);
        let mut sink = CollectingSink::new();
        let report = self.lock().dispatch_event_with_native_context(
            event,
            NativeEventContext::connection_side(),
            &world,
            &mut sink,
            permissions,
        );
        self.record_native_dispatch_failures(&report);
        sink.into_intents()
    }

    /// Emits one operational diagnostic per trusted-native plugin and hook.
    ///
    /// The host makes failures observable through [`DispatchReport`] instead of
    /// logging policy at the library boundary. The application must consume that
    /// report: otherwise a malformed or capability-denied callback could keep
    /// failing block edits closed with no clue in the server log.
    fn record_native_dispatch_failures(&self, report: &DispatchReport) {
        let mut warned = self
            .native_failure_warnings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for failure in report.native_failures() {
            let key = (failure.plugin_id().clone(), failure.hook());
            if !warned.insert(key) {
                continue;
            }
            let denied_capability = report
                .native_capability_denials()
                .iter()
                .find(|denial| denial.plugin_id() == failure.plugin_id())
                .map(NativeCapabilityDenial::capability);
            tracing::warn!(
                plugin = %failure.plugin_id(),
                hook = ?failure.hook(),
                failure = ?failure.failure(),
                ?denied_capability,
                "trusted-native plugin callback failed during connection-side dispatch"
            );
        }
    }
}

/// Everything a connection consults during the play phase.
///
/// Shared behind an [`Arc`](std::sync::Arc) by every connection: the
/// configured [`SpawnProtect`] value, the [`PermissionRegistry`], the command
/// tree, the spawn position teleports return to, and the permission level
/// players act at. The spawn-protection value is informational; only an active
/// plugin enforces it.
pub(crate) struct PlayPolicy {
    guard: SpawnProtect,
    permissions: PermissionRegistry,
    command_tree: ferrumc_command::CommandTree,
    spawn: Vec3,
    /// Players granted operator status; they act at [`OPERATOR_PERMISSION_LEVEL`].
    ops: BTreeSet<PlayerId>,
    /// Level every non-operator player acts at.
    default_permission_level: u8,
}

impl PlayPolicy {
    /// Returns the configured spawn-protection policy.
    ///
    /// Informational only: when built-ins are active, this is the value read back
    /// from the spawn-protect plugin's namespaced storage. When built-ins are
    /// disabled, it is the unapplied configuration seed. Enforcement lives only
    /// in active plugins' `before_block_*` decision hooks, not in a bespoke
    /// connection-side veto.
    pub(crate) fn guard(&self) -> SpawnProtect {
        self.guard
    }

    /// Returns the permission registry.
    pub(crate) fn permissions(&self) -> &PermissionRegistry {
        &self.permissions
    }

    /// Returns the command tree.
    pub(crate) fn command_tree(&self) -> &ferrumc_command::CommandTree {
        &self.command_tree
    }

    /// Returns the world-spawn position `/spawn` teleports to.
    pub(crate) fn spawn(&self) -> Vec3 {
        self.spawn
    }

    /// Returns the permission level `player` acts at: [`OPERATOR_PERMISSION_LEVEL`]
    /// for a configured operator, otherwise the configured default level.
    pub(crate) fn permission_level(&self, player: PlayerId) -> u8 {
        if self.ops.contains(&player) {
            OPERATOR_PERMISSION_LEVEL
        } else {
            self.default_permission_level
        }
    }
}

/// Builds the [`PlayPolicy`] and the long-lived [`BlockEventDispatcher`] for a
/// configured server.
///
/// Builds a persistent [`PluginHost`] and optionally registers + enables the
/// built-in plugins that participate at the intent boundary:
///
/// - [`SpawnProtectPlugin`], which seeds the spawn-protection configuration
///   (centre = spawn column, radius = [`AppConfig::spawn_protect_radius`]) into
///   its private, namespaced storage and enforces it through its
///   `before_block_*` decision hooks. The effective policy is read back from that
///   storage into [`PlayPolicy::guard`] (the namespaced-storage round-trip the
///   deliverable requires); a radius of zero yields a disabled policy that vetoes
///   nothing. The guard value is informational now — enforcement lives in the
///   plugin, not in a bespoke connection-side veto.
/// - [`BlockRulesPlugin`], the second sample, which denies placing a configured
///   block and rewrites another (proving `Deny` and `Replace`).
///
/// If [`AppConfig::plugins_dir`] is configured, each immediate-child directory
/// containing `plugin.toml` is treated as a strict bundle candidate; other
/// child directories are ignored. Candidates are validated, registered, and
/// enabled in deterministic plugin-id order on that same host. Any load,
/// duplicate-id, initialization, or enable failure aborts server startup before
/// it accepts connections. A library mapped before a later failure remains
/// process-resident, as required by the trusted-native lifetime contract.
///
/// The host is moved into the returned [`BlockEventDispatcher`], which every
/// connection shares to run the decision hooks off the simulation tick.
///
/// # Errors
///
/// Returns an error if a plugin bundle cannot be loaded, registered, or enabled,
/// if reading the built-in spawn-protection configuration back fails, or if the
/// bypass permission node is invalid.
pub(crate) fn build_play_policy(
    config: &AppConfig,
) -> anyhow::Result<(PlayPolicy, BlockEventDispatcher)> {
    let spawn = config.spawn();
    let center_x = spawn.x.floor() as i32;
    let center_z = spawn.z.floor() as i32;
    let seed = SpawnProtect::new(center_x, center_z, config.spawn_protect_radius());

    // The long-lived host: register + enable the in-process plugins that vote on
    // block edits at the intent boundary. The shared storage clone lets us read
    // the spawn-protect config back after it seeds itself.
    let storage = InMemoryPluginStorage::new();
    let mut host = PluginHost::new(Box::new(storage.clone()));

    let guard = if config.builtin_plugins() {
        let spawn_protect_id = host.register(Box::new(SpawnProtectPlugin::new(seed)))?;
        host.enable(&spawn_protect_id)?;
        let guard = storage
            .get(&spawn_protect_id, CONFIG_KEY)?
            .and_then(|bytes| SpawnProtect::from_bytes(&bytes))
            .unwrap_or(seed);

        let block_rules_id = host.register(Box::new(BlockRulesPlugin::new()))?;
        host.enable(&block_rules_id)?;

        // The greeter sample exercises the event surface: greet on join, filter a
        // banned chat word (Deny), and observe move/interact.
        let greeter_id = host.register(Box::new(GreeterPlugin::new()))?;
        host.enable(&greeter_id)?;
        guard
    } else {
        seed
    };

    if let Some(dir) = config.plugins_dir() {
        load_trusted_native_plugins(&mut host, dir)?;
    }

    let permissions = PermissionRegistry::from_bypass_names(config.spawn_protect_bypass())?;
    let ops = config
        .ops()
        .iter()
        .map(|name| PlayerId::offline(name))
        .collect();

    let policy = PlayPolicy {
        guard,
        permissions,
        command_tree: build_command_tree_with_limits(config.max_region_fill_volume()),
        spawn,
        ops,
        default_permission_level: config.default_permission_level(),
    };
    tracing::debug!(
        center = ?policy.guard().center(),
        radius = policy.guard().radius(),
        builtin_plugins = config.builtin_plugins(),
        "spawn-protection configuration prepared"
    );
    Ok((policy, BlockEventDispatcher::new(host)))
}

/// Loads and enables every configured trusted-native bundle on the live host.
fn load_trusted_native_plugins(host: &mut PluginHost, dir: &std::path::Path) -> anyhow::Result<()> {
    let loader = PluginLoader::current(NATIVE_PLUGIN_CAPABILITIES)
        .context("constructing the trusted-native plugin loader policy")?;
    let plugins = loader
        .load_directory(dir)
        .with_context(|| format!("loading plugin bundles from {}", dir.display()))?;
    let count = plugins.len();

    for plugin in plugins.into_plugins() {
        let manifest_id = plugin.manifest().id().to_owned();
        let id = host
            .register_trusted_native(plugin)
            .with_context(|| format!("registering trusted-native plugin {manifest_id}"))?;
        host.enable(&id)
            .with_context(|| format!("enabling trusted-native plugin {manifest_id}"))?;
    }

    tracing::info!(plugins = count, dir = %dir.display(), "enabled trusted-native plugins");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(radius: i32, bypass: &[&str]) -> AppConfig {
        let bypass = bypass
            .iter()
            .map(|name| format!("{name:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        AppConfig::from_toml_str(&format!(
            "spawn_protect_radius = {radius}\nspawn_protect_bypass = [{bypass}]\n"
        ))
        .expect("spawn-protection test config is valid")
    }

    #[test]
    fn policy_reflects_configured_radius_and_center() {
        let config = config_with(16, &[]);
        let (policy, _dispatcher) = build_play_policy(&config).expect("policy builds");
        // Default spawn is (8, 64, 8) -> centre column (8, 8).
        assert_eq!(policy.guard().center(), (8, 8));
        assert_eq!(policy.guard().radius(), 16);
        assert!(policy.guard().is_enabled());
    }

    #[test]
    fn zero_radius_is_disabled() {
        let (policy, _dispatcher) = build_play_policy(&config_with(0, &[])).expect("policy builds");
        assert!(!policy.guard().is_enabled());
    }

    #[test]
    fn bypass_list_grants_only_named_players() {
        let (policy, _dispatcher) =
            build_play_policy(&config_with(16, &["Admin"])).expect("policy builds");
        let node = bypass_node().expect("valid bypass node");
        // The bypass grant is queried through the typed permission facade the
        // spawn-protect plugin uses at the intent boundary.
        assert!(policy
            .permissions()
            .has_node(PlayerId::offline("Admin"), &node));
        assert!(!policy
            .permissions()
            .has_node(PlayerId::offline("Griefer"), &node));
    }

    #[test]
    fn operators_act_at_operator_level_others_at_the_default() {
        let config = AppConfig::from_toml_str("ops = [\"Admin\"]\ndefault_permission_level = 0\n")
            .expect("operator test config is valid");
        let (policy, _dispatcher) = build_play_policy(&config).expect("policy builds");
        assert_eq!(
            policy.permission_level(PlayerId::offline("Admin")),
            OPERATOR_PERMISSION_LEVEL
        );
        assert_eq!(policy.permission_level(PlayerId::offline("Random")), 0);
    }

    // --- BlockEventDispatcher: the wired intent boundary ----------------------
    #[test]
    fn disabling_builtins_registers_no_plugins_and_applies_no_block_rule() {
        let config = AppConfig::from_toml_str("builtin_plugins = false")
            .expect("built-in plugin toggle is valid");
        let (policy, dispatcher) = build_play_policy(&config).expect("policy builds");
        let permissions = PermissionFacade::new(policy.permissions());
        let (decision, emitted) = dispatcher.before_block_place(
            &BlockPlaceAttempt::new(
                PlayerId::offline("Steve"),
                BlockPos::new(100, 64, 100),
                ferrumc_plugin_block_rules::GLASS_BLOCK_STATE_ID,
            ),
            &permissions,
        );

        assert!(dispatcher.decision_snapshots().is_empty());
        assert_eq!(decision, ResolvedDecision::Allow);
        assert!(emitted.is_empty());
    }

    #[test]
    fn dispatcher_denies_a_protected_break() {
        let (policy, dispatcher) = build_play_policy(&config_with(16, &[])).expect("policy builds");
        let perms = PermissionFacade::new(policy.permissions());
        // (8, 63, 8) is the protected spawn column; Griefer has no bypass.
        let (decision, _emitted) = dispatcher.before_block_break(
            &BlockBreakAttempt::new(PlayerId::offline("Griefer"), BlockPos::new(8, 63, 8)),
            &perms,
        );
        assert!(matches!(decision, ResolvedDecision::Deny { .. }));
    }

    #[test]
    fn dispatcher_allows_a_protected_break_for_a_bypassing_player() {
        let (policy, dispatcher) =
            build_play_policy(&config_with(16, &["Admin"])).expect("policy builds");
        let perms = PermissionFacade::new(policy.permissions());
        let (decision, _emitted) = dispatcher.before_block_break(
            &BlockBreakAttempt::new(PlayerId::offline("Admin"), BlockPos::new(8, 63, 8)),
            &perms,
        );
        assert_eq!(decision, ResolvedDecision::Allow);
    }

    #[test]
    fn dispatcher_denies_bedrock_placement_via_block_rules() {
        // radius 0 disables spawn protection, so the block-rules plugin is the only
        // voter — proving its Deny reaches the wired dispatcher.
        let (policy, dispatcher) = build_play_policy(&config_with(0, &[])).expect("policy builds");
        let perms = PermissionFacade::new(policy.permissions());
        let (decision, _emitted) = dispatcher.before_block_place(
            &BlockPlaceAttempt::new(
                PlayerId::offline("Steve"),
                BlockPos::new(100, 64, 100),
                ferrumc_plugin_block_rules::DENIED_BLOCK_STATE_ID,
            ),
            &perms,
        );
        assert!(matches!(
            decision,
            ResolvedDecision::Deny { message: Some(_) }
        ));
    }

    #[test]
    fn dispatcher_replaces_glass_placement_via_block_rules() {
        let (policy, dispatcher) = build_play_policy(&config_with(0, &[])).expect("policy builds");
        let perms = PermissionFacade::new(policy.permissions());
        let (decision, _emitted) = dispatcher.before_block_place(
            &BlockPlaceAttempt::new(
                PlayerId::offline("Steve"),
                BlockPos::new(100, 64, 100),
                ferrumc_plugin_block_rules::GLASS_BLOCK_STATE_ID,
            ),
            &perms,
        );
        assert_eq!(
            decision,
            ResolvedDecision::Replace {
                block_state_id: ferrumc_plugin_block_rules::TINTED_GLASS_BLOCK_STATE_ID
            }
        );
    }

    #[test]
    fn decision_snapshots_reflect_dispatched_outcomes() {
        // radius 0 disables spawn protection (it then allows everything), leaving
        // block_rules as the deciding voter: bedrock is denied, glass is replaced.
        let (policy, dispatcher) = build_play_policy(&config_with(0, &[])).expect("policy builds");
        let perms = PermissionFacade::new(policy.permissions());
        let steve = PlayerId::offline("Steve");
        let pos = BlockPos::new(100, 64, 100);

        let _ = dispatcher.before_block_place(
            &BlockPlaceAttempt::new(
                steve,
                pos,
                ferrumc_plugin_block_rules::DENIED_BLOCK_STATE_ID,
            ),
            &perms,
        );
        let _ = dispatcher.before_block_place(
            &BlockPlaceAttempt::new(steve, pos, ferrumc_plugin_block_rules::GLASS_BLOCK_STATE_ID),
            &perms,
        );
        // `1` is stone: allowed by everyone.
        let _ = dispatcher.before_block_place(&BlockPlaceAttempt::new(steve, pos, 1), &perms);

        let snaps = dispatcher.decision_snapshots();
        assert!(!snaps.is_empty(), "every registered plugin appears");
        let deny: u64 = snaps.iter().map(|s| s.decisions.deny).sum();
        let replace: u64 = snaps.iter().map(|s| s.decisions.replace).sum();
        let allow: u64 = snaps.iter().map(|s| s.decisions.allow).sum();
        assert_eq!(deny, 1, "exactly one placement was denied");
        assert_eq!(replace, 1, "exactly one placement was replaced");
        assert!(allow >= 1, "the plain placement was allowed");
    }

    #[test]
    fn dispatcher_allows_a_plain_placement() {
        let (policy, dispatcher) = build_play_policy(&config_with(0, &[])).expect("policy builds");
        let perms = PermissionFacade::new(policy.permissions());
        // `1` is stone: neither denied nor rewritten.
        let (decision, emitted) = dispatcher.before_block_place(
            &BlockPlaceAttempt::new(PlayerId::offline("Steve"), BlockPos::new(100, 64, 100), 1),
            &perms,
        );
        assert_eq!(decision, ResolvedDecision::Allow);
        assert!(emitted.is_empty());
    }
}
