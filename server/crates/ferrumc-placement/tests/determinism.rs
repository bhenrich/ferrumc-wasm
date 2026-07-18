//! Deterministic property coverage for placement resolution.
//!
//! The generator is intentionally local and fixed-seed: the stable gate explores
//! the same bounded cases on every run without adding a property-test dependency.

use std::collections::{BTreeMap, BTreeSet};

use ferrumc_math::{BlockPos, Direction, Vec3};
use ferrumc_placement::{
    compute_placement, is_water_source, NeighborQuery, PlacementContext, PlacementResult,
    PlacementRule,
};
use ferrumc_registry::block_state::{block_metadata, state_id_to_block_name};

const SAMPLE_COUNT: usize = 4_096;
const GENERATOR_SEED: u64 = 0xD37E_21A8_61C0_F11E;

const OAK_SLAB: u32 = 12_054;
const OAK_SLAB_TOP: u32 = 12_052;
const OAK_STAIRS: u32 = 2_949;
const COBBLESTONE_WALL: u32 = 8_706;
const OAK_DOOR: u32 = 4_697;
const WHITE_BED: u32 = 1_734;

const ITEM_STATES: [u32; 27] = [
    0,   // air: registry-valid even though held-item policy normally rejects it
    1,   // stone
    137, // oak log
    OAK_SLAB,
    OAK_STAIRS,
    2_401,  // torch
    6_027,  // oak fence
    4_359,  // furnace
    6_155,  // oak trapdoor
    7_375,  // oak fence gate
    5_935,  // stone button
    5_811,  // lever
    9_916,  // anvil
    13_361, // end rod
    COBBLESTONE_WALL,
    7_053,  // glass pane
    7_015,  // iron bars
    4_751,  // ladder
    7_019,  // chain
    19_561, // lantern
    22_102, // amethyst cluster
    25_813, // pointed dripstone
    21_788, // candle
    OAK_DOOR,
    WHITE_BED,
    u32::MAX - 1,
    u32::MAX,
];

const NEIGHBOR_STATES: [u32; 18] = [
    0,  // air
    1,  // solid cube
    86, // source water
    87, // flowing water
    OAK_SLAB,
    OAK_SLAB_TOP,
    12_120, // stone slab
    OAK_STAIRS,
    2_969, // south-facing oak stairs
    2_989, // west-facing oak stairs
    3_009, // east-facing oak stairs
    COBBLESTONE_WALL,
    7_053,  // glass pane
    21_788, // one unlit candle
    21_786, // one lit candle
    21_800, // four candles
    OAK_DOOR,
    u32::MAX, // unreadable/unknown neighbour state
];

