use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use ferrumc_plugin_abi::{
    FcCommandKind, FcEventKind, FcHostRequestKind, FcResourceHandle, FcStatus, ABI_MAJOR,
    ABI_MINOR, CURRENT_ABI, FC_CAPABILITIES_V1, FC_CAPABILITY_DENIED, FC_CAPABILITY_RECEIVE_EVENTS,
    FC_CAPABILITY_SUBMIT_INTENTS, FC_EVENT_FLAGS_NONE, FC_INVALID_ARGUMENT, FC_OK, FC_PLUGIN_PANIC,
};
use ferrumc_plugin_abi_sys::{
    load, CallbackError, HostCallOutcome, HostServices, LoadedAbiPlugin, OwnedCommand, OwnedEvent,
    OwnedHostRequest, PluginInstance,
};
use ferrumc_plugin_sdk::{Capability, CapabilityManifest};
use ferrumc_plugin_sdk_dynamic::TARGET_TRIPLE;

const DIMENSION_HANDLE: u64 = 0x0d1e_0510;
const PLAYER: [u8; 16] = [0x11; 16];

#[derive(Default)]
struct RecordingHost {
    commands: Vec<OwnedCommand>,
    diagnostics: Vec<(u32, String)>,
    requests: Vec<OwnedHostRequest>,
    override_response: Option<(u32, Vec<u8>)>,
    override_status: Option<(u32, FcStatus)>,
    chunk_loaded: bool,
}

impl RecordingHost {
    fn accepting() -> Self {
        Self {
            chunk_loaded: true,
            ..Self::default()
        }
    }

    fn clear_callback_output(&mut self) {
        self.commands.clear();
        self.diagnostics.clear();
        self.requests.clear();
    }

    fn set_response(&mut self, kind: FcHostRequestKind, response: Vec<u8>) {
        self.override_response = Some((kind.raw(), response));
    }

    fn clear_response(&mut self) {
        self.override_response = None;
    }

    fn set_status(&mut self, kind: FcHostRequestKind, status: FcStatus) {
        self.override_status = Some((kind.raw(), status));
    }
}

impl HostServices for RecordingHost {
    fn call(&mut self, request: OwnedHostRequest) -> HostCallOutcome {
        let status = self
            .override_status
            .filter(|(kind, _)| *kind == request.kind().raw())
            .map(|(_, status)| status);
        let response = self
            .override_response
            .as_ref()
            .filter(|(kind, _)| *kind == request.kind().raw())
            .map_or_else(
                || valid_response(request.kind(), self.chunk_loaded),
                |(_, bytes)| bytes.clone(),
            );
        self.requests.push(request);
        status.map_or(HostCallOutcome::Response(response), HostCallOutcome::Status)
    }

    fn emit(&mut self, command: OwnedCommand) -> FcStatus {
        self.commands.push(command);
        FC_OK
    }

    fn diagnostic(&mut self, level: u32, message: String) -> FcStatus {
        self.diagnostics.push((level, message));
        FC_OK
    }
}

fn valid_response(kind: FcHostRequestKind, chunk_loaded: bool) -> Vec<u8> {
    match kind.raw() {
        1 => DIMENSION_HANDLE.to_le_bytes().to_vec(),
        2 => vec![u8::from(chunk_loaded)],
        3 => 0x0102_0304_u32.to_le_bytes().to_vec(),
        4 => {
            let mut bytes = vec![1];
            push_vec3(&mut bytes, 8.5, 64.0, -4.25);
            bytes
        }
        5 => vec![1],
        6 => {
            let mut bytes = vec![1];
            push_field(&mut bytes, b"stored");
            bytes
        }
        7 => {
            let mut bytes = 2_u32.to_le_bytes().to_vec();
            push_field(&mut bytes, b"alpha");
            push_field(&mut bytes, b"omega");
            bytes
        }
        raw => panic!("unexpected host request kind {raw}"),
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("dynamic SDK crate is nested under server/crates")
        .to_path_buf()
}

fn fixture_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sdk-plugin/Cargo.toml")
}

fn build_fixture(feature: Option<&str>, artifact_name: &str) -> PathBuf {
    let root = repo_root();
    let target = root.join(".codex-tmp/p55-sdk-dynamic-fixture-target");
    let artifacts = root.join(".codex-tmp/p55-sdk-dynamic-fixtures");
    std::fs::create_dir_all(&artifacts).expect("create repo-local fixture artifacts");

    let mut command = Command::new("cargo");
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(fixture_manifest())
        .arg("--locked")
        .arg("--offline")
        .arg("--jobs")
        .arg("1")
        .arg("--no-default-features")
        .arg("--target-dir")
        .arg(&target);
    if let Some(feature) = feature {
        command.arg("--features").arg(feature);
    }
    let output = command.output().expect("run nested fixture build");
    assert!(
        output.status.success(),
        "nested fixture build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let file_name = format!(
        "{}ferrumc_plugin_sdk_dynamic_test_plugin{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let built = target.join("debug").join(file_name);
    let artifact = artifacts.join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        artifact_name,
        std::env::consts::DLL_SUFFIX
    ));
    std::fs::copy(&built, &artifact).unwrap_or_else(|error| {
        panic!(
            "copy fixture {} to {}: {error}",
            built.display(),
            artifact.display()
        )
    });
    artifact
}

