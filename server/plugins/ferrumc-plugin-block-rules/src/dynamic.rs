//! Shared-SDK adapter used by trusted-native packaging.

use ferrumc_plugin_sdk::{
    BlockDecision, Capability, CapabilityManifest, EventContext, Feedback, PlaceAttempt, Plugin,
    PluginDeclaration, PluginError, PluginVersion,
};

use crate::policy::{BlockPolicy, PlacementOutcome, DENIED_MESSAGE, PLUGIN_ID, PLUGIN_NAME};

/// Shared-SDK wrapper for the default block-rules policy.
pub(crate) struct SdkBlockRulesPlugin {
    policy: BlockPolicy,
}

impl SdkBlockRulesPlugin {
    fn decide(&self, block_state_id: u32) -> Result<BlockDecision, PluginError> {
        match self.policy.decide(block_state_id) {
            PlacementOutcome::Allow => Ok(BlockDecision::Allow),
            PlacementOutcome::Deny => Ok(BlockDecision::Deny(Some(Feedback::new(DENIED_MESSAGE)?))),
            PlacementOutcome::Replace(block_state_id) => Ok(BlockDecision::Replace(block_state_id)),
        }
    }
}

impl Plugin for SdkBlockRulesPlugin {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        PLUGIN_ID,
        PLUGIN_NAME,
        PluginVersion::new(0, 1, 0),
        CapabilityManifest::empty().with(Capability::VetoBlockEdits),
    );

    fn create() -> Self {
        Self {
            policy: BlockPolicy::standard(),
        }
    }

    fn before_block_place(
        &mut self,
        attempt: &PlaceAttempt,
        _context: &mut EventContext<'_>,
    ) -> Result<BlockDecision, PluginError> {
        self.decide(attempt.block_state_id())
    }
}

#[cfg(test)]
mod tests {
    use ferrumc_plugin_sdk::{Capability, Plugin};

    use super::*;
    use crate::{DENIED_BLOCK_STATE_ID, GLASS_BLOCK_STATE_ID, TINTED_GLASS_BLOCK_STATE_ID};

    #[test]
    fn declaration_matches_the_bundle_contract() {
        let declaration = SdkBlockRulesPlugin::DECLARATION;
        assert_eq!(declaration.id(), PLUGIN_ID);
        assert_eq!(declaration.name(), PLUGIN_NAME);
        assert_eq!(declaration.version(), PluginVersion::new(0, 1, 0));
        assert_eq!(declaration.requested_capabilities().len(), 1);
        assert!(declaration
            .requested_capabilities()
            .grants(Capability::VetoBlockEdits));
        assert_eq!(declaration.validate(), Ok(()));
    }

    #[test]
    fn sdk_mapping_preserves_each_shared_policy_outcome() {
        let plugin = SdkBlockRulesPlugin::create();
        assert_eq!(
            plugin
                .decide(DENIED_BLOCK_STATE_ID)
                .expect("static feedback is bounded"),
            BlockDecision::Deny(Some(
                Feedback::new(DENIED_MESSAGE).expect("static feedback is bounded")
            ))
        );
        assert_eq!(
            plugin
                .decide(GLASS_BLOCK_STATE_ID)
                .expect("replacement has no fallible field"),
            BlockDecision::Replace(TINTED_GLASS_BLOCK_STATE_ID)
        );
        assert_eq!(
            plugin
                .decide(ferrumc_registry::block_state::ids::STONE)
                .expect("allow has no fallible field"),
            BlockDecision::Allow
        );
    }
}
