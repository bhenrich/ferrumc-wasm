#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use ferrumc_core::{DimensionId, PlayerId, PluginId, TextComponent, Tick, WorldId};
use ferrumc_math::{BlockPos, ChunkPos, ShardPos, Vec3};
use ferrumc_permission::{PermissionNode, Resolution};
use ferrumc_plugin_abi::FC_CAPABILITY_DENIED;
use ferrumc_plugin_api::{
    Capability, CapabilityManifest, CommandSink, EventContext, EventKind, IntentError,
    PermissionApi, Plugin, PluginError, PluginEvent, PluginMetadata, SetupContext, Version,
    WorldIntent, WorldView,
};
use ferrumc_plugin_host::{
    DisableReason, HostConfig, HostError, InMemoryPluginStorage, NativeCallbackFailure,
    NativeEventContext, PluginHost, PluginState, PluginStats,
};
use ferrumc_plugin_loader::{
    LoadedPlugin, PluginCapabilities, PluginCapability, PluginLoader as TrustedNativeLoader,
};

#[path = "../../../plugins/ferrumc-plugin-fixture-dynamic/tests/support/mod.rs"]
mod fixture_support;

use fixture_support::package_bundle;

const DYNAMIC_MESSAGE: &str = "dynamic fixture handled event";

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

struct PanickingWorld;

impl WorldView for PanickingWorld {
    fn dimension(&self) -> DimensionId {
        panic!("an undeclared native world request must be denied before facade access");
    }

    fn is_chunk_loaded(&self, _chunk: ChunkPos) -> bool {
        panic!("an undeclared native world request must be denied before facade access");
    }

    fn block_state_id(&self, _pos: BlockPos) -> Option<u32> {
        panic!("an undeclared native world request must be denied before facade access");
    }

    fn player_position(&self, _player: PlayerId) -> Option<Vec3> {
        panic!("an undeclared native world request must be denied before facade access");
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

struct BoundedSink {
    capacity: usize,
    intents: Vec<WorldIntent>,
    rejected: usize,
}

impl BoundedSink {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            intents: Vec::with_capacity(capacity),
            rejected: 0,
        }
    }
}

impl CommandSink for BoundedSink {
    fn submit(&mut self, intent: WorldIntent) -> Result<(), IntentError> {
        if self.intents.len() >= self.capacity {
            self.rejected += 1;
            return Err(IntentError::QueueFull);
        }
        self.intents.push(intent);
        Ok(())
    }
}

struct MessagePlugin {
    id: &'static str,
    message: &'static str,
}

impl Plugin for MessagePlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(
            PluginId::new(self.id),
            self.id,
            Version::new(1, 0, 0),
            CapabilityManifest::empty()
                .with(Capability::ReceiveEvents)
                .with(Capability::SubmitIntents),
        )
    }

    fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
        ctx.events()?.subscribe(EventKind::BlockBreak);
        ctx.events()?.subscribe(EventKind::PlayerJoin);
        Ok(())
    }

    fn on_event(&mut self, event: &PluginEvent, ctx: &mut EventContext<'_>) {
        let player = match event {
            PluginEvent::BlockBreak { player, .. } | PluginEvent::PlayerJoin { player } => *player,
            _ => return,
        };
        let _ = ctx
            .sink()
            .expect("compiled fixture has submit-intents")
            .submit(WorldIntent::Message {
                player,
                message: TextComponent::text(self.message),
            });
    }
}

