//! Synthetic simulation-tick benchmarks: `SimShard::run_tick` cost as the number
//! of per-tick block edits (`K`) grows.
//!
//! Setup (untimed) makes chunk `(0,0)` resident via a single inline `acquire`
//! (storage miss -> flat generation, driven by [`crate::block_on`]) and spawns
//! one player. Each measured iteration enqueues `K` in-reach `SetBlockExact`
//! edits and times one `run_tick`, reporting nanoseconds per tick (mean mspt) and
//! edits/sec.

use ferrumc_core::PlayerId;
use ferrumc_math::{BlockPos, ChunkPos, ShardPos, Vec3};
use ferrumc_sim::{BlockStateId, ChunkTicket, GameInput, SimShard, TicketReason};
use ferrumc_storage::InMemoryStore;
use ferrumc_world::FlatWorldGenerator;

use crate::block_on::block_on;
use crate::harness::{run_benchmark, timed, Sample};
use crate::report::BenchResult;
use crate::BenchConfig;

/// Group name for these benchmarks.
const GROUP: &str = "sim";

/// The player's spawn position; in-reach edit targets cluster around it.
const SPAWN: Vec3 = Vec3::new(8.0, 64.0, 8.0);

/// Maximum interaction reach, in blocks, mirrored from the simulation so every
/// generated edit target is accepted under the reach check.
const MAX_REACH: f64 = 6.0;

/// Builds and runs the `sim` benchmark group.
#[must_use]
pub fn benchmarks(config: &BenchConfig) -> Vec<BenchResult> {
    let mut out = Vec::new();

    // Precompute each benchmark's name so we can skip the (synchronous but
    // non-trivial) shard setup entirely when the filter excludes the group.
    let names: Vec<(usize, String)> = config
        .sim_edit_counts
        .iter()
        .map(|&k| (k, format!("sim_tick_k{k}")))
        .collect();
    let any_included = names.iter().any(|(_, name)| {
        config
            .spec_if_included(GROUP, name, Some("edits"))
            .is_some()
    });
    if !any_included {
        return out;
    }

    let Some((mut shard, player, positions)) = build_loaded_shard() else {
        return out;
    };

    // Cross-iteration cursors so successive edits hit different in-reach blocks
    // and toggle their state; kept outside the per-K closures so the work is real.
    let mut sequence: i32 = 0;
    let mut position_index: usize = 0;
    let mut toggle = false;
    let position_count = positions.len();

    for (k, name) in &names {
        let edits = *k;
        let Some(spec) = config.spec_if_included(GROUP, name, Some("edits")) else {
            continue;
        };
        let mut result = run_benchmark(&spec, || {
            for _ in 0..edits {
                let position = positions[position_index % position_count];
                position_index += 1;
                let state = if toggle {
                    BlockStateId::AIR
                } else {
                    BlockStateId::new(1)
                };
                toggle = !toggle;
                let _ = shard.enqueue(GameInput::SetBlockExact {
                    player,
                    position,
                    sequence,
                    state,
                });
                sequence += 1;
            }
            let (nanos, outputs) = timed(|| shard.run_tick());
            drop(outputs);
            Sample {
                nanos,
                units: edits as u64,
            }
        });
        result.add_metric("edits_per_tick", "edits", edits as f64);
        let mspt = result.mean_ns / 1.0e6;
        result.add_metric("mean_mspt", "ms", mspt);
        out.push(result);
    }

    out
}

/// Builds a shard with chunk `(0,0)` resident and one spawned player, returning
/// it along with the player id and a set of in-reach, in-chunk edit targets.
///
/// Returns `None` if the (otherwise infallible) inline acquire ever fails, so the
/// caller skips the group rather than panicking.
fn build_loaded_shard() -> Option<(SimShard, PlayerId, Vec<BlockPos>)> {
    let mut shard = SimShard::new(ShardPos::new(0, 0));
    let store = InMemoryStore::new();
    let generator = FlatWorldGenerator::new();

    let acquired = block_on(shard.loaded_chunks_mut().acquire(
        &store,
        &generator,
        ChunkPos::new(0, 0),
        ChunkTicket::of(TicketReason::Player),
    ));
    if acquired.is_err() {
        return None;
    }

    let player = PlayerId::offline("bench");
    let _ = shard.enqueue(GameInput::PlayerJoin {
        player,
        position: SPAWN,
    });
    let _ = shard.run_tick();

    let positions = in_reach_positions();
    if positions.is_empty() {
        return None;
    }
    Some((shard, player, positions))
}

/// Collects block positions within [`MAX_REACH`] of [`SPAWN`] and inside chunk
/// `(0,0)` (so every edit lands in the resident chunk and passes the reach check).
fn in_reach_positions() -> Vec<BlockPos> {
    let mut positions = Vec::new();
    let base = BlockPos::new(8, 63, 8);
    for dy in -3..=3 {
        for dz in -3..=3 {
            for dx in -3..=3 {
                let pos = BlockPos::new(base.x() + dx, base.y() + dy, base.z() + dz);
                // Stay inside chunk (0,0)'s 16x16 column footprint.
                if !(0..16).contains(&pos.x()) || !(0..16).contains(&pos.z()) {
                    continue;
                }
                let centre = Vec3::new(
                    f64::from(pos.x()) + 0.5,
                    f64::from(pos.y()) + 0.5,
                    f64::from(pos.z()) + 0.5,
                );
                if (SPAWN - centre).length_squared() <= MAX_REACH * MAX_REACH {
                    positions.push(pos);
                }
            }
        }
    }
    positions
}
