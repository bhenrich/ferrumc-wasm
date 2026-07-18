use std::path::{Path, PathBuf};
use std::process::Command;

use ferrumc_plugin_sdk::{
    BlockDecision, BlockEvent, BlockPlaceEvent, BlockPos, Capability, CapabilityManifest,
    ChatAttempt, ChunkPos, CommandInvocation, Event, EventDecision, EventKind, HandlerId,
    InteractHand, InteractTarget, InteractionAttempt, MoveEvent, PermissionNode, PlaceAttempt,
    PlayerEvent, PlayerId, Resolution, Tick, TimerId, Vec3, MAX_STORAGE_KEYS,
    MAX_STORAGE_KEY_BYTES, MAX_STORAGE_KEY_LIST_BYTES,
};
use ferrumc_plugin_sdk_builtin::BuiltinPluginFactory;

#[path = "../fixtures/plugin-testhost-sdk/src/plugin.rs"]
mod fixture_plugin;

use ferrumc_testkit::{
    PluginCallbackPhase, PluginEffect, PluginFailureKind, PluginReplayFailure, PluginRun,
    PluginTestHost, PluginTestHostError, ScheduledPluginEvent, MAX_CALLBACK_EFFECTS,
    MAX_SCHEDULED_EVENTS,
};
use fixture_plugin::{
    TesthostFixturePlugin, CAPACITY_TRIGGER_STATE, DECISION_ALLOW_STATE, DECISION_DENY_STATE,
    DIAGNOSTIC_ONLY_POS, FIXTURE_HANDLER_RAW, FIXTURE_TIMER_RAW,
};

const QUERY_POS: BlockPos = BlockPos::new(2, 70, -3);
const BREAK_POS: BlockPos = BlockPos::new(-8, 71, 12);
const PLACE_POS: BlockPos = BlockPos::new(7, 72, -9);
const EXPECTED_DIGEST: &str = "c3536bc360309440137e05b7d27e6d5da16244e6d9146e74b095d68032b74c8b";

fn player() -> PlayerId {
    PlayerId::offline("PluginTesthostFixture")
}

fn permission() -> PermissionNode {
    PermissionNode::parse("ferrumc.fixture.allowed").expect("fixture permission is valid")
}

fn full_length_storage_key(index: usize) -> String {
    let prefix = format!("key-{index:04}-");
    let padding = MAX_STORAGE_KEY_BYTES
        .checked_sub(prefix.len())
        .expect("fixture prefix fits the SDK key bound");
    prefix + &"x".repeat(padding)
}

fn seeded_host(capacity: usize) -> PluginTestHost {
    seeded_host_with(CapabilityManifest::all(), capacity)
}

fn seeded_host_with(granted: CapabilityManifest, capacity: usize) -> PluginTestHost {
    let mut host = PluginTestHost::new(granted, capacity).expect("valid host bounds");
    host.set_chunk_loaded(ChunkPos::new(0, 0), true);
    host.set_chunk_loaded(ChunkPos::new(0, -1), true);
    host.set_block(QUERY_POS, 7);
    host.set_block(PLACE_POS, 4);
    host.set_player_position(player(), Vec3::new(1.5, 64.0, -2.25))
        .expect("finite fixture position");
    host.set_permission(player(), permission(), Resolution::Allowed);
    host.set_storage("obsolete", b"old".to_vec())
        .expect("bounded fixture storage");
    host
}

fn replay_failure(result: Result<PluginRun, PluginTestHostError>) -> PluginReplayFailure {
    match result {
        Err(PluginTestHostError::Replay(failure)) => *failure,
        Err(other) => panic!("expected callback replay failure, got {other}"),
        Ok(run) => panic!("expected callback replay failure, got {}", run.digest()),
    }
}

