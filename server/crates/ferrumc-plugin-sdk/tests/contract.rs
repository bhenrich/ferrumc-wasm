use std::collections::BTreeMap;

use ferrumc_plugin_sdk::{
    BlockDecision, BlockEvent, BlockPos, Capability, CapabilityManifest, ChatAttempt, ChunkPos,
    CommandDefinition, CommandInvocation, CommandNode, CommandNodeKind, DiagnosticLevel, Event,
    EventContext, EventDecision, EventKind, FacadeError, HandlerId, HostServices, InteractHand,
    InteractTarget, InteractionAttempt, LoadContext, PermissionNode, PlaceAttempt, PlayerEvent,
    PlayerId, Plugin, PluginDeclaration, PluginError, PluginVersion, Resolution, Tick, TimerId,
    UnloadContext, Vec3, WorldOperation, MAX_DIAGNOSTIC_BYTES, MAX_MESSAGE_BYTES, MAX_STORAGE_KEYS,
    MAX_STORAGE_KEY_BYTES, MAX_STORAGE_VALUE_BYTES,
};

const WORLD_CHUNK: ChunkPos = ChunkPos::new(3, -2);
const WORLD_BLOCK: BlockPos = BlockPos::new(49, 70, -18);
const WORLD_POSITION: Vec3 = Vec3::new(12.5, 64.0, -8.25);

#[derive(Debug, PartialEq)]
enum Call {
    Subscribe(EventKind),
    Register(CommandDefinition),
    IsChunkLoaded(ChunkPos),
    BlockState(BlockPos),
    PlayerPosition(PlayerId),
    Submit(WorldOperation),
    Permission(PlayerId, PermissionNode),
    StorageGet(String),
    StoragePut(String, Vec<u8>),
    StorageDelete(String),
    StorageKeys,
    Schedule(TimerId, u64),
    Cancel(TimerId),
    Diagnostic(DiagnosticLevel, String),
}

struct FakeHost {
    capabilities: CapabilityManifest,
    calls: Vec<Call>,
    storage: BTreeMap<String, Vec<u8>>,
    listed_keys: Option<Vec<String>>,
    operation_limit: usize,
    submit_attempts: usize,
    accepted_operations: Vec<WorldOperation>,
    timers: BTreeMap<TimerId, u64>,
}

impl FakeHost {
    fn new(capabilities: CapabilityManifest) -> Self {
        Self {
            capabilities,
            calls: Vec::new(),
            storage: BTreeMap::new(),
            listed_keys: None,
            operation_limit: usize::MAX,
            submit_attempts: 0,
            accepted_operations: Vec::new(),
            timers: BTreeMap::new(),
        }
    }

    fn with_operation_limit(capabilities: CapabilityManifest, operation_limit: usize) -> Self {
        Self {
            operation_limit,
            ..Self::new(capabilities)
        }
    }
}

impl HostServices for FakeHost {
    fn capabilities(&self) -> CapabilityManifest {
        self.capabilities
    }

    fn subscribe_event(&mut self, kind: EventKind) -> Result<(), FacadeError> {
        self.calls.push(Call::Subscribe(kind));
        Ok(())
    }

    fn register_command(&mut self, command: &CommandDefinition) -> Result<(), FacadeError> {
        self.calls.push(Call::Register(command.clone()));
        Ok(())
    }

    fn is_chunk_loaded(&mut self, chunk: ChunkPos) -> Result<bool, FacadeError> {
        self.calls.push(Call::IsChunkLoaded(chunk));
        Ok(chunk == WORLD_CHUNK)
    }

    fn block_state_id(&mut self, pos: BlockPos) -> Result<Option<u32>, FacadeError> {
        self.calls.push(Call::BlockState(pos));
        Ok((pos == WORLD_BLOCK).then_some(42))
    }

    fn player_position(&mut self, player: PlayerId) -> Result<Option<Vec3>, FacadeError> {
        self.calls.push(Call::PlayerPosition(player));
        Ok(Some(WORLD_POSITION))
    }

