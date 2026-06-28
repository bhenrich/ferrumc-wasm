//! The in-process [`Plugin`] implementing the block-placement rules.
//!
//! [`BlockRulesPlugin`] is the milestone's second sample plugin: it exists to
//! prove two of the three `before_*` decision outcomes end to end.
//!
//! - It **denies** placing a configured block (by default
//!   [`DENIED_BLOCK_STATE_ID`], `minecraft:bedrock`) by returning
//!   [`PluginBlockDecision::Deny`] from
//!   [`before_block_place`](BlockRulesPlugin::before_block_place).
//! - It **replaces** a configured block on placement (by default
//!   [`GLASS_BLOCK_STATE_ID`] → [`TINTED_GLASS_BLOCK_STATE_ID`]) by returning
//!   [`PluginBlockDecision::Replace`].
//! - Everything else is [`PluginBlockDecision::Allow`]ed, and breaks are never
//!   touched (the default `before_block_break` returns `Allow`).
//!
//! The plugin is stateless beyond its three configured block-state ids and needs
//! only the [`VetoBlockEdits`](Capability::VetoBlockEdits) capability — it does
//! not read the world, query permissions, or use storage.

use ferrumc_core::{PluginId, TextComponent};
use ferrumc_plugin_api::{
    BlockPlaceAttempt, Capability, CapabilityManifest, EventContext, Plugin, PluginBlockDecision,
    PluginMetadata, Version,
};

/// The plugin's stable identifier, shared by the in-process plugin and the
/// `cdylib` C-ABI export.
pub const PLUGIN_ID: &str = "block-rules";

/// The plugin's human-readable display name.
pub const PLUGIN_NAME: &str = "Block Rules";

/// Default block-state id the plugin refuses to let players place.
///
/// `85` is `minecraft:bedrock` in the vendored 1.21.8 snapshot (the same constant
/// `ferrumc-registry` exposes as `block_state::BEDROCK`).
pub const DENIED_BLOCK_STATE_ID: u32 = 85;

/// Default block-state id the plugin rewrites on placement.
///
/// `562` is `minecraft:glass`'s block-state in the vendored 1.21.8 snapshot — the
/// state `ferrumc_registry::item::item_to_block_state(195)` resolves the glass item
/// to. The exact number is load-bearing: `before_block_place` receives the *resolved
/// block-state* (not the item id), so this must equal the runtime block-state a glass
/// placement produces or the [`Replace`](PluginBlockDecision::Replace) rule never
/// fires. A deployment may still override it via [`BlockRulesPlugin::with_blocks`].
pub const GLASS_BLOCK_STATE_ID: u32 = 562;

/// Default block-state id glass placements are rewritten to.
///
/// `23377` is `minecraft:tinted_glass`'s block-state in the vendored 1.21.8 snapshot
/// (`ferrumc_registry::item::item_to_block_state(196)`); see [`GLASS_BLOCK_STATE_ID`]
/// for why the exact value matters.
pub const TINTED_GLASS_BLOCK_STATE_ID: u32 = 23377;

/// The message shown to a player whose placement of the denied block is rejected.
const DENIED_MESSAGE: &str = "You cannot place that block here.";

/// A plugin that denies placing one block-state and rewrites another.
///
/// Construct the default rules (bedrock denied, glass → tinted glass) with
/// [`BlockRulesPlugin::new`], or override the ids with
/// [`BlockRulesPlugin::with_blocks`].
pub struct BlockRulesPlugin {
    /// Block-state id that may never be placed.
    denied: u32,
    /// Block-state id rewritten on placement.
    rewrite_from: u32,
    /// Block-state id [`rewrite_from`](Self::rewrite_from) becomes.
    rewrite_to: u32,
}

impl BlockRulesPlugin {
    /// Builds the plugin with the default rules: deny [`DENIED_BLOCK_STATE_ID`]
    /// and rewrite [`GLASS_BLOCK_STATE_ID`] to [`TINTED_GLASS_BLOCK_STATE_ID`].
    pub const fn new() -> Self {
        Self::with_blocks(
            DENIED_BLOCK_STATE_ID,
            GLASS_BLOCK_STATE_ID,
            TINTED_GLASS_BLOCK_STATE_ID,
        )
    }

    /// Builds the plugin with custom block-state ids: deny `denied`, and rewrite a
    /// placement of `rewrite_from` to `rewrite_to`.
    pub const fn with_blocks(denied: u32, rewrite_from: u32, rewrite_to: u32) -> Self {
        Self {
            denied,
            rewrite_from,
            rewrite_to,
        }
    }

    /// Returns the plugin's stable [`PluginId`].
    pub fn id() -> PluginId {
        PluginId::new(PLUGIN_ID)
    }

    /// Returns the capability manifest the plugin requires.
    ///
    /// It only needs [`VetoBlockEdits`](Capability::VetoBlockEdits) for its
    /// `before_block_place` decision.
    pub const fn capabilities() -> CapabilityManifest {
        CapabilityManifest::empty().with(Capability::VetoBlockEdits)
    }
}