const CURSOR_COMPONENTS: [f64; 5] = [0.0, 0.25, 0.5, 0.75, 1.0];
const YAWS: [f32; 20] = [
    -f32::MAX,
    -720.0,
    -360.0,
    -315.0,
    -180.0,
    -90.0,
    -45.0,
    -0.0,
    0.0,
    44.999,
    45.0,
    89.999,
    90.0,
    179.999,
    180.0,
    269.999,
    270.0,
    315.0,
    720.0,
    f32::MAX,
];
const POSITION_COMPONENTS: [i32; 9] = [
    i32::MIN + 2,
    -30_000_000,
    -319,
    -1,
    0,
    1,
    319,
    30_000_000,
    i32::MAX - 2,
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Snapshot {
    states: BTreeMap<BlockPos, u32>,
    fence_connectable: BTreeSet<BlockPos>,
}

impl NeighborQuery for Snapshot {
    fn is_fence_connectable(&self, position: BlockPos, _fence_block_name: &str) -> bool {
        self.fence_connectable.contains(&position)
    }

    fn block_state_at(&self, position: BlockPos) -> Option<u32> {
        self.states.get(&position).copied()
    }
}

/// `SplitMix64` supplies a small reproducible stream without wall-clock state or a
/// new dependency. It is test-data generation, not a security primitive.
struct FixedGenerator(u64);

impl FixedGenerator {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    fn index(&mut self, bound: usize) -> usize {
        assert!(bound > 0, "generator choices must be non-empty");
        let bound = u64::try_from(bound).expect("choice count fits u64");
        usize::try_from(self.next_u64() % bound).expect("choice index fits usize")
    }

    fn choose<T: Copy>(&mut self, values: &[T]) -> T {
        values[self.index(values.len())]
    }

    fn coin(&mut self) -> bool {
        self.next_u64() & 1 == 0
    }
}

fn generated_snapshot(generator: &mut FixedGenerator, center: BlockPos) -> Snapshot {
    let mut snapshot = Snapshot::default();
    for dx in -1..=1 {
        for dy in -1..=1 {
            for dz in -1..=1 {
                let position = BlockPos::new(center.x() + dx, center.y() + dy, center.z() + dz);
                if generator.coin() {
                    snapshot
                        .states
                        .insert(position, generator.choose(&NEIGHBOR_STATES));
                }
                if generator.coin() {
                    snapshot.fence_connectable.insert(position);
                }
            }
        }
    }
    snapshot
}

fn generated_case(
    case_index: usize,
    generator: &mut FixedGenerator,
) -> (PlacementContext, Snapshot) {
    // Cycle the primary axes systematically; the fixed stream supplies the
    // remaining dimensions and neighbour topology.
    let item_block_state = ITEM_STATES[case_index % ITEM_STATES.len()];
    let face_index = (case_index / ITEM_STATES.len()) % Direction::ALL.len();
    let cursor_index =
        (case_index / (ITEM_STATES.len() * Direction::ALL.len())) % CURSOR_COMPONENTS.len();
    let position = BlockPos::new(
        generator.choose(&POSITION_COMPONENTS),
        generator.choose(&POSITION_COMPONENTS),
        generator.choose(&POSITION_COMPONENTS),
    );
    let context = PlacementContext {
        item_block_state,
        clicked_face: Direction::ALL[face_index],
        cursor_position: Vec3::new(
            generator.choose(&CURSOR_COMPONENTS),
            CURSOR_COMPONENTS[cursor_index],
            generator.choose(&CURSOR_COMPONENTS),
        ),
        player_yaw: generator.choose(&YAWS),
        position,
    };
    let snapshot = generated_snapshot(generator, position);
    (context, snapshot)
}

fn canonical_writes(context: &PlacementContext, result: &PlacementResult) -> Vec<(BlockPos, u32)> {
    let mut writes = Vec::with_capacity(1 + result.extra_blocks.len());
    writes.push((result.place_at.unwrap_or(context.position), result.state_id));
    writes.extend(result.extra_blocks.iter().copied());
    writes
}

fn replay_writes(snapshot: &Snapshot, writes: &[(BlockPos, u32)]) -> Snapshot {
    let mut replayed = snapshot.clone();
    for &(position, state) in writes {
        replayed.states.insert(position, state);
    }
    replayed
}

fn consumes_source_water(
    context: &PlacementContext,
    snapshot: &Snapshot,
    result: &PlacementResult,
) -> bool {
    snapshot
        .block_state_at(context.position)
        .is_some_and(is_water_source)
        && state_id_to_block_name(result.state_id)
            .and_then(block_metadata)
            .is_some_and(|metadata| {
                metadata
                    .properties
                    .iter()
                    .any(|property| property.name == "waterlogged")
            })
}

#[test]
fn fixed_seed_placement_resolution_is_exactly_deterministic() {
    let mut generator = FixedGenerator::new(GENERATOR_SEED);
    let mut resolved = 0usize;
    let mut rejected = 0usize;
    let mut multi_cell = 0usize;
    let mut relocated = 0usize;
    let mut replay_resolved = 0usize;
    let mut replay_resolved_multi_cell = 0usize;

    for case_index in 0..SAMPLE_COUNT {
        let (context, snapshot) = generated_case(case_index, &mut generator);
        let unchanged = snapshot.clone();

        let first = compute_placement(&context, &snapshot);
        let second = compute_placement(&context, &snapshot.clone());
        assert_eq!(
            first, second,
            "case {case_index} diverged for {context:?} with {snapshot:?}"
        );
        assert_eq!(
            snapshot, unchanged,
            "case {case_index} mutated its immutable snapshot"
        );

        let Some(result) = first else {
            rejected += 1;
            continue;
        };
        resolved += 1;
        assert_eq!(result.requested_state, context.item_block_state);

        let writes = canonical_writes(&context, &result);
        let unique_positions = writes
            .iter()
            .map(|(position, _)| *position)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            unique_positions.len(),
            writes.len(),
            "case {case_index} produced overlapping canonical writes"
        );
        assert!(
            writes
                .iter()
                .all(|(_, state)| state_id_to_block_name(*state).is_some()),
            "case {case_index} produced a non-registry state"
        );

        let independently_resolved =
            compute_placement(&context, &unchanged).expect("the same valid input resolves");
        assert_eq!(
            writes,
            canonical_writes(&context, &independently_resolved),
            "case {case_index} changed its canonical write plan"
        );

        let placed = replay_writes(&snapshot, &writes);
        // Relocated merges intentionally change the clicked cell that decides
        // the next action, while waterlogging consumes the source-water input.
        // Every other rule must re-resolve through production code to the exact
        // same plan after its own writes have been applied.
        if result.place_at.is_none() && !consumes_source_water(&context, &snapshot, &result) {
            let replay = compute_placement(&context, &placed)
                .expect("an idempotent resolved placement remains resolvable");
            assert_eq!(
                replay, result,
                "case {case_index} changed after its placement plan was applied"
            );
            let replayed_writes = canonical_writes(&context, &replay);
            assert_eq!(
                replayed_writes, writes,
                "case {case_index} changed canonical writes after placement"
            );
            assert_eq!(
                replay_writes(&placed, &replayed_writes),
                placed,
                "case {case_index} replay was not idempotent"
            );
            replay_resolved += 1;
            if !result.extra_blocks.is_empty() {
                replay_resolved_multi_cell += 1;
            }
        }

        if result.extra_blocks.is_empty() {
            assert_eq!(writes.len(), 1);
        } else {
            multi_cell += 1;
        }
        if result.place_at.is_some() {
            relocated += 1;
        }
    }

    assert!(resolved > SAMPLE_COUNT / 2, "valid families must dominate");
    assert!(rejected > 0, "unknown-state boundaries must be exercised");
    assert!(
        multi_cell > 0,
        "door/bed canonical writes must be exercised"
    );
    assert!(
        relocated > 0,
        "slab/candle relocated writes must be exercised"
    );
    assert!(
        replay_resolved > SAMPLE_COUNT / 2,
        "production place-to-resolve replay must cover most generated cases"
    );
    assert!(
        replay_resolved_multi_cell > 0,
        "door/bed writes must survive production re-resolution"
    );
}