    fn submit_world_operation(&mut self, operation: WorldOperation) -> Result<(), FacadeError> {
        self.submit_attempts += 1;
        if self.accepted_operations.len() >= self.operation_limit {
            return Err(FacadeError::BufferFull);
        }
        self.calls.push(Call::Submit(operation.clone()));
        self.accepted_operations.push(operation);
        Ok(())
    }

    fn resolve_permission(
        &mut self,
        player: PlayerId,
        node: &PermissionNode,
    ) -> Result<Resolution, FacadeError> {
        self.calls.push(Call::Permission(player, node.clone()));
        Ok(Resolution::Allowed)
    }

    fn storage_get(&mut self, key: &str) -> Result<Option<Vec<u8>>, FacadeError> {
        self.calls.push(Call::StorageGet(key.to_owned()));
        Ok(self.storage.get(key).cloned())
    }

    fn storage_put(&mut self, key: &str, value: &[u8]) -> Result<(), FacadeError> {
        self.calls
            .push(Call::StoragePut(key.to_owned(), value.to_vec()));
        self.storage.insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    fn storage_delete(&mut self, key: &str) -> Result<(), FacadeError> {
        self.calls.push(Call::StorageDelete(key.to_owned()));
        self.storage.remove(key);
        Ok(())
    }

    fn storage_keys(&mut self) -> Result<Vec<String>, FacadeError> {
        self.calls.push(Call::StorageKeys);
        Ok(self
            .listed_keys
            .clone()
            .unwrap_or_else(|| self.storage.keys().rev().cloned().collect()))
    }

    fn schedule_timer(&mut self, id: TimerId, delay_ticks: u64) -> Result<(), FacadeError> {
        self.calls.push(Call::Schedule(id, delay_ticks));
        self.timers.insert(id, delay_ticks);
        Ok(())
    }

    fn cancel_timer(&mut self, id: TimerId) -> Result<(), FacadeError> {
        self.calls.push(Call::Cancel(id));
        self.timers.remove(&id);
        Ok(())
    }

    fn diagnostic(&mut self, level: DiagnosticLevel, message: &str) -> Result<(), FacadeError> {
        self.calls.push(Call::Diagnostic(level, message.to_owned()));
        Ok(())
    }
}

fn handler(raw: u64) -> HandlerId {
    HandlerId::new(raw).expect("test handler is nonzero")
}

fn timer(raw: u64) -> TimerId {
    TimerId::new(raw).expect("test timer is nonzero")
}

fn command_definition() -> CommandDefinition {
    let root = CommandNode::new(None, CommandNodeKind::Literal, "guard")
        .expect("valid command root")
        .with_handler(handler(7));
    CommandDefinition::new(vec![root]).expect("valid command tree")
}

fn permission_node() -> PermissionNode {
    PermissionNode::parse("ferrumc.region.guard").expect("valid permission node")
}

fn assert_capability_denied<T>(result: Result<T, FacadeError>, capability: Capability) {
    match result {
        Err(FacadeError::Capability(error)) => assert_eq!(error.capability(), capability),
        Err(error) => panic!("expected a capability denial, got {error:?}"),
        Ok(_) => panic!("expected capability {capability} to be denied"),
    }
}

fn assert_invalid_host_response<T>(result: Result<T, FacadeError>, resource: &'static str) {
    match result {
        Err(FacadeError::InvalidHostResponse {
            resource: actual, ..
        }) => assert_eq!(actual, resource),
        Err(error) => panic!("expected an invalid host response, got {error:?}"),
        Ok(_) => panic!("expected an invalid {resource} host response"),
    }
}

#[test]
fn load_and_unload_accessors_require_their_exact_capability() {
    let mut host = FakeHost::new(CapabilityManifest::all().without(Capability::ReceiveEvents));
    {
        let mut context = LoadContext::new(&mut host);
        assert_capability_denied(context.events(), Capability::ReceiveEvents);
    }
    assert!(host.calls.is_empty());

    let mut host = FakeHost::new(CapabilityManifest::all().without(Capability::RegisterCommands));
    {
        let mut context = LoadContext::new(&mut host);
        assert_capability_denied(context.commands(), Capability::RegisterCommands);
    }
    assert!(host.calls.is_empty());

    let mut host = FakeHost::new(CapabilityManifest::all().without(Capability::Storage));
    {
        let mut context = LoadContext::new(&mut host);
        assert_capability_denied(context.storage(), Capability::Storage);
    }
    assert!(host.calls.is_empty());

    let mut host = FakeHost::new(CapabilityManifest::all().without(Capability::Storage));
    {
        let mut context = UnloadContext::new(&mut host);
        assert_capability_denied(context.storage(), Capability::Storage);
    }
    assert!(host.calls.is_empty());
}

#[test]
fn event_accessors_require_their_exact_capability_without_backend_calls() {
    let mut host = FakeHost::new(CapabilityManifest::all().without(Capability::ReadWorld));
    {
        let mut context = EventContext::new(Tick::new(1), &mut host);
        assert_capability_denied(context.world(), Capability::ReadWorld);
    }
    assert!(host.calls.is_empty());

    let mut host = FakeHost::new(CapabilityManifest::all().without(Capability::SubmitIntents));
    {
        let mut context = EventContext::new(Tick::new(1), &mut host);
        assert_capability_denied(context.operations(), Capability::SubmitIntents);
    }
    assert!(host.calls.is_empty());

    let mut host = FakeHost::new(CapabilityManifest::all().without(Capability::ReadPermissions));
    {
        let mut context = EventContext::new(Tick::new(1), &mut host);
        assert_capability_denied(context.permissions(), Capability::ReadPermissions);
    }
    assert!(host.calls.is_empty());

    let mut host = FakeHost::new(CapabilityManifest::all().without(Capability::Storage));
    {
        let mut context = EventContext::new(Tick::new(1), &mut host);
        assert_capability_denied(context.storage(), Capability::Storage);
    }
    assert!(host.calls.is_empty());
}

#[test]
fn granted_facades_sequentially_reborrow_one_backend_and_route_exact_operations() {
    let player = PlayerId::offline("FacadeUser");
    let permission = permission_node();
    let command = command_definition();
    let timer = timer(9);
    let mut host = FakeHost::new(CapabilityManifest::all());
    host.storage.insert("seed".to_owned(), vec![8, 9]);

    exercise_load_facades(&mut host, &command, timer);
    exercise_event_facades(&mut host, player, &permission, timer);
    exercise_unload_facades(&mut host, timer);

    assert_eq!(
        host.calls,
        expected_facade_calls(player, permission, command, timer)
    );
}

fn exercise_load_facades(host: &mut FakeHost, command: &CommandDefinition, timer: TimerId) {
    let mut context = LoadContext::new(host);
    context
        .events()
        .expect("events granted")
        .subscribe(EventKind::PlayerJoin)
        .expect("subscription accepted");
    context
        .commands()
        .expect("commands granted")
        .register(command)
        .expect("registration accepted");
    context
        .storage()
        .expect("storage granted")
        .put("load-key", &[1, 2])
        .expect("storage write accepted");
    context.timers().schedule(timer, 4).expect("timer accepted");
    context
        .diagnostics()
        .emit(DiagnosticLevel::Info, "loaded")
        .expect("diagnostic accepted");
}

fn exercise_event_facades(
    host: &mut FakeHost,
    player: PlayerId,
    permission: &PermissionNode,
    timer: TimerId,
) {
    let mut context = EventContext::new(Tick::new(77), host);
    assert_eq!(context.tick(), Tick::new(77));
    assert!(context
        .world()
        .expect("world granted")
        .is_chunk_loaded(WORLD_CHUNK)
        .expect("world query accepted"));
    assert_eq!(
        context
            .world()
            .expect("world granted again")
            .block_state_id(WORLD_BLOCK)
            .expect("block query accepted"),
        Some(42)
    );
    assert_eq!(
        context
            .world()
            .expect("world granted again")
            .player_position(player)
            .expect("player query accepted"),
        Some(WORLD_POSITION)
    );
    context
        .operations()
        .expect("operations granted")
        .set_block(WORLD_BLOCK, 12)
        .expect("block operation accepted");
    context
        .operations()
        .expect("operations granted again")
        .teleport(player, WORLD_POSITION)
        .expect("teleport accepted");
    context
        .operations()
        .expect("operations granted again")
        .message(player, "plain")
        .expect("message accepted");
    assert_eq!(
        context
            .permissions()
            .expect("permissions granted")
            .resolve(player, permission)
            .expect("permission query accepted"),
        Resolution::Allowed
    );
    assert_eq!(
        context
            .storage()
            .expect("storage granted")
            .get("seed")
            .expect("storage read accepted"),
        Some(vec![8, 9])
    );
    context
        .timers()
        .cancel(timer)
        .expect("timer cancellation accepted");
    context
        .diagnostics()
        .emit(DiagnosticLevel::Debug, "event")
        .expect("diagnostic accepted");
}

fn exercise_unload_facades(host: &mut FakeHost, timer: TimerId) {
    let mut context = UnloadContext::new(host);
    context
        .storage()
        .expect("storage granted")
        .delete("load-key")
        .expect("storage delete accepted");
    context
        .timers()
        .cancel(timer)
        .expect("idempotent timer cancellation accepted");
    context
        .diagnostics()
        .emit(DiagnosticLevel::Warn, "unloaded")
        .expect("diagnostic accepted");
}

fn expected_facade_calls(
    player: PlayerId,
    permission: PermissionNode,
    command: CommandDefinition,
    timer: TimerId,
) -> Vec<Call> {
    vec![
        Call::Subscribe(EventKind::PlayerJoin),
        Call::Register(command),
        Call::StoragePut("load-key".to_owned(), vec![1, 2]),
        Call::Schedule(timer, 4),
        Call::Diagnostic(DiagnosticLevel::Info, "loaded".to_owned()),
        Call::IsChunkLoaded(WORLD_CHUNK),
        Call::BlockState(WORLD_BLOCK),
        Call::PlayerPosition(player),
        Call::Submit(WorldOperation::SetBlock(
            ferrumc_plugin_sdk::SetBlockOperation::new(WORLD_BLOCK, 12),
        )),
        Call::Submit(WorldOperation::Teleport(
            ferrumc_plugin_sdk::TeleportOperation::new(player, WORLD_POSITION)
                .expect("finite position"),
        )),
        Call::Submit(WorldOperation::Message(
            ferrumc_plugin_sdk::MessageOperation::new(player, "plain").expect("bounded message"),
        )),
        Call::Permission(player, permission),
        Call::StorageGet("seed".to_owned()),
        Call::Cancel(timer),
        Call::Diagnostic(DiagnosticLevel::Debug, "event".to_owned()),
        Call::StorageDelete("load-key".to_owned()),
        Call::Cancel(timer),
        Call::Diagnostic(DiagnosticLevel::Warn, "unloaded".to_owned()),
    ]
}

#[test]
fn storage_is_implicitly_namespaced_bounded_sorted_and_delete_is_idempotent() {
    let max_key = "k".repeat(MAX_STORAGE_KEY_BYTES);
    let too_long_key = "k".repeat(MAX_STORAGE_KEY_BYTES + 1);
    let max_value = vec![7; MAX_STORAGE_VALUE_BYTES];
    let too_large_value = vec![7; MAX_STORAGE_VALUE_BYTES + 1];
    let mut host = FakeHost::new(CapabilityManifest::empty().with(Capability::Storage));
    host.listed_keys = Some(vec!["b".to_owned(), "aa".to_owned(), "a".to_owned()]);

    {
        let mut context = LoadContext::new(&mut host);
        let mut storage = context.storage().expect("storage granted");
        storage
            .put(&max_key, &max_value)
            .expect("maximum key and value fit");
        assert_eq!(
            storage.get(&max_key).expect("maximum value is readable"),
            Some(max_value)
        );
        storage
            .delete(&max_key)
            .expect("present key can be deleted");
        storage
            .delete(&max_key)
            .expect("absent key deletion is idempotent");
        assert_eq!(
            storage.get(&max_key).expect("deleted key can be read"),
            None
        );
        assert_eq!(
            storage.keys().expect("valid host keys are accepted"),
            vec!["a", "aa", "b"]
        );

        assert!(matches!(
            storage.put("", b"value"),
            Err(FacadeError::InvalidInput {
                resource: "storage key",
                ..
            })
        ));
        assert!(matches!(
            storage.put(&too_long_key, b"value"),
            Err(FacadeError::LimitExceeded {
                resource: "storage key",
                ..
            })
        ));
        assert!(matches!(
            storage.put("bounded-key", &too_large_value),
            Err(FacadeError::LimitExceeded {
                resource: "storage value",
                ..
            })
        ));
    }

    assert_eq!(host.calls.len(), 6);
    assert!(matches!(
        &host.calls[0],
        Call::StoragePut(key, value)
            if key == &max_key && value.len() == MAX_STORAGE_VALUE_BYTES
    ));
    assert_eq!(
        host.calls
            .iter()
            .filter(|call| matches!(call, Call::StorageDelete(key) if key == &max_key))
            .count(),
        2
    );
    assert!(!host.storage.contains_key(&max_key));
}

#[test]
fn storage_rejects_out_of_contract_host_values_and_key_lists() {
    let mut oversized_value = FakeHost::new(CapabilityManifest::empty().with(Capability::Storage));
    oversized_value
        .storage
        .insert("key".to_owned(), vec![0; MAX_STORAGE_VALUE_BYTES + 1]);
    {
        let mut context = EventContext::new(Tick::ZERO, &mut oversized_value);
        assert_invalid_host_response(
            context.storage().expect("storage granted").get("key"),
            "storage value",
        );
    }

    let mut too_many_keys = FakeHost::new(CapabilityManifest::empty().with(Capability::Storage));
    too_many_keys.listed_keys = Some(
        (0..=MAX_STORAGE_KEYS)
            .map(|index| format!("key-{index}"))
            .collect(),
    );
    {
        let mut context = EventContext::new(Tick::ZERO, &mut too_many_keys);
        assert_invalid_host_response(
            context.storage().expect("storage granted").keys(),
            "storage key list",
        );
    }

    let mut oversized_list = FakeHost::new(CapabilityManifest::empty().with(Capability::Storage));
    oversized_list.listed_keys = Some(vec!["x".repeat(MAX_STORAGE_KEY_BYTES); MAX_STORAGE_KEYS]);
    {
        let mut context = EventContext::new(Tick::ZERO, &mut oversized_list);
        assert_invalid_host_response(
            context.storage().expect("storage granted").keys(),
            "storage key list",
        );
    }

    let mut invalid_key = FakeHost::new(CapabilityManifest::empty().with(Capability::Storage));
    invalid_key.listed_keys = Some(vec![String::new()]);
    {
        let mut context = EventContext::new(Tick::ZERO, &mut invalid_key);
        assert_invalid_host_response(
            context.storage().expect("storage granted").keys(),
            "storage key list",
        );
    }
}

#[test]
fn world_operations_validate_values_before_the_bounded_backend() {
    let player = PlayerId::offline("OperationUser");
    let plain = r#"<red>{"text":"not JSON"} & unchanged"#;
    let maximum_message = "m".repeat(MAX_MESSAGE_BYTES);
    let oversized_message = "m".repeat(MAX_MESSAGE_BYTES + 1);
    let capabilities = CapabilityManifest::empty().with(Capability::SubmitIntents);
    let mut host = FakeHost::with_operation_limit(capabilities, 4);

    {
        let mut context = EventContext::new(Tick::new(40), &mut host);
        let mut operations = context.operations().expect("operations granted");
        operations
            .set_block(WORLD_BLOCK, 55)
            .expect("block operation accepted");
        operations
            .teleport(player, WORLD_POSITION)
            .expect("finite teleport accepted");
        operations
            .message(player, plain)
            .expect("plain text accepted unchanged");
        operations
            .message(player, maximum_message)
            .expect("maximum message accepted");
        assert_eq!(
            operations.set_block(BlockPos::ORIGIN, 1),
            Err(FacadeError::BufferFull)
        );

        for position in [
            Vec3::new(f64::NAN, 0.0, 0.0),
            Vec3::new(0.0, f64::INFINITY, 0.0),
            Vec3::new(0.0, 0.0, f64::NEG_INFINITY),
        ] {
            assert!(matches!(
                operations.teleport(player, position),
                Err(FacadeError::InvalidInput {
                    resource: "teleport position",
                    ..
                })
            ));
        }
        assert!(matches!(
            operations.message(player, oversized_message),
            Err(FacadeError::LimitExceeded {
                resource: "message",
                ..
            })
        ));
    }

    assert_eq!(host.submit_attempts, 5);
    assert_eq!(host.accepted_operations.len(), 4);
    assert!(matches!(
        &host.accepted_operations[0],
        WorldOperation::SetBlock(operation)
            if operation.pos() == WORLD_BLOCK && operation.block_state_id() == 55
    ));
    assert!(matches!(
        &host.accepted_operations[1],
        WorldOperation::Teleport(operation)
            if operation.player() == player && operation.position() == WORLD_POSITION
    ));
    assert!(matches!(
        &host.accepted_operations[2],
        WorldOperation::Message(operation)
            if operation.player() == player && operation.message() == plain
    ));
    assert!(matches!(
        &host.accepted_operations[3],
        WorldOperation::Message(operation) if operation.message().len() == MAX_MESSAGE_BYTES
    ));
}

#[test]
fn timers_are_nonzero_tick_delays_and_diagnostics_are_bounded() {
    assert_eq!(TimerId::new(0), None);
    let timer = timer(5);
    let mut host = FakeHost::new(CapabilityManifest::empty());

    {
        let mut context = EventContext::new(Tick::new(123), &mut host);
        assert_eq!(context.tick(), Tick::new(123));
        assert!(matches!(
            context.timers().schedule(timer, 0),
            Err(FacadeError::InvalidInput {
                resource: "timer delay",
                ..
            })
        ));
        context
            .timers()
            .schedule(timer, 20)
            .expect("positive tick delay accepted");
        context
            .timers()
            .schedule(timer, u64::MAX)
            .expect("maximum tick delay accepted");
        context
            .timers()
            .cancel(timer)
            .expect("timer cancellation accepted");
        context
            .timers()
            .cancel(timer)
            .expect("timer cancellation is idempotent");

        context
            .diagnostics()
            .emit(DiagnosticLevel::Trace, &"d".repeat(MAX_DIAGNOSTIC_BYTES))
            .expect("maximum diagnostic accepted");
        assert!(matches!(
            context.diagnostics().emit(
                DiagnosticLevel::Error,
                &"d".repeat(MAX_DIAGNOSTIC_BYTES + 1)
            ),
            Err(FacadeError::LimitExceeded {
                resource: "diagnostic",
                ..
            })
        ));
    }

    assert_eq!(
        host.calls
            .iter()
            .filter(|call| matches!(call, Call::Schedule(..)))
            .count(),
        2
    );
    assert_eq!(
        host.calls
            .iter()
            .filter(|call| matches!(call, Call::Cancel(..)))
            .count(),
        2
    );
    assert_eq!(
        host.calls
            .iter()
            .filter(|call| matches!(call, Call::Diagnostic(..)))
            .count(),
        1
    );
    assert!(host.timers.is_empty());
}

struct ContractPlugin {
    callbacks: usize,
}

impl Plugin for ContractPlugin {
    const DECLARATION: PluginDeclaration = PluginDeclaration::new(
        "contract-plugin",
        "Contract Plugin",
        PluginVersion::new(1, 2, 3),
        CapabilityManifest::all(),
    );

