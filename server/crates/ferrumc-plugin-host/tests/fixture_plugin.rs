//! Integration tests exercising the host through in-process fixture plugins.
//!
//! These cover the milestone's required scenarios: registering and enabling a
//! plugin, event dispatch reaching it, panic containment, command registration,
//! capability gating, and storage namespace separation.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use ferrumc_command::{literal, CommandResult, CommandSource};
use ferrumc_core::{DimensionId, PlayerId, PluginId, TextComponent};
use ferrumc_math::{BlockPos, ChunkPos, Vec3};
use ferrumc_permission::{PermissionNode, Resolution};
use ferrumc_plugin_api::{
    Capability, CapabilityManifest, CommandSink, EventContext, EventKind, IntentError,
    PermissionApi, Plugin, PluginError, PluginEvent, PluginMetadata, SetupContext, Version,
    WorldIntent, WorldView,
};
use ferrumc_plugin_host::{
    DisableReason, HostConfig, HostError, InMemoryPluginStorage, PluginHost, PluginState,
    PluginStats, PluginStorageBackend,
};

// --- Fake host-injected facades -------------------------------------------

/// A world view that reports nothing loaded; enough to satisfy dispatch.
struct FakeWorld;

impl WorldView for FakeWorld {
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

/// A sink that records every submitted intent.
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

/// A permission facade that allows everything.
struct AllowAll;

impl PermissionApi for AllowAll {
    fn has_permission(&self, _player: PlayerId, _node: &PermissionNode) -> bool {
        true
    }
    fn resolve(&self, _player: PlayerId, _node: &PermissionNode) -> Resolution {
        Resolution::Allowed
    }
}

fn join_event() -> PluginEvent {
    PluginEvent::PlayerJoin {
        player: PlayerId::offline("Steve"),
    }
}

fn dispatch_join(host: &mut PluginHost) -> ferrumc_plugin_host::DispatchReport {
    let world = FakeWorld;
    let mut sink = RecordingSink::default();
    let perms = AllowAll;
    host.dispatch_event(&join_event(), &world, &mut sink, &perms)
}

// --- Fixture plugins -------------------------------------------------------

/// Counts player joins, registers a `ping` command, and records joins in
/// storage. Requests every capability.
struct CounterPlugin {
    id: &'static str,
    joins: Arc<AtomicUsize>,
}

impl Plugin for CounterPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(
            PluginId::new(self.id),
            "Counter",
            Version::new(1, 0, 0),
            CapabilityManifest::all(),
        )
    }

    fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
        ctx.events()?
            .subscribe(EventKind::PlayerJoin)
            .subscribe(EventKind::BlockBreak);
        ctx.commands()?.register(
            literal("ping").executes(|_| CommandResult::success(TextComponent::text("pong"))),
        );
        Ok(())
    }

    fn on_event(&mut self, event: &PluginEvent, ctx: &mut EventContext<'_>) {
        if let PluginEvent::PlayerJoin { player } = event {
            self.joins.fetch_add(1, Ordering::SeqCst);
            if let Ok(storage) = ctx.storage() {
                let _ = storage.put("last-join", player.to_string().as_bytes());
            }
        }
    }
}

/// Panics on every event it receives.
struct PanicPlugin {
    id: &'static str,
}

impl Plugin for PanicPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(
            PluginId::new(self.id),
            "Panic",
            Version::new(0, 1, 0),
            CapabilityManifest::empty().with(Capability::ReceiveEvents),
        )
    }

    fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
        ctx.events()?.subscribe(EventKind::PlayerJoin);
        Ok(())
    }

    fn on_event(&mut self, _event: &PluginEvent, _ctx: &mut EventContext<'_>) {
        panic!("fixture plugin panic");
    }
}

/// Subscribes during setup, then on each event records whether it was allowed to
/// reach its storage facade. Requests all capabilities; the host restricts it.
struct ProbePlugin {
    id: &'static str,
    storage_denied: Arc<AtomicBool>,
}

impl Plugin for ProbePlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(
            PluginId::new(self.id),
            "Probe",
            Version::new(0, 1, 0),
            CapabilityManifest::all(),
        )
    }

    fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
        ctx.events()?.subscribe(EventKind::PlayerJoin);
        Ok(())
    }

    fn on_event(&mut self, _event: &PluginEvent, ctx: &mut EventContext<'_>) {
        self.storage_denied
            .store(ctx.storage().is_err(), Ordering::SeqCst);
    }
}

