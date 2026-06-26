//! Integration tests for the deterministic tick harness.
//!
//! These exercise the milestone's required behaviours through the public API
//! only: tick-boundary application, determinism, bounded-inbox backpressure, and
//! the [`ferrumc_core::Tick`] counter advancing.

use std::num::NonZeroUsize;

use ferrumc_core::{PlayerId, Tick};
use ferrumc_math::{ShardPos, Vec3};
use ferrumc_sim::{
    GameInput, GameOutput, SimError, SimHarness, SimShard, TickCoordinator, TickOutcome, TickRate,
};

fn player(name: &str) -> PlayerId {
    PlayerId::offline(name)
}

/// Runs a scripted scenario: each element is the batch of inputs submitted
/// before one `tick`. Returns every [`TickOutcome`] in order.
fn run_scenario(batches: &[Vec<GameInput>]) -> Vec<TickOutcome> {
    let mut harness = SimHarness::new(TickRate::VANILLA, ShardPos::new(2, -3));
    let mut outcomes = Vec::new();
    for batch in batches {
        for input in batch {
            harness.submit(input.clone()).expect("inbox has room");
        }
        outcomes.push(harness.tick().expect("tick"));
    }
    outcomes
}

fn sample_scenario() -> Vec<Vec<GameInput>> {
    let alice = player("alice");
    let bob = player("bob");
    vec![
        vec![
            GameInput::PlayerJoin {
                player: alice,
                position: Vec3::new(0.0, 64.0, 0.0),
            },
            GameInput::PlayerJoin {
                player: bob,
                position: Vec3::new(10.0, 64.0, 10.0),
            },
        ],
        vec![
            GameInput::PlayerMove {
                player: alice,
                position: Vec3::new(1.0, 64.0, 0.0),
            },
            GameInput::PlayerLeave { player: bob },
        ],
        // bob already left: this move is ignored, producing no output.
        vec![GameInput::PlayerMove {
            player: bob,
            position: Vec3::new(99.0, 64.0, 99.0),
        }],
        // An empty tick still advances the counter and yields no outputs.
        vec![],
    ]
}

#[test]
fn inputs_apply_at_next_tick_boundary_not_mid_tick() {
    let mut harness = SimHarness::new(TickRate::VANILLA, ShardPos::new(0, 0));
    let alice = player("alice");

    harness
        .submit(GameInput::PlayerJoin {
            player: alice,
            position: Vec3::new(1.0, 64.0, 2.0),
        })
        .expect("inbox has room");

    // The input is queued, not applied: no state change before the boundary.
    assert_eq!(harness.shard().player_count(), 0);
    assert_eq!(harness.shard().player_position(alice), None);
    assert_eq!(harness.shard().inbox_len(), 1);
    assert_eq!(harness.current_tick(), Tick::ZERO);

    // Crossing the tick boundary applies it.
    let outcome = harness.tick().expect("tick");
    assert_eq!(outcome.tick(), Tick::new(1));
    assert_eq!(
        outcome.outputs(),
        &[GameOutput::PlayerSpawned {
            player: alice,
            position: Vec3::new(1.0, 64.0, 2.0)
        }]
    );
    assert_eq!(harness.shard().player_count(), 1);
    assert_eq!(
        harness.shard().player_position(alice),
        Some(Vec3::new(1.0, 64.0, 2.0))
    );
    assert_eq!(harness.shard().inbox_len(), 0);

    // An input submitted after the tick waits for the *next* tick.
    harness
        .submit(GameInput::PlayerMove {
            player: alice,
            position: Vec3::new(5.0, 64.0, 2.0),
        })
        .expect("inbox has room");
    assert_eq!(
        harness.shard().player_position(alice),
        Some(Vec3::new(1.0, 64.0, 2.0)),
        "move must not take effect until the next tick"
    );
    harness.tick().expect("tick");
    assert_eq!(
        harness.shard().player_position(alice),
        Some(Vec3::new(5.0, 64.0, 2.0))
    );
}

