use std::path::{Path, PathBuf};
use std::process::Command;

use ferrumc_plugin_region_guard::{builtin_factory, RegionGuardPlugin};
use ferrumc_plugin_sdk::{
    BlockDecision, BlockEvent, BlockPos, Capability, CapabilityManifest, Event, EventDecision,
    EventKind, Feedback, MessageOperation, PlaceAttempt, PlayerId, Plugin, Tick,
};
use ferrumc_testkit::{PluginEffect, PluginTestHost, ScheduledPluginEvent};

const EXPECTED_DIGEST: &str = "fbcac23e3fa4328757046977f3f8e3a74055d2b10d48d21690eb0a144c2d8836";
const PROTECTED_MESSAGE: &str = "This region is protected.";

fn player() -> PlayerId {
    PlayerId::offline("RegionGuardFixture")
}

fn requested_capabilities() -> CapabilityManifest {
    CapabilityManifest::empty()
        .with(Capability::VetoBlockEdits)
        .with(Capability::SubmitIntents)
}

fn scheduled(tick: u64, event: Event) -> ScheduledPluginEvent {
    ScheduledPluginEvent::new(Tick::new(tick), event)
}

fn event_log() -> Vec<ScheduledPluginEvent> {
    let player = player();
    vec![
        scheduled(
            1,
            Event::BlockPlaceAttempt(PlaceAttempt::new(
                player,
                BlockPos::new(-16, i32::MIN, 16),
                19,
            )),
        ),
        scheduled(
            2,
            Event::BlockPlaceAttempt(PlaceAttempt::new(player, BlockPos::new(17, 0, 0), 23)),
        ),
        scheduled(
            3,
            Event::BlockBreakAttempt(BlockEvent::new(player, BlockPos::new(16, i32::MAX, -16))),
        ),
        scheduled(
            4,
            Event::BlockBreakAttempt(BlockEvent::new(player, BlockPos::new(0, 64, -17))),
        ),
        scheduled(
            5,
            Event::BlockPlaceAttempt(PlaceAttempt::new(
                player,
                BlockPos::new(i32::MIN, i32::MIN, i32::MAX),
                u32::MAX,
            )),
        ),
        scheduled(
            6,
            Event::BlockBreakAttempt(BlockEvent::new(
                player,
                BlockPos::new(i32::MAX, i32::MAX, i32::MIN),
            )),
        ),
    ]
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("plugin crate is nested under server/plugins")
        .to_path_buf()
}

