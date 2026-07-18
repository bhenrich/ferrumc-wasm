#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ferrumc_plugin_abi::{
    FcCommandKind, FcEventKind, FcHostRequestKind, FcResourceHandle, FC_CAPABILITY_DENIED,
    FC_CAPABILITY_READ_WORLD, FC_CAPABILITY_RECEIVE_EVENTS, FC_CAPABILITY_SUBMIT_INTENTS,
    FC_EVENT_FLAGS_NONE, FC_INVALID_ARGUMENT, FC_OK, FC_PLUGIN_PANIC,
};
use ferrumc_plugin_loader::{
    HostCallOutcome, HostServices, OwnedCommand, OwnedEvent, OwnedHostRequest, PluginCapabilities,
    PluginCapability, PluginLoadError, PluginLoader,
};

mod support;

use support::package_bundle;
struct RecordingServices {
    granted_capabilities: u64,
    commands: Vec<OwnedCommand>,
    requests: Vec<OwnedHostRequest>,
}

impl HostServices for RecordingServices {
    fn call(&mut self, request: OwnedHostRequest) -> HostCallOutcome {
        let required = if request.kind() == FcHostRequestKind::DIMENSION {
            FC_CAPABILITY_READ_WORLD
        } else {
            return HostCallOutcome::Status(FC_INVALID_ARGUMENT);
        };
        self.requests.push(request);
        if self.granted_capabilities & required == 0 {
            HostCallOutcome::Status(FC_CAPABILITY_DENIED)
        } else {
            HostCallOutcome::Response(Vec::new())
        }
    }

    fn emit(&mut self, command: OwnedCommand) -> ferrumc_plugin_abi::FcStatus {
        let required = if command.kind() == FcCommandKind::SUBSCRIBE_EVENT {
            FC_CAPABILITY_RECEIVE_EVENTS
        } else if command.kind() == FcCommandKind::MESSAGE {
            FC_CAPABILITY_SUBMIT_INTENTS
        } else {
            return FC_INVALID_ARGUMENT;
        };
        if self.granted_capabilities & required == 0 {
            return FC_CAPABILITY_DENIED;
        }
        self.commands.push(command);
        FC_OK
    }

    fn diagnostic(&mut self, _level: u32, _message: String) -> ferrumc_plugin_abi::FcStatus {
        FC_OK
    }
}