#[test]
fn multiple_moves_in_one_tick_coalesce_to_the_latest_position() {
    let mut harness = SimHarness::new(TickRate::VANILLA, ShardPos::new(0, 0));
    let p = player("dora");
    harness
        .submit(GameInput::PlayerJoin {
            player: p,
            position: Vec3::new(0.0, 64.0, 0.0),
        })
        .expect("room");
    let _ = harness.tick().expect("tick");

    // Three moves submitted before one tick collapse to a single applied
    // position and a single PlayerMoved at the boundary.
    for x in [1.0, 2.0, 3.0] {
        harness
            .submit(GameInput::PlayerMove {
                player: p,
                position: Vec3::new(x, 64.0, 0.0),
            })
            .expect("room");
    }
    let outcome = harness.tick().expect("tick");
    assert_eq!(
        outcome.outputs(),
        &[GameOutput::PlayerMoved {
            player: p,
            position: Vec3::new(3.0, 64.0, 0.0)
        }]
    );
    assert_eq!(
        harness.shard().player_position(p),
        Some(Vec3::new(3.0, 64.0, 0.0))
    );
}

#[test]
fn invalid_coordinates_are_rejected_at_the_boundary() {
    let mut harness = SimHarness::new(TickRate::VANILLA, ShardPos::new(0, 0));
    let p = player("eli");
    let spawn = Vec3::new(8.0, 64.0, 8.0);
    harness
        .submit(GameInput::PlayerJoin {
            player: p,
            position: spawn,
        })
        .expect("room");
    let _ = harness.tick().expect("tick");

    harness
        .submit(GameInput::PlayerMove {
            player: p,
            position: Vec3::new(f64::INFINITY, 64.0, 8.0),
        })
        .expect("room");
    let outcome = harness.tick().expect("tick");
    // Rejected: state is untouched and a snap-back correction is emitted.
    assert_eq!(
        outcome.outputs(),
        &[GameOutput::PlayerPositionCorrected {
            player: p,
            position: spawn
        }]
    );
    assert_eq!(harness.shard().player_position(p), Some(spawn));
}

#[test]
fn identical_input_sequences_produce_identical_outputs() {
    let scenario = sample_scenario();
    let run_a = run_scenario(&scenario);
    let run_b = run_scenario(&scenario);
    assert_eq!(
        run_a, run_b,
        "determinism: identical inputs, identical outputs"
    );

    // Spot-check the shape: the third tick (bob already gone) is empty, and the
    // fourth (no inputs) is empty too, but both still advance the tick.
    assert!(run_a[2].outputs().is_empty());
    assert!(run_a[3].outputs().is_empty());
    assert_eq!(run_a[3].tick(), Tick::new(4));
}

#[test]
fn inbox_full_rejects_without_dropping_or_blocking() {
    let cap = NonZeroUsize::new(2).expect("nonzero capacity");
    let mut shard = SimShard::with_inbox_capacity(ShardPos::new(0, 0), cap);
    let p = player("carol");

    shard
        .enqueue(GameInput::PlayerJoin {
            player: p,
            position: Vec3::ZERO,
        })
        .expect("first input fits");
    shard
        .enqueue(GameInput::PlayerMove {
            player: p,
            position: Vec3::new(1.0, 0.0, 0.0),
        })
        .expect("second input fits");
    assert!(shard.is_inbox_full());

    // The third input is rejected with a classified error; nothing is dropped.
    let err = shard
        .enqueue(GameInput::PlayerMove {
            player: p,
            position: Vec3::new(2.0, 0.0, 0.0),
        })
        .expect_err("inbox is full");
    assert_eq!(err, SimError::InboxFull { capacity: 2 });
    assert_eq!(shard.inbox_len(), 2);

    // Draining at the tick boundary clears the inbox and accepts inputs again.
    let outputs = shard.run_tick();
    assert_eq!(outputs.len(), 2);
    assert!(!shard.is_inbox_full());
    shard
        .enqueue(GameInput::PlayerLeave { player: p })
        .expect("room after drain");
}

#[test]
fn tick_counter_advances_via_core_tick() {
    let mut harness = SimHarness::new(TickRate::VANILLA, ShardPos::new(0, 0));
    assert_eq!(harness.current_tick(), Tick::ZERO);
    for expected in 1..=5u64 {
        let outcome = harness.tick().expect("tick");
        assert_eq!(outcome.tick(), Tick::new(expected));
        assert_eq!(harness.current_tick(), Tick::new(expected));
    }
}

#[test]
fn coordinator_reports_overflow_instead_of_wrapping() {
    let mut coordinator = TickCoordinator::resuming_at(TickRate::VANILLA, Tick::new(u64::MAX));
    assert_eq!(coordinator.advance(), Err(SimError::TickOverflow));
    assert_eq!(coordinator.current(), Tick::new(u64::MAX));
}
