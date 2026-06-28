//! Region (cuboid) block-edit operations and their safety bounds.
//!
//! A region edit ([`GameInput::RegionEdit`](crate::GameInput::RegionEdit)) names
//! a [`Cuboid`](ferrumc_math::Cuboid) of blocks and a [`RegionOp`] describing how
//! every block in it changes. The shard applies the whole cuboid at one tick
//! boundary through the *same* single block-edit funnel a single edit uses (under
//! [`MutationCause::Command`](crate::MutationCause::Command)), capturing the prior
//! states so the edit can be undone.
//!
//! [`RegionLimits`] bounds that work: a single edit can address at most
//! [`RegionLimits::max_volume`] blocks, and a player retains at most
//! [`RegionLimits::max_undo_entries`] undoable edits. The command layer rejects an
//! over-cap region with a user-facing error *before* it reaches the shard; the
//! shard re-checks defensively so no caller can ever make a tick iterate an
//! unbounded region.

use ferrumc_world::BlockStateId;

/// How every block in a region edit's cuboid changes.
///
/// The enum is `#[non_exhaustive]`: more region operations (e.g. a masked
/// stack/move) may be added later, so downstream `match`es must include a
/// wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegionOp {
    /// Set every block in the cuboid to `state`.
    Fill {
        /// The block-state every cell becomes.
        state: BlockStateId,
    },
    /// Set every block in the cuboid that currently equals `from` to `to`,
    /// leaving every other block untouched.
    Replace {
        /// The block-state to match.
        from: BlockStateId,
        /// The block-state a matching cell becomes.
        to: BlockStateId,
    },
}

/// Default ceiling on the number of blocks a single region edit may address.
///
/// `32_768` is a 32x32x32 cube — large enough to shape terrain on camera. It is
/// applied incrementally under [`RegionLimits::max_blocks_per_tick`], so even at
/// the cap a fill never does all its work in one tick. The app overrides this
/// from configuration (`max_region_fill_volume`).
pub const DEFAULT_MAX_REGION_VOLUME: u64 = 32_768;

/// Default number of undoable region edits retained per player.
///
/// The oldest is evicted once a player exceeds this many edits. Combined with
/// [`DEFAULT_MAX_REGION_VOLUME`] this bounds the per-player undo memory.
pub const DEFAULT_MAX_UNDO_ENTRIES: usize = 16;

/// Default number of region cells a shard processes per tick across all in-flight
/// edits.
///
/// `8_192` lets a default-cap fill complete in four ticks (~200 ms at 20 TPS)
/// while keeping a normal fill (a few thousand blocks) instant. The budget bounds
/// per-tick work even if an operator raises [`max_volume`](RegionLimits::max_volume),
/// honouring the "defer, never stall the tick" overload rule.
pub const DEFAULT_MAX_BLOCKS_PER_TICK: usize = 8_192;

/// Bounds applied to region edits so a command can never make a shard iterate or
/// retain an unbounded amount of work.
///
/// A shard is constructed with [`RegionLimits::default`] and the app replaces it
/// with operator-configured values via
/// [`SimShard::set_region_limits`](crate::SimShard::set_region_limits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionLimits {
    /// Maximum number of blocks a single `/fill` or `/replace` may address. A
    /// region whose volume exceeds this is rejected.
    pub max_volume: u64,
    /// Maximum number of undoable edits retained per player; the oldest is
    /// evicted when a new edit would exceed it. Zero disables undo history.
    pub max_undo_entries: usize,
    /// Maximum number of region cells the shard processes per tick across all
    /// in-flight edits, so a large fill is applied across several ticks instead of
    /// stalling one. A value below `1` is treated as `1` so progress is always
    /// made.
    pub max_blocks_per_tick: usize,
}

impl Default for RegionLimits {
    fn default() -> Self {
        Self {
            max_volume: DEFAULT_MAX_REGION_VOLUME,
            max_undo_entries: DEFAULT_MAX_UNDO_ENTRIES,
            max_blocks_per_tick: DEFAULT_MAX_BLOCKS_PER_TICK,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_caps() {
        let limits = RegionLimits::default();
        assert_eq!(limits.max_volume, 32_768);
        assert_eq!(limits.max_undo_entries, 16);
        assert_eq!(limits.max_blocks_per_tick, 8_192);
    }

    #[test]
    fn region_op_compares_by_value() {
        let fill = RegionOp::Fill {
            state: BlockStateId::new(1),
        };
        assert_eq!(
            fill,
            RegionOp::Fill {
                state: BlockStateId::new(1)
            }
        );
        assert_ne!(
            fill,
            RegionOp::Replace {
                from: BlockStateId::AIR,
                to: BlockStateId::new(1),
            }
        );
    }
}
