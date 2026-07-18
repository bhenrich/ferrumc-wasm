//! Standard negative-path QA for the trusted native plugin loader of a
//! Minecraft game server.

#![forbid(unsafe_code)]

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use ferrumc_core::{DimensionId, PlayerId, PluginId, TextComponent, Tick, WorldId};
use ferrumc_math::{BlockPos, ChunkPos, ShardPos, Vec3};
use ferrumc_permission::{PermissionNode, Resolution};
use ferrumc_plugin_abi::{FcPluginFunctionsV1, FC_PLUGIN_PANIC};
use ferrumc_plugin_abi_sys::{AbiRecord, LoadError as BoundaryLoadError, ValidationError};
use ferrumc_plugin_api::{
    Capability, CapabilityManifest, CommandSink, EventContext, EventKind, IntentError,
    PermissionApi, Plugin, PluginError, PluginEvent, PluginMetadata, SetupContext, Version,
    WorldIntent, WorldView,
};
use ferrumc_plugin_host::{
    DisableReason, HostError, InMemoryPluginStorage, NativeCallbackFailure, NativeEventContext,
    PluginHost, PluginState, PluginStats,
};
use ferrumc_plugin_loader::{
    LoadedPlugin, PluginCapabilities, PluginCapability, PluginLoadError, PluginLoader,
};
use sha2::{Digest, Sha256};

const INVALID_FIXTURE_ID: &str = "p52-abi-invalid-input";
const INVALID_FIXTURE_NAME: &str = "Packet 52 ABI Invalid Input";
const VALID_FIXTURE_ID: &str = "ferrumc-fixture-dynamic";
const VALID_FIXTURE_NAME: &str = "FerrumC Dynamic Fixture";
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_METADATA_BYTES: usize = 4 * 1024;
const EXCESS_DECLARED_LENGTH: u64 = 4_097;
const CONTINUATION_MESSAGE: &str = "compiled plugin continued";
const PANIC_DIAGNOSTIC: &str = "trusted native callback returned FC_PLUGIN_PANIC; staged commands were discarded and the plugin was disabled";

#[derive(Clone, Copy)]
enum InvalidInputCase {
    ShortFunctionTable,
    MissingEntrypoint,
    MissingFunctionsValue,
    MissingInitCallback,
    MissingEventCallback,
    MissingShutdownCallback,
    MissingMetadataBuffer,
    ExcessDeclaredLength,
}

impl InvalidInputCase {
    const fn feature(self) -> &'static str {
        match self {
            Self::ShortFunctionTable => "short-function-table",
            Self::MissingEntrypoint => "missing-entrypoint",
            Self::MissingFunctionsValue => "missing-functions-value",
            Self::MissingInitCallback => "missing-init-callback",
            Self::MissingEventCallback => "missing-event-callback",
            Self::MissingShutdownCallback => "missing-shutdown-callback",
            Self::MissingMetadataBuffer => "missing-metadata-buffer",
            Self::ExcessDeclaredLength => "excess-declared-length",
        }
    }

    const fn label(self) -> &'static str {
        self.feature()
    }
}

#[derive(Debug, Eq, PartialEq)]
struct HostSnapshot {
    registered: usize,
    baseline_state: Option<PluginState>,
    baseline_stats: Option<PluginStats>,
    baseline_subscribed: bool,
    decision_rows: Vec<(String, u64, u64, u64, u64)>,
}

#[derive(Default)]
struct RecordingSink {
    intents: Vec<WorldIntent>,
}

impl CommandSink for RecordingSink {
    fn submit(&mut self, intent: WorldIntent) -> Result<(), IntentError> {
        self.intents.push(intent);
        Ok(())
    }
}

struct EmptyWorld;

impl WorldView for EmptyWorld {
    fn dimension(&self) -> DimensionId {
        DimensionId::new(0)
    }

    fn is_chunk_loaded(&self, _chunk: ChunkPos) -> bool {
        false
    }

    fn block_state_id(&self, _pos: BlockPos) -> Option<u32> {
        None
    }

    fn player_position(&self, _player: PlayerId) -> Option<Vec3> {
        None
    }
}