impl Default for BlockRulesPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for BlockRulesPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(
            Self::id(),
            PLUGIN_NAME,
            Version::new(0, 1, 0),
            Self::capabilities(),
        )
        .with_description("Denies placing a configured block and rewrites another on placement")
    }

    fn before_block_place(
        &mut self,
        ev: &BlockPlaceAttempt,
        _ctx: &mut EventContext<'_>,
    ) -> PluginBlockDecision {
        let state = ev.block_state_id();
        if state == self.denied {
            PluginBlockDecision::Deny {
                message: Some(TextComponent::text(DENIED_MESSAGE)),
            }
        } else if state == self.rewrite_from {
            PluginBlockDecision::Replace {
                block_state_id: self.rewrite_to,
            }
        } else {
            PluginBlockDecision::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use ferrumc_core::{DimensionId, PlayerId};
    use ferrumc_math::{BlockPos, ChunkPos, Vec3};
    use ferrumc_permission::{PermissionNode, Resolution};
    use ferrumc_plugin_api::{
        BlockBreakAttempt, CommandSink, IntentError, PermissionApi, PluginStorageApi, StorageError,
        WorldIntent, WorldView,
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

    /// Runs `before_block_place` for a placement of `state` and returns the
    /// decision.
    fn decide_place(plugin: &mut BlockRulesPlugin, state: u32) -> PluginBlockDecision {
        let world = NullWorld;
        let mut sink = RecordingSink::default();
        let perms = NullPermissions;
        let storage = MapStorage::default();
        let mut ctx = EventContext::new(
            CapabilityManifest::all(),
            &world,
            &mut sink,
            &perms,
            &storage,
        );
        plugin.before_block_place(
            &BlockPlaceAttempt::new(PlayerId::offline("Steve"), BlockPos::new(0, 64, 0), state),
            &mut ctx,
        )
    }

    #[test]
    fn placing_the_denied_block_is_denied() {
        let mut plugin = BlockRulesPlugin::new();
        let decision = decide_place(&mut plugin, DENIED_BLOCK_STATE_ID);
        assert!(matches!(
            decision,
            PluginBlockDecision::Deny { message: Some(_) }
        ));
    }

    #[test]
    fn placing_glass_is_replaced_with_tinted_glass() {
        let mut plugin = BlockRulesPlugin::new();
        let decision = decide_place(&mut plugin, GLASS_BLOCK_STATE_ID);
        assert_eq!(
            decision,
            PluginBlockDecision::Replace {
                block_state_id: TINTED_GLASS_BLOCK_STATE_ID
            }
        );
    }

    #[test]
    fn placing_any_other_block_is_allowed() {
        let mut plugin = BlockRulesPlugin::new();
        // `1` is `minecraft:stone`: not configured, so it passes through.
        assert_eq!(decide_place(&mut plugin, 1), PluginBlockDecision::Allow);
    }

    #[test]
    fn breaking_is_never_vetoed() {
        let mut plugin = BlockRulesPlugin::new();
        let world = NullWorld;
        let mut sink = RecordingSink::default();
        let perms = NullPermissions;
        let storage = MapStorage::default();
        let mut ctx = EventContext::new(
            CapabilityManifest::all(),
            &world,
            &mut sink,
            &perms,
            &storage,
        );
        let decision = plugin.before_block_break(
            &BlockBreakAttempt::new(PlayerId::offline("Steve"), BlockPos::new(0, 64, 0)),
            &mut ctx,
        );
        assert_eq!(decision, PluginBlockDecision::Allow);
    }

    #[test]
    fn custom_block_ids_are_honoured() {
        let mut plugin = BlockRulesPlugin::with_blocks(100, 200, 300);
        assert!(decide_place(&mut plugin, 100).is_deny());
        assert_eq!(
            decide_place(&mut plugin, 200),
            PluginBlockDecision::Replace {
                block_state_id: 300
            }
        );
        assert_eq!(decide_place(&mut plugin, 85), PluginBlockDecision::Allow);
    }

    #[test]
    fn default_block_state_ids_match_the_real_registry() {
        // These are NOT free-choice demo values. `before_block_place` receives a
        // *resolved block-state* (not an item id), so each default must equal the
        // real 1.21.8 block-state a placement produces or the rule never fires at
        // runtime. Pin the literals and cross-check the registry so a future drift
        // back to demo ids (or a registry change) fails the build rather than
        // silently breaking the rule (the bug the demo ids 199/9279 caused).
        assert_eq!(GLASS_BLOCK_STATE_ID, 562);
        assert_eq!(TINTED_GLASS_BLOCK_STATE_ID, 23377);
        assert_eq!(DENIED_BLOCK_STATE_ID, 85);

        // The glass item (195) and tinted_glass item (196) place exactly these
        // states, and the bedrock item (58) places the denied state.
        assert_eq!(
            ferrumc_registry::item::item_to_block_state(195),
            Some(GLASS_BLOCK_STATE_ID)
        );
        assert_eq!(
            ferrumc_registry::item::item_to_block_state(196),
            Some(TINTED_GLASS_BLOCK_STATE_ID)
        );
        assert_eq!(
            ferrumc_registry::item::item_to_block_state(58),
            Some(DENIED_BLOCK_STATE_ID)
        );
    }
}