#[test]
fn real_cdylib_loads_with_exact_hash_and_wrong_hash_is_rejected() {
    let server_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("fixture crate is nested under server/plugins");
    let repo_root = server_root
        .parent()
        .expect("server directory is nested under the repository");
    let target_dir = repo_root.join(".codex-tmp/p53-fixture-target");
    let output = Command::new(env!("CARGO"))
        .current_dir(server_root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args([
            "build",
            "-p",
            "ferrumc-plugin-fixture-dynamic",
            "--lib",
            "--message-format=json-render-diagnostics",
        ])
        .output()
        .expect("spawn nested fixture build");
    assert!(
        output.status.success(),
        "fixture build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let library = fixture_library(&output.stdout);
    assert!(
        library.is_file(),
        "fixture library missing at {}",
        library.display()
    );
    let scratch = repo_root
        .join(".codex-tmp/p53-real-bundle")
        .join(std::process::id().to_string());
    remove_if_present(&scratch);
    let valid_root = scratch.join("valid");
    let valid_bundle = package_bundle(&library, &valid_root);
    let manifest = fs::read_to_string(valid_bundle.join("plugin.toml"))
        .expect("read generated valid manifest");
    assert!(!manifest.contains("{{"));

    let capabilities = PluginCapabilities::empty()
        .with(PluginCapability::ReceiveEvents)
        .with(PluginCapability::SubmitIntents);
    let loader = PluginLoader::current(capabilities).expect("construct current loader");
    let plugins = loader
        .load_directory(&valid_root)
        .expect("load real ABI-v1 fixture");
    assert_eq!(plugins.len(), 1);
    let fixture = plugins
        .get("ferrumc-fixture-dynamic")
        .expect("loaded fixture id");
    assert_eq!(
        fixture.metadata().requested_capabilities(),
        FC_CAPABILITY_RECEIVE_EVENTS | FC_CAPABILITY_SUBMIT_INTENTS
    );
    exercise_real_callbacks(fixture);

    let wrong_root = scratch.join("wrong");
    let wrong_bundle = package_bundle(&library, &wrong_root);
    corrupt_first_hash_nibble(&wrong_bundle.join("plugin.toml"));
    let Err(error) = loader.load_directory(&wrong_root) else {
        panic!("wrong fixture hash must be rejected");
    };
    assert!(matches!(error, PluginLoadError::LibraryHashMismatch { .. }));

    cleanup_loaded_bundle(&scratch);
}

fn exercise_real_callbacks(fixture: &ferrumc_plugin_loader::LoadedPlugin) {
    let mut services = RecordingServices {
        granted_capabilities: FC_CAPABILITY_RECEIVE_EVENTS | FC_CAPABILITY_SUBMIT_INTENTS,
        commands: Vec::new(),
        requests: Vec::new(),
    };
    let mut active = fixture
        .initialize(&mut services)
        .expect("initialize real fixture");
    assert_subscriptions(&services);

    let player = [0x5a; 16];
    exercise_successful_event(&mut active, &mut services, player);
    exercise_denied_event(&mut active, &mut services, player);
    exercise_malformed_and_panic_events(&mut active, &mut services, player);

    assert_eq!(
        active
            .shutdown(&mut services)
            .expect("shutdown real fixture"),
        FC_OK
    );
}

fn assert_subscriptions(services: &RecordingServices) {
    assert_eq!(services.commands.len(), 3);
    assert!(services
        .commands
        .iter()
        .all(|command| command.kind() == FcCommandKind::SUBSCRIBE_EVENT));
    assert_eq!(
        services
            .commands
            .iter()
            .map(OwnedCommand::payload)
            .collect::<Vec<_>>(),
        [
            FcEventKind::BLOCK_BREAK.raw().to_le_bytes().as_slice(),
            FcEventKind::AFTER_BLOCK_BREAK
                .raw()
                .to_le_bytes()
                .as_slice(),
            FcEventKind::PLAYER_JOIN.raw().to_le_bytes().as_slice(),
        ]
    );
}

fn exercise_successful_event(
    active: &mut ferrumc_plugin_loader::ActivePlugin,
    services: &mut RecordingServices,
    player: [u8; 16],
) {
    services.commands.clear();
    let event = OwnedEvent::new(
        FcEventKind::BLOCK_BREAK,
        FC_EVENT_FLAGS_NONE,
        7,
        FcResourceHandle::from_raw(1),
        block_break_payload(player),
    );
    assert_eq!(
        active
            .on_event(&event, services)
            .expect("invoke successful fixture event"),
        FC_OK
    );
    assert_eq!(services.commands.len(), 1);
    assert_eq!(services.commands[0].kind(), FcCommandKind::MESSAGE);
    assert_eq!(
        services.commands[0].payload(),
        expected_message_payload(player)
    );
}

fn exercise_denied_event(
    active: &mut ferrumc_plugin_loader::ActivePlugin,
    services: &mut RecordingServices,
    player: [u8; 16],
) {
    services.commands.clear();
    let event = OwnedEvent::new(
        FcEventKind::AFTER_BLOCK_BREAK,
        FC_EVENT_FLAGS_NONE,
        8,
        FcResourceHandle::from_raw(1),
        block_break_payload(player),
    );
    assert_eq!(
        active
            .on_event(&event, services)
            .expect("invoke denied fixture event"),
        FC_CAPABILITY_DENIED
    );
    assert_eq!(services.commands.len(), 1);
    assert_eq!(
        services.commands[0].payload(),
        expected_message_payload(player)
    );
    assert_eq!(services.requests.len(), 1);
    assert_eq!(services.requests[0].kind(), FcHostRequestKind::DIMENSION);
    assert_eq!(services.requests[0].target(), FcResourceHandle::INVALID);
    assert!(services.requests[0].payload().is_empty());
}

fn exercise_malformed_and_panic_events(
    active: &mut ferrumc_plugin_loader::ActivePlugin,
    services: &mut RecordingServices,
    player: [u8; 16],
) {
    services.commands.clear();
    for malformed in [player[..15].to_vec(), {
        let mut trailing = player.to_vec();
        trailing.push(0);
        trailing
    }] {
        let event = OwnedEvent::new(
            FcEventKind::PLAYER_JOIN,
            FC_EVENT_FLAGS_NONE,
            9,
            FcResourceHandle::from_raw(1),
            malformed,
        );
        assert_eq!(
            active
                .on_event(&event, services)
                .expect("invoke malformed player-join fixture event"),
            FC_INVALID_ARGUMENT
        );
        assert!(services.commands.is_empty());
    }

    let panic_status = OwnedEvent::new(
        FcEventKind::PLAYER_JOIN,
        FC_EVENT_FLAGS_NONE,
        10,
        FcResourceHandle::from_raw(1),
        player.to_vec(),
    );
    assert_eq!(
        active
            .on_event(&panic_status, services)
            .expect("invoke panic-status fixture event"),
        FC_PLUGIN_PANIC
    );
    assert_eq!(services.commands.len(), 1);
    assert_eq!(
        services.commands[0].payload(),
        expected_message_payload(player)
    );
}

fn expected_message_payload(player: [u8; 16]) -> Vec<u8> {
    const MESSAGE: &str = "dynamic fixture handled event";

    let mut payload = Vec::with_capacity(16 + 4 + MESSAGE.len());
    payload.extend_from_slice(&player);
    payload.extend_from_slice(
        &u32::try_from(MESSAGE.len())
            .expect("fixture message length fits u32")
            .to_le_bytes(),
    );
    payload.extend_from_slice(MESSAGE.as_bytes());
    payload
}

fn block_break_payload(player: [u8; 16]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(36);
    payload.extend_from_slice(&36_u32.to_le_bytes());
    payload.extend_from_slice(&ferrumc_plugin_abi::ABI_MAJOR.to_le_bytes());
    payload.extend_from_slice(&ferrumc_plugin_abi::ABI_MINOR.to_le_bytes());
    payload.extend_from_slice(&player);
    payload.extend_from_slice(&10_i32.to_le_bytes());
    payload.extend_from_slice(&64_i32.to_le_bytes());
    payload.extend_from_slice(&(-3_i32).to_le_bytes());
    payload
}

fn fixture_library(cargo_stdout: &[u8]) -> PathBuf {
    for line in cargo_stdout.split(|byte| *byte == b'\n') {
        let Ok(message) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if message.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact")
            || message
                .pointer("/target/name")
                .and_then(serde_json::Value::as_str)
                != Some("ferrumc_plugin_fixture_dynamic")
        {
            continue;
        }
        let Some(filenames) = message
            .get("filenames")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for filename in filenames {
            let Some(filename) = filename.as_str() else {
                continue;
            };
            if filename.ends_with(std::env::consts::DLL_SUFFIX) {
                return PathBuf::from(filename);
            }
        }
    }
    panic!("Cargo did not report the dynamic fixture library artifact");
}

fn corrupt_first_hash_nibble(path: &Path) {
    const MARKER: &str = "library_sha256 = \"";

    let mut manifest = fs::read_to_string(path).expect("read fixture manifest for corruption");
    let start = manifest
        .find(MARKER)
        .map(|index| index + MARKER.len())
        .expect("fixture manifest hash field");
    let end = start + 1;
    let replacement = if manifest.as_bytes()[start] == b'0' {
        "1"
    } else {
        "0"
    };
    manifest.replace_range(start..end, replacement);
    fs::write(path, manifest).expect("write corrupted fixture manifest");
}

fn remove_if_present(path: &Path) {
    if let Err(error) = fs::remove_dir_all(path) {
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

#[cfg(not(target_os = "windows"))]
fn cleanup_loaded_bundle(path: &Path) {
    remove_if_present(path);
}

#[cfg(target_os = "windows")]
fn cleanup_loaded_bundle(_path: &Path) {
    // The loader intentionally keeps the DLL resident until process exit, and
    // Windows does not permit deleting a mapped DLL. The next test process can
    // remove this process-id-scoped directory before loading it.
}