fn event_log() -> Vec<ScheduledPluginEvent> {
    let player = player();
    let handler = HandlerId::new(FIXTURE_HANDLER_RAW).expect("fixture handler is nonzero");
    let timer = TimerId::new(FIXTURE_TIMER_RAW).expect("fixture timer is nonzero");

    vec![
        scheduled(1, Event::PlayerJoin(PlayerEvent::new(player))),
        scheduled(
            2,
            Event::AfterBlockPlace(BlockPlaceEvent::new(player, PLACE_POS, 23)),
        ),
        scheduled(
            3,
            Event::AfterBlockBreak(BlockEvent::new(player, BREAK_POS)),
        ),
        scheduled(
            4,
            Event::PlayerMove(MoveEvent::new(
                player,
                BlockPos::new(0, 64, 0),
                BlockPos::new(1, 64, 0),
            )),
        ),
        scheduled(5, Event::BlockBreak(BlockEvent::new(player, BREAK_POS))),
        scheduled(6, Event::PlayerLeave(PlayerEvent::new(player))),
        scheduled(
            7,
            Event::BlockPlaceAttempt(PlaceAttempt::new(player, PLACE_POS, 5)),
        ),
        scheduled(
            8,
            Event::BlockBreakAttempt(BlockEvent::new(player, BREAK_POS)),
        ),
        scheduled(
            9,
            Event::ChatAttempt(
                ChatAttempt::new(player, "deterministic hello").expect("bounded chat"),
            ),
        ),
        scheduled(
            10,
            Event::InteractAttempt(InteractionAttempt::new(
                player,
                InteractHand::Main,
                InteractTarget::Air,
            )),
        ),
        scheduled(
            11,
            Event::Command(
                CommandInvocation::new(handler, player, Vec::new())
                    .expect("bounded fixture invocation"),
            ),
        ),
        scheduled(12, Event::Timer(timer)),
    ]
}

fn scheduled(tick: u64, event: Event) -> ScheduledPluginEvent {
    ScheduledPluginEvent::new(Tick::new(tick), event)
}

fn builtin_factory() -> BuiltinPluginFactory {
    BuiltinPluginFactory::new::<TesthostFixturePlugin>().expect("valid fixture declaration")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("testkit is nested under server/crates")
        .to_path_buf()
}

fn fixture_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/plugin-testhost-sdk/Cargo.toml")
}

fn build_dynamic_fixture() -> PathBuf {
    let root = repo_root();
    let scope = format!("p57-plugin-testhost-{}", std::process::id());
    let target = root.join(format!(".codex-tmp/{scope}-target"));
    let artifacts = root.join(format!(".codex-tmp/{scope}-artifacts"));
    std::fs::create_dir_all(&artifacts).expect("create repo-local fixture artifact directory");

    let output = Command::new(env!("CARGO"))
        .arg("build")
        .arg("--manifest-path")
        .arg(fixture_manifest())
        .arg("--locked")
        .arg("--offline")
        .arg("--jobs")
        .arg("1")
        .arg("--no-default-features")
        .arg("--features")
        .arg("dynamic")
        .arg("--target-dir")
        .arg(&target)
        .arg("--message-format=json-render-diagnostics")
        .output()
        .expect("run nested fixture build");
    assert!(
        output.status.success(),
        "nested fixture build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let built = dynamic_artifact_from_messages(&output.stdout);
    let artifact = artifacts.join(format!(
        "{}plugin_testhost_fixture{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    std::fs::copy(&built, &artifact).unwrap_or_else(|error| {
        panic!(
            "copy fixture {} to {}: {error}",
            built.display(),
            artifact.display()
        )
    });
    std::fs::remove_dir_all(&target)
        .unwrap_or_else(|error| panic!("remove nested target {}: {error}", target.display()));
    artifact
}

fn dynamic_artifact_from_messages(stdout: &[u8]) -> PathBuf {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter(|message| message["target"]["name"] == "ferrumc_plugin_testhost_fixture")
        .filter(|message| {
            message["target"]["crate_types"]
                .as_array()
                .is_some_and(|types| types.iter().any(|kind| kind == "cdylib"))
        })
        .flat_map(|message| message["filenames"].as_array().cloned().unwrap_or_default())
        .filter_map(|filename| filename.as_str().map(PathBuf::from))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(std::env::consts::DLL_SUFFIX))
        })
        .expect("cargo reported the fixture cdylib artifact")
}