#[test]
fn documented_unknown_and_safe_coordinate_boundaries_are_deterministic() {
    let safe_extremes = [
        BlockPos::new(i32::MIN + 2, i32::MIN + 2, i32::MIN + 2),
        BlockPos::new(i32::MAX - 2, i32::MAX - 2, i32::MAX - 2),
    ];

    for position in safe_extremes {
        for unknown in [u32::MAX - 1, u32::MAX] {
            let context = PlacementContext {
                item_block_state: unknown,
                clicked_face: Direction::East,
                cursor_position: Vec3::new(0.0, 0.5, 1.0),
                player_yaw: f32::MAX,
                position,
            };
            assert_eq!(compute_placement(&context, &Snapshot::default()), None);
            assert_eq!(compute_placement(&context, &Snapshot::default()), None);
        }

        let mut snapshot = Snapshot::default();
        for direction in Direction::ALL {
            snapshot.states.insert(position.offset(direction), u32::MAX);
        }
        for (item, yaw) in [
            (OAK_STAIRS, -f32::MAX),
            (COBBLESTONE_WALL, f32::MAX),
            (OAK_DOOR, -0.0),
            (WHITE_BED, 0.0),
        ] {
            let context = PlacementContext {
                item_block_state: item,
                clicked_face: Direction::Down,
                cursor_position: Vec3::new(0.0, 1.0, 1.0),
                player_yaw: yaw,
                position,
            };
            let first = compute_placement(&context, &snapshot);
            assert_eq!(first, compute_placement(&context, &snapshot));
            assert!(
                first.is_some(),
                "registry-valid placement must degrade around unreadable neighbours"
            );
        }
    }
}