fn load_fixture(path: &Path) -> LoadedAbiPlugin {
    load(path).unwrap_or_else(|error| panic!("load fixture {}: {error}", path.display()))
}

fn initialize(
    plugin: &LoadedAbiPlugin,
    host: &mut RecordingHost,
) -> Result<PluginInstance, CallbackError> {
    plugin.initialize(FC_CAPABILITIES_V1, host)
}

fn event(kind: FcEventKind, payload: Vec<u8>) -> OwnedEvent {
    OwnedEvent::new(
        kind,
        FC_EVENT_FLAGS_NONE,
        42,
        FcResourceHandle::from_raw(9),
        payload,
    )
}

fn invoke(instance: &mut PluginInstance, host: &mut RecordingHost, event: &OwnedEvent) -> FcStatus {
    instance
        .on_event(event, host)
        .expect("validated host-side invocation")
}

fn push_header(bytes: &mut Vec<u8>, size: u32) {
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&ABI_MAJOR.to_le_bytes());
    bytes.extend_from_slice(&ABI_MINOR.to_le_bytes());
}

fn push_player(bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&PLAYER);
}

fn push_pos(bytes: &mut Vec<u8>, x: i32, y: i32, z: i32) {
    bytes.extend_from_slice(&x.to_le_bytes());
    bytes.extend_from_slice(&y.to_le_bytes());
    bytes.extend_from_slice(&z.to_le_bytes());
}

fn push_vec3(bytes: &mut Vec<u8>, x: f64, y: f64, z: f64) {
    bytes.extend_from_slice(&x.to_bits().to_le_bytes());
    bytes.extend_from_slice(&y.to_bits().to_le_bytes());
    bytes.extend_from_slice(&z.to_bits().to_le_bytes());
}

fn push_field(bytes: &mut Vec<u8>, value: &[u8]) {
    let len = u32::try_from(value.len()).expect("test field length fits u32");
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(value);
}

fn block_record(x: i32, y: i32, z: i32) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_header(&mut bytes, 36);
    push_player(&mut bytes);
    push_pos(&mut bytes, x, y, z);
    bytes
}

fn extended_block_record(x: i32, y: i32, z: i32) -> Vec<u8> {
    let mut bytes = block_record(x, y, z);
    bytes[0..4].copy_from_slice(&40_u32.to_le_bytes());
    bytes.extend_from_slice(&[0xa1, 0xb2, 0xc3, 0xd4]);
    bytes
}

fn place_payload(block_state_id: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_player(&mut bytes);
    push_pos(&mut bytes, 1, 2, 3);
    bytes.extend_from_slice(&block_state_id.to_le_bytes());
    bytes
}

fn move_payload() -> Vec<u8> {
    let mut bytes = Vec::new();
    push_player(&mut bytes);
    push_pos(&mut bytes, 1, 2, 3);
    push_pos(&mut bytes, -4, 5, 6);
    bytes
}

fn chat_payload(message: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_player(&mut bytes);
    push_field(&mut bytes, message);
    bytes
}

fn interaction_air_payload() -> Vec<u8> {
    let mut bytes = Vec::new();
    push_player(&mut bytes);
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes
}

fn interaction_block_payload() -> Vec<u8> {
    let mut bytes = Vec::new();
    push_player(&mut bytes);
    bytes.extend_from_slice(&[0, 1, 0, 0]);
    push_pos(&mut bytes, 7, 8, 9);
    bytes.extend_from_slice(&[5, 0, 0, 0]);
    bytes
}

fn interaction_entity_payload() -> Vec<u8> {
    let mut bytes = Vec::new();
    push_player(&mut bytes);
    bytes.extend_from_slice(&[1, 2, 0, 0]);
    bytes.extend_from_slice(&123_456_i32.to_le_bytes());
    bytes
}

fn command_payload(handler: u64) -> Vec<u8> {
    let mut bytes = handler.to_le_bytes().to_vec();
    push_player(&mut bytes);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes
}

fn command_with_typed_arguments_payload(handler: u64) -> Vec<u8> {
    let mut bytes = handler.to_le_bytes().to_vec();
    push_player(&mut bytes);
    bytes.extend_from_slice(&2_u32.to_le_bytes());

    push_field(&mut bytes, b"target");
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    push_field(&mut bytes, b"spawn");

    push_field(&mut bytes, b"count");
    bytes.extend_from_slice(&[1, 0, 0, 0]);
    bytes.extend_from_slice(&(-17_i64).to_le_bytes());
    bytes
}