#[test]
// One real library build deliberately feeds every parity/failure scenario
// through the same resident artifact, avoiding concurrent nested Cargo runs.
#[allow(clippy::too_many_lines)]
fn same_source_replay_is_deterministic_across_both_packaging_modes() {
    let host = seeded_host(64);
    let events = event_log();

    let first = host
        .replay_builtin(builtin_factory(), &events)
        .expect("first built-in replay");
    let repeated = host
        .replay_builtin(builtin_factory(), &events)
        .expect("repeated built-in replay");
    assert_eq!(first.effects(), repeated.effects());
    assert_eq!(first.state(), repeated.state());
    assert_eq!(first.digest(), repeated.digest());

    let dynamic_path = build_dynamic_fixture();
    let dynamic = host
        .replay_dynamic(&dynamic_path, &events)
        .expect("real cdylib replay");
    assert_eq!(first.effects(), dynamic.effects());
    assert_eq!(first.state(), dynamic.state());
    assert_eq!(first.digest(), dynamic.digest());
    assert_eq!(first.digest().as_hex(), EXPECTED_DIGEST);

    let mut different_handles = host.clone();
    different_handles
        .set_dynamic_dimension_handle(0x1111_2222)
        .expect("nonzero distinct dimension handle");
    different_handles
        .set_dynamic_shard_handle(0x3333_4444)
        .expect("nonzero distinct shard handle");
    let handle_variant = different_handles
        .replay_dynamic(&dynamic_path, &events)
        .expect("dynamic replay with different raw resource handles");
    assert_eq!(dynamic.effects(), handle_variant.effects());
    assert_eq!(dynamic.state(), handle_variant.state());
    assert_eq!(dynamic.digest(), handle_variant.digest());

    let copied_path = dynamic_path.with_file_name(format!(
        "{}renamed_fixture{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    std::fs::copy(&dynamic_path, &copied_path).expect("copy fixture under different path");
    let renamed = host
        .replay_dynamic(&copied_path, &events)
        .expect("replay copied cdylib");
    assert_eq!(first.digest(), renamed.digest());

    let sparse_pos = BlockPos::new(9, 73, -2);
    let sparse_events = [scheduled(
        1,
        Event::BlockPlaceAttempt(PlaceAttempt::new(player(), sparse_pos, 5)),
    )];
    let sparse_builtin = host
        .replay_builtin(builtin_factory(), &sparse_events)
        .expect("built-in loaded sparse-block query");
    let sparse_dynamic = host
        .replay_dynamic(&dynamic_path, &sparse_events)
        .expect("dynamic loaded sparse-block query");
    assert_eq!(sparse_builtin.effects(), sparse_dynamic.effects());
    assert_eq!(sparse_builtin.state(), sparse_dynamic.state());
    assert_eq!(sparse_builtin.digest(), sparse_dynamic.digest());
    assert_eq!(sparse_builtin.state().block_state_id(sparse_pos), Some(6));
    assert!(sparse_builtin
        .state()
        .messages()
        .iter()
        .any(|message| message.message() == "place previous=0"));

    let decision_events = [
        scheduled(
            1,
            Event::BlockPlaceAttempt(PlaceAttempt::new(
                player(),
                BlockPos::new(10, 65, 10),
                DECISION_ALLOW_STATE,
            )),
        ),
        scheduled(
            2,
            Event::BlockPlaceAttempt(PlaceAttempt::new(
                player(),
                BlockPos::new(11, 65, 10),
                DECISION_DENY_STATE,
            )),
        ),
        scheduled(
            3,
            Event::BlockPlaceAttempt(PlaceAttempt::new(player(), BlockPos::new(12, 65, 10), 5)),
        ),
        scheduled(
            4,
            Event::BlockBreakAttempt(BlockEvent::new(player(), BREAK_POS)),
        ),
        scheduled(
            5,
            Event::ChatAttempt(ChatAttempt::new(player(), "decision allow").expect("bounded chat")),
        ),
        scheduled(
            6,
            Event::InteractAttempt(InteractionAttempt::new(
                player(),
                InteractHand::Off,
                InteractTarget::Air,
            )),
        ),
    ];
    let decision_builtin = host
        .replay_builtin(builtin_factory(), &decision_events)
        .expect("built-in decision variants");
    let decision_dynamic = host
        .replay_dynamic(&dynamic_path, &decision_events)
        .expect("dynamic decision variants");
    assert_eq!(decision_builtin.effects(), decision_dynamic.effects());
    assert_eq!(decision_builtin.state(), decision_dynamic.state());
    assert_eq!(decision_builtin.digest(), decision_dynamic.digest());
    assert!(decision_builtin
        .effects()
        .contains(&PluginEffect::BlockDecision(BlockDecision::Allow)));
    assert!(decision_builtin.effects().iter().any(|effect| matches!(
        effect,
        PluginEffect::BlockDecision(BlockDecision::Deny(Some(feedback)))
            if feedback.message() == "fixture denied placement"
    )));
    assert!(decision_builtin
        .effects()
        .contains(&PluginEffect::BlockDecision(BlockDecision::Replace(88))));
    assert!(decision_builtin.effects().iter().any(|effect| matches!(
        effect,
        PluginEffect::EventDecision {
            kind: EventKind::BlockBreakAttempt,
            decision: EventDecision::Deny(Some(_)),
        }
    )));
    assert!(decision_builtin.effects().iter().any(|effect| matches!(
        effect,
        PluginEffect::EventDecision {
            kind: EventKind::ChatAttempt,
            decision: EventDecision::Allow,
        }
    )));
    assert!(decision_builtin.effects().iter().any(|effect| matches!(
        effect,
        PluginEffect::EventDecision {
            kind: EventKind::InteractAttempt,
            decision: EventDecision::Deny(None),
        }
    )));

    let capacity_events = [scheduled(
        1,
        Event::BlockPlaceAttempt(PlaceAttempt::new(
            player(),
            PLACE_POS,
            CAPACITY_TRIGGER_STATE,
        )),
    )];
    let capacity_host = seeded_host(12);
    let capacity_builtin =
        replay_failure(capacity_host.replay_builtin(builtin_factory(), &capacity_events));
    let capacity_dynamic =
        replay_failure(capacity_host.replay_dynamic(&dynamic_path, &capacity_events));
    assert_eq!(capacity_builtin.phase(), capacity_dynamic.phase());
    assert_eq!(
        capacity_builtin.phase(),
        PluginCallbackPhase::Event {
            index: 0,
            tick: Tick::new(1),
            kind: EventKind::BlockPlaceAttempt,
        }
    );
    assert_eq!(capacity_builtin.kind(), &PluginFailureKind::BufferFull);
    assert_eq!(capacity_builtin.kind(), capacity_dynamic.kind());
    assert_eq!(
        capacity_builtin.report().effects(),
        capacity_dynamic.report().effects()
    );
    assert_eq!(
        capacity_builtin.report().state(),
        capacity_dynamic.report().state()
    );
    assert_eq!(
        capacity_builtin.report().digest(),
        capacity_dynamic.report().digest()
    );
    assert_eq!(capacity_builtin.report().effects().len(), 11);
    assert_eq!(
        capacity_builtin.report().state().block_state_id(PLACE_POS),
        Some(4)
    );
    assert!(capacity_builtin.report().state().messages().is_empty());
    assert!(!capacity_builtin.report().effects().iter().any(|effect| {
        matches!(
            effect,
            PluginEffect::BlockDecision(_) | PluginEffect::EventDecision { .. }
        )
    }));

    let rollback_events = [scheduled(
        1,
        Event::ChatAttempt(ChatAttempt::new(player(), "rollback").expect("bounded chat")),
    )];
    let rollback_builtin = replay_failure(host.replay_builtin(builtin_factory(), &rollback_events));
    let rollback_dynamic = replay_failure(host.replay_dynamic(&dynamic_path, &rollback_events));
    assert_eq!(rollback_builtin.phase(), rollback_dynamic.phase());
    assert_eq!(
        rollback_builtin.phase(),
        PluginCallbackPhase::Event {
            index: 0,
            tick: Tick::new(1),
            kind: EventKind::ChatAttempt,
        }
    );
    assert!(matches!(
        rollback_builtin.kind(),
        PluginFailureKind::Cooperative
    ));
    assert_eq!(rollback_builtin.kind(), rollback_dynamic.kind());
    assert_eq!(
        rollback_builtin.report().effects(),
        rollback_dynamic.report().effects()
    );
    assert_eq!(
        rollback_builtin.report().state(),
        rollback_dynamic.report().state()
    );
    assert_eq!(
        rollback_builtin.report().digest(),
        rollback_dynamic.report().digest()
    );
    assert_eq!(rollback_builtin.report().effects().len(), 11);
    assert_eq!(
        rollback_builtin
            .report()
            .state()
            .storage_value("rolled-back"),
        None
    );
    assert!(rollback_builtin.report().state().messages().is_empty());
    for failure in [&rollback_builtin, &rollback_dynamic] {
        assert!(failure
            .report()
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message() == "rollback diagnostic"));
    }

    let panic_events = [scheduled(
        1,
        Event::ChatAttempt(ChatAttempt::new(player(), "panic").expect("bounded chat")),
    )];
    let panic_builtin = replay_failure(host.replay_builtin(builtin_factory(), &panic_events));
    let panic_dynamic = replay_failure(host.replay_dynamic(&dynamic_path, &panic_events));
    assert_eq!(panic_builtin.phase(), panic_dynamic.phase());
    assert_eq!(
        panic_builtin.phase(),
        PluginCallbackPhase::Event {
            index: 0,
            tick: Tick::new(1),
            kind: EventKind::ChatAttempt,
        }
    );
    assert_eq!(panic_builtin.kind(), &PluginFailureKind::Panicked);
    assert_eq!(panic_builtin.kind(), panic_dynamic.kind());
    assert_eq!(
        panic_builtin.report().effects(),
        panic_dynamic.report().effects()
    );
    assert_eq!(
        panic_builtin.report().state(),
        panic_dynamic.report().state()
    );
    assert_eq!(
        panic_builtin.report().digest(),
        panic_dynamic.report().digest()
    );
    assert_eq!(panic_builtin.report().effects().len(), 11);
    assert!(panic_builtin.report().state().messages().is_empty());
    for failure in [&panic_builtin, &panic_dynamic] {
        assert!(failure
            .report()
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message() == "panic diagnostic"));
    }

    for (capability, event) in [
        (
            Capability::VetoBlockEdits,
            Event::BlockPlaceAttempt(PlaceAttempt::new(player(), PLACE_POS, 5)),
        ),
        (
            Capability::VetoEvents,
            Event::ChatAttempt(
                ChatAttempt::new(player(), "capability denied").expect("bounded chat"),
            ),
        ),
    ] {
        let restricted = seeded_host_with(CapabilityManifest::all().without(capability), 64);
        let denied_events = [scheduled(1, event)];
        let denied_builtin =
            replay_failure(restricted.replay_builtin(builtin_factory(), &denied_events));
        let denied_dynamic =
            replay_failure(restricted.replay_dynamic(&dynamic_path, &denied_events));
        assert_eq!(denied_builtin.phase(), denied_dynamic.phase());
        assert_eq!(
            denied_builtin.phase(),
            PluginCallbackPhase::Event {
                index: 0,
                tick: Tick::new(1),
                kind: denied_events[0].event().kind(),
            }
        );
        assert_eq!(
            denied_builtin.kind(),
            &PluginFailureKind::CapabilityDenied(capability)
        );
        assert_eq!(denied_builtin.kind(), denied_dynamic.kind());
        assert_eq!(
            denied_builtin.report().effects(),
            denied_dynamic.report().effects()
        );
        assert_eq!(
            denied_builtin.report().state(),
            denied_dynamic.report().state()
        );
        assert_eq!(
            denied_builtin.report().digest(),
            denied_dynamic.report().digest()
        );
        assert_eq!(denied_builtin.report().effects().len(), 11);
        assert!(denied_builtin.report().state().messages().is_empty());
    }

    let mut changed_host = seeded_host(64);
    changed_host.set_block(QUERY_POS, 8);
    let changed = changed_host
        .replay_builtin(builtin_factory(), &events)
        .expect("semantic mutation replay");
    assert_ne!(first.digest(), changed.digest());
}

