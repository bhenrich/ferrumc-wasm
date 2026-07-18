use std::collections::BTreeMap;
use std::path::Path;

use ferrumc_plugin_sdk::{
    BlockDecision, BlockEvent, BlockPlaceEvent, BlockPos, Capability, CapabilityManifest,
    ChatAttempt, ChunkPos, CommandArgument, CommandDefinition, CommandInvocation, CommandNode,
    CommandNodeKind, DiagnosticLevel, Direction, EntityId, Event, EventContext, EventDecision,
    EventKind, FacadeError, Feedback, HandlerId, HostServices, InteractHand, InteractTarget,
    InteractionAttempt, LoadContext, MoveEvent, PermissionNode, PlaceAttempt, PlayerEvent,
    PlayerId, Plugin, PluginDeclaration, PluginError, PluginVersion, Resolution, Tick, TimerId,
    UnloadContext, Vec3, WorldOperation,
};
use ferrumc_plugin_sdk_builtin::{
    BuiltinCallbackError, BuiltinPluginFactory, BuiltinPluginInstance, CallbackOutcome,
};

const CALL_TICK: Tick = Tick::new(77);
const BLOCK: BlockPos = BlockPos::new(4, 65, -3);
const POSITION: Vec3 = Vec3::new(4.5, 65.0, -2.5);

#[derive(Debug, Clone, PartialEq)]
enum Effect {
    Subscribe(EventKind),
    Register(CommandDefinition),
    Operation(WorldOperation),
    StoragePut(String, Vec<u8>),
    StorageDelete(String),
    Schedule(TimerId, u64),
    Cancel(TimerId),
    BlockDecision(BlockDecision),
    EventDecision(EventDecision),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BackendCall {
    Subscribe,
    Register,
    ChunkLoaded,
    BlockState,
    PlayerPosition,
    Submit,
    Permission,
    StorageGet,
    StoragePut,
    StorageDelete,
    StorageKeys,
    Schedule,
    Cancel,
}

struct TransactionalHost {
    reported_capabilities: CapabilityManifest,
    stage_capacity: usize,
    staged: Vec<Effect>,
    committed: Vec<Effect>,
    diagnostics: Vec<(DiagnosticLevel, String)>,
    calls: Vec<BackendCall>,
    storage: BTreeMap<String, Vec<u8>>,
}

impl TransactionalHost {
    fn new(reported_capabilities: CapabilityManifest, stage_capacity: usize) -> Self {
        Self {
            reported_capabilities,
            stage_capacity,
            staged: Vec::new(),
            committed: Vec::new(),
            diagnostics: Vec::new(),
            calls: Vec::new(),
            storage: BTreeMap::new(),
        }
    }

    fn begin(&mut self) {
        assert!(
            self.staged.is_empty(),
            "previous callback left staged effects"
        );
    }

    fn finish(&mut self, commit: bool) {
        let staged = std::mem::take(&mut self.staged);
        if !commit {
            return;
        }
        for effect in &staged {
            match effect {
                Effect::StoragePut(key, value) => {
                    self.storage.insert(key.clone(), value.clone());
                }
                Effect::StorageDelete(key) => {
                    self.storage.remove(key);
                }
                _ => {}
            }
        }
        self.committed.extend(staged);
    }

    fn stage(&mut self, effect: Effect) -> Result<(), FacadeError> {
        if self.staged.len() >= self.stage_capacity {
            return Err(FacadeError::BufferFull);
        }
        self.staged.push(effect);
        Ok(())
    }

    fn stage_outcome(&mut self, outcome: &CallbackOutcome) -> Result<(), FacadeError> {
        match outcome {
            CallbackOutcome::Complete => Ok(()),
            CallbackOutcome::BlockDecision(decision) => {
                self.stage(Effect::BlockDecision(decision.clone()))
            }
            CallbackOutcome::EventDecision(decision) => {
                self.stage(Effect::EventDecision(decision.clone()))
            }
            _ => Err(FacadeError::Unavailable {
                operation: "future callback outcome",
            }),
        }
    }

    fn clear_observations(&mut self) {
        self.calls.clear();
        self.diagnostics.clear();
        self.committed.clear();
    }
}

impl HostServices for TransactionalHost {
    fn capabilities(&self) -> CapabilityManifest {
        self.reported_capabilities
    }

    fn subscribe_event(&mut self, kind: EventKind) -> Result<(), FacadeError> {
        self.calls.push(BackendCall::Subscribe);
        self.stage(Effect::Subscribe(kind))
    }

    fn register_command(&mut self, command: &CommandDefinition) -> Result<(), FacadeError> {
        self.calls.push(BackendCall::Register);
        self.stage(Effect::Register(command.clone()))
    }

    fn is_chunk_loaded(&mut self, _chunk: ChunkPos) -> Result<bool, FacadeError> {
        self.calls.push(BackendCall::ChunkLoaded);
        Ok(true)
    }

    fn block_state_id(&mut self, _pos: BlockPos) -> Result<Option<u32>, FacadeError> {
        self.calls.push(BackendCall::BlockState);
        Ok(Some(0x1020_3040))
    }

    fn player_position(&mut self, _player: PlayerId) -> Result<Option<Vec3>, FacadeError> {
        self.calls.push(BackendCall::PlayerPosition);
        Ok(Some(POSITION))
    }

    fn submit_world_operation(&mut self, operation: WorldOperation) -> Result<(), FacadeError> {
        self.calls.push(BackendCall::Submit);
        self.stage(Effect::Operation(operation))
    }

    fn resolve_permission(
        &mut self,
        _player: PlayerId,
        _node: &PermissionNode,
    ) -> Result<Resolution, FacadeError> {
        self.calls.push(BackendCall::Permission);
        Ok(Resolution::Allowed)
    }

    fn storage_get(&mut self, key: &str) -> Result<Option<Vec<u8>>, FacadeError> {
        self.calls.push(BackendCall::StorageGet);
        Ok(self.storage.get(key).cloned())
    }