#[test]
fn compiled_and_trusted_native_hooks_have_one_deterministic_order() {
    let (native, scratch) = load_fixture("deterministic-order");
    let mut host = PluginHost::with_config(
        Box::new(InMemoryPluginStorage::new()),
        HostConfig::new().with_max_plugins(3),
    );
    let first = host
        .register(Box::new(MessagePlugin {
            id: "z-compiled-first",
            message: "compiled first",
        }))
        .expect("register first compiled plugin");
    let native_id = host
        .register_trusted_native(native)
        .expect("register trusted native fixture");
    let duplicate = host
        .register(Box::new(MessagePlugin {
            id: "ferrumc-fixture-dynamic",
            message: "duplicate",
        }))
        .expect_err("compiled/native ids share one duplicate namespace");
    assert_eq!(duplicate, HostError::DuplicateId(native_id.clone()));
    let second = host
        .register(Box::new(MessagePlugin {
            id: "a-compiled-second",
            message: "compiled second",
        }))
        .expect("register second compiled plugin");
    assert_eq!(
        host.register(Box::new(MessagePlugin {
            id: "over-capacity",
            message: "must not register",
        })),
        Err(HostError::CapacityExceeded { max: 3 }),
        "the registry capacity spans both plugin representations"
    );
    assert_eq!(
        host.plugin_decision_reports()
            .iter()
            .map(|row| row.name.as_str())
            .collect::<Vec<_>>(),
        [
            "z-compiled-first",
            "FerrumC Dynamic Fixture",
            "a-compiled-second",
        ]
    );

    // Enable order intentionally differs from the one global registration
    // order shared by compiled-in and trusted native hooks.
    host.enable(&second).expect("enable second compiled plugin");
    host.enable(&native_id).expect("enable native fixture");
    host.enable(&first).expect("enable first compiled plugin");

    let player = PlayerId::offline("P48Order");
    let event = block_break(player);
    let world = EmptyWorld;
    let permissions = DenyAllPermissions;
    let mut sink = RecordingSink::default();

    for dispatch in 0..2 {
        let prior = sink.intents.len();
        let report = host.dispatch_event_with_native_context(
            &event,
            native_event_context(100 + dispatch),
            &world,
            &mut sink,
            &permissions,
        );
        assert_eq!(report.delivered(), 3, "dispatch {dispatch}");
        assert!(report.panicked().is_empty(), "dispatch {dispatch}");
        assert!(
            report.native_capability_denials().is_empty(),
            "dispatch {dispatch}"
        );
        assert!(report.native_failures().is_empty(), "dispatch {dispatch}");
        assert_messages(
            &sink.intents[prior..],
            player,
            &["compiled first", DYNAMIC_MESSAGE, "compiled second"],
        );
    }

    drop(host);
    cleanup_loaded_bundle(&scratch);
}

#[test]
fn trusted_native_commit_uses_the_existing_bounded_sink() {
    let (native, scratch) = load_fixture("bounded-sink");
    let mut host = PluginHost::in_memory();
    let compiled_id = host
        .register(Box::new(MessagePlugin {
            id: "compiled-fills-sink",
            message: "compiled accepted",
        }))
        .expect("register compiled plugin");
    let native_id = host
        .register_trusted_native(native)
        .expect("register trusted native fixture");
    host.enable(&compiled_id).expect("enable compiled plugin");
    host.enable(&native_id).expect("enable native fixture");

    let player = PlayerId::offline("P48Bounded");
    let world = EmptyWorld;
    let permissions = DenyAllPermissions;
    let mut sink = BoundedSink::new(1);
    let report = host.dispatch_event_with_native_context(
        &block_break(player),
        native_event_context(200),
        &world,
        &mut sink,
        &permissions,
    );

    assert_messages(&sink.intents, player, &["compiled accepted"]);
    assert_eq!(sink.rejected, 1);
    assert!(report.native_capability_denials().is_empty());
    let failures = report.native_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].plugin_id(), &native_id);
    assert_eq!(failures[0].hook(), EventKind::BlockBreak);
    assert!(matches!(
        failures[0].failure(),
        NativeCallbackFailure::CommandSink(IntentError::QueueFull)
    ));

    drop(host);
    cleanup_loaded_bundle(&scratch);
}

#[test]
fn undeclared_native_capability_is_typed_and_rolls_back_staged_intents() {
    let (native, scratch) = load_fixture("capability-denial");
    let mut host = PluginHost::in_memory();
    let native_id = host
        .register_trusted_native(native)
        .expect("register trusted native fixture");
    host.enable(&native_id).expect("enable native fixture");

    let player = PlayerId::offline("P48Denied");
    let permissions = DenyAllPermissions;
    let mut denied_sink = RecordingSink::default();
    let denied = host.dispatch_event_with_native_context(
        &PluginEvent::AfterBlockBreak {
            player,
            pos: BlockPos::new(10, 64, -3),
        },
        native_event_context(300),
        &PanickingWorld,
        &mut denied_sink,
        &permissions,
    );

    assert!(
        denied_sink.intents.is_empty(),
        "the message staged before the denied request must be rolled back"
    );
    let denials = denied.native_capability_denials();
    assert_eq!(denials.len(), 1);
    assert_eq!(denials[0].plugin_id(), &native_id);
    assert_eq!(denials[0].capability(), Capability::ReadWorld);
    let failures = denied.native_failures();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].plugin_id(), &native_id);
    assert_eq!(failures[0].hook(), EventKind::AfterBlockBreak);
    assert!(matches!(
        failures[0].failure(),
        NativeCallbackFailure::Status(status) if *status == FC_CAPABILITY_DENIED
    ));
    assert!(denied.panicked().is_empty());
    assert!(denied.native_panics().is_empty());
    assert!(host.is_enabled(&native_id));
    assert_eq!(host.stats(&native_id).map(PluginStats::panics), Some(0));

    let world = EmptyWorld;
    let mut success_sink = RecordingSink::default();
    let success = host.dispatch_event_with_native_context(
        &block_break(player),
        native_event_context(301),
        &world,
        &mut success_sink,
        &permissions,
    );
    assert_eq!(success.delivered(), 1);
    assert!(success.native_capability_denials().is_empty());
    assert!(success.native_failures().is_empty());
    assert_messages(&success_sink.intents, player, &[DYNAMIC_MESSAGE]);

    drop(host);
    cleanup_loaded_bundle(&scratch);
}

