//! The in-process [`Plugin`] implementation that owns the spawn-protection
//! policy, its persisted configuration, and the player-facing messaging.
//!
//! The plugin is driven by the host through the ordinary lifecycle:
//!
//! - [`on_enable`](SpawnProtectPlugin::on_enable) loads the [`SpawnProtect`]
//!   policy from the plugin's private, namespaced storage (seeding the default
//!   if none is stored) and subscribes to the join and block-break events.
//! - [`on_event`](SpawnProtectPlugin::on_event) welcomes a joining player and,
//!   for a block break inside the protected square, consults the permission API
//!   for the bypass node and tells an unauthorized actor it was denied.
//!
//! The authoritative *veto* of a block edit is the policy in [`SpawnProtect`],
//! which the application consults before the edit reaches the simulation (the
//! C ABI carries no event hook for a true cross-boundary veto yet — see the
//! crate documentation). This in-process plugin demonstrates the full SDK
//! surface the milestone requires: namespaced storage for configuration, the
//! permission API for the bypass check, mutation intents for messaging, and
//! event subscription.

use ferrumc_core::{PluginId, TextComponent};
use ferrumc_permission::{NodeParseError, PermissionNode};
use ferrumc_plugin_api::{
    Capability, CapabilityManifest, EventContext, EventKind, Plugin, PluginError, PluginEvent,
    PluginMetadata, SetupContext, Version, WorldIntent,
};

use crate::guard::SpawnProtect;

/// The plugin's stable identifier, shared by the in-process plugin and the
/// `cdylib` C-ABI export.
pub const PLUGIN_ID: &str = "spawn-protect";

/// The plugin's human-readable display name.
pub const PLUGIN_NAME: &str = "Spawn Protection";

/// The permission node an actor must hold to edit blocks inside the protected
/// spawn area.
pub const BYPASS_PERMISSION: &str = "ferrumc.spawnprotect.bypass";

/// The storage key under which the plugin persists its [`SpawnProtect`] config.
pub const CONFIG_KEY: &str = "config";

/// The message sent to a player when they join the server.
const WELCOME_MESSAGE: &str = "Welcome! The spawn area is protected.";

/// The message sent to a player whose block edit was denied by spawn protection.
const DENIED_MESSAGE: &str = "You cannot edit blocks in the protected spawn area.";

/// Parses the [`BYPASS_PERMISSION`] string into a [`PermissionNode`].
///
/// Returns the validated node, or a [`NodeParseError`] if the constant is ever
/// changed to something invalid (it never is in shipped builds). Callers that
/// cannot propagate the error fail closed by treating a parse failure as "no
/// bypass", so a misconfiguration can never silently *grant* access.
pub fn bypass_node() -> Result<PermissionNode, NodeParseError> {
    PermissionNode::parse(BYPASS_PERMISSION)
}

/// The spawn-protection plugin.
///
/// Construct one with [`SpawnProtectPlugin::new`], passing the default policy to
/// seed on first run. The effective policy is loaded from (or written to) the
/// plugin's private storage during [`on_enable`](Plugin::on_enable).
pub struct SpawnProtectPlugin {
    /// The policy to seed on first enable, replaced by the stored policy if one
    /// already exists.
    guard: SpawnProtect,
}

impl SpawnProtectPlugin {
    /// Builds the plugin with `guard` as the configuration to seed when the
    /// plugin's storage has none yet.
    pub const fn new(guard: SpawnProtect) -> Self {
        Self { guard }
    }

    /// Returns the plugin's stable [`PluginId`].
    pub fn id() -> PluginId {
        PluginId::new(PLUGIN_ID)
    }

    /// Returns the capability manifest the plugin requires.
    ///
    /// It needs to receive events, query permissions, submit messaging intents,
    /// and use its private storage.
    pub const fn capabilities() -> CapabilityManifest {
        CapabilityManifest::empty()
            .with(Capability::ReceiveEvents)
            .with(Capability::SubmitIntents)
            .with(Capability::ReadPermissions)
            .with(Capability::Storage)
    }

