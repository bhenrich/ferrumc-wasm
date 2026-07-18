#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use ferrumc_plugin_sdk::{
    BlockDecision, BlockEvent, BlockPos, Capability, CapabilityManifest, EventContext,
    EventDecision, Feedback, PlaceAttempt, Plugin, PluginDeclaration, PluginError, PluginVersion,
};

const REGION_MIN: i32 = -16;
const REGION_MAX: i32 = 16;
const PROTECTED_MESSAGE: &str = "This region is protected.";

/// Deterministic square-region policy authored against the shared plugin SDK.
pub struct RegionGuardPlugin;

impl RegionGuardPlugin {
    /// Returns whether a block lies inside the protected horizontal square.
    ///
    /// Both horizontal bounds are inclusive and the vertical coordinate is
    /// deliberately ignored.
    pub const fn protects(pos: BlockPos) -> bool {
        pos.x() >= REGION_MIN
            && pos.x() <= REGION_MAX
            && pos.z() >= REGION_MIN
            && pos.z() <= REGION_MAX
    }

    fn protected_feedback(
        player: ferrumc_plugin_sdk::PlayerId,
        context: &mut EventContext<'_>,
    ) -> Result<Feedback, PluginError> {
        context.operations()?.message(player, PROTECTED_MESSAGE)?;
        Ok(Feedback::new(PROTECTED_MESSAGE)?)
    }
}

impl Plugin for RegionGuardPlugin {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "region-guard",
        "Region Guard",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty()
            .with(Capability::VetoBlockEdits)
            .with(Capability::SubmitIntents),
    );

    fn create() -> Self {
        Self
    }

    fn before_block_place(
        &mut self,
        attempt: &PlaceAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<BlockDecision, PluginError> {
        if Self::protects(attempt.pos()) {
            return Ok(BlockDecision::Deny(Some(Self::protected_feedback(
                attempt.player(),
                context,
            )?)));
        }
        Ok(BlockDecision::Allow)
    }

    fn before_block_break(
        &mut self,
        attempt: &BlockEvent,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        if Self::protects(attempt.pos()) {
            return Ok(EventDecision::Deny(Some(Self::protected_feedback(
                attempt.player(),
                context,
            )?)));
        }
        Ok(EventDecision::Allow)
    }
}

/// Builds the compiled-in adapter from the same declaration used by dynamic
/// packaging.
#[cfg(feature = "builtin")]
pub fn builtin_factory(
) -> Result<ferrumc_plugin_sdk_builtin::BuiltinPluginFactory, ferrumc_plugin_sdk::DeclarationError>
{
    ferrumc_plugin_sdk_builtin::BuiltinPluginFactory::new::<RegionGuardPlugin>()
}

#[cfg(feature = "dynamic")]
ferrumc_plugin_sdk_dynamic::export_plugin!(crate::RegionGuardPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_square_is_inclusive_and_height_independent() {
        for y in [i32::MIN, -1, 0, 1, i32::MAX] {
            for (x, z) in [
                (REGION_MIN, REGION_MIN),
                (REGION_MIN, REGION_MAX),
                (REGION_MAX, REGION_MIN),
                (REGION_MAX, REGION_MAX),
                (REGION_MIN, 0),
                (REGION_MAX, 0),
                (0, REGION_MIN),
                (0, REGION_MAX),
                (0, 0),
            ] {
                assert!(RegionGuardPlugin::protects(BlockPos::new(x, y, z)));
            }
        }
    }

    #[test]
    fn adjacent_and_extreme_horizontal_positions_are_outside() {
        for pos in [
            BlockPos::new(REGION_MIN - 1, 0, 0),
            BlockPos::new(REGION_MAX + 1, 0, 0),
            BlockPos::new(0, 0, REGION_MIN - 1),
            BlockPos::new(0, 0, REGION_MAX + 1),
            BlockPos::new(REGION_MIN - 1, 0, REGION_MAX),
            BlockPos::new(REGION_MAX + 1, 0, REGION_MIN),
            BlockPos::new(REGION_MAX, 0, REGION_MIN - 1),
            BlockPos::new(REGION_MIN, 0, REGION_MAX + 1),
            BlockPos::new(i32::MIN, i32::MIN, i32::MAX),
            BlockPos::new(i32::MAX, i32::MAX, i32::MIN),
        ] {
            assert!(!RegionGuardPlugin::protects(pos));
        }
    }

    #[test]
    fn declaration_requests_only_the_two_required_facades() {
        let requested = RegionGuardPlugin::DECLARATION.requested_capabilities();
        assert_eq!(requested.len(), 2);
        assert!(requested.grants(Capability::VetoBlockEdits));
        assert!(requested.grants(Capability::SubmitIntents));
        for capability in Capability::ALL {
            assert_eq!(
                requested.grants(capability),
                matches!(
                    capability,
                    Capability::VetoBlockEdits | Capability::SubmitIntents
                )
            );
        }
    }
}