/// Writes a fixed value to a shared key on join. Requests events + storage.
struct WriterPlugin {
    id: &'static str,
    value: &'static [u8],
}

impl Plugin for WriterPlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new(
            PluginId::new(self.id),
            "Writer",
            Version::new(0, 1, 0),
            CapabilityManifest::empty()
                .with(Capability::ReceiveEvents)
                .with(Capability::Storage),
        )
    }

    fn on_enable(&mut self, ctx: &mut SetupContext<'_>) -> Result<(), PluginError> {
        ctx.events()?.subscribe(EventKind::PlayerJoin);
        Ok(())
    }

    fn on_event(&mut self, event: &PluginEvent, ctx: &mut EventContext<'_>) {
        if matches!(event, PluginEvent::PlayerJoin { .. }) {
            if let Ok(storage) = ctx.storage() {
                let _ = storage.put("shared", self.value);
            }
        }
    }
}

// --- Tests -----------------------------------------------------------------

#[test]
fn register_enable_and_event_dispatch_reaches_plugin() {
    let joins = Arc::new(AtomicUsize::new(0));
    let mut host = PluginHost::in_memory();

    let id = host
        .register(Box::new(CounterPlugin {
            id: "counter",
            joins: Arc::clone(&joins),
        }))
        .expect("registers");
    assert_eq!(host.state(&id), Some(PluginState::Registered));

    host.enable(&id).expect("enables");
    assert!(host.is_enabled(&id));
    assert!(host.is_subscribed(&id, EventKind::PlayerJoin));

    let report = dispatch_join(&mut host);
    assert_eq!(report.delivered(), 1);
    assert!(report.panicked().is_empty());
    assert_eq!(joins.load(Ordering::SeqCst), 1, "event reached the plugin");
}

#[test]
fn command_registration_feeds_the_host_tree() {
    let mut host = PluginHost::in_memory();
    let id = host
        .register(Box::new(CounterPlugin {
            id: "counter",
            joins: Arc::new(AtomicUsize::new(0)),
        }))
        .expect("registers");
    host.enable(&id).expect("enables");

    let source = CommandSource::console(4);
    let result = host
        .command_tree()
        .dispatch("ping", &source)
        .expect("ping is registered");
    assert!(result.is_success());
    assert_eq!(result.feedback().to_plain_string(), "pong");
}

#[test]
fn panicking_plugin_is_caught_and_disabled_host_survives() {
    let joins = Arc::new(AtomicUsize::new(0));
    let mut host = PluginHost::in_memory();

    let panic_id = host
        .register(Box::new(PanicPlugin { id: "panic" }))
        .expect("registers panic plugin");
    let counter_id = host
        .register(Box::new(CounterPlugin {
            id: "counter",
            joins: Arc::clone(&joins),
        }))
        .expect("registers counter plugin");
    host.enable(&panic_id).expect("enables panic plugin");
    host.enable(&counter_id).expect("enables counter plugin");

    // First dispatch: the panic plugin blows up but is contained; the counter
    // plugin still receives the event and the host keeps running.
    let report = dispatch_join(&mut host);
    assert_eq!(report.panicked(), std::slice::from_ref(&panic_id));
    assert_eq!(report.delivered(), 1, "the surviving plugin still ran");
    assert_eq!(joins.load(Ordering::SeqCst), 1);

    // The panicked plugin is disabled with a classifying reason.
    assert_eq!(
        host.state(&panic_id),
        Some(PluginState::Disabled(DisableReason::Panicked))
    );
    assert!(!host.is_enabled(&panic_id));
    assert_eq!(host.stats(&panic_id).map(PluginStats::panics), Some(1));

    // Second dispatch: the disabled plugin is skipped entirely; the host is fine.
    let report = dispatch_join(&mut host);
    assert!(report.panicked().is_empty());
    assert_eq!(report.delivered(), 1);
    assert_eq!(joins.load(Ordering::SeqCst), 2);
}