fn command_with_text_argument(
    handler: u64,
    name: &[u8],
    kind: u8,
    reserved: [u8; 3],
    value: &[u8],
) -> Vec<u8> {
    let mut bytes = handler.to_le_bytes().to_vec();
    push_player(&mut bytes);
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    push_field(&mut bytes, name);
    bytes.push(kind);
    bytes.extend_from_slice(&reserved);
    push_field(&mut bytes, value);
    bytes
}

fn timer_payload(timer: u64) -> Vec<u8> {
    timer.to_le_bytes().to_vec()
}

fn valid_events() -> Vec<(FcEventKind, Vec<u8>)> {
    vec![
        (FcEventKind::PLAYER_JOIN, PLAYER.to_vec()),
        (FcEventKind::PLAYER_LEAVE, PLAYER.to_vec()),
        (FcEventKind::BLOCK_BREAK, block_record(10, 11, 12)),
        (FcEventKind::BLOCK_BREAK, extended_block_record(10, 11, 12)),
        (FcEventKind::AFTER_BLOCK_PLACE, place_payload(23)),
        (FcEventKind::AFTER_BLOCK_BREAK, block_record(13, 14, 15)),
        (FcEventKind::PLAYER_MOVE, move_payload()),
        (FcEventKind::BLOCK_PLACE_ATTEMPT, place_payload(24)),
        (FcEventKind::BLOCK_BREAK_ATTEMPT, block_record(16, 17, 18)),
        (FcEventKind::CHAT_ATTEMPT, chat_payload(b"hello")),
        (FcEventKind::INTERACT_ATTEMPT, interaction_air_payload()),
        (FcEventKind::INTERACT_ATTEMPT, interaction_block_payload()),
        (FcEventKind::INTERACT_ATTEMPT, interaction_entity_payload()),
        (FcEventKind::COMMAND, command_payload(7)),
        (
            FcEventKind::COMMAND,
            command_with_typed_arguments_payload(7),
        ),
        (FcEventKind::TIMER, timer_payload(1)),
    ]
}

// Keeping the hostile cases together makes omissions across the versioned
// grammar visible during review.
#[allow(clippy::too_many_lines)]
fn malformed_event_payloads() -> Vec<(&'static str, FcEventKind, Vec<u8>)> {
    let mut cases = Vec::new();
    for (kind, payload) in valid_events() {
        let mut truncated = payload.clone();
        let _last = truncated.pop();
        cases.push(("truncated event payload", kind, truncated));

        let mut trailing = payload;
        trailing.push(0xaa);
        cases.push(("trailing event payload", kind, trailing));
    }

    let mut bad_header = block_record(1, 2, 3);
    bad_header[0..4].copy_from_slice(&35_u32.to_le_bytes());
    cases.push((
        "wrong versioned record size",
        FcEventKind::BLOCK_BREAK,
        bad_header,
    ));
    let mut bad_header = block_record(1, 2, 3);
    bad_header[4..6].copy_from_slice(&(ABI_MAJOR + 1).to_le_bytes());
    cases.push((
        "wrong versioned record major",
        FcEventKind::BLOCK_BREAK,
        bad_header,
    ));

    cases.push((
        "oversized chat",
        FcEventKind::CHAT_ATTEMPT,
        chat_payload(&vec![b'x'; 257]),
    ));
    cases.push((
        "invalid chat UTF-8",
        FcEventKind::CHAT_ATTEMPT,
        chat_payload(&[0xff]),
    ));

    let mut bad_hand = interaction_air_payload();
    bad_hand[16] = 2;
    cases.push((
        "unknown interaction hand",
        FcEventKind::INTERACT_ATTEMPT,
        bad_hand,
    ));
    let mut bad_target = interaction_air_payload();
    bad_target[17] = 3;
    cases.push((
        "unknown interaction target",
        FcEventKind::INTERACT_ATTEMPT,
        bad_target,
    ));
    let mut bad_reserved = interaction_air_payload();
    bad_reserved[18] = 1;
    cases.push((
        "nonzero interaction reserved word",
        FcEventKind::INTERACT_ATTEMPT,
        bad_reserved,
    ));
    let mut bad_face = interaction_block_payload();
    bad_face[32] = 6;
    cases.push((
        "unknown interaction face",
        FcEventKind::INTERACT_ATTEMPT,
        bad_face,
    ));
    let mut bad_reserved = interaction_block_payload();
    bad_reserved[33] = 1;
    cases.push((
        "nonzero block-target reserved byte",
        FcEventKind::INTERACT_ATTEMPT,
        bad_reserved,
    ));

    cases.push((
        "zero command handler",
        FcEventKind::COMMAND,
        command_payload(0),
    ));
    let mut too_many_arguments = 7_u64.to_le_bytes().to_vec();
    push_player(&mut too_many_arguments);
    too_many_arguments.extend_from_slice(&65_u32.to_le_bytes());
    cases.push((
        "oversized command argument count",
        FcEventKind::COMMAND,
        too_many_arguments,
    ));
    cases.push((
        "unknown command argument tag",
        FcEventKind::COMMAND,
        command_with_text_argument(7, b"name", 2, [0; 3], b"value"),
    ));
    cases.push((
        "nonzero command argument reserved byte",
        FcEventKind::COMMAND,
        command_with_text_argument(7, b"name", 0, [1, 0, 0], b"value"),
    ));
    cases.push((
        "empty command argument name",
        FcEventKind::COMMAND,
        command_with_text_argument(7, b"", 0, [0; 3], b"value"),
    ));
    cases.push((
        "invalid command argument UTF-8",
        FcEventKind::COMMAND,
        command_with_text_argument(7, &[0xff], 0, [0; 3], b"value"),
    ));
    cases.push((
        "oversized command text argument",
        FcEventKind::COMMAND,
        command_with_text_argument(7, b"name", 0, [0; 3], &vec![b'x'; 4_097]),
    ));

    let mut aggregate = 7_u64.to_le_bytes().to_vec();
    push_player(&mut aggregate);
    aggregate.extend_from_slice(&16_u32.to_le_bytes());
    for _ in 0..16 {
        push_field(&mut aggregate, &[b'n'; 64]);
        aggregate.extend_from_slice(&[0, 0, 0, 0]);
        push_field(&mut aggregate, &vec![b'x'; 4_096]);
    }
    cases.push((
        "oversized aggregate command invocation",
        FcEventKind::COMMAND,
        aggregate,
    ));

    cases.push(("zero timer id", FcEventKind::TIMER, timer_payload(0)));
    cases
}