    fn storage_put(&mut self, key: &str, value: &[u8]) -> Result<(), FacadeError> {
        self.calls.push(BackendCall::StoragePut);
        self.stage(Effect::StoragePut(key.to_owned(), value.to_vec()))
    }

    fn storage_delete(&mut self, key: &str) -> Result<(), FacadeError> {
        self.calls.push(BackendCall::StorageDelete);
        self.stage(Effect::StorageDelete(key.to_owned()))
    }

    fn storage_keys(&mut self) -> Result<Vec<String>, FacadeError> {
        self.calls.push(BackendCall::StorageKeys);
        Ok(self.storage.keys().cloned().collect())
    }

    fn schedule_timer(&mut self, id: TimerId, delay_ticks: u64) -> Result<(), FacadeError> {
        self.calls.push(BackendCall::Schedule);
        self.stage(Effect::Schedule(id, delay_ticks))
    }

    fn cancel_timer(&mut self, id: TimerId) -> Result<(), FacadeError> {
        self.calls.push(BackendCall::Cancel);
        self.stage(Effect::Cancel(id))
    }

    fn diagnostic(&mut self, level: DiagnosticLevel, message: &str) -> Result<(), FacadeError> {
        // Diagnostics are bounded observability records, not transactional
        // mutation commands, so callback failure does not erase them.
        self.diagnostics.push((level, message.to_owned()));
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DispatchFailure {
    Adapter(BuiltinCallbackError),
    DecisionBufferFull,
}

fn initialize(
    factory: &BuiltinPluginFactory,
    granted: CapabilityManifest,
    host: &mut TransactionalHost,
) -> Result<BuiltinPluginInstance, BuiltinCallbackError> {
    host.begin();
    let result = factory.initialize(granted, host);
    host.finish(result.is_ok());
    result
}

fn dispatch(
    instance: &mut BuiltinPluginInstance,
    event: &Event,
    host: &mut TransactionalHost,
) -> Result<CallbackOutcome, DispatchFailure> {
    host.begin();
    let result = instance
        .handle_event(event, CALL_TICK, host)
        .map_err(DispatchFailure::Adapter)
        .and_then(|outcome| {
            host.stage_outcome(&outcome)
                .map_err(|_| DispatchFailure::DecisionBufferFull)?;
            Ok(outcome)
        });
    host.finish(result.is_ok());
    result
}

fn shutdown(
    instance: BuiltinPluginInstance,
    host: &mut TransactionalHost,
) -> Result<(), BuiltinCallbackError> {
    host.begin();
    let result = instance.shutdown(host);
    host.finish(result.is_ok());
    result
}

fn player() -> PlayerId {
    PlayerId::offline("BuiltinContract")
}

fn timer(raw: u64) -> TimerId {
    TimerId::new(raw).expect("test timer is nonzero")
}

fn handler(raw: u64) -> HandlerId {
    HandlerId::new(raw).expect("test handler is nonzero")
}

fn command_definition() -> CommandDefinition {
    let root = CommandNode::new(None, CommandNodeKind::Literal, "builtin")
        .expect("valid root")
        .with_handler(handler(7));
    let child =
        CommandNode::new(Some(0), CommandNodeKind::GreedyText, "message").expect("valid child");
    CommandDefinition::new(vec![root, child]).expect("valid command preorder")
}

fn command_invocation() -> CommandInvocation {
    CommandInvocation::new(
        handler(7),
        player(),
        vec![
            CommandArgument::text("message", "hello").expect("bounded text"),
            CommandArgument::integer("count", -2).expect("bounded integer"),
        ],
    )
    .expect("bounded invocation")
}

fn all_events() -> Vec<Event> {
    let player = player();
    vec![
        Event::PlayerJoin(PlayerEvent::new(player)),
        Event::PlayerLeave(PlayerEvent::new(player)),
        Event::BlockBreak(BlockEvent::new(player, BLOCK)),
        Event::AfterBlockPlace(BlockPlaceEvent::new(player, BLOCK, 11)),
        Event::AfterBlockBreak(BlockEvent::new(player, BLOCK)),
        Event::PlayerMove(MoveEvent::new(player, BLOCK, BlockPos::new(5, 65, -3))),
        Event::BlockPlaceAttempt(PlaceAttempt::new(player, BLOCK, 12)),
        Event::BlockBreakAttempt(BlockEvent::new(player, BLOCK)),
        Event::ChatAttempt(ChatAttempt::new(player, "hello").expect("bounded chat")),
        Event::InteractAttempt(InteractionAttempt::new(
            player,
            InteractHand::Off,
            InteractTarget::Block {
                pos: BLOCK,
                face: Direction::Up,
            },
        )),
        Event::Command(command_invocation()),
        Event::Timer(timer(9)),
    ]
}

struct RoutePlugin;

impl Plugin for RoutePlugin {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-route",
        "Builtin Route",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::all(),
    );

    fn create() -> Self {
        Self
    }

    fn on_event(
        &mut self,
        event: &Event,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        if context.tick() != CALL_TICK {
            return Err(PluginError::failed("wrong callback tick"));
        }
        context
            .diagnostics()
            .emit(DiagnosticLevel::Info, &format!("event:{:?}", event.kind()))?;
        Ok(())
    }

    fn before_block_place(
        &mut self,
        _attempt: &PlaceAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<BlockDecision, PluginError> {
        context
            .diagnostics()
            .emit(DiagnosticLevel::Info, "decision:place")?;
        Ok(BlockDecision::Replace(0x5566_7788))
    }

    fn before_block_break(
        &mut self,
        _attempt: &BlockEvent,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        context
            .diagnostics()
            .emit(DiagnosticLevel::Info, "decision:break")?;
        Ok(EventDecision::Deny(Some(Feedback::new("break denied")?)))
    }

    fn before_chat(
        &mut self,
        _attempt: &ChatAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        context
            .diagnostics()
            .emit(DiagnosticLevel::Info, "decision:chat")?;
        Ok(EventDecision::Deny(None))
    }

    fn before_interact(
        &mut self,
        _attempt: &InteractionAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        context
            .diagnostics()
            .emit(DiagnosticLevel::Info, "decision:interact")?;
        Ok(EventDecision::Allow)
    }

    fn on_command(
        &mut self,
        invocation: &CommandInvocation,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        if invocation.arguments().len() != 2 {
            return Err(PluginError::failed("command was not routed intact"));
        }
        context
            .diagnostics()
            .emit(DiagnosticLevel::Info, "event:command")?;
        Ok(())
    }

    fn on_timer(&mut self, id: TimerId, context: &mut EventContext<'_>) -> Result<(), PluginError> {
        if id != timer(9) {
            return Err(PluginError::failed("timer was not routed intact"));
        }
        context
            .diagnostics()
            .emit(DiagnosticLevel::Info, "event:timer")?;
        Ok(())
    }
}

fn expected_outcomes() -> Vec<CallbackOutcome> {
    vec![
        CallbackOutcome::Complete,
        CallbackOutcome::Complete,
        CallbackOutcome::Complete,
        CallbackOutcome::Complete,
        CallbackOutcome::Complete,
        CallbackOutcome::Complete,
        CallbackOutcome::BlockDecision(BlockDecision::Replace(0x5566_7788)),
        CallbackOutcome::EventDecision(EventDecision::Deny(Some(
            Feedback::new("break denied").expect("bounded feedback"),
        ))),
        CallbackOutcome::EventDecision(EventDecision::Deny(None)),
        CallbackOutcome::EventDecision(EventDecision::Allow),
        CallbackOutcome::Complete,
        CallbackOutcome::Complete,
    ]
}

fn required_event_capability(kind: EventKind) -> Option<Capability> {
    match kind {
        EventKind::PlayerJoin
        | EventKind::PlayerLeave
        | EventKind::BlockBreak
        | EventKind::AfterBlockPlace
        | EventKind::AfterBlockBreak
        | EventKind::PlayerMove => Some(Capability::ReceiveEvents),
        EventKind::BlockPlaceAttempt | EventKind::BlockBreakAttempt => {
            Some(Capability::VetoBlockEdits)
        }
        EventKind::ChatAttempt | EventKind::InteractAttempt => Some(Capability::VetoEvents),
        EventKind::Command => Some(Capability::RegisterCommands),
        EventKind::Timer => None,
        _ => panic!("future SDK event kind is not part of the twelve-event contract"),
    }
}

#[test]
fn all_twelve_events_route_to_their_exact_callbacks_and_outcomes() {
    let factory = BuiltinPluginFactory::new::<RoutePlugin>().expect("valid factory");
    let mut host = TransactionalHost::new(CapabilityManifest::all(), 64);
    let mut instance =
        initialize(&factory, CapabilityManifest::all(), &mut host).expect("load route plugin");

    for (event, expected) in all_events().iter().zip(expected_outcomes()) {
        assert_eq!(dispatch(&mut instance, event, &mut host), Ok(expected));
    }

    assert_eq!(host.diagnostics.len(), 12);
    assert_eq!(
        host.committed
            .iter()
            .filter(|effect| matches!(effect, Effect::BlockDecision(_)))
            .count(),
        1
    );
    assert_eq!(
        host.committed
            .iter()
            .filter(|effect| matches!(effect, Effect::EventDecision(_)))
            .count(),
        3
    );
    shutdown(instance, &mut host).expect("route plugin shutdown");
}

#[test]
fn every_event_requires_exactly_its_documented_capability_before_plugin_code() {
    let factory = BuiltinPluginFactory::new::<RoutePlugin>().expect("valid factory");

    for (event, expected) in all_events().iter().zip(expected_outcomes()) {
        let required = required_event_capability(event.kind());
        let granted = required.map_or_else(CapabilityManifest::empty, |capability| {
            CapabilityManifest::empty().with(capability)
        });
        let mut host = TransactionalHost::new(granted, 8);
        let mut instance = initialize(&factory, granted, &mut host).expect("exact grant loads");
        host.clear_observations();

        assert_eq!(
            dispatch(&mut instance, event, &mut host),
            Ok(expected),
            "{:?} required an extra capability",
            event.kind()
        );
        assert_eq!(
            host.diagnostics.len(),
            1,
            "{:?} did not reach its callback",
            event.kind()
        );

        if let Some(capability) = required {
            let mut denied_host = TransactionalHost::new(CapabilityManifest::empty(), 8);
            let mut denied_instance =
                initialize(&factory, CapabilityManifest::empty(), &mut denied_host)
                    .expect("an empty effective grant still loads");
            denied_host.clear_observations();
            assert_eq!(
                dispatch(&mut denied_instance, event, &mut denied_host),
                Err(DispatchFailure::Adapter(
                    BuiltinCallbackError::CapabilityDenied(capability)
                )),
                "{:?} used the wrong capability gate",
                event.kind()
            );
            assert!(denied_host.calls.is_empty());
            assert!(denied_host.diagnostics.is_empty());
            assert!(denied_host.committed.is_empty());
        }
    }
}

struct MaskProbe;

impl Plugin for MaskProbe {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-mask-probe",
        "Builtin Mask Probe",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty()
            .with(Capability::ReceiveEvents)
            .with(Capability::SubmitIntents)
            .with(Capability::Storage),
    );

    fn create() -> Self {
        Self
    }

    fn on_load(&mut self, context: &mut LoadContext<'_>) -> Result<(), PluginError> {
        let capabilities = context.capabilities();
        match context.commands() {
            Err(FacadeError::Capability(error))
                if error.capability() == Capability::RegisterCommands => {}
            Err(error) => {
                return Err(PluginError::failed(format!(
                    "unexpected unrequested-facade result: {error}"
                )))
            }
            Ok(_) => {
                return Err(PluginError::failed(
                    "unrequested commands facade was exposed",
                ))
            }
        }
        context.diagnostics().emit(
            DiagnosticLevel::Info,
            &format!("caps:{:08x}", capabilities.bits()),
        )?;
        Ok(())
    }
}

struct MustNotCreate;

impl Plugin for MustNotCreate {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-must-not-create",
        "Builtin Must Not Create",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty().with(Capability::ReadWorld),
    );

