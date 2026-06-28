//! Plugin bring-up and the per-connection play policy.
//!
//! At startup the application:
//!
//! 1. scans the configured `/plugins` directory and loads every dynamic
//!    (`cdylib`) plugin across the C ABI ([`load_plugins`]), proving the dynamic
//!    loader end to end; and
//! 2. builds one long-lived [`PluginHost`] ([`build_play_policy`]) and enables the
//!    in-process plugins that vote on block edits at the intent boundary:
//!    [`SpawnProtectPlugin`] (which also seeds and reads back its configuration
//!    from private, namespaced storage) and the [`BlockRulesPlugin`] sample. The
//!    host is wrapped in a [`BlockEventDispatcher`] every connection shares.
//!
//! The resulting [`PlayPolicy`] bundles the rest of what a connection consults
//! during play: the per-player bypass permissions, the command tree, and the
//! spawn position; the [`BlockEventDispatcher`] carries the block-edit decisions.
//!
//! ## Where block edits are decided
//!
//! Block-edit enforcement is the plugins' `before_block_place` /
//! `before_block_break` decision hooks, consulted by the
//! [`BlockEventDispatcher`] at the *intent boundary* on the connection task —
//! before the edit reaches the simulation, and never inside the deterministic,
//! plugin-free tick. The hooks run under a [`std::sync::Mutex`] with no lock held
//! across an `.await`, and a panicking plugin is contained and fails safe to a
//! deny.
//!
//! The C ABI carries no event hook, so a dynamically-loaded `cdylib` cannot
//! receive a block decision across the boundary (the block-decision surface is
//! in-process only). The dynamic load therefore proves the loader; the in-process
//! plugins provide the real decisions and exercise the full SDK the deliverable
//! names: the veto-block-edits decision hooks, namespaced storage, and the
//! permission API for the bypass node.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use ferrumc_core::{DimensionId, PlayerId};
use ferrumc_math::{BlockPos, ChunkPos, Vec3, WorldIntent};
use ferrumc_permission::{Grant, PermissionNode, Resolution, Subject};
use ferrumc_plugin_api::{
    BlockBreakAttempt, BlockPlaceAttempt, CommandSink, IntentError, PermissionApi, PluginEvent,
    WorldView, MAX_EMITTED_INTENTS,
};
use ferrumc_plugin_block_rules::BlockRulesPlugin;
use ferrumc_plugin_host::{
    InMemoryPluginStorage, PluginHost, PluginLoader, PluginStorageBackend, ResolvedDecision,
};
use ferrumc_plugin_spawn_protect::{bypass_node, SpawnProtect, SpawnProtectPlugin, CONFIG_KEY};

use crate::command::build_command_tree;
use crate::config::AppConfig;

/// Permission *level* granted to a configured operator.
///
/// Mirrors vanilla's top operator tier (level 4): it satisfies every operator
/// gate, including [`GAMEMODE_LEVEL`](crate::command::GAMEMODE_LEVEL). Only the
/// players named in [`AppConfig::ops`] act at this level; everyone else acts at
/// the configured [`AppConfig::default_permission_level`] (0 by default), so the
/// gate is meaningful instead of granting every connection operator rights.
pub(crate) const OPERATOR_PERMISSION_LEVEL: u8 = 4;

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

/// A world-less [`WorldView`] handed to plugins during a block decision.
///
/// The decision runs on the connection task, which is forbidden from reading
/// authoritative world state (only the simulation owns it). So plugins consulted
/// at the intent boundary see a view that reports nothing loaded: they decide from
/// the incoming attempt (position, state, actor) and permissions, not from live
/// world reads. A plugin needing real world reads in `before_*` is unsupported
/// until the decision is routed through the driver/sim (documented follow-up).
struct NullWorldView;

impl WorldView for NullWorldView {
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

/// The long-lived plugin host wrapped for shared, off-tick block-event dispatch.
///
/// Shared behind an [`Arc`](std::sync::Arc) by every connection task (it lives on
/// [`ConnContext`](crate::connection::ConnContext)). Connection tasks run
/// concurrently, but [`PluginHost`] is `Send` and not `Sync` (plugins are called
/// one at a time), so it is guarded by a [`std::sync::Mutex`]. The plugin calls
/// are synchronous and panic-isolated — the lock is acquired, the decision is
/// folded, and the lock is released *before* any async I/O (acks, resyncs,
/// system-chat, intent routing) happens, so the guard is never held across an
/// `.await`. This is a per-edit serialization point, not a broad lock over world
/// state: it guards only the plugin registry, which owns no world.
pub(crate) struct BlockEventDispatcher {
    host: Mutex<PluginHost>,
}

impl BlockEventDispatcher {
    /// Wraps an enabled [`PluginHost`] for shared dispatch.
    pub(crate) fn new(host: PluginHost) -> Self {
        Self {
            host: Mutex::new(host),
        }
    }

    /// Locks the host, recovering the guard even if a previous holder panicked.
    ///
    /// Plugin panics are caught inside the host ([`std::panic::catch_unwind`]), so
    /// the mutex is not poisoned in practice; recovering on the off chance keeps a
    /// stray poison from wedging every future block edit (and avoids an `unwrap`).
    fn lock(&self) -> std::sync::MutexGuard<'_, PluginHost> {
        self.host
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Consults the loaded plugins about a pending block *placement*, returning the
    /// combined [`ResolvedDecision`] and any world-mutation intents to route.
    ///
    /// Runs the synchronous, panic-isolated host dispatch under the lock and
    /// returns owned data, so the caller does its async routing after the lock is
    /// released.
    pub(crate) fn before_block_place(
        &self,
        attempt: &BlockPlaceAttempt,
        permissions: &dyn PermissionApi,
    ) -> (ResolvedDecision, Vec<WorldIntent>) {
        let world = NullWorldView;
        let mut sink = CollectingSink::new();
        let resolved =
            self.lock()
                .dispatch_block_place_decision(attempt, &world, &mut sink, permissions);
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
        let world = NullWorldView;
        let mut sink = CollectingSink::new();
        let resolved =
            self.lock()
                .dispatch_block_break_decision(attempt, &world, &mut sink, permissions);
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
        self.dispatch_after(&PluginEvent::AfterBlockBreak { player, pos }, permissions)
    }