fn assert_exact_command_encodings(commands: &[OwnedCommand]) {
    let unique_kinds: BTreeSet<u32> = commands
        .iter()
        .map(|command| command.kind().raw())
        .collect();
    assert_eq!(unique_kinds, (1_u32..=12).collect());

    let command = command_of_kind(commands, FcCommandKind::SUBSCRIBE_EVENT);
    assert_eq!(
        command.payload(),
        FcEventKind::PLAYER_JOIN.raw().to_le_bytes()
    );
    assert_eq!(command.target(), FcResourceHandle::INVALID);

    let command = command_of_kind(commands, FcCommandKind::REGISTER_COMMAND);
    let mut expected = 4_u32.to_le_bytes().to_vec();
    push_command_node(
        &mut expected,
        u32::MAX,
        0,
        true,
        2,
        0,
        0,
        b"sdkfixture",
        b"",
        7,
    );
    push_command_node(&mut expected, 0, 1, false, 0xff, 0, 0, b"target", b"", 0);
    push_command_node(
        &mut expected,
        1,
        2,
        false,
        0xff,
        0,
        0,
        b"message",
        b"ferrumc.fixture.use",
        0,
    );
    push_command_node(&mut expected, 2, 3, true, 0xff, -4, 9, b"count", b"", 8);
    assert_eq!(command.payload(), expected);

    let command = command_of_kind(commands, FcCommandKind::STORAGE_PUT);
    let mut expected = Vec::new();
    push_field(&mut expected, b"boot");
    push_field(&mut expected, b"binary-value");
    assert_eq!(command.payload(), expected);

    let command = command_of_kind(commands, FcCommandKind::STORAGE_DELETE);
    let mut expected = Vec::new();
    push_field(&mut expected, b"stale");
    assert_eq!(command.payload(), expected);

    let command = command_of_kind(commands, FcCommandKind::SCHEDULE_TIMER);
    let mut expected = 1_u64.to_le_bytes().to_vec();
    expected.extend_from_slice(&20_u64.to_le_bytes());
    assert_eq!(command.payload(), expected);

    let command = command_of_kind(commands, FcCommandKind::CANCEL_TIMER);
    assert_eq!(command.payload(), 2_u64.to_le_bytes());

    let command = command_of_kind(commands, FcCommandKind::SET_BLOCK);
    let mut expected = Vec::new();
    push_header(&mut expected, 24);
    push_pos(&mut expected, 10, 11, 12);
    expected.extend_from_slice(&0x1122_3344_u32.to_le_bytes());
    assert_eq!(command.payload(), expected);
    assert_eq!(command.target().raw(), DIMENSION_HANDLE);

    let command = command_of_kind(commands, FcCommandKind::TELEPORT);
    let mut expected = PLAYER.to_vec();
    push_vec3(&mut expected, 1.25, 64.0, -2.5);
    assert_eq!(command.payload(), expected);
    assert_eq!(command.target(), FcResourceHandle::INVALID);

    let command = command_of_kind(commands, FcCommandKind::MESSAGE);
    let mut expected = PLAYER.to_vec();
    push_field(&mut expected, b"binary hello");
    assert_eq!(command.payload(), expected);

    assert!(commands
        .iter()
        .any(|command| command.kind() == FcCommandKind::DECISION_ALLOW
            && command.payload().is_empty()));
    assert!(commands.iter().any(|command| {
        if command.kind() != FcCommandKind::DECISION_DENY {
            return false;
        }
        let mut expected = Vec::new();
        push_field(&mut expected, b"break denied");
        command.payload() == expected
    }));
    assert!(commands.iter().any(|command| {
        command.kind() == FcCommandKind::DECISION_DENY && command.payload().is_empty()
    }));
    assert!(commands.iter().any(|command| {
        command.kind() == FcCommandKind::DECISION_REPLACE_BLOCK
            && command.payload() == 0x5566_7788_u32.to_le_bytes()
    }));

    for command in commands {
        assert_binary_not_json(command.payload());
    }
}