fn build_dynamic_plugin() -> PathBuf {
    let root = repo_root();
    let scope = format!("p58-region-guard-{}", std::process::id());
    let target = root.join(format!(".codex-tmp/{scope}-target"));
    let artifacts = root.join(format!(".codex-tmp/{scope}-artifacts"));
    std::fs::create_dir_all(&artifacts).expect("create repo-local artifact directory");

    let output = Command::new(env!("CARGO"))
        .arg("build")
        .arg("--manifest-path")
        .arg(root.join("server/Cargo.toml"))
        .arg("-p")
        .arg("ferrumc-plugin-region-guard")
        .arg("--lib")
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
        .expect("run nested region-guard build");
    assert!(
        output.status.success(),
        "nested region-guard build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let built = dynamic_artifact_from_messages(&output.stdout);
    let artifact = artifacts.join(format!(
        "{}ferrumc_plugin_region_guard{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    std::fs::copy(&built, &artifact).unwrap_or_else(|error| {
        panic!(
            "copy region-guard artifact {} to {}: {error}",
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
        .filter(|message| message["target"]["name"] == "ferrumc_plugin_region_guard")
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
        .expect("Cargo reported the region-guard cdylib artifact")
}

fn expected_effects() -> Vec<PluginEffect> {
    let player = player();
    let message =
        || MessageOperation::new(player, PROTECTED_MESSAGE).expect("example message is bounded");
    let feedback =
        || Feedback::new(PROTECTED_MESSAGE).expect("example decision feedback is bounded");

    vec![
        PluginEffect::Message(message()),
        PluginEffect::BlockDecision(BlockDecision::Deny(Some(feedback()))),
        PluginEffect::BlockDecision(BlockDecision::Allow),
        PluginEffect::Message(message()),
        PluginEffect::EventDecision {
            kind: EventKind::BlockBreakAttempt,
            decision: EventDecision::Deny(Some(feedback())),
        },
        PluginEffect::EventDecision {
            kind: EventKind::BlockBreakAttempt,
            decision: EventDecision::Allow,
        },
        PluginEffect::BlockDecision(BlockDecision::Allow),
        PluginEffect::EventDecision {
            kind: EventKind::BlockBreakAttempt,
            decision: EventDecision::Allow,
        },
    ]
}

#[test]
fn region_guard_identical_digest_builtin_vs_dynamic() {
    let declaration = RegionGuardPlugin::DECLARATION;
    let requested = requested_capabilities();
    assert_eq!(declaration.requested_capabilities(), requested);
    let factory = builtin_factory().expect("valid built-in declaration");
    assert_eq!(factory.declaration(), declaration);
    assert_eq!(factory.requested_capabilities(), requested);

    let dynamic_path = build_dynamic_plugin();
    let dynamic_plugin =
        ferrumc_plugin_abi_sys::load(&dynamic_path).expect("load real region-guard cdylib");
    let metadata = dynamic_plugin.metadata();
    assert_eq!(metadata.id(), declaration.id());
    assert_eq!(metadata.name(), declaration.name());
    assert_eq!(metadata.version().major(), declaration.version().major());
    assert_eq!(metadata.version().minor(), declaration.version().minor());
    assert_eq!(metadata.version().patch(), declaration.version().patch());
    assert_eq!(
        metadata.requested_capabilities(),
        u64::from(requested.bits())
    );

    let host = PluginTestHost::new(requested, 2).expect("valid exact-capability host");
    let events = event_log();
    let builtin = host
        .replay_builtin(factory, &events)
        .expect("compiled-in region-guard replay");
    let dynamic = host
        .replay_dynamic(&dynamic_path, &events)
        .expect("trusted native region-guard replay");

    assert_eq!(builtin.effects(), expected_effects());
    assert_eq!(builtin.effects(), dynamic.effects());
    assert_eq!(builtin.state(), dynamic.state());
    assert_eq!(builtin.digest(), dynamic.digest());
    assert_eq!(builtin.digest().as_hex(), EXPECTED_DIGEST);

    let state = builtin.state();
    assert!(builtin.diagnostics().is_empty());
    assert!(dynamic.diagnostics().is_empty());
    assert_eq!(
        state.messages(),
        [
            MessageOperation::new(player(), PROTECTED_MESSAGE)
                .expect("first expected message is bounded"),
            MessageOperation::new(player(), PROTECTED_MESSAGE)
                .expect("second expected message is bounded"),
        ]
    );
    assert!(state.subscriptions().is_empty());
    assert!(state.commands().is_empty());
    assert!(state.storage().is_empty());
    assert!(state.timers().is_empty());
    assert_eq!(state.player_position(player()), None);
    for pos in [
        BlockPos::new(-16, i32::MIN, 16),
        BlockPos::new(17, 0, 0),
        BlockPos::new(16, i32::MAX, -16),
        BlockPos::new(0, 64, -17),
        BlockPos::new(i32::MIN, i32::MIN, i32::MAX),
        BlockPos::new(i32::MAX, i32::MAX, i32::MIN),
    ] {
        assert_eq!(state.block_state_id(pos), None);
        assert!(!state.is_chunk_loaded(pos.to_chunk_pos()));
    }

    let mut perturbed_events = events;
    perturbed_events[0] = scheduled(
        1,
        Event::BlockPlaceAttempt(PlaceAttempt::new(
            player(),
            BlockPos::new(-17, i32::MIN, 16),
            19,
        )),
    );
    let perturbed = host
        .replay_builtin(
            builtin_factory().expect("valid perturbed built-in declaration"),
            &perturbed_events,
        )
        .expect("perturbed compiled-in replay");
    assert_ne!(builtin.effects(), perturbed.effects());
    assert_ne!(builtin.state(), perturbed.state());
    assert_ne!(builtin.digest(), perturbed.digest());
}
