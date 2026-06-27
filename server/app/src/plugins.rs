//! Plugin bring-up and the per-connection play policy.
//!
//! At startup the application:
//!
//! 1. scans the configured `/plugins` directory and loads every dynamic
//!    (`cdylib`) plugin across the M28 C ABI ([`load_plugins`]), proving the
//!    dynamic loader end to end; and
//! 2. drives the in-process [`SpawnProtectPlugin`] through a [`PluginHost`] so it
//!    seeds (and reads back) its configuration from private, namespaced storage,
//!    yielding the [`SpawnProtect`] policy the server enforces.
//!
//! The resulting [`PlayPolicy`] bundles everything a connection consults during
//! play: the spawn-protection veto, the per-player bypass permissions, the
//! command tree, and the spawn position.
//!
//! ## Why the veto is enforced in-process
//!
//! The C ABI carries no event hook, so a dynamically-loaded plugin cannot
//! receive a block event or return a veto across the boundary (extending the ABI
//! is out of this milestone's scope). The dynamic load therefore proves the
//! loader, while the authoritative veto is the [`SpawnProtect`] policy the
//! connection consults before a break/place reaches the simulation. The
//! in-process plugin still exercises the full SDK the deliverable names:
//! namespaced storage for its config and the permission API for the bypass node.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ferrumc_core::PlayerId;
use ferrumc_math::Vec3;
use ferrumc_permission::{Grant, PermissionNode, Subject};
use ferrumc_plugin_host::{InMemoryPluginStorage, PluginHost, PluginLoader, PluginStorageBackend};
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
/// (for the spawn-protection bypass) and per command (for node-string checks).
#[derive(Debug, Default)]
pub(crate) struct PermissionRegistry {
    subjects: BTreeMap<PlayerId, Subject>,
    bypass: Option<PermissionNode>,
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
        Ok(Self {
            subjects,
            bypass: Some(bypass),
        })
    }

    /// Returns whether `player` holds the spawn-protection bypass permission.
    pub(crate) fn has_bypass(&self, player: PlayerId) -> bool {
        let Some(node) = &self.bypass else {
            return false;
        };
        self.subjects
            .get(&player)
            .is_some_and(|subject| subject.has_permission(node))
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
    /// Returns the spawn-protection policy.
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

/// Builds the [`PlayPolicy`] for a configured server.
///
/// Seeds the spawn-protection configuration (centre = spawn column, radius =
/// [`AppConfig::spawn_protect_radius`]) into the in-process plugin's private
/// storage by enabling it through a [`PluginHost`], then reads the effective
/// policy back out of that storage — exercising the namespaced-storage round-trip
/// the deliverable requires. A radius of zero yields a disabled policy that
/// vetoes nothing.
///
/// # Errors
///
/// Returns an error if the plugin cannot be registered or enabled, if reading the
/// seeded configuration back fails, or if the bypass permission node is invalid.
pub(crate) fn build_play_policy(config: &AppConfig) -> anyhow::Result<PlayPolicy> {
    let center_x = config.spawn.x.floor() as i32;
    let center_z = config.spawn.z.floor() as i32;
    let seed = SpawnProtect::new(center_x, center_z, config.spawn_protect_radius);

    // Drive the in-process plugin so it seeds (or adopts) its config in private
    // storage, then read the effective policy back from that same namespace.
    let storage = InMemoryPluginStorage::new();
    let mut host = PluginHost::new(Box::new(storage.clone()));
    let id = host.register(Box::new(SpawnProtectPlugin::new(seed)))?;
    host.enable(&id)?;
    let guard = storage
        .get(&id, CONFIG_KEY)?
        .and_then(|bytes| SpawnProtect::from_bytes(&bytes))
        .unwrap_or(seed);

    let permissions = PermissionRegistry::from_bypass_names(&config.spawn_protect_bypass)?;
    let ops = config
        .ops
        .iter()
        .map(|name| PlayerId::offline(name))
        .collect();

    Ok(PlayPolicy {
        guard,
        permissions,
        command_tree: build_command_tree(),
        spawn: config.spawn,
        ops,
        default_permission_level: config.default_permission_level,
    })
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
        let policy = build_play_policy(&config).expect("policy builds");
        // Default spawn is (8, 64, 8) -> centre column (8, 8).
        assert_eq!(policy.guard().center(), (8, 8));
        assert_eq!(policy.guard().radius(), 16);
        assert!(policy.guard().is_enabled());
    }

    #[test]
    fn zero_radius_is_disabled() {
        let policy = build_play_policy(&config_with(0, &[])).expect("policy builds");
        assert!(!policy.guard().is_enabled());
    }

    #[test]
    fn bypass_list_grants_only_named_players() {
        let policy = build_play_policy(&config_with(16, &["Admin"])).expect("policy builds");
        assert!(policy.permissions().has_bypass(PlayerId::offline("Admin")));
        assert!(!policy
            .permissions()
            .has_bypass(PlayerId::offline("Griefer")));
    }

    #[test]
    fn operators_act_at_operator_level_others_at_the_default() {
        let config = AppConfig {
            ops: vec!["Admin".to_string()],
            default_permission_level: 0,
            ..AppConfig::default()
        };
        let policy = build_play_policy(&config).expect("policy builds");
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
}