    /// Shared body of the `after_*` notifications: dispatch the event and drain the
    /// intents subscribers emitted.
    fn dispatch_after(
        &self,
        event: &PluginEvent,
        permissions: &dyn PermissionApi,
    ) -> Vec<WorldIntent> {
        let world = NullWorldView;
        let mut sink = CollectingSink::new();
        self.lock()
            .dispatch_event(event, &world, &mut sink, permissions);
        sink.into_intents()
    }
}

/// Everything a connection consults during the play phase.
///
/// Shared behind an [`Arc`](std::sync::Arc) by every connection: the
/// spawn-protection [`SpawnProtect`] veto, the [`PermissionRegistry`], the
/// command tree, the spawn position teleports return to, and the permission
/// level players act at.
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
    /// Returns the effective spawn-protection policy seeded into the plugin.
    ///
    /// Informational only: enforcement now lives in the spawn-protect plugin's
    /// `before_block_*` decision hooks (run through the
    /// [`BlockEventDispatcher`]), not in a bespoke connection-side veto. This
    /// accessor exposes the configuration that was read back from the plugin's
    /// namespaced storage, for diagnostics and tests.
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
/// Builds a persistent [`PluginHost`] and registers + enables the in-process
/// plugins that participate at the intent boundary:
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
/// The host is moved into the returned [`BlockEventDispatcher`], which every
/// connection shares to run the decision hooks off the simulation tick.
///
/// # Errors
///
/// Returns an error if a plugin cannot be registered or enabled, if reading the
/// seeded configuration back fails, or if the bypass permission node is invalid.
pub(crate) fn build_play_policy(
    config: &AppConfig,
) -> anyhow::Result<(PlayPolicy, BlockEventDispatcher)> {
    let center_x = config.spawn.x.floor() as i32;
    let center_z = config.spawn.z.floor() as i32;
    let seed = SpawnProtect::new(center_x, center_z, config.spawn_protect_radius);

    // The long-lived host: register + enable the in-process plugins that vote on
    // block edits at the intent boundary. The shared storage clone lets us read
    // the spawn-protect config back after it seeds itself.
    let storage = InMemoryPluginStorage::new();
    let mut host = PluginHost::new(Box::new(storage.clone()));

    let spawn_protect_id = host.register(Box::new(SpawnProtectPlugin::new(seed)))?;
    host.enable(&spawn_protect_id)?;
    let guard = storage
        .get(&spawn_protect_id, CONFIG_KEY)?
        .and_then(|bytes| SpawnProtect::from_bytes(&bytes))
        .unwrap_or(seed);

    let block_rules_id = host.register(Box::new(BlockRulesPlugin::new()))?;
    host.enable(&block_rules_id)?;

    let permissions = PermissionRegistry::from_bypass_names(&config.spawn_protect_bypass)?;
    let ops = config
        .ops
        .iter()
        .map(|name| PlayerId::offline(name))
        .collect();

    let policy = PlayPolicy {
        guard,
        permissions,
        command_tree: build_command_tree(),
        spawn: config.spawn,
        ops,
        default_permission_level: config.default_permission_level,
    };
    tracing::debug!(
        center = ?policy.guard().center(),
        radius = policy.guard().radius(),
        "spawn-protection policy loaded from plugin storage"
    );
    Ok((policy, BlockEventDispatcher::new(host)))
}

/// Scans `dir` for dynamic (`cdylib`) plugins and loads each across the C ABI,
/// returning the number that loaded successfully.
///
/// This proves the M28 dynamic loader runs from the application: every library is
/// attempted, failures are logged and skipped, and the loaded plugins are
/// registered with a throwaway host (they carry no event hook across the ABI, so
/// nothing else drives them — see the module docs).
///
/// # Errors
///
/// Returns an error only if `dir` itself cannot be scanned.
pub fn load_plugins(dir: &Path) -> anyhow::Result<usize> {
    let mut host = PluginHost::in_memory();
    let report = PluginLoader::new().load_dir(dir, &mut host)?;
    for (path, err) in report.failed() {
        tracing::warn!(path = %path.display(), %err, "plugin failed to load");
    }
    Ok(report.loaded_count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(radius: i32, bypass: &[&str]) -> AppConfig {
        AppConfig {
            spawn_protect_radius: radius,
            spawn_protect_bypass: bypass.iter().map(|s| (*s).to_string()).collect(),
            ..AppConfig::default()
        }
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
        let config = AppConfig {
            ops: vec!["Admin".to_string()],
            default_permission_level: 0,
            ..AppConfig::default()
        };
        let (policy, _dispatcher) = build_play_policy(&config).expect("policy builds");
        assert_eq!(
            policy.permission_level(PlayerId::offline("Admin")),
            OPERATOR_PERMISSION_LEVEL
        );
        assert_eq!(policy.permission_level(PlayerId::offline("Random")), 0);
    }

    #[test]
    fn load_plugins_errors_on_missing_directory() {
        let err = load_plugins(Path::new("/definitely/not/here/ferrumc")).expect_err("missing dir");
        assert!(err.to_string().contains("plugin directory") || err.to_string().contains("scan"));
    }

    // --- BlockEventDispatcher: the wired intent boundary ----------------------

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