    fn create() -> Self {
        panic!("backend mismatch reached plugin construction")
    }
}

struct UnloadMustNotRun;

impl Plugin for UnloadMustNotRun {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-unload-must-not-run",
        "Builtin Unload Must Not Run",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty().with(Capability::ReadWorld),
    );

    fn create() -> Self {
        Self
    }

    fn on_unload(&mut self, _context: &mut UnloadContext<'_>) -> Result<(), PluginError> {
        panic!("backend mismatch reached unload callback")
    }
}

#[test]
fn effective_grants_are_exact_intersections_and_backend_extras_stay_hidden() {
    let factory = BuiltinPluginFactory::new::<MaskProbe>().expect("valid factory");
    assert_eq!(factory.declaration(), MaskProbe::DECLARATION);
    assert_eq!(
        factory.requested_capabilities(),
        MaskProbe::DECLARATION.requested_capabilities()
    );

    let mut broad_host = TransactionalHost::new(CapabilityManifest::all(), 8);
    let broad_instance =
        initialize(&factory, CapabilityManifest::all(), &mut broad_host).expect("broad host loads");
    assert_eq!(
        broad_instance.capabilities(),
        MaskProbe::DECLARATION.requested_capabilities()
    );
    assert_eq!(broad_instance.declaration(), MaskProbe::DECLARATION);
    assert_eq!(
        broad_host.diagnostics,
        vec![(
            DiagnosticLevel::Info,
            format!(
                "caps:{:08x}",
                MaskProbe::DECLARATION.requested_capabilities().bits()
            )
        )]
    );
    assert!(!broad_host.calls.contains(&BackendCall::Register));

    let explicit = CapabilityManifest::empty().with(Capability::ReceiveEvents);
    let mut denied_host = TransactionalHost::new(CapabilityManifest::all(), 8);
    let denied_instance =
        initialize(&factory, explicit, &mut denied_host).expect("subset grant loads");
    assert_eq!(denied_instance.capabilities(), explicit);
    assert_eq!(
        denied_host.diagnostics,
        vec![(
            DiagnosticLevel::Info,
            format!("caps:{:08x}", explicit.bits())
        )]
    );
}

