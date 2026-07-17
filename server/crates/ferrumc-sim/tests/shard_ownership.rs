use std::collections::BTreeSet;

use ferrumc_core::{DimensionId, WorldId};
use ferrumc_math::{BlockPos, ChunkPos, ShardPos};
use ferrumc_sim::{
    ShardId, ShardLifecycle, ShardLifecycleState, ShardOwnership, ShardOwnershipMap,
    ShardPartitioner, ShardRegion, SimError, MAX_SHARD_COORD, MIN_SHARD_COORD, SHARD_WIDTH_CHUNKS,
};

const WORLD: WorldId = WorldId::new(7);
const DIMENSION: DimensionId = DimensionId::new(3);

#[test]
fn every_valid_coordinate_maps_to_exactly_one_logical_shard() {
    let mut ownerships = Vec::new();
    let mut map = ShardOwnershipMap::new();
    for shard_x in -2..=2 {
        for shard_z in -2..=2 {
            let shard_id =
                ShardPartitioner::for_shard(WORLD, DIMENSION, ShardPos::new(shard_x, shard_z))
                    .expect("small shard coordinates form valid regions");
            let ownership = ShardOwnership::new(shard_id);
            map.claim(ownership)
                .expect("fixed partition regions do not overlap");
            ownerships.push(ownership);
        }
    }

    for chunk_x in -16..=23 {
        for chunk_z in -16..=23 {
            let chunk = ChunkPos::new(chunk_x, chunk_z);
            let containing: Vec<_> = ownerships
                .iter()
                .copied()
                .filter(|ownership| ownership.region().contains_chunk(WORLD, DIMENSION, chunk))
                .collect();
            assert_eq!(containing.len(), 1, "{chunk:?} must have exactly one owner");

            let partitioned = ShardPartitioner::for_chunk(WORLD, DIMENSION, chunk);
            assert_eq!(partitioned, containing[0].shard_id());
            assert_eq!(
                map.owner_for_chunk(WORLD, DIMENSION, chunk),
                Some(partitioned),
            );
        }
    }

    for ownership in ownerships {
        let region = ownership.region();
        let mut chunks = BTreeSet::new();
        for chunk_x in region.min_chunk().x()..=region.max_chunk().x() {
            for chunk_z in region.min_chunk().z()..=region.max_chunk().z() {
                let chunk = ChunkPos::new(chunk_x, chunk_z);
                assert!(region.contains_chunk(WORLD, DIMENSION, chunk));
                assert_eq!(
                    ShardPartitioner::for_chunk(WORLD, DIMENSION, chunk),
                    ownership.shard_id(),
                );
                chunks.insert(chunk);
            }
        }
        assert_eq!(
            chunks.len(),
            usize::try_from(SHARD_WIDTH_CHUNKS * SHARD_WIDTH_CHUNKS)
                .expect("positive shard area fits usize"),
        );
    }

    let other_world = ShardPartitioner::for_chunk(WorldId::new(8), DIMENSION, ChunkPos::new(0, 0));
    let other_dimension =
        ShardPartitioner::for_chunk(WORLD, DimensionId::new(4), ChunkPos::new(0, 0));
    let local = ShardPartitioner::for_chunk(WORLD, DIMENSION, ChunkPos::new(0, 0));
    assert_ne!(other_world, local);
    assert_ne!(other_dimension, local);
    assert!(!other_world.region().overlaps(local.region()));
    assert!(!other_dimension.region().overlaps(local.region()));

    let maximum_scope = ShardPartitioner::for_chunk(
        WorldId::new(u32::MAX),
        DimensionId::new(u32::MAX),
        ChunkPos::new(0, 0),
    );
    assert_eq!(maximum_scope.world(), WorldId::new(u32::MAX));
    assert_eq!(maximum_scope.dimension(), DimensionId::new(u32::MAX));
    assert_ne!(maximum_scope, local);
}