#[allow(clippy::too_many_arguments)]
fn push_command_node(
    bytes: &mut Vec<u8>,
    parent: u32,
    kind: u8,
    executable: bool,
    required_level: u8,
    min: i64,
    max: i64,
    name: &[u8],
    permission: &[u8],
    handler: u64,
) {
    bytes.extend_from_slice(&parent.to_le_bytes());
    bytes.extend_from_slice(&[kind, u8::from(executable), required_level, 0]);
    bytes.extend_from_slice(&min.to_le_bytes());
    bytes.extend_from_slice(&max.to_le_bytes());
    push_field(bytes, name);
    push_field(bytes, permission);
    bytes.extend_from_slice(&handler.to_le_bytes());
}

fn command_of_kind(commands: &[OwnedCommand], kind: FcCommandKind) -> &OwnedCommand {
    commands
        .iter()
        .find(|command| command.kind() == kind)
        .unwrap_or_else(|| panic!("missing command kind {}", kind.raw()))
}

fn assert_exact_request_encodings(requests: &[OwnedHostRequest]) {
    let dimension = requests
        .iter()
        .find(|request| request.kind() == FcHostRequestKind::DIMENSION)
        .expect("dimension request");
    assert_eq!(dimension.target(), FcResourceHandle::INVALID);
    assert!(dimension.payload().is_empty());

    let chunk = requests
        .iter()
        .find(|request| {
            request.kind() == FcHostRequestKind::CHUNK_LOADED
                && request.payload() == [(-3_i32).to_le_bytes(), 9_i32.to_le_bytes()].concat()
        })
        .expect("typed chunk-loaded request");
    assert_eq!(chunk.target().raw(), DIMENSION_HANDLE);

    let block = requests
        .iter()
        .find(|request| request.kind() == FcHostRequestKind::BLOCK_STATE)
        .expect("block-state request");
    let mut expected = Vec::new();
    push_header(&mut expected, 20);
    push_pos(&mut expected, 4, 5, 6);
    assert_eq!(block.payload(), expected);
    assert_eq!(block.target().raw(), DIMENSION_HANDLE);

    let player_position = requests
        .iter()
        .find(|request| request.kind() == FcHostRequestKind::PLAYER_POSITION)
        .expect("player-position request");
    assert_eq!(player_position.payload(), PLAYER);
    assert_eq!(player_position.target(), FcResourceHandle::INVALID);

    let permission = requests
        .iter()
        .find(|request| request.kind() == FcHostRequestKind::PERMISSION_RESOLVE)
        .expect("permission request");
    let mut expected = PLAYER.to_vec();
    push_field(&mut expected, b"ferrumc.fixture.use");
    assert_eq!(permission.payload(), expected);

    let storage_get = requests
        .iter()
        .find(|request| request.kind() == FcHostRequestKind::STORAGE_GET)
        .expect("storage get request");
    let mut expected = Vec::new();
    push_field(&mut expected, b"fixture-key");
    assert_eq!(storage_get.payload(), expected);

    let storage_keys = requests
        .iter()
        .find(|request| request.kind() == FcHostRequestKind::STORAGE_KEYS)
        .expect("storage keys request");
    assert!(storage_keys.payload().is_empty());

    for request in requests {
        assert_binary_not_json(request.payload());
    }
}

fn assert_binary_not_json(payload: &[u8]) {
    assert!(
        !matches!(payload.first(), Some(b'{' | b'[')),
        "ABI hot-path payload unexpectedly starts like JSON"
    );
}

fn assert_no_json_codec_dependency_or_source() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest =
        std::fs::read_to_string(crate_root.join("Cargo.toml")).expect("read dynamic SDK manifest");
    assert!(!manifest.contains("serde_json"));

    for entry in std::fs::read_dir(crate_root.join("src")).expect("read dynamic SDK sources") {
        let path = entry.expect("source directory entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        assert!(
            !source.contains("serde_json::")
                && !source.contains("serde::Serialize")
                && !source.contains("serde::Deserialize"),
            "{} introduced a serialization hot path",
            path.display()
        );
    }
}