#[test]
fn backend_mismatch_is_denied_before_create_event_or_unload_code() {
    let factory = BuiltinPluginFactory::new::<MustNotCreate>().expect("valid factory");
    let explicit = CapabilityManifest::empty().with(Capability::ReadWorld);
    let mut missing = TransactionalHost::new(CapabilityManifest::empty(), 8);
    assert!(matches!(
        initialize(&factory, explicit, &mut missing),
        Err(BuiltinCallbackError::CapabilityDenied(
            Capability::ReadWorld
        ))
    ));
    assert!(missing.calls.is_empty());
    assert!(missing.committed.is_empty());

    let factory = BuiltinPluginFactory::new::<RoutePlugin>().expect("valid factory");
    let effective = CapabilityManifest::empty()
        .with(Capability::ReceiveEvents)
        .with(Capability::SubmitIntents);
    let mut host = TransactionalHost::new(effective, 8);
    let mut instance = initialize(&factory, effective, &mut host).expect("matching backend loads");
    host.clear_observations();
    host.reported_capabilities = CapabilityManifest::empty().with(Capability::ReceiveEvents);
    assert_eq!(
        dispatch(
            &mut instance,
            &Event::PlayerJoin(PlayerEvent::new(player())),
            &mut host
        ),
        Err(DispatchFailure::Adapter(
            BuiltinCallbackError::CapabilityDenied(Capability::SubmitIntents)
        ))
    );
    assert!(host.calls.is_empty());
    assert!(host.diagnostics.is_empty());

    host.reported_capabilities = effective;
    let unload_factory =
        BuiltinPluginFactory::new::<UnloadMustNotRun>().expect("valid unload factory");
    let unload_capability = CapabilityManifest::empty().with(Capability::ReadWorld);
    let mut unload_host = TransactionalHost::new(unload_capability, 8);
    let second = initialize(&unload_factory, unload_capability, &mut unload_host)
        .expect("unload probe loads");
    unload_host.clear_observations();
    unload_host.reported_capabilities = CapabilityManifest::empty();
    assert_eq!(
        shutdown(second, &mut unload_host),
        Err(BuiltinCallbackError::CapabilityDenied(
            Capability::ReadWorld
        ))
    );
    assert!(unload_host.calls.is_empty());
    assert!(unload_host.diagnostics.is_empty());
    assert!(unload_host.committed.is_empty());
}

struct FacadePlugin {
    callbacks: u32,
}