    fn create() -> Self {
        Self { callbacks: 0 }
    }

    fn on_load(&mut self, _context: &mut LoadContext<'_>) -> Result<(), PluginError> {
        self.callbacks += 1;
        Ok(())
    }

    fn on_event(
        &mut self,
        _event: &Event,
        _context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        self.callbacks += 1;
        Ok(())
    }

    fn before_block_place(
        &mut self,
        _attempt: &PlaceAttempt,
        _context: &mut EventContext<'_>,
    ) -> Result<BlockDecision, PluginError> {
        self.callbacks += 1;
        Ok(BlockDecision::Replace(99))
    }

    fn before_block_break(
        &mut self,
        _attempt: &BlockEvent,
        _context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        self.callbacks += 1;
        Ok(EventDecision::Deny(None))
    }

    fn before_chat(
        &mut self,
        _attempt: &ChatAttempt,
        _context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        self.callbacks += 1;
        Ok(EventDecision::Deny(None))
    }

    fn before_interact(
        &mut self,
        _attempt: &InteractionAttempt,
        _context: &mut EventContext<'_>,
    ) -> Result<EventDecision, PluginError> {
        self.callbacks += 1;
        Ok(EventDecision::Allow)
    }

    fn on_command(
        &mut self,
        _invocation: &CommandInvocation,
        _context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        self.callbacks += 1;
        Ok(())
    }

