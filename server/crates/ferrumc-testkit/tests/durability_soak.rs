use std::time::Duration;

use ferrumc_core::GameMode;
use ferrumc_math::BlockPos;
use ferrumc_storage::{BlockMutationLogRecord, MutationActor, MutationLogCause, SchemaVersion};
use ferrumc_testkit::{
    DurabilityScenario, DurabilitySoakAction, DurabilitySoakFault, DurabilitySoakHarness,
    DurabilitySoakScenario, DurabilitySoakStepOutcome, DurabilitySurface, FaultStage,
    DURABILITY_SOAK_CYCLES, DURABILITY_SOAK_PENDING_CAPACITY, DURABILITY_SOAK_STEPS,
    MAX_DURABILITY_SOAK_STORE_TRACE,
};
use ferrumc_world::BlockStateId;

const SEED: u64 = 0x6200_0043_d00d_f00d;
const BASE_TICK: u64 = 900;
const EXPECTED_DIGEST: &str = "afffd956be348a85dee553f2aa2ba809e52b2aa2427ec8512cee11fb83ae7845";
const VIRTUAL_RUN_DEADLINE: Duration = Duration::from_secs(1);

fn mutation() -> BlockMutationLogRecord {
    BlockMutationLogRecord::new(
        SchemaVersion::new(7),
        99,
        BASE_TICK,
        MutationActor::System,
        BlockPos::new(-12, 70, 34),
        BlockStateId::new(1),
        BlockStateId::new(9),
        MutationLogCause::Test,
    )
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::too_many_lines)] // One table-style regression pins the complete 43-step oracle.
async fn fixed_schedule_is_bounded_deterministic_and_reaches_known_digest() {
    tokio::time::pause();

    let first = tokio::time::timeout(
        VIRTUAL_RUN_DEADLINE,
        DurabilitySoakHarness::new(SEED, mutation()).run(),
    )
    .await
    .expect("first fixed soak must complete before its virtual deadline")
    .expect("first fixed soak");
    let second = tokio::time::timeout(
        VIRTUAL_RUN_DEADLINE,
        DurabilitySoakHarness::new(SEED, mutation()).run(),
    )
    .await
    .expect("second fixed soak must complete before its virtual deadline")
    .expect("second fixed soak");
    assert_eq!(first, second, "identical inputs must reproduce the report");

    assert_eq!(first.battery().cases().len(), 23);
    assert!(first
        .battery()
        .case(
            DurabilitySurface::Journal,
            DurabilityScenario::ReceiptReplay,
        )
        .is_some());
    assert_eq!(first.steps().len(), DURABILITY_SOAK_STEPS);
    assert_eq!(DURABILITY_SOAK_CYCLES, 6);
    assert_eq!(DURABILITY_SOAK_PENDING_CAPACITY, 1);
    assert_eq!(
        first.steps()[0].outcome(),
        DurabilitySoakStepOutcome::BatteryCompleted { cases: 23 }
    );

    let cycle_actions = [
        DurabilitySoakAction::Connect,
        DurabilitySoakAction::QueueEdit,
        DurabilitySoakAction::PersistJournal,
        DurabilitySoakAction::ResolveJournal,
        DurabilitySoakAction::PersistPlayer,
        DurabilitySoakAction::Disconnect,
        DurabilitySoakAction::Restart,
    ];
    let scenarios = [
        DurabilitySoakScenario::Clean,
        DurabilitySoakScenario::JournalBeforeCommitRetry,
        DurabilitySoakScenario::JournalAcknowledgementLossReplay,
        DurabilitySoakScenario::PlayerCommitError,
        DurabilitySoakScenario::PlayerAcknowledgementLoss,
        DurabilitySoakScenario::Clean,
    ];
    for (cycle, steps) in first.steps()[1..].chunks_exact(7).enumerate() {
        assert_eq!(
            steps
                .iter()
                .map(ferrumc_testkit::DurabilitySoakStep::action)
                .collect::<Vec<_>>(),
            cycle_actions,
            "cycle {cycle}"
        );
        assert!(steps
            .iter()
            .all(|step| step.cycle() == Some(cycle) && step.scenario() == Some(scenarios[cycle])));
        assert_eq!(
            steps[1].outcome(),
            DurabilitySoakStepOutcome::EditQueued {
                pending: 1,
                capacity: 1,
            }
        );
    }
    assert!(first
        .steps()
        .iter()
        .enumerate()
        .all(|(index, step)| step.index() == index));

    assert_eq!(
        first.steps()[10].outcome(),
        DurabilitySoakStepOutcome::Fault(DurabilitySoakFault::JournalBeforeCommit)
    );
    assert!(matches!(
        first.steps()[11].outcome(),
        DurabilitySoakStepOutcome::JournalRetried(_)
    ));
    assert_eq!(
        first.steps()[17].outcome(),
        DurabilitySoakStepOutcome::Fault(DurabilitySoakFault::JournalAcknowledgementLost)
    );
    assert!(matches!(
        first.steps()[18].outcome(),
        DurabilitySoakStepOutcome::JournalReceiptReplayed(_)
    ));
    assert_eq!(
        first.steps()[26].outcome(),
        DurabilitySoakStepOutcome::Fault(DurabilitySoakFault::PlayerCommit)
    );
    assert_eq!(
        first.steps()[28].outcome(),
        DurabilitySoakStepOutcome::Restarted {
            durable_player_generation: 2,
        }
    );
    assert_eq!(
        first.steps()[33].outcome(),
        DurabilitySoakStepOutcome::Fault(DurabilitySoakFault::PlayerAcknowledgementLost)
    );
    assert_eq!(
        first.steps()[42].outcome(),
        DurabilitySoakStepOutcome::Restarted {
            durable_player_generation: 5,
        }
    );

    let end = first.end_state();
    assert!(!end.connected());
    assert_eq!(end.pending_edits(), 0);
    assert_eq!(end.completed_cycles(), 6);
    assert_eq!(end.committed_mutations(), 6);
    assert_eq!(end.last_mutation_id(), Some(5));
    assert_eq!(end.durable_player_generation(), 5);
    assert_eq!(end.attempted_operations(), 20);
    assert_eq!(end.committed_operations(), 11);
    assert_eq!(end.successful_responses(), 16);
    assert_eq!(end.store_trace_entries(), 52);

    assert_eq!(
        first
            .committed()
            .block_mutations()
            .iter()
            .map(BlockMutationLogRecord::id)
            .collect::<Vec<_>>(),
        [0, 1, 2, 3, 4, 5]
    );
    assert_eq!(
        first
            .committed()
            .block_mutations()
            .iter()
            .map(BlockMutationLogRecord::tick)
            .collect::<Vec<_>>(),
        [900, 901, 902, 903, 904, 905]
    );
    let player = first
        .committed()
        .player(end.player())
        .expect("final player record");
    assert_eq!(player.schema_version(), SchemaVersion::new(1));
    assert_eq!(player.game_mode(), GameMode::Creative);
    assert_eq!(
        player.data(),
        [SEED.to_be_bytes().as_slice(), &[5]].concat()
    );

    assert_eq!(first.store_trace().len(), 52);
    assert!(first.store_trace().len() <= MAX_DURABILITY_SOAK_STORE_TRACE);
    assert_eq!(
        first
            .store_trace()
            .iter()
            .filter(|entry| entry.stage() == FaultStage::BeforeCommitFailure)
            .count(),
        1
    );
    assert_eq!(
        first
            .store_trace()
            .iter()
            .filter(|entry| entry.stage() == FaultStage::CommitFailure)
            .count(),
        1
    );
    assert_eq!(
        first
            .store_trace()
            .iter()
            .filter(|entry| entry.stage() == FaultStage::AcknowledgementLost)
            .count(),
        2
    );
    assert_eq!(
        first
            .store_trace()
            .iter()
            .filter(|entry| entry.stage() == FaultStage::ReceiptReplayed)
            .count(),
        1
    );
    assert!(first
        .store_trace()
        .windows(2)
        .all(|pair| pair[0].sequence() < pair[1].sequence()));

    assert_eq!(first.digest().as_hex(), EXPECTED_DIGEST);
}