#[test]
fn partitioner_floors_negative_positive_and_integer_boundaries() {
    let chunk_cases = [
        (i32::MIN, i32::MIN >> 3),
        (-9, -2),
        (-8, -1),
        (-1, -1),
        (0, 0),
        (7, 0),
        (8, 1),
        (i32::MAX, i32::MAX >> 3),
    ];
    for (coordinate, expected_shard) in chunk_cases {
        let chunk = ChunkPos::new(coordinate, -coordinate.saturating_add(1));
        let shard_id = ShardPartitioner::for_chunk(WORLD, DIMENSION, chunk);
        assert_eq!(shard_id.position().x(), expected_shard);
        assert_eq!(shard_id.position().z(), chunk.z() >> 3);
        assert!(shard_id.region().contains_chunk(WORLD, DIMENSION, chunk));
    }
    for coordinate in -1024..=1024 {
        let shard_id = ShardPartitioner::for_chunk(WORLD, DIMENSION, ChunkPos::new(coordinate, 0));
        assert_eq!(shard_id.position().x(), coordinate.div_euclid(8));
    }

    let block_cases = [
        (i32::MIN, i32::MIN >> 7),
        (-129, -2),
        (-128, -1),
        (-1, -1),
        (0, 0),
        (127, 0),
        (128, 1),
        (i32::MAX, i32::MAX >> 7),
    ];
    for (coordinate, expected_shard) in block_cases {
        let low = BlockPos::new(coordinate, i32::MIN, coordinate);
        let high = BlockPos::new(coordinate, i32::MAX, coordinate);
        let low_owner = ShardPartitioner::for_block(WORLD, DIMENSION, low);
        let high_owner = ShardPartitioner::for_block(WORLD, DIMENSION, high);
        assert_eq!(
            low_owner, high_owner,
            "vertical position cannot change ownership",
        );
        assert_eq!(
            low_owner,
            ShardPartitioner::for_chunk(WORLD, DIMENSION, low.to_chunk_pos()),
        );
        assert_eq!(low_owner.position().x(), expected_shard);
        assert_eq!(low_owner.position().z(), expected_shard);
    }
    for coordinate in -1024..=1024 {
        let shard_id =
            ShardPartitioner::for_block(WORLD, DIMENSION, BlockPos::new(coordinate, 0, 0));
        assert_eq!(shard_id.position().x(), coordinate.div_euclid(128));
    }
}

#[test]
fn shard_region_extremes_are_exactly_eight_by_eight() {
    let minimum = ShardPartitioner::for_shard(
        WORLD,
        DIMENSION,
        ShardPos::new(MIN_SHARD_COORD, MIN_SHARD_COORD),
    )
    .expect("minimum chunk-derived shard is valid");
    assert_eq!(
        minimum.region().min_chunk(),
        ChunkPos::new(i32::MIN, i32::MIN),
    );
    assert_eq!(
        minimum.region().max_chunk(),
        ChunkPos::new(i32::MIN + 7, i32::MIN + 7),
    );

    let maximum = ShardPartitioner::for_shard(
        WORLD,
        DIMENSION,
        ShardPos::new(MAX_SHARD_COORD, MAX_SHARD_COORD),
    )
    .expect("maximum chunk-derived shard is valid");
    assert_eq!(
        maximum.region().min_chunk(),
        ChunkPos::new(i32::MAX - 7, i32::MAX - 7),
    );
    assert_eq!(
        maximum.region().max_chunk(),
        ChunkPos::new(i32::MAX, i32::MAX),
    );

    let mixed = ShardPartitioner::for_shard(
        WORLD,
        DIMENSION,
        ShardPos::new(MIN_SHARD_COORD, MAX_SHARD_COORD),
    )
    .expect("mixed extreme axes form a valid region");
    assert_eq!(
        mixed.region().min_chunk(),
        ChunkPos::new(i32::MIN, i32::MAX - 7),
    );
    assert_eq!(
        mixed.region().max_chunk(),
        ChunkPos::new(i32::MIN + 7, i32::MAX),
    );

    for invalid in [
        ShardPos::new(MIN_SHARD_COORD - 1, 0),
        ShardPos::new(0, MIN_SHARD_COORD - 1),
        ShardPos::new(MAX_SHARD_COORD + 1, 0),
        ShardPos::new(0, MAX_SHARD_COORD + 1),
    ] {
        assert_eq!(
            ShardPartitioner::for_shard(WORLD, DIMENSION, invalid)
                .expect_err("a region outside the ChunkPos domain must not wrap"),
            SimError::ShardRegionOutOfRange { position: invalid },
        );
    }
}