struct DenyAllPermissions;

impl PermissionApi for DenyAllPermissions {
    fn has_permission(&self, _player: PlayerId, _node: &PermissionNode) -> bool {
        false
    }

    fn resolve(&self, _player: PlayerId, _node: &PermissionNode) -> Resolution {
        Resolution::Unset
    }
}

struct ContinuingPlugin;

impl Plugin for ContinuingPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(
            PluginId::new("p52-compiled-continuation"),
            "Packet 52 compiled continuation",
            Version::new(1, 0, 0),
            CapabilityManifest::empty()
                .with(Capability::ReceiveEvents)
                .with(Capability::SubmitIntents),
        )
    }

    fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
        ctx.events()?.subscribe(EventKind::PlayerJoin);
        Ok(())
    }

    fn on_event(&mut self, event: &PluginEvent, ctx: &mut EventContext<'_>) {
        let PluginEvent::PlayerJoin { player } = event else {
            return;
        };
        ctx.sink()
            .expect("compiled test plugin has submit-intents")
            .submit(WorldIntent::Message {
                player: *player,
                message: TextComponent::text(CONTINUATION_MESSAGE),
            })
            .expect("recording sink accepts the continuation intent");
    }
}

#[test]
fn wrongly_shaped_function_table_is_rejected_without_state_mutation() {
    let (mut host, baseline_id) = seeded_host();
    let before = host_snapshot(&host, &baseline_id);
    let (error, scratch) = load_invalid_input(&mut host, InvalidInputCase::ShortFunctionTable);

    assert!(matches!(
        error,
        PluginLoadError::ShortAbiRecord {
            id,
            record: AbiRecord::FunctionTable,
            declared: 8,
            required,
        } if id == INVALID_FIXTURE_ID
            && required == FcPluginFunctionsV1::STRUCT_SIZE
    ));
    assert_eq!(host_snapshot(&host, &baseline_id), before);
    cleanup_loaded_bundle(&scratch);
}

#[test]
fn missing_entrypoint_and_required_values_have_typed_outcomes() {
    let (mut host, baseline_id) = seeded_host();
    let before = host_snapshot(&host, &baseline_id);
    let (error, scratch) = load_invalid_input(&mut host, InvalidInputCase::MissingEntrypoint);
    match error {
        PluginLoadError::NativeBoundary { id, source, .. } => {
            assert_eq!(id, INVALID_FIXTURE_ID);
            assert!(matches!(
                source,
                BoundaryLoadError::MissingEntrypoint { .. }
            ));
        }
        other => panic!("expected a typed missing-entrypoint error, got {other}"),
    }
    assert_eq!(host_snapshot(&host, &baseline_id), before);
    cleanup_loaded_bundle(&scratch);

    let cases = [
        (
            InvalidInputCase::MissingFunctionsValue,
            AbiRecord::Descriptor,
            "functions result",
        ),
        (
            InvalidInputCase::MissingInitCallback,
            AbiRecord::FunctionTable,
            "init",
        ),
        (
            InvalidInputCase::MissingEventCallback,
            AbiRecord::FunctionTable,
            "on_event",
        ),
        (
            InvalidInputCase::MissingShutdownCallback,
            AbiRecord::FunctionTable,
            "shutdown",
        ),
    ];

    for (case, expected_record, expected_slot) in cases {
        let (mut host, baseline_id) = seeded_host();
        let before = host_snapshot(&host, &baseline_id);
        let (error, scratch) = load_invalid_input(&mut host, case);
        assert!(matches!(
            error,
            PluginLoadError::NullRequiredPointer {
                id,
                record,
                slot,
            } if id == INVALID_FIXTURE_ID
                && record == expected_record
                && slot == expected_slot
        ));
        assert_eq!(host_snapshot(&host, &baseline_id), before);
        cleanup_loaded_bundle(&scratch);
    }

    let (mut host, baseline_id) = seeded_host();
    let before = host_snapshot(&host, &baseline_id);
    let (error, scratch) = load_invalid_input(&mut host, InvalidInputCase::MissingMetadataBuffer);
    match error {
        PluginLoadError::NativeBoundary { id, source, .. } => {
            assert_eq!(id, INVALID_FIXTURE_ID);
            assert!(matches!(
                source.validation_error(),
                Some(ValidationError::NullMetadataPointer { field: "id" })
            ));
        }
        other => panic!("expected a typed missing-metadata error, got {other}"),
    }
    assert_eq!(host_snapshot(&host, &baseline_id), before);
    cleanup_loaded_bundle(&scratch);
}