#[test]
fn diagnostics_are_observable_but_excluded_from_the_semantic_digest() {
    let host = seeded_host(64);
    let events = event_log();
    let baseline = host
        .replay_builtin(builtin_factory(), &events)
        .expect("baseline replay");

    let mut with_diagnostic = events;
    with_diagnostic.push(scheduled(
        13,
        Event::AfterBlockBreak(BlockEvent::new(player(), DIAGNOSTIC_ONLY_POS)),
    ));
    let observed = host
        .replay_builtin(builtin_factory(), &with_diagnostic)
        .expect("diagnostic-only replay");

    assert_eq!(baseline.effects(), observed.effects());
    assert_eq!(baseline.state(), observed.state());
    assert_eq!(
        observed.diagnostics().len(),
        baseline.diagnostics().len() + 1
    );
    assert_eq!(baseline.digest(), observed.digest());
}

#[test]
// Keeping the ordered effect assertions together makes callback commit order
// visible as one lifecycle contract instead of hiding it across helpers.
#[allow(clippy::too_many_lines)]
fn lifecycle_commits_exact_state_and_semantic_effects() {
    let run = seeded_host(64)
        .replay_builtin(builtin_factory(), &event_log())
        .expect("complete built-in lifecycle");
    let state = run.state();

    assert!(state.is_chunk_loaded(ChunkPos::new(0, 0)));
    assert_eq!(state.block_state_id(QUERY_POS), Some(7));
    assert_eq!(state.block_state_id(BREAK_POS), Some(0));
    assert_eq!(state.block_state_id(PLACE_POS), Some(6));
    assert_eq!(
        state.player_position(player()),
        Some(Vec3::new(4.5, 80.0, -8.25))
    );
    assert_eq!(
        state.permission(player(), &permission()),
        Resolution::Allowed
    );
    assert_eq!(state.storage_value("obsolete"), None);
    assert_eq!(state.storage_value("boot"), None);
    assert_eq!(
        state.storage_value("placed"),
        Some(23_u32.to_le_bytes().as_slice())
    );
    assert_eq!(state.storage_value("command"), Some(b"invoked".as_slice()));
    assert!(state.timers().is_empty());
    assert_eq!(
        state.subscriptions(),
        [
            EventKind::PlayerJoin,
            EventKind::PlayerLeave,
            EventKind::BlockBreak,
            EventKind::AfterBlockPlace,
            EventKind::AfterBlockBreak,
            EventKind::PlayerMove,
        ]
    );
    assert_eq!(state.commands().len(), 1);
    assert_eq!(state.commands()[0].nodes().len(), 1);
    assert_eq!(state.commands()[0].nodes()[0].name(), "fixture");
    assert_eq!(
        state.commands()[0].nodes()[0]
            .handler()
            .expect("fixture command handler")
            .get(),
        FIXTURE_HANDLER_RAW
    );
    assert_eq!(
        state
            .messages()
            .iter()
            .map(ferrumc_plugin_sdk::MessageOperation::message)
            .collect::<Vec<_>>(),
        [
            "join loaded=true block=7 position=1.5,64,-2.25 allowed=true",
            "after-break stored=ready",
            "move keys=boot,placed",
            "place previous=4",
            "break inspected",
            "chat: deterministic hello",
            "interaction inspected",
            "command invoked",
        ]
    );

    let effects = run.effects();
    assert_eq!(effects.len(), 31);
    for (index, kind) in [
        EventKind::PlayerJoin,
        EventKind::PlayerLeave,
        EventKind::BlockBreak,
        EventKind::AfterBlockPlace,
        EventKind::AfterBlockBreak,
        EventKind::PlayerMove,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(&effects[index], &PluginEffect::SubscribeEvent(kind));
    }
    assert!(matches!(effects[6], PluginEffect::RegisterCommand(_)));
    assert!(matches!(
        &effects[7],
        PluginEffect::StoragePut { key, value }
            if key == "boot" && value == b"ready"
    ));
    assert!(matches!(
        &effects[8],
        PluginEffect::StorageDelete { key } if key == "obsolete"
    ));
    assert!(matches!(
        effects[9],
        PluginEffect::ScheduleTimer { id, due_tick }
            if id.get() == FIXTURE_TIMER_RAW && due_tick == Tick::new(4)
    ));
    assert!(matches!(
        effects[10],
        PluginEffect::CancelTimer { id } if id.get() == 18
    ));
    assert!(matches!(&effects[11], PluginEffect::Message(message)
        if message.message().starts_with("join loaded=true")));
    assert!(matches!(
        &effects[12],
        PluginEffect::StoragePut { key, value }
            if key == "placed" && value.as_slice() == 23_u32.to_le_bytes()
    ));
    assert!(matches!(&effects[13], PluginEffect::Message(message)
        if message.message() == "after-break stored=ready"));
    assert!(matches!(&effects[14], PluginEffect::Message(message)
        if message.message() == "move keys=boot,placed"));
    assert!(matches!(
        effects[15],
        PluginEffect::SetBlock {
            pos,
            block_state_id: 0
        } if pos == BREAK_POS
    ));
    assert!(matches!(&effects[16], PluginEffect::Teleport(operation)
        if operation.player() == player()
            && operation.position() == Vec3::new(4.5, 80.0, -8.25)));
    assert!(matches!(
        effects[17],
        PluginEffect::SetBlock {
            pos,
            block_state_id: 6
        } if pos == PLACE_POS
    ));
    assert!(matches!(&effects[18], PluginEffect::Message(message)
        if message.message() == "place previous=4"));
    assert_eq!(
        effects[19],
        PluginEffect::BlockDecision(BlockDecision::Replace(88))
    );
    assert!(matches!(&effects[20], PluginEffect::Message(message)
        if message.message() == "break inspected"));
    assert!(matches!(
        &effects[21],
        PluginEffect::EventDecision {
            kind: EventKind::BlockBreakAttempt,
            decision: EventDecision::Deny(Some(feedback)),
        } if feedback.message() == "fixture denied break"
    ));
    assert!(matches!(&effects[22], PluginEffect::Message(message)
        if message.message() == "chat: deterministic hello"));
    assert_eq!(
        effects[23],
        PluginEffect::EventDecision {
            kind: EventKind::ChatAttempt,
            decision: EventDecision::Allow,
        }
    );
    assert!(matches!(&effects[24], PluginEffect::Message(message)
        if message.message() == "interaction inspected"));
    assert_eq!(
        effects[25],
        PluginEffect::EventDecision {
            kind: EventKind::InteractAttempt,
            decision: EventDecision::Deny(None),
        }
    );
    assert!(matches!(
        &effects[26],
        PluginEffect::StoragePut { key, value }
            if key == "command" && value == b"invoked"
    ));
    assert!(matches!(&effects[27], PluginEffect::Message(message)
        if message.message() == "command invoked"));
    assert!(matches!(
        effects[28],
        PluginEffect::CancelTimer { id } if id.get() == FIXTURE_TIMER_RAW
    ));
    assert!(matches!(
        &effects[29],
        PluginEffect::StorageDelete { key } if key == "boot"
    ));
    assert!(matches!(
        effects[30],
        PluginEffect::CancelTimer { id } if id.get() == FIXTURE_TIMER_RAW
    ));
}

