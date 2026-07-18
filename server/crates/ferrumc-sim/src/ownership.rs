//! Typed logical-shard identity, ownership, and lifecycle values.
//!
//! This module defines the deterministic spatial seam used by the crate's
//! non-default shadow scheduler without changing the current single-shard
//! runtime. A [`ShardId`] canonically names one world/dimension scoped 8x8
//! chunk cell. Logical worker assignments and directory registrations remain
//! separate concepts and may process or route many logical shards.

use core::fmt;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

use ferrumc_core::{DimensionId, WorldId};
use ferrumc_math::{BlockPos, ChunkPos, ShardPos};

use crate::error::{SimError, SimResult};

/// Width and depth of one logical shard region, in chunks.
pub const SHARD_WIDTH_CHUNKS: i32 = 8;

/// Smallest shard-axis coordinate that can describe an 8-chunk region in the
/// [`ChunkPos`] domain.
pub const MIN_SHARD_COORD: i32 = i32::MIN >> 3;

/// Largest shard-axis coordinate that can describe an 8-chunk region in the
/// [`ChunkPos`] domain.
pub const MAX_SHARD_COORD: i32 = i32::MAX >> 3;

/// The canonical identity of one logical 8x8 simulation shard.
///
/// World, dimension, and typed shard position are all part of the identity, so
/// coordinate partitioning is collision-free and independent of worker or
/// registration allocation order. The identity is scoped to the running
/// world's typed ids; it is not a storage-format identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardId {
    world: WorldId,
    dimension: DimensionId,
    position: ShardPos,
}

impl ShardId {
    /// Creates a canonical id after validating that `position` describes a
    /// complete 8x8 region in the [`ChunkPos`] domain.
    pub fn try_new(world: WorldId, dimension: DimensionId, position: ShardPos) -> SimResult<Self> {
        if !is_valid_shard_position(position) {
            return Err(SimError::ShardRegionOutOfRange { position });
        }
        Ok(Self {
            world,
            dimension,
            position,
        })
    }

    /// Builds an id from a position already derived from a [`ChunkPos`].
    fn from_chunk_position(world: WorldId, dimension: DimensionId, position: ShardPos) -> Self {
        // `ChunkPos::to_shard_pos` can only produce positions inside the
        // validated range, so this constructor preserves the id invariant.
        Self {
            world,
            dimension,
            position,
        }
    }

    /// Returns the world containing this logical shard.
    pub const fn world(self) -> WorldId {
        self.world
    }

    /// Returns the dimension containing this logical shard.
    pub const fn dimension(self) -> DimensionId {
        self.dimension
    }

    /// Returns this logical shard's typed grid position.
    pub const fn position(self) -> ShardPos {
        self.position
    }

    /// Returns the fixed spatial region named by this id.
    pub const fn region(self) -> ShardRegion {
        ShardRegion { shard_id: self }
    }
}

impl fmt::Display for ShardId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "world {}, dimension {}, shard ({},{})",
            self.world,
            self.dimension,
            self.position.x(),
            self.position.z(),
        )
    }
}

/// The fixed 8x8 chunk region belonging to one [`ShardId`].
///
/// The region derives from the canonical id instead of accepting independent
/// bounds, so an id and its spatial ownership can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardRegion {
    shard_id: ShardId,
}

impl ShardRegion {
    /// Returns the canonical logical shard identity.
    pub const fn shard_id(self) -> ShardId {
        self.shard_id
    }

    /// Returns the world containing this region.
    pub const fn world(self) -> WorldId {
        self.shard_id.world()
    }

    /// Returns the dimension containing this region.
    pub const fn dimension(self) -> DimensionId {
        self.shard_id.dimension()
    }

    /// Returns this region's typed position in the shard grid.
    pub const fn position(self) -> ShardPos {
        self.shard_id.position()
    }

    /// Returns the inclusive minimum chunk corner of this region.
    pub const fn min_chunk(self) -> ChunkPos {
        // `ShardId::try_new` rejects positions for which the existing math
        // conversion would wrap; coordinate-derived ids satisfy the same bound
        // by construction.
        self.position().origin_chunk()
    }