// This table deliberately enumerates every response family and failure shape.
#[allow(clippy::too_many_lines)]
fn assert_malformed_responses_fail_closed(instance: &mut PluginInstance, host: &mut RecordingHost) {
    let mut cases = Vec::new();
    cases.extend([
        (
            "truncated dimension",
            FcHostRequestKind::DIMENSION,
            Vec::new(),
            FcEventKind::PLAYER_JOIN,
            PLAYER.to_vec(),
        ),
        (
            "zero dimension",
            FcHostRequestKind::DIMENSION,
            0_u64.to_le_bytes().to_vec(),
            FcEventKind::PLAYER_JOIN,
            PLAYER.to_vec(),
        ),
        (
            "trailing dimension",
            FcHostRequestKind::DIMENSION,
            [1_u64.to_le_bytes().as_slice(), &[0]].concat(),
            FcEventKind::PLAYER_JOIN,
            PLAYER.to_vec(),
        ),
        (
            "truncated chunk bool",
            FcHostRequestKind::CHUNK_LOADED,
            Vec::new(),
            FcEventKind::PLAYER_JOIN,
            PLAYER.to_vec(),
        ),
        (
            "unknown chunk bool tag",
            FcHostRequestKind::CHUNK_LOADED,
            vec![2],
            FcEventKind::PLAYER_JOIN,
            PLAYER.to_vec(),
        ),
        (
            "trailing chunk bool",
            FcHostRequestKind::CHUNK_LOADED,
            vec![1, 0],
            FcEventKind::PLAYER_JOIN,
            PLAYER.to_vec(),
        ),
        (
            "truncated block state",
            FcHostRequestKind::BLOCK_STATE,
            vec![1, 2, 3],
            FcEventKind::PLAYER_LEAVE,
            PLAYER.to_vec(),
        ),
        (
            "trailing block state",
            FcHostRequestKind::BLOCK_STATE,
            [1_u32.to_le_bytes().as_slice(), &[0]].concat(),
            FcEventKind::PLAYER_LEAVE,
            PLAYER.to_vec(),
        ),
        (
            "truncated player position",
            FcHostRequestKind::PLAYER_POSITION,
            vec![1; 24],
            FcEventKind::BLOCK_BREAK,
            block_record(10, 11, 12),
        ),
        (
            "unknown player-position tag",
            FcHostRequestKind::PLAYER_POSITION,
            vec![2],
            FcEventKind::BLOCK_BREAK,
            block_record(10, 11, 12),
        ),
        (
            "trailing absent player position",
            FcHostRequestKind::PLAYER_POSITION,
            vec![0, 0],
            FcEventKind::BLOCK_BREAK,
            block_record(10, 11, 12),
        ),
        (
            "truncated permission resolution",
            FcHostRequestKind::PERMISSION_RESOLVE,
            Vec::new(),
            FcEventKind::AFTER_BLOCK_PLACE,
            place_payload(23),
        ),
        (
            "unknown permission resolution",
            FcHostRequestKind::PERMISSION_RESOLVE,
            vec![3],
            FcEventKind::AFTER_BLOCK_PLACE,
            place_payload(23),
        ),
        (
            "trailing permission resolution",
            FcHostRequestKind::PERMISSION_RESOLVE,
            vec![1, 0],
            FcEventKind::AFTER_BLOCK_PLACE,
            place_payload(23),
        ),
        (
            "truncated storage value",
            FcHostRequestKind::STORAGE_GET,
            vec![1],
            FcEventKind::AFTER_BLOCK_BREAK,
            block_record(13, 14, 15),
        ),
        (
            "unknown storage presence tag",
            FcHostRequestKind::STORAGE_GET,
            vec![2],
            FcEventKind::AFTER_BLOCK_BREAK,
            block_record(13, 14, 15),
        ),
        (
            "trailing absent storage value",
            FcHostRequestKind::STORAGE_GET,
            vec![0, 0],
            FcEventKind::AFTER_BLOCK_BREAK,
            block_record(13, 14, 15),
        ),
        (
            "truncated storage key list",
            FcHostRequestKind::STORAGE_KEYS,
            vec![0, 0, 0],
            FcEventKind::PLAYER_MOVE,
            move_payload(),
        ),
        (
            "oversized storage key count",
            FcHostRequestKind::STORAGE_KEYS,
            257_u32.to_le_bytes().to_vec(),
            FcEventKind::PLAYER_MOVE,
            move_payload(),
        ),
        (
            "trailing storage key list",
            FcHostRequestKind::STORAGE_KEYS,
            [0_u32.to_le_bytes().as_slice(), &[0]].concat(),
            FcEventKind::PLAYER_MOVE,
            move_payload(),
        ),
    ]);

    let mut nonfinite = vec![1];
    push_vec3(&mut nonfinite, f64::NAN, 2.0, 3.0);
    cases.push((
        "non-finite player position",
        FcHostRequestKind::PLAYER_POSITION,
        nonfinite,
        FcEventKind::BLOCK_BREAK,
        block_record(10, 11, 12),
    ));

    let mut oversized_storage = vec![1];
    oversized_storage.extend_from_slice(&65_532_u32.to_le_bytes());
    cases.push((
        "oversized storage value length",
        FcHostRequestKind::STORAGE_GET,
        oversized_storage,
        FcEventKind::AFTER_BLOCK_BREAK,
        block_record(13, 14, 15),
    ));

    let mut invalid_utf8_key = 1_u32.to_le_bytes().to_vec();
    push_field(&mut invalid_utf8_key, &[0xff]);
    cases.push((
        "invalid storage-key UTF-8",
        FcHostRequestKind::STORAGE_KEYS,
        invalid_utf8_key,
        FcEventKind::PLAYER_MOVE,
        move_payload(),
    ));
    let mut empty_key = 1_u32.to_le_bytes().to_vec();
    push_field(&mut empty_key, b"");
    cases.push((
        "empty storage key",
        FcHostRequestKind::STORAGE_KEYS,
        empty_key,
        FcEventKind::PLAYER_MOVE,
        move_payload(),
    ));
    let mut oversized_key = 1_u32.to_le_bytes().to_vec();
    oversized_key.extend_from_slice(&257_u32.to_le_bytes());
    cases.push((
        "oversized storage-key length",
        FcHostRequestKind::STORAGE_KEYS,
        oversized_key,
        FcEventKind::PLAYER_MOVE,
        move_payload(),
    ));

    for (label, response_kind, response, event_kind, payload) in cases {
        host.clear_callback_output();
        host.set_response(response_kind, response);
        assert_eq!(
            invoke(instance, host, &event(event_kind, payload)),
            ferrumc_plugin_abi::FC_ERROR,
            "{label}"
        );
        assert!(
            !host.diagnostics.is_empty(),
            "{label} did not produce a bounded failure diagnostic"
        );
    }
    host.clear_response();
}