#[test]
fn excess_declared_length_is_rejected_before_buffer_access() {
    let (mut host, baseline_id) = seeded_host();
    let before = host_snapshot(&host, &baseline_id);
    let (error, scratch) = load_invalid_input(&mut host, InvalidInputCase::ExcessDeclaredLength);

    match error {
        PluginLoadError::NativeBoundary { id, source, .. } => {
            assert_eq!(id, INVALID_FIXTURE_ID);
            assert!(matches!(
                source.validation_error(),
                Some(ValidationError::MetadataTooLong {
                    field: "id",
                    declared: EXCESS_DECLARED_LENGTH,
                    maximum: MAX_METADATA_BYTES,
                })
            ));
        }
        other => panic!("expected a typed excess-length error, got {other}"),
    }
    assert_eq!(host_snapshot(&host, &baseline_id), before);
    cleanup_loaded_bundle(&scratch);
}

#[test]
fn panic_status_discards_fixture_mutations_and_disables_it() {
    let (native, scratch) = load_panicking_fixture();
    let mut host = PluginHost::new(Box::new(InMemoryPluginStorage::new()));
    let native_id = host
        .register_trusted_native(native)
        .expect("register the trusted native plugin fixture");
    let continuation_id = host
        .register(Box::new(ContinuingPlugin))
        .expect("register the unrelated compiled plugin");
    host.enable(&native_id)
        .expect("enable the trusted native plugin fixture");
    host.enable(&continuation_id)
        .expect("enable the unrelated compiled plugin");

    let player = PlayerId::offline("P52InvalidInput");
    let event = PluginEvent::PlayerJoin { player };
    let world = EmptyWorld;
    let permissions = DenyAllPermissions;
    let mut sink = RecordingSink::default();
    let report = host.dispatch_event_with_native_context(
        &event,
        native_event_context(520),
        &world,
        &mut sink,
        &permissions,
    );

    assert_messages(&sink.intents, player, &[CONTINUATION_MESSAGE]);
    assert_eq!(report.delivered(), 1);
    assert_eq!(report.panicked(), std::slice::from_ref(&native_id));
    assert!(matches!(
        report.native_failures(),
        [failure]
            if failure.plugin_id() == &native_id
                && failure.hook() == EventKind::PlayerJoin
                && matches!(
                    failure.failure(),
                    NativeCallbackFailure::Status(status) if *status == FC_PLUGIN_PANIC
                )
    ));
    assert!(matches!(
        report.native_panics(),
        [record]
            if record.plugin_id() == &native_id
                && record.hook() == EventKind::PlayerJoin
                && record.diagnostic() == PANIC_DIAGNOSTIC
    ));
    assert_eq!(
        host.state(&native_id),
        Some(PluginState::Disabled(DisableReason::Panicked))
    );
    assert_eq!(host.stats(&native_id).map(PluginStats::panics), Some(1));
    assert!(!host.is_subscribed(&native_id, EventKind::PlayerJoin));
    assert_eq!(
        host.enable(&native_id),
        Err(HostError::NativePanicDisabled(native_id.clone()))
    );

    let prior = sink.intents.len();
    let later = host.dispatch_event_with_native_context(
        &event,
        native_event_context(521),
        &world,
        &mut sink,
        &permissions,
    );
    assert_eq!(later.delivered(), 1);
    assert!(later.panicked().is_empty());
    assert!(later.native_panics().is_empty());
    assert_messages(&sink.intents[prior..], player, &[CONTINUATION_MESSAGE]);
    assert_eq!(host.stats(&native_id).map(PluginStats::panics), Some(1));

    drop(host);
    cleanup_loaded_bundle(&scratch);
}