#[test]
fn trusted_native_panic_status_discards_disables_and_records_context() {
    let (native, scratch) = load_fixture("panic-status");
    let mut host = PluginHost::in_memory();
    let native_id = host
        .register_trusted_native(native)
        .expect("register trusted native fixture");
    let compiled_id = host
        .register(Box::new(MessagePlugin {
            id: "compiled-after-native-panic",
            message: "compiled continued",
        }))
        .expect("register unrelated compiled plugin");
    host.enable(&native_id).expect("enable native fixture");
    host.enable(&compiled_id)
        .expect("enable unrelated compiled plugin");

    let player = PlayerId::offline("P49Panic");
    let event = PluginEvent::PlayerJoin { player };
    let world = EmptyWorld;
    let permissions = DenyAllPermissions;
    let mut sink = RecordingSink::default();
    let report = host.dispatch_event_with_native_context(
        &event,
        native_event_context(350),
        &world,
        &mut sink,
        &permissions,
    );

    assert_messages(&sink.intents, player, &["compiled continued"]);
    assert_eq!(report.delivered(), 1);
    assert_eq!(report.panicked(), std::slice::from_ref(&native_id));
    assert!(matches!(
        report.native_failures(),
        [failure]
            if failure.plugin_id() == &native_id
                && failure.hook() == EventKind::PlayerJoin
                && matches!(
                    failure.failure(),
                    NativeCallbackFailure::Status(status)
                        if *status == ferrumc_plugin_abi::FC_PLUGIN_PANIC
                )
    ));
    let panic_records = report.native_panics();
    assert!(matches!(
        panic_records,
        [record]
            if record.plugin_id() == &native_id
                && record.hook() == EventKind::PlayerJoin
                && record.diagnostic()
                    == "trusted native callback returned FC_PLUGIN_PANIC; staged commands were discarded and the plugin was disabled"
    ));
    assert_eq!(
        host.state(&native_id),
        Some(PluginState::Disabled(DisableReason::Panicked))
    );
    assert_eq!(host.stats(&native_id).map(PluginStats::panics), Some(1));
    assert!(!host.is_subscribed(&native_id, EventKind::PlayerJoin));
    assert_eq!(
        host.disable(&native_id),
        Err(HostError::NotEnabled(native_id.clone())),
        "the failed instance was already retired without another callback"
    );
    assert_eq!(
        host.enable(&native_id),
        Err(HostError::NativePanicDisabled(native_id.clone())),
        "this registration cannot allocate another instance after panic retirement"
    );

    let prior = sink.intents.len();
    let later = host.dispatch_event_with_native_context(
        &event,
        native_event_context(351),
        &world,
        &mut sink,
        &permissions,
    );
    assert_eq!(later.delivered(), 1);
    assert!(later.panicked().is_empty());
    assert!(later.native_panics().is_empty());
    assert_messages(&sink.intents[prior..], player, &["compiled continued"]);
    assert_eq!(host.stats(&native_id).map(PluginStats::panics), Some(1));

    drop(host);
    cleanup_loaded_bundle(&scratch);
}

