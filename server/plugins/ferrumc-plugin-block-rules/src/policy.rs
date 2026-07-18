//! Packaging-independent block-placement policy.

/// The plugin's stable identifier.
pub const PLUGIN_ID: &str = "block-rules";

/// The plugin's human-readable display name.
pub const PLUGIN_NAME: &str = "Block Rules";

/// Default block-state id the plugin refuses to let players place.
///
/// Sourced from `ferrumc_registry::block_state::ids::BEDROCK`, the default
/// state of `minecraft:bedrock` in the pinned 1.21.8 registry.
pub const DENIED_BLOCK_STATE_ID: u32 = ferrumc_registry::block_state::ids::BEDROCK;

/// Default block-state id the plugin rewrites on placement.
///
/// Sourced from `ferrumc_registry::block_state::ids::GLASS`. This must remain
/// the state produced by placing the glass item or the rule will not fire.
pub const GLASS_BLOCK_STATE_ID: u32 = ferrumc_registry::block_state::ids::GLASS;

/// Default block-state id glass placements are rewritten to.
///
/// Sourced from `ferrumc_registry::block_state::ids::TINTED_GLASS`.
pub const TINTED_GLASS_BLOCK_STATE_ID: u32 = ferrumc_registry::block_state::ids::TINTED_GLASS;

/// The feedback attached to a denied bedrock placement.
#[cfg(any(feature = "builtin", feature = "dynamic", test))]
pub(crate) const DENIED_MESSAGE: &str = "You cannot place that block here.";

/// A packaging-neutral placement outcome.
#[cfg(any(feature = "builtin", feature = "dynamic", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlacementOutcome {
    /// Allow the requested state unchanged.
    Allow,
    /// Deny the placement.
    Deny,
    /// Allow the placement after replacing its state.
    Replace(u32),
}

/// The pure state-id policy shared by both plugin adapters.
#[cfg(any(feature = "builtin", feature = "dynamic", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlockPolicy {
    denied: u32,
    rewrite_from: u32,
    rewrite_to: u32,
}

#[cfg(any(feature = "builtin", feature = "dynamic", test))]
impl BlockPolicy {
    /// Builds the default bedrock-denial and glass-rewrite policy.
    pub(crate) const fn standard() -> Self {
        Self::new(
            DENIED_BLOCK_STATE_ID,
            GLASS_BLOCK_STATE_ID,
            TINTED_GLASS_BLOCK_STATE_ID,
        )
    }

    /// Builds a policy from caller-selected opaque block-state ids.
    pub(crate) const fn new(denied: u32, rewrite_from: u32, rewrite_to: u32) -> Self {
        Self {
            denied,
            rewrite_from,
            rewrite_to,
        }
    }

    /// Decides one placement without consulting host state or mutating data.
    pub(crate) const fn decide(self, state: u32) -> PlacementOutcome {
        if state == self.denied {
            PlacementOutcome::Deny
        } else if state == self.rewrite_from {
            PlacementOutcome::Replace(self.rewrite_to)
        } else {
            PlacementOutcome::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_denies_replaces_and_allows_in_stable_order() {
        let policy = BlockPolicy::standard();
        assert_eq!(policy.decide(DENIED_BLOCK_STATE_ID), PlacementOutcome::Deny);
        assert_eq!(
            policy.decide(GLASS_BLOCK_STATE_ID),
            PlacementOutcome::Replace(TINTED_GLASS_BLOCK_STATE_ID)
        );
        assert_eq!(
            policy.decide(ferrumc_registry::block_state::ids::STONE),
            PlacementOutcome::Allow
        );
    }

    #[test]
    fn denial_wins_when_custom_policy_ids_overlap() {
        let policy = BlockPolicy::new(42, 42, 99);
        assert_eq!(policy.decide(42), PlacementOutcome::Deny);
    }

    #[test]
    fn default_ids_match_item_placement_mappings() {
        use ferrumc_registry::item;

        assert_eq!(
            item::item_to_block_state(item::ids::GLASS),
            Some(GLASS_BLOCK_STATE_ID)
        );
        assert_eq!(
            item::item_to_block_state(item::ids::TINTED_GLASS),
            Some(TINTED_GLASS_BLOCK_STATE_ID)
        );
        assert_eq!(
            item::item_to_block_state(item::ids::BEDROCK),
            Some(DENIED_BLOCK_STATE_ID)
        );
    }
}