fn load_invalid_input(host: &mut PluginHost, case: InvalidInputCase) -> (PluginLoadError, PathBuf) {
    let scratch = scratch_root().join(case.label());
    remove_if_present(&scratch);
    let target = scratch.join("target");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/abi-invalid-input");
    let output = Command::new(env!("CARGO"))
        .current_dir(&fixture)
        .env("CARGO_TARGET_DIR", &target)
        .args([
            "build",
            "--locked",
            "--jobs=1",
            "--lib",
            "--no-default-features",
            "--features",
            case.feature(),
            "--message-format=json-render-diagnostics",
        ])
        .output()
        .expect("spawn invalid-input fixture build");
    assert!(
        output.status.success(),
        "invalid-input fixture build failed for {}:\n{}",
        case.label(),
        String::from_utf8_lossy(&output.stderr)
    );
    let library = cargo_library(
        &output.stdout,
        "ferrumc_testkit_abi_invalid_input",
        "invalid-input fixture",
    );
    let plugins_root = scratch.join("plugins");
    package_invalid_bundle(&library, &plugins_root);

    let plugin_loader =
        PluginLoader::current(PluginCapabilities::empty()).expect("construct current loader");
    let error = match plugin_loader.load_directory(&plugins_root) {
        Err(error) => error,
        Ok(loaded_plugins) => {
            let mut registered = 0_usize;
            for plugin in loaded_plugins.into_plugins() {
                host.register_trusted_native(plugin)
                    .expect("an unexpectedly accepted fixture has a distinct plugin id");
                registered = registered.saturating_add(1);
            }
            panic!(
                "invalid-input fixture `{}` reached host registration ({registered} plugins)",
                case.label()
            );
        }
    };
    (error, scratch)
}

fn load_panicking_fixture() -> (LoadedPlugin, PathBuf) {
    let server = server_root();
    let scratch = scratch_root().join("panic-status");
    remove_if_present(&scratch);
    let target = scratch.join("target");
    let output = Command::new(env!("CARGO"))
        .current_dir(&server)
        .env("CARGO_TARGET_DIR", &target)
        .args([
            "build",
            "--locked",
            "--jobs=1",
            "-p",
            "ferrumc-plugin-fixture-dynamic",
            "--lib",
            "--message-format=json-render-diagnostics",
        ])
        .output()
        .expect("spawn valid fixture build");
    assert!(
        output.status.success(),
        "valid fixture build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let library = cargo_library(
        &output.stdout,
        "ferrumc_plugin_fixture_dynamic",
        "valid fixture",
    );
    let plugins_root = scratch.join("plugins");
    package_valid_bundle(&library, &plugins_root);

    let available = PluginCapabilities::empty()
        .with(PluginCapability::ReceiveEvents)
        .with(PluginCapability::SubmitIntents);
    let plugin_loader = PluginLoader::current(available).expect("construct current loader");
    let loaded_plugins = plugin_loader
        .load_directory(&plugins_root)
        .expect("load valid fixture through the real loader");
    let mut plugins = loaded_plugins.into_plugins().into_iter();
    let fixture = plugins.next().expect("one valid fixture is loaded");
    assert!(plugins.next().is_none(), "only the valid fixture is loaded");
    (fixture, scratch)
}

fn package_invalid_bundle(library: &Path, plugins_root: &Path) {
    package_fixture_bundle(
        library,
        plugins_root,
        INVALID_FIXTURE_ID,
        INVALID_FIXTURE_NAME,
        "[]",
    );
}

fn package_valid_bundle(library: &Path, plugins_root: &Path) {
    package_fixture_bundle(
        library,
        plugins_root,
        VALID_FIXTURE_ID,
        VALID_FIXTURE_NAME,
        "[\"receive-events\", \"submit-intents\"]",
    );
}