impl Plugin for FacadePlugin {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-facades",
        "Builtin Facades",
        PluginVersion::new(1, 2, 3),
        CapabilityManifest::all(),
    );

    fn create() -> Self {
        Self { callbacks: 0 }
    }

    fn on_load(&mut self, context: &mut LoadContext<'_>) -> Result<(), PluginError> {
        if context.capabilities() != CapabilityManifest::all() {
            return Err(PluginError::failed("load context grant mismatch"));
        }
        context.events()?.subscribe(EventKind::PlayerJoin)?;
        context.commands()?.register(&command_definition())?;
        context.storage()?.put("boot", b"load-value")?;
        context.timers().schedule(timer(1), 20)?;
        context
            .diagnostics()
            .emit(DiagnosticLevel::Info, "facade:load:0")?;
        Ok(())
    }

    fn on_event(
        &mut self,
        event: &Event,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        let Event::PlayerJoin(join) = event else {
            return Err(PluginError::failed("unexpected facade test event"));
        };
        self.callbacks += 1;
        if context.capabilities() != CapabilityManifest::all() || context.tick() != CALL_TICK {
            return Err(PluginError::failed("event context contract mismatch"));
        }

        let mut world = context.world()?;
        if !world.is_chunk_loaded(BLOCK.to_chunk_pos())?
            || world.block_state_id(BLOCK)? != Some(0x1020_3040)
            || world.player_position(join.player())? != Some(POSITION)
        {
            return Err(PluginError::failed("world facade response mismatch"));
        }

        let mut operations = context.operations()?;
        operations.set_block(BLOCK, 0x1122_3344)?;
        operations.teleport(join.player(), POSITION)?;
        operations.message(join.player(), "plain message")?;

        let permission =
            PermissionNode::parse("ferrumc.builtin.use").expect("test permission is valid");
        if context.permissions()?.resolve(join.player(), &permission)? != Resolution::Allowed {
            return Err(PluginError::failed("permission facade response mismatch"));
        }

        let mut storage = context.storage()?;
        if storage.get("seed")? != Some(vec![8, 9]) || storage.keys()? != vec!["boot", "seed"] {
            return Err(PluginError::failed("storage facade response mismatch"));
        }
        storage.put("event", b"event-value")?;
        storage.delete("boot")?;
        context.timers().schedule(timer(2), 5)?;
        context.timers().cancel(timer(1))?;
        context.diagnostics().emit(
            DiagnosticLevel::Debug,
            &format!("facade:event:{}", self.callbacks),
        )?;
        Ok(())
    }

    fn on_unload(&mut self, context: &mut UnloadContext<'_>) -> Result<(), PluginError> {
        context.storage()?.delete("event")?;
        context.timers().cancel(timer(2))?;
        context
            .diagnostics()
            .emit(DiagnosticLevel::Warn, "facade:unload")?;
        Ok(())
    }
}

struct InvalidDeclaration;

impl Plugin for InvalidDeclaration {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "",
        "Invalid",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty(),
    );

    fn create() -> Self {
        Self
    }
}

#[test]
fn factory_validates_type_erases_and_creates_fresh_facade_identical_instances() {
    assert!(BuiltinPluginFactory::new::<InvalidDeclaration>().is_err());

    let factories = [
        BuiltinPluginFactory::new::<FacadePlugin>().expect("facade factory"),
        BuiltinPluginFactory::new::<MaskProbe>().expect("mask factory"),
    ];
    assert_eq!(factories[0].declaration().id(), "builtin-facades");
    assert_eq!(factories[1].declaration().id(), "builtin-mask-probe");

    for _ in 0..2 {
        let mut host = TransactionalHost::new(CapabilityManifest::all(), 32);
        host.storage.insert("seed".to_owned(), vec![8, 9]);
        let mut instance =
            initialize(&factories[0], CapabilityManifest::all(), &mut host).expect("load");
        assert_eq!(
            dispatch(
                &mut instance,
                &Event::PlayerJoin(PlayerEvent::new(player())),
                &mut host
            ),
            Ok(CallbackOutcome::Complete)
        );
        shutdown(instance, &mut host).expect("shutdown");

        assert_eq!(
            host.diagnostics,
            vec![
                (DiagnosticLevel::Info, "facade:load:0".to_owned()),
                (DiagnosticLevel::Debug, "facade:event:1".to_owned()),
                (DiagnosticLevel::Warn, "facade:unload".to_owned()),
            ]
        );
        assert_eq!(
            host.storage,
            BTreeMap::from([("seed".to_owned(), vec![8, 9])])
        );
        assert!(host
            .committed
            .iter()
            .any(|effect| matches!(effect, Effect::Subscribe(EventKind::PlayerJoin))));
        assert!(host.committed.iter().any(
            |effect| matches!(effect, Effect::Register(command) if command == &command_definition())
        ));
        assert!(host.committed.iter().any(|effect| {
            matches!(
                effect,
                Effect::Operation(WorldOperation::SetBlock(operation))
                    if operation.pos() == BLOCK
                        && operation.block_state_id() == 0x1122_3344
            )
        }));
        for expected_effect in [
            Effect::StoragePut("boot".to_owned(), b"load-value".to_vec()),
            Effect::StoragePut("event".to_owned(), b"event-value".to_vec()),
            Effect::StorageDelete("boot".to_owned()),
            Effect::StorageDelete("event".to_owned()),
            Effect::Schedule(timer(1), 20),
            Effect::Schedule(timer(2), 5),
            Effect::Cancel(timer(1)),
            Effect::Cancel(timer(2)),
        ] {
            assert!(
                host.committed.contains(&expected_effect),
                "shared facade did not commit {expected_effect:?}"
            );
        }
        assert!(host.committed.iter().any(|effect| {
            matches!(
                effect,
                Effect::Operation(WorldOperation::Teleport(operation))
                    if operation.player() == player() && operation.position() == POSITION
            )
        }));
        assert!(host.committed.iter().any(|effect| {
            matches!(
                effect,
                Effect::Operation(WorldOperation::Message(operation))
                    if operation.player() == player() && operation.message() == "plain message"
            )
        }));
        for expected_call in [
            BackendCall::Subscribe,
            BackendCall::Register,
            BackendCall::ChunkLoaded,
            BackendCall::BlockState,
            BackendCall::PlayerPosition,
            BackendCall::Permission,
            BackendCall::StorageGet,
            BackendCall::StorageKeys,
        ] {
            assert!(
                host.calls.contains(&expected_call),
                "shared facade did not reach {expected_call:?}"
            );
        }
    }
}

struct BufferPlugin;

impl Plugin for BufferPlugin {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-buffer",
        "Builtin Buffer",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty()
            .with(Capability::ReceiveEvents)
            .with(Capability::SubmitIntents),
    );

    fn create() -> Self {
        Self
    }

    fn on_event(
        &mut self,
        event: &Event,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        context
            .diagnostics()
            .emit(DiagnosticLevel::Warn, "buffer callback observed")?;
        let mut operations = context.operations()?;
        operations.message(player(), "first")?;
        if matches!(event, Event::PlayerJoin(_)) {
            operations.message(player(), "second")?;
        }
        Ok(())
    }
}

struct DecisionStagePlugin;