    /// Returns the policy currently held by the plugin.
    ///
    /// After [`on_enable`](Plugin::on_enable) this reflects the configuration
    /// loaded from (or seeded into) storage.
    pub const fn guard(&self) -> SpawnProtect {
        self.guard
    }
}

impl Plugin for SpawnProtectPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(
            Self::id(),
            PLUGIN_NAME,
            Version::new(0, 1, 0),
            Self::capabilities(),
        )
        .with_description("Protects the spawn area from unauthorized block edits")
    }

    fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
        // Load the policy from private storage, or seed the default if absent.
        let storage = ctx.storage()?;
        match storage.get(CONFIG_KEY)? {
            Some(bytes) => {
                if let Some(stored) = SpawnProtect::from_bytes(&bytes) {
                    self.guard = stored;
                } else {
                    // Corrupt entry: overwrite it with the seed so the next read
                    // is well-formed rather than silently mis-parsing.
                    storage.put(CONFIG_KEY, &self.guard.to_bytes())?;
                }
            }
            None => storage.put(CONFIG_KEY, &self.guard.to_bytes())?,
        }

        ctx.events()?
            .subscribe(EventKind::PlayerJoin)
            .subscribe(EventKind::BlockBreak);
        Ok(())
    }

    fn on_event(&mut self, event: &PluginEvent, ctx: &mut EventContext<'_>) {
        match event {
            PluginEvent::PlayerJoin { player } => {
                if let Ok(sink) = ctx.sink() {
                    let _ = sink.submit(WorldIntent::Message {
                        player: *player,
                        message: TextComponent::text(WELCOME_MESSAGE),
                    });
                }
            }
            PluginEvent::BlockBreak { player, pos } => {
                // Only protected columns are interesting.
                if !self.guard.is_protected(*pos) {
                    return;
                }
                // Fail closed: any error querying permissions means "no bypass".
                let has_bypass = match (ctx.permissions(), bypass_node()) {
                    (Ok(perms), Ok(node)) => perms.has_permission(*player, &node),
                    _ => false,
                };
                if self.guard.vetoes(*pos, has_bypass) {
                    if let Ok(sink) = ctx.sink() {
                        let _ = sink.submit(WorldIntent::Message {
                            player: *player,
                            message: TextComponent::text(DENIED_MESSAGE),
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use ferrumc_core::{DimensionId, PlayerId};
    use ferrumc_math::{BlockPos, ChunkPos, Vec3};
    use ferrumc_permission::Resolution;
    use ferrumc_plugin_api::{
        CommandRegistrar, CommandSink, EventRegistrar, IntentError, PermissionApi,
        PluginStorageApi, StorageError, WorldView,
    };

    use super::*;

    /// An in-memory storage facade for one namespace.
    #[derive(Default)]
    struct MapStorage {
        map: RefCell<HashMap<String, Vec<u8>>>,
    }

    impl PluginStorageApi for MapStorage {
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
            Ok(self.map.borrow().get(key).cloned())
        }
        fn put(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
            self.map.borrow_mut().insert(key.to_owned(), value.to_vec());
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<bool, StorageError> {
            Ok(self.map.borrow_mut().remove(key).is_some())
        }
        fn keys(&self) -> Result<Vec<String>, StorageError> {
            Ok(self.map.borrow().keys().cloned().collect())
        }
    }

    /// A world view that reports nothing loaded.
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

    /// Records every submitted intent.
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

    /// A permission facade granting the bypass node to exactly one player.
    struct BypassFor(PlayerId);
    impl PermissionApi for BypassFor {
        fn has_permission(&self, player: PlayerId, node: &PermissionNode) -> bool {
            player == self.0 && node == &bypass_node().expect("valid node")
        }
        fn resolve(&self, player: PlayerId, node: &PermissionNode) -> Resolution {
            if self.has_permission(player, node) {
                Resolution::Allowed
            } else {
                Resolution::Unset
            }
        }
    }

    fn enable(plugin: &mut SpawnProtectPlugin, storage: &dyn PluginStorageApi) {
        let mut events = EventRegistrar::new();
        let mut commands = CommandRegistrar::new();
        let mut ctx = SetupContext::new(
            CapabilityManifest::all(),
            &mut events,
            &mut commands,
            storage,
        );
        plugin.on_enable(&mut ctx).expect("enables");
    }

    fn dispatch(
        plugin: &mut SpawnProtectPlugin,
        event: &PluginEvent,
        sink: &mut RecordingSink,
        perms: &dyn PermissionApi,
        storage: &dyn PluginStorageApi,
    ) {
        let world = NullWorld;
        let mut ctx = EventContext::new(CapabilityManifest::all(), &world, sink, perms, storage);
        plugin.on_event(event, &mut ctx);
    }

    #[test]
    fn on_enable_seeds_then_reads_back_config() {
        let storage = MapStorage::default();
        let mut plugin = SpawnProtectPlugin::new(SpawnProtect::new(8, 8, 16));
        enable(&mut plugin, &storage);
        // The seed landed in storage and survives a reload.
        let stored = storage.get(CONFIG_KEY).expect("get").expect("seeded");
        assert_eq!(SpawnProtect::from_bytes(&stored), Some(plugin.guard()));

        // A second plugin with a different seed adopts the stored value instead.
        let mut other = SpawnProtectPlugin::new(SpawnProtect::new(0, 0, 1));
        enable(&mut other, &storage);
        assert_eq!(other.guard(), SpawnProtect::new(8, 8, 16));
    }

    #[test]
    fn join_welcomes_the_player() {
        let storage = MapStorage::default();
        let mut plugin = SpawnProtectPlugin::new(SpawnProtect::new(8, 8, 16));
        enable(&mut plugin, &storage);

        let player = PlayerId::offline("Steve");
        let mut sink = RecordingSink::default();
        dispatch(
            &mut plugin,
            &PluginEvent::PlayerJoin { player },
            &mut sink,
            &BypassFor(PlayerId::offline("nobody")),
            &storage,
        );
        assert_eq!(sink.intents.len(), 1);
        assert!(matches!(
            &sink.intents[0],
            WorldIntent::Message { player: p, .. } if *p == player
        ));
    }

    #[test]
    fn protected_break_without_bypass_is_denied() {
        let storage = MapStorage::default();
        let mut plugin = SpawnProtectPlugin::new(SpawnProtect::new(8, 8, 16));
        enable(&mut plugin, &storage);

        let griefer = PlayerId::offline("Griefer");
        let mut sink = RecordingSink::default();
        dispatch(
            &mut plugin,
            &PluginEvent::BlockBreak {
                player: griefer,
                pos: BlockPos::new(8, 63, 8),
            },
            &mut sink,
            &BypassFor(PlayerId::offline("Admin")),
            &storage,
        );
        // The unauthorized actor is told it was denied.
        assert_eq!(sink.intents.len(), 1);
    }

    #[test]
    fn protected_break_with_bypass_is_allowed() {
        let storage = MapStorage::default();
        let mut plugin = SpawnProtectPlugin::new(SpawnProtect::new(8, 8, 16));
        enable(&mut plugin, &storage);

        let admin = PlayerId::offline("Admin");
        let mut sink = RecordingSink::default();
        dispatch(
            &mut plugin,
            &PluginEvent::BlockBreak {
                player: admin,
                pos: BlockPos::new(8, 63, 8),
            },
            &mut sink,
            &BypassFor(admin),
            &storage,
        );
        // A bypassing actor triggers no denial message.
        assert!(sink.intents.is_empty());
    }

    #[test]
    fn unprotected_break_is_ignored() {
        let storage = MapStorage::default();
        let mut plugin = SpawnProtectPlugin::new(SpawnProtect::new(8, 8, 4));
        enable(&mut plugin, &storage);

        let griefer = PlayerId::offline("Griefer");
        let mut sink = RecordingSink::default();
        dispatch(
            &mut plugin,
            &PluginEvent::BlockBreak {
                player: griefer,
                pos: BlockPos::new(1000, 63, 1000),
            },
            &mut sink,
            &BypassFor(PlayerId::offline("Admin")),
            &storage,
        );
        assert!(sink.intents.is_empty());
    }
}