fn package_fixture_bundle(
    library: &Path,
    plugins_root: &Path,
    id: &str,
    name: &str,
    capabilities: &str,
) {
    let filename = library
        .file_name()
        .and_then(|name| name.to_str())
        .expect("fixture artifact has a UTF-8 filename");
    assert!(
        !filename.is_empty()
            && filename
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "fixture artifact filename is safe for the TOML manifest"
    );

    let bundle = plugins_root.join(id);
    fs::create_dir_all(&bundle).expect("create fixture bundle");
    let copied = bundle.join(filename);
    fs::copy(library, &copied).expect("copy fixture library");
    let digest = hex_digest(sha256_file(&copied));
    let manifest = format!(
        "id = \"{id}\"\n\
         name = \"{name}\"\n\
         version = \"1.0.0\"\n\
         abi_major = 1\n\
         abi_minor = 0\n\
         server_api = \"={}\"\n\
         library = \"{filename}\"\n\
         library_sha256 = \"{digest}\"\n\
         capabilities = {capabilities}\n",
        env!("CARGO_PKG_VERSION")
    );
    let temporary = bundle.join(".plugin.toml.tmp");
    fs::write(&temporary, manifest).expect("write fixture manifest");
    fs::rename(temporary, bundle.join("plugin.toml")).expect("publish fixture manifest");
}

fn cargo_library(cargo_stdout: &[u8], expected_target: &str, description: &str) -> PathBuf {
    for line in cargo_stdout.split(|byte| *byte == b'\n') {
        let Ok(message) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if message.get("reason").and_then(serde_json::Value::as_str) != Some("compiler-artifact")
            || message
                .pointer("/target/name")
                .and_then(serde_json::Value::as_str)
                != Some(expected_target)
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
    panic!("Cargo did not report the {description} cdylib artifact");
}

fn sha256_file(path: &Path) -> [u8; 32] {
    let mut file = File::open(path).expect("open copied fixture for hashing");
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer).expect("hash copied fixture");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher.finalize().into()
}

fn hex_digest(digest: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn seeded_host() -> (PluginHost, PluginId) {
    let mut host = PluginHost::in_memory();
    let baseline_id = host
        .register(Box::new(ContinuingPlugin))
        .expect("register the baseline compiled plugin");
    host.enable(&baseline_id)
        .expect("enable the baseline compiled plugin");
    (host, baseline_id)
}

fn host_snapshot(host: &PluginHost, baseline_id: &PluginId) -> HostSnapshot {
    HostSnapshot {
        registered: host.len(),
        baseline_state: host.state(baseline_id),
        baseline_stats: host.stats(baseline_id),
        baseline_subscribed: host.is_subscribed(baseline_id, EventKind::PlayerJoin),
        decision_rows: host
            .plugin_decision_reports()
            .into_iter()
            .map(|row| (row.name, row.allow, row.deny, row.replace, row.panic))
            .collect(),
    }
}

fn native_event_context(tick: u64) -> NativeEventContext {
    NativeEventContext::new(
        Tick::new(tick),
        WorldId::new(0),
        DimensionId::new(0),
        ShardPos::new(0, -1),
    )
}

fn assert_messages(intents: &[WorldIntent], player: PlayerId, expected: &[&str]) {
    assert_eq!(intents.len(), expected.len());
    for (intent, expected_text) in intents.iter().zip(expected) {
        let WorldIntent::Message {
            player: recipient,
            message,
        } = intent
        else {
            panic!("expected a message intent, got {intent:?}");
        };
        assert_eq!(*recipient, player);
        assert_eq!(message.content(), *expected_text);
    }
}

fn server_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("testkit crate is nested under server/crates")
        .to_path_buf()
}

fn repository_root() -> PathBuf {
    server_root()
        .parent()
        .expect("server directory is nested under the repository")
        .to_path_buf()
}

fn scratch_root() -> PathBuf {
    repository_root()
        .join(".codex-tmp/p52-abi-invalid-input")
        .join(std::process::id().to_string())
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
fn cleanup_loaded_bundle(path: &Path) {
    remove_if_present(&path.join("target"));
    let plugins = path.join("plugins");
    for entry in fs::read_dir(&plugins).expect("read Windows fixture bundles during cleanup") {
        let bundle = entry.expect("read Windows fixture bundle entry").path();
        fs::remove_file(bundle.join("plugin.toml"))
            .expect("remove the unmapped Windows fixture manifest");
    }
    // The loader keeps the copied DLL resident until process exit, so only
    // that mapped file and its parent directories remain in local scratch.
}