#[test]
fn trusted_native_disable_and_reenable_creates_a_working_fresh_instance() {
    let (native, scratch) = load_fixture("disable-reenable");
    let mut host = PluginHost::in_memory();
    let native_id = host
        .register_trusted_native(native)
        .expect("register trusted native fixture");
    let player = PlayerId::offline("P48Lifecycle");
    let event = block_break(player);
    let world = EmptyWorld;
    let permissions = DenyAllPermissions;

    host.enable(&native_id).expect("first enable");
    assert_eq!(host.state(&native_id), Some(PluginState::Enabled));
    assert!(host.is_subscribed(&native_id, EventKind::BlockBreak));

    let mut missing_context_sink = RecordingSink::default();
    let missing_context =
        host.dispatch_event(&event, &world, &mut missing_context_sink, &permissions);
    assert_eq!(missing_context.delivered(), 0);
    assert!(missing_context_sink.intents.is_empty());
    assert!(matches!(
        missing_context.native_failures(),
        [failure]
            if failure.plugin_id() == &native_id
                && matches!(
                    failure.failure(),
                    NativeCallbackFailure::EventContextUnavailable
                )
    ));

    let mut first_sink = RecordingSink::default();
    let first = host.dispatch_event_with_native_context(
        &event,
        native_event_context(400),
        &world,
        &mut first_sink,
        &permissions,
    );
    assert_eq!(first.delivered(), 1);
    assert_messages(&first_sink.intents, player, &[DYNAMIC_MESSAGE]);

    host.disable(&native_id).expect("first disable");
    assert_eq!(
        host.state(&native_id),
        Some(PluginState::Disabled(DisableReason::Manual))
    );
    assert!(!host.is_subscribed(&native_id, EventKind::BlockBreak));
    let mut disabled_sink = RecordingSink::default();
    let disabled = host.dispatch_event(&event, &world, &mut disabled_sink, &permissions);
    assert_eq!(disabled.delivered(), 0);
    assert!(disabled_sink.intents.is_empty());

    host.enable(&native_id)
        .expect("re-enable from retained factory");
    assert_eq!(host.state(&native_id), Some(PluginState::Enabled));
    assert!(host.is_subscribed(&native_id, EventKind::BlockBreak));
    let mut second_sink = RecordingSink::default();
    let second = host.dispatch_event_with_native_context(
        &event,
        native_event_context(401),
        &world,
        &mut second_sink,
        &permissions,
    );
    assert_eq!(second.delivered(), 1);
    assert!(second.native_failures().is_empty());
    assert_messages(&second_sink.intents, player, &[DYNAMIC_MESSAGE]);

    host.disable(&native_id).expect("second disable");
    assert_eq!(
        host.state(&native_id),
        Some(PluginState::Disabled(DisableReason::Manual))
    );

    drop(host);
    cleanup_loaded_bundle(&scratch);
}

fn block_break(player: PlayerId) -> PluginEvent {
    PluginEvent::BlockBreak {
        player,
        pos: BlockPos::new(10, 64, -3),
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

fn load_fixture(case: &str) -> (LoadedPlugin, PathBuf) {
    let server_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("plugin-host crate is nested under server/crates");
    let repo_root = server_root
        .parent()
        .expect("server directory is nested under the repository");
    let target_dir = repo_root.join(".codex-tmp/p48-fixture-target");
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
        .expect("spawn nested dynamic-fixture build");
    assert!(
        output.status.success(),
        "dynamic fixture build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let library = fixture_library(&output.stdout);
    assert!(
        library.is_file(),
        "dynamic fixture library missing at {}",
        library.display()
    );
    let scratch = repo_root
        .join(".codex-tmp/p48-trusted-native-runtime")
        .join(std::process::id().to_string())
        .join(case);
    remove_if_present(&scratch);
    let plugins_root = scratch.join("plugins");
    package_bundle(&library, &plugins_root);

    let available = PluginCapabilities::empty()
        .with(PluginCapability::ReceiveEvents)
        .with(PluginCapability::SubmitIntents);
    let loader = TrustedNativeLoader::current(available).expect("construct current native loader");
    let native_set = loader
        .load_directory(&plugins_root)
        .expect("load real dynamic fixture bundle");
    let mut plugins = native_set.into_plugins().into_iter();
    let fixture = plugins.next().expect("one dynamic fixture is loaded");
    assert!(
        plugins.next().is_none(),
        "only one fixture plugin is loaded"
    );
    (fixture, scratch)
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
    // The loader keeps native libraries resident until process exit, and a
    // mapped Windows DLL cannot be deleted. The process-scoped bundle is
    // intentionally left under repo-local scratch for later manual cleanup.
}