impl Plugin for DecisionStagePlugin {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-decision-stage",
        "Builtin Decision Stage",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty()
            .with(Capability::SubmitIntents)
            .with(Capability::VetoBlockEdits),
    );

    fn create() -> Self {
        Self
    }

    fn before_block_place(
        &mut self,
        _attempt: &PlaceAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<BlockDecision, PluginError> {
        context.operations()?.message(player(), "before decision")?;
        Ok(BlockDecision::Replace(99))
    }

    fn before_block_break(
        &mut self,
        _attempt: &BlockEvent,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        context.operations()?.message(player(), "discard me")?;
        Err(PluginError::failed("cooperative decision failure"))
    }
}

fn fail_closed_block(result: &Result<CallbackOutcome, DispatchFailure>) -> BlockDecision {
    match result {
        Ok(CallbackOutcome::BlockDecision(decision)) => decision.clone(),
        _ => BlockDecision::Deny(None),
    }
}

fn fail_closed_event(result: &Result<CallbackOutcome, DispatchFailure>) -> EventDecision {
    match result {
        Ok(CallbackOutcome::EventDecision(decision)) => decision.clone(),
        _ => EventDecision::Deny(None),
    }
}

#[test]
fn bounded_effect_and_decision_slots_discard_prior_staging_and_fail_closed() {
    let factory = BuiltinPluginFactory::new::<BufferPlugin>().expect("valid factory");
    let capabilities = BufferPlugin::DECLARATION.requested_capabilities();
    let mut host = TransactionalHost::new(capabilities, 1);
    let mut instance = initialize(&factory, capabilities, &mut host).expect("load");
    host.clear_observations();

    let error = dispatch(
        &mut instance,
        &Event::PlayerJoin(PlayerEvent::new(player())),
        &mut host,
    );
    assert!(matches!(
        error,
        Err(DispatchFailure::Adapter(BuiltinCallbackError::Cooperative(
            PluginError::Facade(FacadeError::BufferFull)
        )))
    ));
    assert!(host.committed.is_empty());
    assert!(host.staged.is_empty());
    assert_eq!(
        host.diagnostics,
        vec![(DiagnosticLevel::Warn, "buffer callback observed".to_owned())]
    );

    assert_eq!(
        dispatch(
            &mut instance,
            &Event::PlayerLeave(PlayerEvent::new(player())),
            &mut host
        ),
        Ok(CallbackOutcome::Complete)
    );
    assert_eq!(
        host.committed
            .iter()
            .filter(|effect| matches!(effect, Effect::Operation(_)))
            .count(),
        1
    );

    let decision_factory =
        BuiltinPluginFactory::new::<DecisionStagePlugin>().expect("valid factory");
    let decision_caps = DecisionStagePlugin::DECLARATION.requested_capabilities();
    let mut decision_host = TransactionalHost::new(decision_caps, 1);
    let mut decision_instance =
        initialize(&decision_factory, decision_caps, &mut decision_host).expect("load");
    decision_host.clear_observations();
    let event = Event::BlockPlaceAttempt(PlaceAttempt::new(player(), BLOCK, 1));
    let full = dispatch(&mut decision_instance, &event, &mut decision_host);
    assert_eq!(full, Err(DispatchFailure::DecisionBufferFull));
    assert_eq!(fail_closed_block(&full), BlockDecision::Deny(None));
    assert!(decision_host.committed.is_empty());
    assert!(decision_host.staged.is_empty());

    decision_host.stage_capacity = 2;
    assert_eq!(
        dispatch(&mut decision_instance, &event, &mut decision_host),
        Ok(CallbackOutcome::BlockDecision(BlockDecision::Replace(99)))
    );
    assert_eq!(decision_host.committed.len(), 2);

    decision_host.clear_observations();
    let cooperative = dispatch(
        &mut decision_instance,
        &Event::BlockBreakAttempt(BlockEvent::new(player(), BLOCK)),
        &mut decision_host,
    );
    assert!(matches!(
        cooperative,
        Err(DispatchFailure::Adapter(BuiltinCallbackError::Cooperative(
            PluginError::Failed(_)
        )))
    ));
    assert_eq!(fail_closed_event(&cooperative), EventDecision::Deny(None));
    assert!(decision_host.committed.is_empty());

    assert_eq!(
        dispatch(&mut decision_instance, &event, &mut decision_host),
        Ok(CallbackOutcome::BlockDecision(BlockDecision::Replace(99)))
    );
}

struct PanicPayload;

impl Drop for PanicPayload {
    fn drop(&mut self) {
        panic!("panic payload destructor also panicked")
    }
}

fn staged_panic(context: &mut EventContext<'_>, label: &str, hostile_payload: bool) -> ! {
    context
        .diagnostics()
        .emit(DiagnosticLevel::Error, label)
        .expect("bounded panic diagnostic");
    context
        .operations()
        .expect("panic fixture has SubmitIntents")
        .message(player(), "discard panic effect")
        .expect("panic fixture stage has capacity");
    if hostile_payload {
        std::panic::panic_any(PanicPayload);
    }
    panic!("{label}")
}

struct PanicRoutesPlugin;

impl Plugin for PanicRoutesPlugin {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-panic-routes",
        "Builtin Panic Routes",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::all(),
    );

    fn create() -> Self {
        Self
    }

    fn on_event(
        &mut self,
        _event: &Event,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        staged_panic(context, "panic:event", false)
    }

    fn before_block_place(
        &mut self,
        _attempt: &PlaceAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<BlockDecision, PluginError> {
        staged_panic(context, "panic:block-place", false)
    }

    fn before_block_break(
        &mut self,
        _attempt: &BlockEvent,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        staged_panic(context, "panic:block-break", false)
    }

    fn before_chat(
        &mut self,
        _attempt: &ChatAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        staged_panic(context, "panic:chat", false)
    }

    fn before_interact(
        &mut self,
        _attempt: &InteractionAttempt,
        context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        staged_panic(context, "panic:interact", false)
    }

    fn on_command(
        &mut self,
        _invocation: &CommandInvocation,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        staged_panic(context, "panic:command", false)
    }

    fn on_timer(
        &mut self,
        _timer: TimerId,
        context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        staged_panic(context, "panic:timer-payload-drop", true)
    }

    fn on_unload(&mut self, _context: &mut UnloadContext<'_>) -> Result<(), PluginError> {
        panic!("poisoned plugin reached unload")
    }
}