#[derive(Debug, PartialEq)]
struct SlabMergeFixture {
    item_state: u32,
    face: Direction,
    cursor_y: f64,
    yaw: f32,
    position: BlockPos,
    existing_position: BlockPos,
    existing_state: u32,
    expected_state: u32,
}

fn parse_direction(value: &str) -> Direction {
    match value {
        "down" => Direction::Down,
        "up" => Direction::Up,
        "north" => Direction::North,
        "south" => Direction::South,
        "west" => Direction::West,
        "east" => Direction::East,
        other => panic!("unknown fixture direction {other}"),
    }
}

fn slab_merge_fixture() -> SlabMergeFixture {
    let line = include_str!("fixtures/slab_merge_relocation.case")
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .expect("fixture contains one data row");
    let mut fields = line.split_ascii_whitespace();
    let item_state = fields
        .next()
        .expect("item state")
        .parse()
        .expect("item state is u32");
    let face = parse_direction(fields.next().expect("clicked face"));
    let cursor_y = fields
        .next()
        .expect("cursor y")
        .parse()
        .expect("cursor y is f64");
    let yaw = fields.next().expect("yaw").parse().expect("yaw is f32");
    let x = fields
        .next()
        .expect("position x")
        .parse()
        .expect("position x is i32");
    let y = fields
        .next()
        .expect("position y")
        .parse()
        .expect("position y is i32");
    let z = fields
        .next()
        .expect("position z")
        .parse()
        .expect("position z is i32");
    let existing_x = fields
        .next()
        .expect("existing position x")
        .parse()
        .expect("existing position x is i32");
    let existing_y = fields
        .next()
        .expect("existing position y")
        .parse()
        .expect("existing position y is i32");
    let existing_z = fields
        .next()
        .expect("existing position z")
        .parse()
        .expect("existing position z is i32");
    let existing_state = fields
        .next()
        .expect("existing state")
        .parse()
        .expect("existing state is u32");
    let expected_state = fields
        .next()
        .expect("expected state")
        .parse()
        .expect("expected state is u32");
    assert!(fields.next().is_none(), "fixture has exactly twelve fields");

    SlabMergeFixture {
        item_state,
        face,
        cursor_y,
        yaw,
        position: BlockPos::new(x, y, z),
        existing_position: BlockPos::new(existing_x, existing_y, existing_z),
        existing_state,
        expected_state,
    }
}

#[test]
fn pinned_minimal_case_resolves_and_applies_a_relocated_slab_merge() {
    let fixture = slab_merge_fixture();
    let context = PlacementContext {
        item_block_state: fixture.item_state,
        clicked_face: fixture.face,
        cursor_position: Vec3::new(0.5, fixture.cursor_y, 0.5),
        player_yaw: fixture.yaw,
        position: fixture.position,
    };
    let mut snapshot = Snapshot::default();
    snapshot
        .states
        .insert(fixture.existing_position, fixture.existing_state);

    let result =
        compute_placement(&context, &snapshot).expect("the fixture carries a registry-valid slab");
    assert_eq!(result.state_id, fixture.expected_state);
    assert_eq!(result.state_id, 12_056);
    assert_eq!(result.requested_state, OAK_SLAB);
    assert_eq!(result.rule, PlacementRule::DoubleSlab);
    assert_eq!(result.place_at, Some(fixture.existing_position));
    assert!(result.extra_blocks.is_empty());

    let writes = canonical_writes(&context, &result);
    assert_eq!(writes, vec![(fixture.existing_position, 12_056)]);
    let placed = replay_writes(&snapshot, &writes);
    assert_eq!(placed.block_state_at(fixture.position), None);
    assert_eq!(
        placed.block_state_at(fixture.existing_position),
        Some(fixture.expected_state)
    );

    // The first action consumed the half slab at the clicked cell, so issuing
    // the same click again is deliberately a new ordinary placement rather than
    // an idempotent merge replay.
    let next = compute_placement(&context, &placed)
        .expect("the same held slab remains a registry-valid placement");
    assert_eq!(next.rule, PlacementRule::Slab);
    assert_eq!(next.place_at, None);
}