    /// Returns the inclusive maximum chunk corner of this region.
    pub const fn max_chunk(self) -> ChunkPos {
        let min = self.min_chunk();
        ChunkPos::new(
            min.x().saturating_add(SHARD_WIDTH_CHUNKS - 1),
            min.z().saturating_add(SHARD_WIDTH_CHUNKS - 1),
        )
    }

    /// Returns whether `chunk` belongs to this exact world/dimension region.
    pub fn contains_chunk(self, world: WorldId, dimension: DimensionId, chunk: ChunkPos) -> bool {
        self.world() == world
            && self.dimension() == dimension
            && self.position() == chunk.to_shard_pos()
    }

    /// Returns whether `block` belongs to this exact world/dimension region.
    pub fn contains_block(self, world: WorldId, dimension: DimensionId, block: BlockPos) -> bool {
        self.contains_chunk(world, dimension, block.to_chunk_pos())
    }

    /// Returns whether this region and `other` claim any common chunk.
    ///
    /// Every valid region is aligned to the fixed 8x8 partition, so two scoped
    /// regions overlap exactly when their canonical ids are equal.
    pub fn overlaps(self, other: Self) -> bool {
        self.shard_id == other.shard_id
    }
}

impl fmt::Display for ShardRegion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.shard_id.fmt(formatter)
    }
}

/// A validated ownership claim for the region belonging to one [`ShardId`].
///
/// The region is derived from the id, making mismatched identity/bounds
/// unrepresentable. [`ShardOwnershipMap::claim`] rejects duplicate activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardOwnership {
    shard_id: ShardId,
    region: ShardRegion,
}

impl ShardOwnership {
    /// Creates the canonical ownership claim for `shard_id`.
    pub const fn new(shard_id: ShardId) -> Self {
        Self {
            shard_id,
            region: shard_id.region(),
        }
    }

    /// Returns the canonical logical shard identity.
    pub const fn shard_id(self) -> ShardId {
        self.shard_id
    }

    /// Returns the claimed spatial region.
    pub const fn region(self) -> ShardRegion {
        self.region
    }
}

/// Deterministically maps typed world coordinates to logical shard identities.
///
/// This is a stateless spatial transform, not a directory or worker pool.
///
/// Raw coordinate pairs are not accepted:
///
/// ```compile_fail
/// use ferrumc_core::{DimensionId, WorldId};
/// use ferrumc_sim::ShardPartitioner;
///
/// let raw_coordinates = (0, 0);
/// let _ = ShardPartitioner::for_chunk(
///     WorldId::new(0),
///     DimensionId::new(0),
///     raw_coordinates,
/// );
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ShardPartitioner;

impl ShardPartitioner {
    /// Returns the unique logical shard containing `chunk`.
    pub fn for_chunk(world: WorldId, dimension: DimensionId, chunk: ChunkPos) -> ShardId {
        ShardId::from_chunk_position(world, dimension, chunk.to_shard_pos())
    }

    /// Returns the unique logical shard containing `block`.
    pub fn for_block(world: WorldId, dimension: DimensionId, block: BlockPos) -> ShardId {
        Self::for_chunk(world, dimension, block.to_chunk_pos())
    }

    /// Returns the logical shard at an explicit grid position.
    ///
    /// Unlike coordinate-derived partitioning, an arbitrary [`ShardPos`] may be
    /// outside the range capable of representing a complete 8x8 [`ChunkPos`]
    /// region, so this path is fallible.
    pub fn for_shard(
        world: WorldId,
        dimension: DimensionId,
        position: ShardPos,
    ) -> SimResult<ShardId> {
        ShardId::try_new(world, dimension, position)
    }
}

/// A deterministic set of validated logical-shard ownership claims.
///
/// Claims are ordered by [`ShardRegion`] and reject duplicate overlap
/// atomically. This value stores no channels, workers, leases, generations, or
/// endpoint handles.
#[derive(Debug, Clone, Default)]
pub struct ShardOwnershipMap {
    by_region: BTreeMap<ShardRegion, ShardId>,
}

impl ShardOwnershipMap {
    /// Creates an empty ownership map.
    pub const fn new() -> Self {
        Self {
            by_region: BTreeMap::new(),
        }
    }