fn assert_chunk_absence_short_circuits_block_state(
    instance: &mut PluginInstance,
    host: &mut RecordingHost,
) {
    host.clear_callback_output();
    host.chunk_loaded = false;
    assert_eq!(
        invoke(
            instance,
            host,
            &event(FcEventKind::PLAYER_LEAVE, PLAYER.to_vec())
        ),
        FC_OK
    );
    assert!(host
        .requests
        .iter()
        .any(|request| request.kind() == FcHostRequestKind::CHUNK_LOADED));
    assert!(host
        .requests
        .iter()
        .all(|request| request.kind() != FcHostRequestKind::BLOCK_STATE));
    host.chunk_loaded = true;
}

fn assert_missing_dimension_capability_fails_closed(plugin: &LoadedAbiPlugin) {
    let mut host = RecordingHost::accepting();
    let mut instance = plugin
        .initialize(
            FC_CAPABILITY_RECEIVE_EVENTS | FC_CAPABILITY_SUBMIT_INTENTS,
            &mut host,
        )
        .expect("initialize restricted fixture");
    host.clear_callback_output();
    host.set_status(FcHostRequestKind::DIMENSION, FC_CAPABILITY_DENIED);
    assert_eq!(
        invoke(
            &mut instance,
            &mut host,
            &event(FcEventKind::BLOCK_BREAK, block_record(10, 11, 12))
        ),
        ferrumc_plugin_abi::FC_ERROR
    );
    assert!(host
        .requests
        .iter()
        .all(|request| request.kind() != FcHostRequestKind::PLAYER_POSITION));
    assert!(host
        .requests
        .iter()
        .any(|request| request.kind() == FcHostRequestKind::DIMENSION));
    assert!(host.commands.is_empty());
    assert!(!host.diagnostics.is_empty());
    assert_eq!(
        instance
            .shutdown(&mut host)
            .expect("restricted fixture shutdown"),
        FC_OK
    );
}

fn assert_no_unwind_for_plugin_panic(
    plugin: &LoadedAbiPlugin,
    kind: FcEventKind,
    payload: Vec<u8>,
) {
    let mut host = RecordingHost::accepting();
    let mut instance = initialize(plugin, &mut host).expect("initialize panic-test instance");
    host.clear_callback_output();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        instance.on_event(&event(kind, payload), &mut host)
    }));
    let callback = outcome.expect("plugin unwind escaped the ABI callback");
    assert_eq!(
        callback.expect("host-side callback validation"),
        FC_PLUGIN_PANIC
    );
    assert!(host.commands.is_empty());
    assert!(!host.diagnostics.is_empty());
}

fn assert_initialize_panic(plugin: &LoadedAbiPlugin) {
    let mut host = RecordingHost::accepting();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        initialize(plugin, &mut host)
    }));
    match outcome.expect("initialization unwind escaped the ABI callback") {
        Err(CallbackError::Status(status)) => assert_eq!(status, FC_PLUGIN_PANIC),
        other => panic!("expected FC_PLUGIN_PANIC initialization status, got {other:?}"),
    }
}