#[test]
// Configuration boundaries are grouped to prove each rejection happens before
// any lifecycle callback or partial report exists.
#[allow(clippy::too_many_lines)]
fn invalid_bounds_schedule_and_resource_handles_are_rejected_before_replay() {
    assert!(matches!(
        PluginTestHost::new(CapabilityManifest::all(), 0),
        Err(PluginTestHostError::InvalidCapacity {
            requested: 0,
            maximum: MAX_CALLBACK_EFFECTS,
        })
    ));
    assert!(matches!(
        PluginTestHost::new(CapabilityManifest::all(), MAX_CALLBACK_EFFECTS + 1),
        Err(PluginTestHostError::InvalidCapacity {
            requested,
            maximum: MAX_CALLBACK_EFFECTS,
        }) if requested == MAX_CALLBACK_EFFECTS + 1
    ));

    let mut host = PluginTestHost::new(CapabilityManifest::all(), 64).expect("valid host");
    assert!(matches!(
        host.set_player_position(player(), Vec3::new(f64::NAN, 0.0, 0.0)),
        Err(PluginTestHostError::NonFinitePlayerPosition)
    ));
    assert!(matches!(
        host.set_storage("", Vec::new()),
        Err(PluginTestHostError::InvalidStorage(_))
    ));
    assert!(matches!(
        host.set_dynamic_dimension_handle(0),
        Err(PluginTestHostError::InvalidResourceHandles)
    ));
    host.set_dynamic_dimension_handle(0x1234)
        .expect("valid dimension handle");
    assert!(matches!(
        host.set_dynamic_shard_handle(0x1234),
        Err(PluginTestHostError::InvalidResourceHandles)
    ));

    let decreasing = [
        scheduled(
            2,
            Event::ChatAttempt(ChatAttempt::new(player(), "later").expect("bounded chat")),
        ),
        scheduled(
            1,
            Event::ChatAttempt(ChatAttempt::new(player(), "earlier").expect("bounded chat")),
        ),
    ];
    let error = seeded_host(64)
        .replay_builtin(builtin_factory(), &decreasing)
        .expect_err("decreasing schedule must fail before load");
    assert!(error.partial_report().is_none());
    assert!(matches!(
        error,
        PluginTestHostError::DecreasingTick {
            index: 1,
            previous,
            current,
        } if previous == Tick::new(2) && current == Tick::new(1)
    ));

    let too_many = vec![
        scheduled(
            1,
            Event::ChatAttempt(ChatAttempt::new(player(), "bounded").expect("bounded chat")),
        );
        MAX_SCHEDULED_EVENTS + 1
    ];
    let error = seeded_host(64)
        .replay_builtin(builtin_factory(), &too_many)
        .expect_err("oversized schedule must fail before load");
    assert!(error.partial_report().is_none());
    assert!(matches!(
        error,
        PluginTestHostError::TooManyScheduledEvents {
            len,
            maximum: MAX_SCHEDULED_EVENTS,
        } if len == MAX_SCHEDULED_EVENTS + 1
    ));

    let mut storage_host =
        PluginTestHost::new(CapabilityManifest::all(), 64).expect("valid storage host");
    for index in 0..MAX_STORAGE_KEYS {
        storage_host
            .set_storage(format!("k{index:04}"), Vec::new())
            .expect("storage key within count and encoded-list bounds");
    }
    storage_host
        .set_storage("k0000", b"replacement".to_vec())
        .expect("replacement does not consume another key slot");
    assert!(matches!(
        storage_host.set_storage("overflow", Vec::new()),
        Err(PluginTestHostError::InvalidStorage(reason))
            if reason.contains("key count")
    ));

    let mut key_list_host =
        PluginTestHost::new(CapabilityManifest::all(), 64).expect("valid key-list host");
    let keys_that_fit = MAX_STORAGE_KEY_LIST_BYTES / (4 + MAX_STORAGE_KEY_BYTES);
    assert!(keys_that_fit < MAX_STORAGE_KEYS);
    for index in 0..keys_that_fit {
        key_list_host
            .set_storage(full_length_storage_key(index), Vec::new())
            .expect("aggregate encoded key list remains within its byte bound");
    }
    assert!(matches!(
        key_list_host.set_storage(full_length_storage_key(keys_that_fit), Vec::new()),
        Err(PluginTestHostError::InvalidStorage(reason))
            if reason.contains("key-list bytes")
    ));
}

#[test]
fn semantic_digest_canonicalizes_signed_zero_in_seeded_state() {
    let mut positive = seeded_host(64);
    positive
        .set_player_position(player(), Vec3::new(0.0, 0.0, 0.0))
        .expect("finite positive zero");
    let mut negative = seeded_host(64);
    negative
        .set_player_position(player(), Vec3::new(-0.0, -0.0, -0.0))
        .expect("finite negative zero");

    let positive = positive
        .replay_builtin(builtin_factory(), &[])
        .expect("positive-zero replay");
    let negative = negative
        .replay_builtin(builtin_factory(), &[])
        .expect("negative-zero replay");
    assert_eq!(positive.effects(), negative.effects());
    assert_eq!(positive.state(), negative.state());
    assert_eq!(positive.digest(), negative.digest());
}