#[test]
fn capability_gating_denies_setup_without_receive_events() {
    let mut host = PluginHost::in_memory();
    // The probe wants to subscribe in on_enable, but we grant nothing.
    let id = host
        .register_with_grants(
            Box::new(ProbePlugin {
                id: "probe",
                storage_denied: Arc::new(AtomicBool::new(false)),
            }),
            CapabilityManifest::empty(),
        )
        .expect("registers");

    let err = host.enable(&id).expect_err("setup must be denied");
    match err {
        HostError::PluginFailed { id: failed, source } => {
            assert_eq!(failed, id);
            assert_eq!(
                source,
                PluginError::Capability(ferrumc_plugin_api::CapabilityError::missing(
                    Capability::ReceiveEvents
                ))
            );
        }
        other => panic!("expected PluginFailed, got {other:?}"),
    }
    assert_eq!(
        host.state(&id),
        Some(PluginState::Disabled(DisableReason::EnableFailed))
    );
}

#[test]
fn capability_gating_controls_event_facade_access() {
    // Granted ReceiveEvents only: storage access during the event is denied.
    let denied_flag = Arc::new(AtomicBool::new(false));
    let mut host = PluginHost::in_memory();
    let denied_id = host
        .register_with_grants(
            Box::new(ProbePlugin {
                id: "denied",
                storage_denied: Arc::clone(&denied_flag),
            }),
            CapabilityManifest::empty().with(Capability::ReceiveEvents),
        )
        .expect("registers denied probe");
    host.enable(&denied_id).expect("enables");

    // Granted ReceiveEvents + Storage: storage access is allowed.
    let allowed_flag = Arc::new(AtomicBool::new(true));
    let allowed_id = host
        .register_with_grants(
            Box::new(ProbePlugin {
                id: "allowed",
                storage_denied: Arc::clone(&allowed_flag),
            }),
            CapabilityManifest::empty()
                .with(Capability::ReceiveEvents)
                .with(Capability::Storage),
        )
        .expect("registers allowed probe");
    host.enable(&allowed_id).expect("enables");

    dispatch_join(&mut host);

    assert!(
        denied_flag.load(Ordering::SeqCst),
        "storage must be denied without the Storage capability"
    );
    assert!(
        !allowed_flag.load(Ordering::SeqCst),
        "storage must be allowed with the Storage capability"
    );
}

#[test]
fn storage_namespaces_are_separate_per_plugin() {
    let store = InMemoryPluginStorage::new();
    let mut host = PluginHost::new(Box::new(store.clone()));

    let w1 = host
        .register(Box::new(WriterPlugin {
            id: "writer-1",
            value: b"one",
        }))
        .expect("registers writer 1");
    let w2 = host
        .register(Box::new(WriterPlugin {
            id: "writer-2",
            value: b"two",
        }))
        .expect("registers writer 2");
    host.enable(&w1).expect("enables writer 1");
    host.enable(&w2).expect("enables writer 2");

    dispatch_join(&mut host);

    // Both wrote the same key, but each landed in its own namespace.
    assert_eq!(
        store.get(&w1, "shared").expect("get w1").as_deref(),
        Some(&b"one"[..])
    );
    assert_eq!(
        store.get(&w2, "shared").expect("get w2").as_deref(),
        Some(&b"two"[..])
    );
}

#[test]
fn budget_overrun_is_recorded_and_can_disable() {
    // A zero budget means any nonzero call duration is an overrun. The fixture
    // does real work (an atomic increment), so its call takes a nonzero time.
    let config = HostConfig::new()
        .with_call_budget(ferrumc_plugin_host::CallBudget::new(
            std::time::Duration::ZERO,
        ))
        .with_disable_on_overrun(true);
    let mut host = PluginHost::with_config(Box::new(InMemoryPluginStorage::new()), config);

    let id = host
        .register(Box::new(CounterPlugin {
            id: "counter",
            joins: Arc::new(AtomicUsize::new(0)),
        }))
        .expect("registers");
    host.enable(&id).expect("enables");

    let report = dispatch_join(&mut host);
    // The report records the event-specific overrun precisely.
    assert_eq!(report.budget_exceeded(), std::slice::from_ref(&id));
    // The lifetime counter includes the on_enable overrun too (zero budget), so
    // at minimum the event overrun is recorded.
    assert!(host.stats(&id).is_some_and(|s| s.budget_overruns() >= 1));
    assert_eq!(
        host.state(&id),
        Some(PluginState::Disabled(DisableReason::BudgetExceeded)),
        "overrun disabled the plugin"
    );
}
