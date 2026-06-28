//! Placement benchmarks: `compute_placement` throughput for each block family,
//! plus an all-families mix.
//!
//! The default block-state ids below are the canonical ones from
//! `ferrumc_placement`'s own tests, so the inputs exercise real registry lookups.

use ferrumc_math::{BlockPos, Direction, Vec3};
use ferrumc_placement::{compute_placement, NeighborQuery, NoNeighbors, PlacementContext};

use crate::harness::{run_benchmark, timed, Sample};
use crate::report::BenchResult;
use crate::BenchConfig;

/// Group name for these benchmarks.
const GROUP: &str = "placement";

// Default block-state ids for one representative member of each placement family.
const STONE: u32 = 1; // SimpleCube
const OAK_LOG: u32 = 137; // AxisFromFace
const OAK_SLAB: u32 = 12_054; // Slab
const OAK_STAIRS: u32 = 2_949; // StairBasic
const TORCH: u32 = 2_401; // TorchFloor / TorchWall
const OAK_FENCE: u32 = 6_027; // FenceLike
const FURNACE: u32 = 4_359; // HorizontalFacing

/// A [`NeighborQuery`] where the listed positions connect (everything else does
/// not), used to exercise the fence connectivity path.
struct ConnectingNeighbors {
    connectable: Vec<BlockPos>,
}

impl NeighborQuery for ConnectingNeighbors {
    fn is_fence_connectable(&self, position: BlockPos, _fence_block_name: &str) -> bool {
        self.connectable.contains(&position)
    }
}

/// Builds a placement context for `item` clicked on `face` at cursor height
/// `cursor_y` with the player facing `yaw`.
fn context(item: u32, face: Direction, cursor_y: f64, yaw: f32) -> PlacementContext {
    PlacementContext {
        item_block_state: item,
        clicked_face: face,
        cursor_position: Vec3::new(0.5, cursor_y, 0.5),
        player_yaw: yaw,
        position: BlockPos::new(0, 64, 0),
    }
}

/// Builds and runs the `placement` benchmark group.
#[must_use]
pub fn benchmarks(config: &BenchConfig) -> Vec<BenchResult> {
    let mut out = Vec::new();

    // A fence at (0,64,0) with two connectable cardinal neighbours (north + east).
    let fence_neighbors = ConnectingNeighbors {
        connectable: vec![BlockPos::new(0, 64, -1), BlockPos::new(1, 64, 0)],
    };

    // (name, context, neighbour query) for each single-family benchmark.
    let no_neighbors = NoNeighbors;
    let cases: [(&str, PlacementContext, &dyn NeighborQuery); 8] = [
        (
            "simple_cube",
            context(STONE, Direction::North, 0.9, 123.0),
            &no_neighbors,
        ),
        (
            "axis_from_face",
            context(OAK_LOG, Direction::East, 0.5, 0.0),
            &no_neighbors,
        ),
        (
            "slab",
            context(OAK_SLAB, Direction::North, 0.8, 0.0),
            &no_neighbors,
        ),
        (
            "stair_basic",
            context(OAK_STAIRS, Direction::East, 0.2, 0.0),
            &no_neighbors,
        ),
        (
            "torch_floor",
            context(TORCH, Direction::Up, 0.5, 0.0),
            &no_neighbors,
        ),
        (
            "torch_wall",
            context(TORCH, Direction::North, 0.5, 0.0),
            &no_neighbors,
        ),
        (
            "horizontal_facing",
            context(FURNACE, Direction::Up, 0.5, 0.0),
            &fence_neighbors,
        ),
        (
            "fence_like",
            context(OAK_FENCE, Direction::Up, 0.5, 0.0),
            &fence_neighbors,
        ),
    ];

    for (name, ctx, neighbors) in &cases {
        let bench_name = format!("placement_{name}");
        let Some(spec) = config.spec_if_included(GROUP, &bench_name, None) else {
            continue;
        };
        out.push(run_benchmark(&spec, || {
            let (nanos, result) = timed(|| compute_placement(ctx, *neighbors));
            let _ = result;
            Sample { nanos, units: 1 }
        }));
    }

    // Aggregate: compute every family once per iteration.
    if let Some(spec) =
        config.spec_if_included(GROUP, "placement_mix_all_families", Some("placements"))
    {
        let family_count = cases.len() as u64;
        out.push(run_benchmark(&spec, || {
            let (nanos, _) = timed(|| {
                let mut computed = 0u32;
                for (_, ctx, neighbors) in &cases {
                    let result = compute_placement(ctx, *neighbors);
                    if result.is_some() {
                        computed += 1;
                    }
                }
                computed
            });
            Sample {
                nanos,
                units: family_count,
            }
        }));
    }

    out
}