    fn on_timer(
        &mut self,
        _timer: TimerId,
        _context: &mut EventContext<'_>,
    ) -> Result<(), PluginError> {
        self.callbacks += 1;
        Ok(())
    }

    fn on_unload(&mut self, _context: &mut UnloadContext<'_>) -> Result<(), PluginError> {
        self.callbacks += 1;
        Ok(())
    }
}

#[test]
fn plugin_declaration_and_every_callback_share_the_public_contract() {
    assert_eq!(ContractPlugin::DECLARATION.id(), "contract-plugin");
    assert_eq!(ContractPlugin::DECLARATION.version().major(), 1);
    assert_eq!(ContractPlugin::DECLARATION.validate(), Ok(()));

    let player = PlayerId::offline("PluginUser");
    let block = BlockEvent::new(player, WORLD_BLOCK);
    let place = PlaceAttempt::new(player, WORLD_BLOCK, 12);
    let chat = ChatAttempt::new(player, "hello").expect("bounded chat");
    let interaction = InteractionAttempt::new(player, InteractHand::Main, InteractTarget::Air);
    let invocation =
        CommandInvocation::new(handler(11), player, Vec::new()).expect("bounded invocation");
    let timer = timer(12);
    let mut plugin = ContractPlugin::create();
    let mut host = FakeHost::new(CapabilityManifest::all());

    {
        let mut context = LoadContext::new(&mut host);
        plugin.on_load(&mut context).expect("load callback");
    }
    {
        let mut context = EventContext::new(Tick::new(8), &mut host);
        plugin
            .on_event(&Event::PlayerJoin(PlayerEvent::new(player)), &mut context)
            .expect("event callback");
        assert_eq!(
            plugin
                .before_block_place(&place, &mut context)
                .expect("place callback"),
            BlockDecision::Replace(99)
        );
        assert_eq!(
            plugin
                .before_block_break(&block, &mut context)
                .expect("break callback"),
            EventDecision::Deny(None)
        );
        assert_eq!(
            plugin
                .before_chat(&chat, &mut context)
                .expect("chat callback"),
            EventDecision::Deny(None)
        );
        assert_eq!(
            plugin
                .before_interact(&interaction, &mut context)
                .expect("interaction callback"),
            EventDecision::Allow
        );
        plugin
            .on_command(&invocation, &mut context)
            .expect("command callback");
        plugin
            .on_timer(timer, &mut context)
            .expect("timer callback");
    }
    {
        let mut context = UnloadContext::new(&mut host);
        plugin.on_unload(&mut context).expect("unload callback");
    }

    assert_eq!(plugin.callbacks, 9);
    assert!(host.calls.is_empty());
}