#[test]
fn overlapping_ownership_is_rejected_atomically() {
    let original_id = ShardPartitioner::for_chunk(WORLD, DIMENSION, ChunkPos::new(-1, -1));
    let original_region = original_id.region();
    let original = ShardOwnership::new(original_id);
    let mut map = ShardOwnershipMap::new();
    map.claim(original).expect("first claim succeeds");

    let error = map
        .claim(original)
        .expect_err("the same logical region cannot be claimed twice");
    assert_eq!(
        error,
        SimError::OverlappingShardOwnership {
            region: original_region,
            shard: original_id,
        },
    );
    assert_eq!(map.len(), 1);
    assert_eq!(map.owner(original_region), Some(original_id));

    let adjacent = ShardPartitioner::for_chunk(WORLD, DIMENSION, ChunkPos::new(0, 0));
    map.claim(ShardOwnership::new(adjacent))
        .expect("an adjacent fixed region is disjoint");
    let other_world =
        ShardPartitioner::for_chunk(WorldId::new(8), DIMENSION, ChunkPos::new(-1, -1));
    map.claim(ShardOwnership::new(other_world))
        .expect("the same coordinates in another world are disjoint");
    let other_dimension =
        ShardPartitioner::for_chunk(WORLD, DimensionId::new(4), ChunkPos::new(-1, -1));
    map.claim(ShardOwnership::new(other_dimension))
        .expect("the same coordinates in another dimension are disjoint");
    assert_eq!(map.len(), 4);

    let mut expected = vec![original_id, adjacent, other_world, other_dimension];
    expected.sort_unstable();
    let iterated: Vec<_> = map.iter().map(ShardOwnership::shard_id).collect();
    assert_eq!(iterated, expected);

    assert_eq!(
        map.owner_for_block(WORLD, DIMENSION, BlockPos::ORIGIN),
        Some(adjacent),
    );
    assert_eq!(
        map.release(adjacent.region()),
        Some(ShardOwnership::new(adjacent)),
    );
    assert_eq!(
        map.owner_for_block(WORLD, DIMENSION, BlockPos::ORIGIN),
        None,
    );
    assert_eq!(map.len(), 3);
    map.claim(ShardOwnership::new(adjacent))
        .expect("a released canonical region can be reclaimed");
    assert_eq!(map.len(), 4);
}

#[test]
fn invalid_lifecycle_transitions_are_typed_and_atomic() {
    const STATES: [ShardLifecycleState; 4] = [
        ShardLifecycleState::Created,
        ShardLifecycleState::Active,
        ShardLifecycleState::Draining,
        ShardLifecycleState::Stopped,
    ];
    const ALLOWED: [(ShardLifecycleState, ShardLifecycleState); 3] = [
        (ShardLifecycleState::Created, ShardLifecycleState::Active),
        (ShardLifecycleState::Active, ShardLifecycleState::Draining),
        (ShardLifecycleState::Draining, ShardLifecycleState::Stopped),
    ];

    let shard = ShardPartitioner::for_chunk(WORLD, DIMENSION, ChunkPos::new(0, 0));
    for from in STATES {
        for to in STATES {
            let mut lifecycle = lifecycle_at(shard, from);
            let result = lifecycle.transition_to(to);
            if ALLOWED.contains(&(from, to)) {
                assert_eq!(result, Ok(()));
                assert_eq!(lifecycle.state(), to);
            } else {
                assert_eq!(
                    result,
                    Err(SimError::InvalidShardLifecycleTransition { shard, from, to }),
                );
                assert_eq!(
                    lifecycle.state(),
                    from,
                    "a rejected transition must not mutate lifecycle state",
                );
            }
        }
    }

    let mut lifecycle = ShardLifecycle::new(shard);
    for state in [
        ShardLifecycleState::Active,
        ShardLifecycleState::Draining,
        ShardLifecycleState::Stopped,
    ] {
        lifecycle
            .transition_to(state)
            .expect("the documented forward lifecycle is valid");
    }
    assert_eq!(lifecycle.state(), ShardLifecycleState::Stopped);
}

#[test]
fn partitioner_public_api_uses_typed_coordinates() {
    let _: fn(WorldId, DimensionId, ChunkPos) -> ShardId = ShardPartitioner::for_chunk;
    let _: fn(WorldId, DimensionId, BlockPos) -> ShardId = ShardPartitioner::for_block;
    let _: fn(WorldId, DimensionId, ShardPos) -> Result<ShardId, SimError> =
        ShardPartitioner::for_shard;

    let shard_id = ShardPartitioner::for_chunk(WORLD, DIMENSION, ChunkPos::new(0, 0));
    let ownership = ShardOwnership::new(shard_id);
    let _: ShardId = ownership.shard_id();
    let _: ShardRegion = ownership.region();
}

fn lifecycle_at(shard: ShardId, target: ShardLifecycleState) -> ShardLifecycle {
    let mut lifecycle = ShardLifecycle::new(shard);
    for state in [
        ShardLifecycleState::Active,
        ShardLifecycleState::Draining,
        ShardLifecycleState::Stopped,
    ] {
        if lifecycle.state() == target {
            break;
        }
        lifecycle
            .transition_to(state)
            .expect("helper follows only valid lifecycle transitions");
    }
    assert_eq!(lifecycle.state(), target);
    lifecycle
}