fn assert_shutdown_panic(plugin: &LoadedAbiPlugin) {
    let mut host = RecordingHost::accepting();
    let instance = initialize(plugin, &mut host).expect("initialize shutdown-panic fixture");
    host.clear_callback_output();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        instance.shutdown(&mut host)
    }));
    assert_eq!(
        outcome
            .expect("shutdown unwind escaped the ABI callback")
            .expect("host-side shutdown validation"),
        FC_PLUGIN_PANIC
    );
    assert!(!host.diagnostics.is_empty());
}

#[test]
fn real_cdylib_proves_binary_contract_and_contains_all_unwinds() {
    let normal_path = build_fixture(None, "sdk-dynamic-normal");
    let create_panic_path = build_fixture(Some("panic-create"), "sdk-dynamic-panic-create");
    let load_panic_path = build_fixture(Some("panic-load"), "sdk-dynamic-panic-load");
    let unload_panic_path = build_fixture(Some("panic-unload"), "sdk-dynamic-panic-unload");
    let drop_panic_path = build_fixture(Some("panic-drop"), "sdk-dynamic-panic-drop");

    let plugin = load_fixture(&normal_path);
    assert_eq!(plugin.metadata().id(), "sdk-dynamic-fixture");
    assert_eq!(plugin.metadata().name(), "SDK Dynamic Fixture");
    assert_eq!(plugin.metadata().abi_version(), CURRENT_ABI);
    assert_eq!(plugin.metadata().version().major(), 1);
    assert_eq!(plugin.metadata().version().minor(), 2);
    assert_eq!(plugin.metadata().version().patch(), 3);
    assert_eq!(
        plugin.metadata().requested_capabilities(),
        FC_CAPABILITIES_V1
    );
    assert_eq!(
        u64::from(CapabilityManifest::all().bits()),
        FC_CAPABILITIES_V1
    );
    for capability in Capability::ALL {
        assert!(CapabilityManifest::all().grants(capability));
    }
    assert_eq!(plugin.metadata().target(), TARGET_TRIPLE);
    assert_no_json_codec_dependency_or_source();

    let mut host = RecordingHost::accepting();
    let mut instance = initialize(&plugin, &mut host).expect("initialize SDK fixture");
    for (kind, payload) in valid_events() {
        assert_eq!(
            invoke(&mut instance, &mut host, &event(kind, payload)),
            FC_OK
        );
    }
    assert_exact_command_encodings(&host.commands);
    assert_exact_request_encodings(&host.requests);

    for (label, kind, payload) in malformed_event_payloads() {
        host.clear_callback_output();
        assert_eq!(
            invoke(&mut instance, &mut host, &event(kind, payload)),
            FC_INVALID_ARGUMENT,
            "{label}"
        );
        assert!(host.commands.is_empty(), "{label} reached plugin logic");
    }
    host.clear_callback_output();
    assert_eq!(
        invoke(
            &mut instance,
            &mut host,
            &event(FcEventKind::from_raw(0xffff_fffe), Vec::new())
        ),
        FC_INVALID_ARGUMENT
    );
    let bad_flags = OwnedEvent::new(
        FcEventKind::PLAYER_JOIN,
        1,
        42,
        FcResourceHandle::from_raw(9),
        PLAYER.to_vec(),
    );
    assert_eq!(
        instance
            .on_event(&bad_flags, &mut host)
            .expect("host-side bad-flags invocation"),
        FC_INVALID_ARGUMENT
    );

    assert_malformed_responses_fail_closed(&mut instance, &mut host);
    assert_chunk_absence_short_circuits_block_state(&mut instance, &mut host);
    assert_missing_dimension_capability_fails_closed(&plugin);

    host.clear_callback_output();
    assert_eq!(
        instance
            .shutdown(&mut host)
            .expect("normal fixture shutdown"),
        FC_OK
    );

    assert_no_unwind_for_plugin_panic(
        &plugin,
        FcEventKind::BLOCK_BREAK,
        block_record(i32::MIN, 1, 2),
    );
    assert_no_unwind_for_plugin_panic(
        &plugin,
        FcEventKind::BLOCK_PLACE_ATTEMPT,
        place_payload(u32::MAX),
    );
    assert_no_unwind_for_plugin_panic(&plugin, FcEventKind::COMMAND, command_payload(99));
    assert_no_unwind_for_plugin_panic(&plugin, FcEventKind::TIMER, timer_payload(99));
    assert_no_unwind_for_plugin_panic(&plugin, FcEventKind::TIMER, timer_payload(100));

    assert_initialize_panic(&load_fixture(&create_panic_path));
    assert_initialize_panic(&load_fixture(&load_panic_path));
    assert_shutdown_panic(&load_fixture(&unload_panic_path));
    assert_shutdown_panic(&load_fixture(&drop_panic_path));
}