#[test]
fn every_callback_family_contains_unwind_discards_effects_and_poisons_instance() {
    let factory = BuiltinPluginFactory::new::<PanicRoutesPlugin>().expect("valid factory");
    let panic_events = vec![
        Event::PlayerJoin(PlayerEvent::new(player())),
        Event::BlockPlaceAttempt(PlaceAttempt::new(player(), BLOCK, 1)),
        Event::BlockBreakAttempt(BlockEvent::new(player(), BLOCK)),
        Event::ChatAttempt(ChatAttempt::new(player(), "panic").expect("bounded chat")),
        Event::InteractAttempt(InteractionAttempt::new(
            player(),
            InteractHand::Main,
            InteractTarget::Entity {
                entity: EntityId::new(44),
            },
        )),
        Event::Command(command_invocation()),
        Event::Timer(timer(9)),
    ];

    for event in panic_events {
        let mut host = TransactionalHost::new(CapabilityManifest::all(), 8);
        let mut instance =
            initialize(&factory, CapabilityManifest::all(), &mut host).expect("load panic probe");
        host.clear_observations();
        assert_eq!(
            dispatch(&mut instance, &event, &mut host),
            Err(DispatchFailure::Adapter(BuiltinCallbackError::Panicked)),
            "{:?} unwind was not classified",
            event.kind()
        );
        assert!(host.committed.is_empty());
        assert!(host.staged.is_empty());
        assert_eq!(host.diagnostics.len(), 1);

        let calls_after_panic = host.calls.len();
        let diagnostics_after_panic = host.diagnostics.len();
        assert_eq!(
            dispatch(&mut instance, &event, &mut host),
            Err(DispatchFailure::Adapter(BuiltinCallbackError::Poisoned))
        );
        assert_eq!(host.calls.len(), calls_after_panic);
        assert_eq!(host.diagnostics.len(), diagnostics_after_panic);
        assert_eq!(
            shutdown(instance, &mut host),
            Err(BuiltinCallbackError::Poisoned)
        );
        assert_eq!(host.diagnostics.len(), diagnostics_after_panic);
    }
}

struct CreatePanic;

impl Plugin for CreatePanic {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-create-panic",
        "Builtin Create Panic",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty(),
    );

    fn create() -> Self {
        panic!("create panic")
    }
}

struct LoadError;

impl Plugin for LoadError {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-load-error",
        "Builtin Load Error",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty().with(Capability::Storage),
    );

    fn create() -> Self {
        Self
    }

    fn on_load(&mut self, context: &mut LoadContext<'_>) -> Result<(), PluginError> {
        context.storage()?.put("load-error", b"discard")?;
        context
            .diagnostics()
            .emit(DiagnosticLevel::Error, "load cooperative error")?;
        Err(PluginError::failed("load failed"))
    }
}

struct LoadPanic;

impl Plugin for LoadPanic {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-load-panic",
        "Builtin Load Panic",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty().with(Capability::Storage),
    );

    fn create() -> Self {
        Self
    }

    fn on_load(&mut self, context: &mut LoadContext<'_>) -> Result<(), PluginError> {
        context.storage()?.put("load-panic", b"discard")?;
        context
            .diagnostics()
            .emit(DiagnosticLevel::Error, "load panic")?;
        panic!("load panic")
    }
}

struct LoadErrorDropPanic;

impl Drop for LoadErrorDropPanic {
    fn drop(&mut self) {
        panic!("drop after load error")
    }
}

impl Plugin for LoadErrorDropPanic {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-load-error-drop-panic",
        "Builtin Load Error Drop Panic",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty(),
    );

    fn create() -> Self {
        Self
    }

    fn on_load(&mut self, context: &mut LoadContext<'_>) -> Result<(), PluginError> {
        context
            .diagnostics()
            .emit(DiagnosticLevel::Error, "load error before drop panic")?;
        Err(PluginError::failed("load failed before drop"))
    }
}

#[test]
fn create_and_load_failures_return_no_instance_and_discard_mutations() {
    let mut empty_host = TransactionalHost::new(CapabilityManifest::empty(), 8);
    let create_factory = BuiltinPluginFactory::new::<CreatePanic>().expect("valid factory");
    assert!(matches!(
        initialize(
            &create_factory,
            CapabilityManifest::empty(),
            &mut empty_host
        ),
        Err(BuiltinCallbackError::Panicked)
    ));
    assert!(empty_host.committed.is_empty());

    let storage = CapabilityManifest::empty().with(Capability::Storage);
    let mut error_host = TransactionalHost::new(storage, 8);
    let error_factory = BuiltinPluginFactory::new::<LoadError>().expect("valid factory");
    assert!(matches!(
        initialize(&error_factory, storage, &mut error_host),
        Err(BuiltinCallbackError::Cooperative(PluginError::Failed(_)))
    ));
    assert!(error_host.committed.is_empty());
    assert!(!error_host.storage.contains_key("load-error"));
    assert_eq!(error_host.diagnostics.len(), 1);

    let mut panic_host = TransactionalHost::new(storage, 8);
    let panic_factory = BuiltinPluginFactory::new::<LoadPanic>().expect("valid factory");
    assert!(matches!(
        initialize(&panic_factory, storage, &mut panic_host),
        Err(BuiltinCallbackError::Panicked)
    ));
    assert!(panic_host.committed.is_empty());
    assert!(!panic_host.storage.contains_key("load-panic"));
    assert_eq!(panic_host.diagnostics.len(), 1);

    let mut drop_host = TransactionalHost::new(CapabilityManifest::empty(), 8);
    let drop_factory = BuiltinPluginFactory::new::<LoadErrorDropPanic>().expect("valid factory");
    assert!(matches!(
        initialize(&drop_factory, CapabilityManifest::empty(), &mut drop_host),
        Err(BuiltinCallbackError::Panicked)
    ));
    assert!(drop_host.committed.is_empty());
    assert_eq!(drop_host.diagnostics.len(), 1);
}