    /// Claims one canonical logical shard region.
    ///
    /// If the region is already active, the map is left unchanged and a typed
    /// [`SimError::OverlappingShardOwnership`] identifies the duplicate.
    pub fn claim(&mut self, ownership: ShardOwnership) -> SimResult<()> {
        match self.by_region.entry(ownership.region()) {
            Entry::Vacant(entry) => {
                entry.insert(ownership.shard_id());
                Ok(())
            }
            Entry::Occupied(_) => Err(SimError::OverlappingShardOwnership {
                region: ownership.region(),
                shard: ownership.shard_id(),
            }),
        }
    }

    /// Releases `region` and returns its former ownership claim.
    pub fn release(&mut self, region: ShardRegion) -> Option<ShardOwnership> {
        self.by_region.remove(&region).map(ShardOwnership::new)
    }

    /// Returns the canonical owner of `region`, if it is claimed.
    pub fn owner(&self, region: ShardRegion) -> Option<ShardId> {
        self.by_region.get(&region).copied()
    }

    /// Returns the claimed logical shard containing `chunk`, if present.
    pub fn owner_for_chunk(
        &self,
        world: WorldId,
        dimension: DimensionId,
        chunk: ChunkPos,
    ) -> Option<ShardId> {
        let shard_id = ShardPartitioner::for_chunk(world, dimension, chunk);
        self.owner(shard_id.region())
    }

    /// Returns the claimed logical shard containing `block`, if present.
    pub fn owner_for_block(
        &self,
        world: WorldId,
        dimension: DimensionId,
        block: BlockPos,
    ) -> Option<ShardId> {
        let shard_id = ShardPartitioner::for_block(world, dimension, block);
        self.owner(shard_id.region())
    }

    /// Returns ownership claims in deterministic region order.
    pub fn iter(&self) -> impl Iterator<Item = ShardOwnership> + '_ {
        self.by_region.values().copied().map(ShardOwnership::new)
    }

    /// Returns the number of claimed logical shard regions.
    pub fn len(&self) -> usize {
        self.by_region.len()
    }

    /// Returns whether no logical shard regions are claimed.
    pub fn is_empty(&self) -> bool {
        self.by_region.is_empty()
    }
}

/// The scheduling lifecycle of one logical shard.
///
/// The only allowed progression is `Created -> Active -> Draining -> Stopped`.
/// Same-state, backward, skipped, and post-stop transitions are rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ShardLifecycleState {
    /// The shard identity exists but is not eligible to tick.
    Created,
    /// The shard is eligible for deterministic tick scheduling.
    Active,
    /// The shard accepts no new work and is completing admitted work.
    Draining,
    /// The shard is terminal and cannot be reactivated.
    Stopped,
}

impl fmt::Display for ShardLifecycleState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Created => "created",
            Self::Active => "active",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
        };
        formatter.write_str(name)
    }
}

/// Atomic lifecycle state for one [`ShardId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardLifecycle {
    shard: ShardId,
    state: ShardLifecycleState,
}

impl ShardLifecycle {
    /// Creates a lifecycle in [`ShardLifecycleState::Created`].
    pub const fn new(shard: ShardId) -> Self {
        Self {
            shard,
            state: ShardLifecycleState::Created,
        }
    }

    /// Returns the logical shard whose lifecycle this is.
    pub const fn shard_id(self) -> ShardId {
        self.shard
    }

    /// Returns the current lifecycle state.
    pub const fn state(self) -> ShardLifecycleState {
        self.state
    }

    /// Advances to `next` if it is the one allowed successor.
    ///
    /// Invalid transitions return a typed error without mutating this value.
    pub fn transition_to(&mut self, next: ShardLifecycleState) -> SimResult<()> {
        let allowed = matches!(
            (self.state, next),
            (ShardLifecycleState::Created, ShardLifecycleState::Active)
                | (ShardLifecycleState::Active, ShardLifecycleState::Draining)
                | (ShardLifecycleState::Draining, ShardLifecycleState::Stopped)
        );
        if !allowed {
            return Err(SimError::InvalidShardLifecycleTransition {
                shard: self.shard,
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        Ok(())
    }
}

/// Returns whether both axes can represent one complete fixed shard region.
const fn is_valid_shard_position(position: ShardPos) -> bool {
    position.x() >= MIN_SHARD_COORD
        && position.x() <= MAX_SHARD_COORD
        && position.z() >= MIN_SHARD_COORD
        && position.z() <= MAX_SHARD_COORD
}