struct UnloadError;

impl Plugin for UnloadError {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-unload-error",
        "Builtin Unload Error",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty().with(Capability::Storage),
    );

    fn create() -> Self {
        Self
    }

    fn on_unload(&mut self, context: &mut UnloadContext<'_>) -> Result<(), PluginError> {
        context.storage()?.put("unload-error", b"discard")?;
        context
            .diagnostics()
            .emit(DiagnosticLevel::Error, "unload cooperative error")?;
        Err(PluginError::failed("unload failed"))
    }
}

struct UnloadPanic;

impl Plugin for UnloadPanic {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-unload-panic",
        "Builtin Unload Panic",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty().with(Capability::Storage),
    );

    fn create() -> Self {
        Self
    }

    fn on_unload(&mut self, context: &mut UnloadContext<'_>) -> Result<(), PluginError> {
        context.storage()?.put("unload-panic", b"discard")?;
        context
            .diagnostics()
            .emit(DiagnosticLevel::Error, "unload panic")?;
        panic!("unload panic")
    }
}

struct DropPanic;

impl Drop for DropPanic {
    fn drop(&mut self) {
        panic!("plugin destructor panic")
    }
}

impl Plugin for DropPanic {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "builtin-drop-panic",
        "Builtin Drop Panic",
        PluginVersion::new(1, 0, 0),
        CapabilityManifest::empty().with(Capability::Storage),
    );

    fn create() -> Self {
        Self
    }

    fn on_unload(&mut self, context: &mut UnloadContext<'_>) -> Result<(), PluginError> {
        context.storage()?.put("drop-panic", b"discard")?;
        context
            .diagnostics()
            .emit(DiagnosticLevel::Error, "before destructor panic")?;
        Ok(())
    }
}

#[test]
fn consuming_shutdown_classifies_unload_and_drop_failures_and_implicit_drop_contains() {
    let storage = CapabilityManifest::empty().with(Capability::Storage);

    let mut error_host = TransactionalHost::new(storage, 8);
    let error_factory = BuiltinPluginFactory::new::<UnloadError>().expect("valid factory");
    let error_instance =
        initialize(&error_factory, storage, &mut error_host).expect("load error fixture");
    error_host.clear_observations();
    assert!(matches!(
        shutdown(error_instance, &mut error_host),
        Err(BuiltinCallbackError::Cooperative(PluginError::Failed(_)))
    ));
    assert!(error_host.committed.is_empty());
    assert!(!error_host.storage.contains_key("unload-error"));
    assert_eq!(error_host.diagnostics.len(), 1);

    let mut panic_host = TransactionalHost::new(storage, 8);
    let panic_factory = BuiltinPluginFactory::new::<UnloadPanic>().expect("valid factory");
    let panic_instance =
        initialize(&panic_factory, storage, &mut panic_host).expect("load panic fixture");
    panic_host.clear_observations();
    assert_eq!(
        shutdown(panic_instance, &mut panic_host),
        Err(BuiltinCallbackError::Panicked)
    );
    assert!(panic_host.committed.is_empty());
    assert!(!panic_host.storage.contains_key("unload-panic"));
    assert_eq!(panic_host.diagnostics.len(), 1);

    let mut drop_host = TransactionalHost::new(storage, 8);
    let drop_factory = BuiltinPluginFactory::new::<DropPanic>().expect("valid factory");
    let drop_instance =
        initialize(&drop_factory, storage, &mut drop_host).expect("load drop fixture");
    drop_host.clear_observations();
    assert_eq!(
        shutdown(drop_instance, &mut drop_host),
        Err(BuiltinCallbackError::Panicked)
    );
    assert!(drop_host.committed.is_empty());
    assert!(!drop_host.storage.contains_key("drop-panic"));
    assert_eq!(drop_host.diagnostics.len(), 1);

    let mut implicit_host = TransactionalHost::new(storage, 8);
    let implicit_instance =
        initialize(&drop_factory, storage, &mut implicit_host).expect("load implicit fixture");
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(implicit_instance);
    }));
    assert!(caught.is_ok(), "implicit instance Drop leaked plugin panic");
}

#[test]
fn adapter_manifest_and_source_expose_no_raw_or_concrete_escape_hatch() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest =
        std::fs::read_to_string(crate_root.join("Cargo.toml")).expect("read builtin manifest");
    for forbidden in [
        "ferrumc-sim",
        "ferrumc-world",
        "ferrumc-storage",
        "ferrumc-net",
        "ferrumc-plugin-api",
        "ferrumc-plugin-host",
        "ferrumc-plugin-sdk-dynamic",
        "ferrumc-plugin-abi",
        "ferrumc-plugin-abi-sys",
        "ferrumc-plugin-loader",
        "ferrumc-testkit",
        "tokio",
    ] {
        assert!(
            !manifest.contains(forbidden),
            "built-in adapter acquired forbidden dependency {forbidden}"
        );
    }

    for entry in std::fs::read_dir(crate_root.join("src")).expect("read builtin sources") {
        let path = entry.expect("source entry").path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        for forbidden in [
            "pub fn downcast",
            "pub fn as_any",
            "pub fn into_inner",
            "pub fn inner(",
            "pub fn plugin(",
            "pub fn plugin_mut",
            "pub fn services(",
            "pub fn concrete",
            "impl Deref",
            "impl std::ops::Deref",
            "impl core::ops::Deref",
            "pub use ferrumc_plugin_sdk::Plugin",
            "FcResourceHandle",
            "PluginCall",
            "*mut ",
            "*const ",
            "extern \"C\"",
        ] {
            assert!(
                !source.contains(forbidden),
                "{} exposes forbidden API `{forbidden}`",
                path.display()
            );
        }
    }
}
