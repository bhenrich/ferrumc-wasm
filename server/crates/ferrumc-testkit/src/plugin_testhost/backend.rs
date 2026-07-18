//! Shared deterministic state and fresh per-callback transaction stages.

use std::collections::{BTreeMap, BTreeSet};

use ferrumc_plugin_abi::{
    FcStatus, FC_CAPABILITY_DENIED, FC_COMMAND_BUFFER_FULL, FC_DIAGNOSTIC_DEBUG,
    FC_DIAGNOSTIC_ERROR, FC_DIAGNOSTIC_INFO, FC_DIAGNOSTIC_TRACE, FC_DIAGNOSTIC_WARN, FC_ERROR,
    FC_INVALID_ARGUMENT, FC_OK,
};
use ferrumc_plugin_abi_sys::{
    HostCallOutcome, HostServices as AbiHostServices, OwnedCommand, OwnedHostRequest,
};
use ferrumc_plugin_sdk::{
    BlockDecision, BlockPos, Capability, CapabilityManifest, ChunkPos, CommandDefinition,
    DiagnosticLevel, EventDecision, EventKind, FacadeError, HostServices, PermissionNode, PlayerId,
    Resolution, Tick, TimerId, Vec3, WorldOperation, MAX_DIAGNOSTIC_BYTES, MAX_STORAGE_KEYS,
    MAX_STORAGE_KEY_BYTES, MAX_STORAGE_KEY_LIST_BYTES, MAX_STORAGE_VALUE_BYTES,
};

use super::codec::{self, AbiCommand, AbiRequest};
use super::{
    PermissionSetting, PluginDiagnostic, PluginDiagnosticPhase, PluginEffect, PluginFailureKind,
    PluginStateSnapshot, ScheduledTimer, StorageEntry,
};

pub(crate) const DEFAULT_DIMENSION_HANDLE: u64 = 0x4652_4d43;
const MAX_DIAGNOSTICS_PER_CALLBACK: usize = 64;
const MAX_DIAGNOSTIC_BYTES_PER_CALLBACK: usize =
    MAX_DIAGNOSTICS_PER_CALLBACK * MAX_DIAGNOSTIC_BYTES;

#[derive(Clone, Debug)]
pub(crate) struct SeedState {
    pub(crate) loaded_chunks: BTreeSet<ChunkPos>,
    pub(crate) blocks: BTreeMap<BlockPos, u32>,
    pub(crate) player_positions: BTreeMap<PlayerId, Vec3>,
    pub(crate) permissions: Vec<PermissionSetting>,
    pub(crate) storage: BTreeMap<String, Vec<u8>>,
}

impl SeedState {
    pub(crate) fn empty() -> Self {
        Self {
            loaded_chunks: BTreeSet::new(),
            blocks: BTreeMap::new(),
            player_positions: BTreeMap::new(),
            permissions: Vec::new(),
            storage: BTreeMap::new(),
        }
    }
}

pub(crate) struct RuntimeState {
    loaded_chunks: BTreeSet<ChunkPos>,
    blocks: BTreeMap<BlockPos, u32>,
    player_positions: BTreeMap<PlayerId, Vec3>,
    permissions: Vec<PermissionSetting>,
    storage: BTreeMap<String, Vec<u8>>,
    timers: BTreeMap<TimerId, Tick>,
    subscriptions: BTreeSet<EventKind>,
    commands: Vec<CommandDefinition>,
    messages: Vec<ferrumc_plugin_sdk::MessageOperation>,
    effects: Vec<PluginEffect>,
    diagnostics: Vec<PluginDiagnostic>,
}

impl RuntimeState {
    pub(crate) fn from_seed(seed: &SeedState) -> Self {
        Self {
            loaded_chunks: seed.loaded_chunks.clone(),
            blocks: seed.blocks.clone(),
            player_positions: seed.player_positions.clone(),
            permissions: seed.permissions.clone(),
            storage: seed.storage.clone(),
            timers: BTreeMap::new(),
            subscriptions: BTreeSet::new(),
            commands: Vec::new(),
            messages: Vec::new(),
            effects: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    pub(crate) fn is_subscribed(&self, kind: EventKind) -> bool {
        self.subscriptions.contains(&kind)
    }

    pub(crate) fn retain_diagnostics(&mut self, diagnostics: Vec<PluginDiagnostic>) {
        self.diagnostics.extend(diagnostics);
    }

    pub(crate) fn commit(&mut self, staged: Vec<PluginEffect>) {
        for effect in &staged {
            match effect {
                PluginEffect::SubscribeEvent(kind) => {
                    self.subscriptions.insert(*kind);
                }
                PluginEffect::RegisterCommand(command) => self.commands.push(command.clone()),
                PluginEffect::SetBlock {
                    pos,
                    block_state_id,
                } => {
                    self.blocks.insert(*pos, *block_state_id);
                }
                PluginEffect::Teleport(operation) => {
                    self.player_positions
                        .insert(operation.player(), operation.position());
                }
                PluginEffect::Message(operation) => self.messages.push(operation.clone()),
                PluginEffect::StoragePut { key, value } => {
                    self.storage.insert(key.clone(), value.clone());
                }
                PluginEffect::StorageDelete { key } => {
                    self.storage.remove(key);
                }
                PluginEffect::ScheduleTimer { id, due_tick } => {
                    self.timers.insert(*id, *due_tick);
                }
                PluginEffect::CancelTimer { id } => {
                    self.timers.remove(id);
                }
                PluginEffect::BlockDecision(_) | PluginEffect::EventDecision { .. } => {}
            }
        }
        self.effects.extend(staged);
    }

    pub(crate) fn report(&self) -> Result<super::PluginRun, &'static str> {
        let snapshot = PluginStateSnapshot {
            loaded_chunks: self.loaded_chunks.iter().copied().collect(),
            blocks: self
                .blocks
                .iter()
                .map(|(pos, block)| (*pos, *block))
                .collect(),
            player_positions: self
                .player_positions
                .iter()
                .map(|(player, position)| (*player, *position))
                .collect(),
            permissions: self.permissions.clone(),
            storage: self
                .storage
                .iter()
                .map(|(key, value)| StorageEntry::new(key.clone(), value.clone()))
                .collect(),
            timers: self
                .timers
                .iter()
                .map(|(id, tick)| ScheduledTimer::new(*id, *tick))
                .collect(),
            subscriptions: self.subscriptions.iter().copied().collect(),
            commands: self.commands.clone(),
            messages: self.messages.clone(),
        };
        super::PluginRun::new(self.effects.clone(), self.diagnostics.clone(), snapshot)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FramePhase {
    Load,
    Event,
    Unload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecisionRoute {
    None,
    BlockPlace,
    Event(EventKind),
}

pub(crate) struct CallbackFrame<'state> {
    state: &'state RuntimeState,
    capabilities: CapabilityManifest,
    phase: FramePhase,
    diagnostic_phase: PluginDiagnosticPhase,
    tick: Tick,
    capacity: usize,
    dimension_handle: u64,
    decision_route: DecisionRoute,
    decision_count: usize,
    staged: Vec<PluginEffect>,
    diagnostics: Vec<PluginDiagnostic>,
    diagnostic_bytes: usize,
    failure: Option<PluginFailureKind>,
}

impl<'state> CallbackFrame<'state> {
    pub(crate) fn new(
        state: &'state RuntimeState,
        capabilities: CapabilityManifest,
        phase: FramePhase,
        tick: Tick,
        capacity: usize,
        dimension_handle: u64,
        event_kind: Option<EventKind>,
    ) -> Self {
        let decision_route = match event_kind {
            Some(EventKind::BlockPlaceAttempt) => DecisionRoute::BlockPlace,
            Some(
                kind @ (EventKind::BlockBreakAttempt
                | EventKind::ChatAttempt
                | EventKind::InteractAttempt),
            ) => DecisionRoute::Event(kind),
            _ => DecisionRoute::None,
        };
        let diagnostic_phase = match phase {
            FramePhase::Load => PluginDiagnosticPhase::Load,
            FramePhase::Event => PluginDiagnosticPhase::Event(tick),
            FramePhase::Unload => PluginDiagnosticPhase::Unload,
        };
        Self {
            state,
            capabilities,
            phase,
            diagnostic_phase,
            tick,
            capacity,
            dimension_handle,
            decision_route,
            decision_count: 0,
            staged: Vec::with_capacity(capacity),
            diagnostics: Vec::new(),
            diagnostic_bytes: 0,
            failure: None,
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Vec<PluginEffect>,
        Vec<PluginDiagnostic>,
        Option<PluginFailureKind>,
    ) {
        (self.staged, self.diagnostics, self.failure)
    }

    pub(crate) fn admit_builtin_decision(
        &mut self,
        outcome: ferrumc_plugin_sdk_builtin::CallbackOutcome,
    ) -> Result<(), PluginFailureKind> {
        if !matches!(
            outcome,
            ferrumc_plugin_sdk_builtin::CallbackOutcome::Complete
                | ferrumc_plugin_sdk_builtin::CallbackOutcome::BlockDecision(_)
                | ferrumc_plugin_sdk_builtin::CallbackOutcome::EventDecision(_)
        ) {
            return Err(PluginFailureKind::UnsupportedSemanticValue(
                "built-in callback outcome",
            ));
        }
        match (self.decision_route, outcome) {
            (DecisionRoute::None, ferrumc_plugin_sdk_builtin::CallbackOutcome::Complete) => Ok(()),
            (
                DecisionRoute::BlockPlace,
                ferrumc_plugin_sdk_builtin::CallbackOutcome::BlockDecision(decision),
            ) => self.admit_decision(PluginEffect::BlockDecision(decision)),
            (
                DecisionRoute::Event(kind),
                ferrumc_plugin_sdk_builtin::CallbackOutcome::EventDecision(decision),
            ) => self.admit_decision(PluginEffect::EventDecision { kind, decision }),
            (DecisionRoute::BlockPlace | DecisionRoute::Event(_), _) => {
                Err(PluginFailureKind::MissingDecision)
            }
            (DecisionRoute::None, _) => Err(PluginFailureKind::WrongDecision),
        }
    }

    pub(crate) fn validate_decision(&self) -> Result<(), PluginFailureKind> {
        match (self.decision_route, self.decision_count) {
            (DecisionRoute::None, 0) | (DecisionRoute::BlockPlace | DecisionRoute::Event(_), 1) => {
                Ok(())
            }
            (DecisionRoute::None, _) => Err(PluginFailureKind::WrongDecision),
            (DecisionRoute::BlockPlace | DecisionRoute::Event(_), 0) => {
                Err(PluginFailureKind::MissingDecision)
            }
            (DecisionRoute::BlockPlace | DecisionRoute::Event(_), _) => {
                Err(PluginFailureKind::DuplicateDecision)
            }
        }
    }

    fn admit(&mut self, effect: PluginEffect) -> Result<(), FacadeError> {
        match &effect {
            PluginEffect::SubscribeEvent(kind) if !known_event_kind(*kind) => {
                self.record_failure(PluginFailureKind::UnsupportedSemanticValue(
                    "event subscription kind",
                ));
                return Err(FacadeError::Unavailable {
                    operation: "future event subscription kind",
                });
            }
            PluginEffect::RegisterCommand(command) if !known_command(command) => {
                self.record_failure(PluginFailureKind::UnsupportedSemanticValue(
                    "command node kind",
                ));
                return Err(FacadeError::Unavailable {
                    operation: "future command node kind",
                });
            }
            PluginEffect::StoragePut { key, value } => {
                self.validate_storage_projection(Some((key, value.len())), None)?;
            }
            PluginEffect::StorageDelete { key } => {
                self.validate_storage_projection(None, Some(key))?;
            }
            _ => {}
        }
        if self.staged.len() >= self.capacity {
            self.record_failure(PluginFailureKind::BufferFull);
            return Err(FacadeError::BufferFull);
        }
        self.staged.push(effect);
        Ok(())
    }

    fn admit_decision(&mut self, effect: PluginEffect) -> Result<(), PluginFailureKind> {
        if !known_decision_effect(&effect) {
            self.record_failure(PluginFailureKind::UnsupportedSemanticValue(
                "decision value",
            ));
            return Err(PluginFailureKind::UnsupportedSemanticValue(
                "decision value",
            ));
        }
        if self.decision_count != 0 {
            self.record_failure(PluginFailureKind::DuplicateDecision);
            return Err(PluginFailureKind::DuplicateDecision);
        }
        if self.staged.len() >= self.capacity {
            self.record_failure(PluginFailureKind::BufferFull);
            return Err(PluginFailureKind::BufferFull);
        }
        self.staged.push(effect);
        self.decision_count = 1;
        Ok(())
    }

    fn record_failure(&mut self, failure: PluginFailureKind) {
        if self.failure.is_none() {
            self.failure = Some(failure);
        }
    }

    fn require_capability(&mut self, capability: Capability) -> Result<(), FacadeError> {
        if self.capabilities.grants(capability) {
            Ok(())
        } else {
            self.record_failure(PluginFailureKind::CapabilityDenied(capability));
            self.capabilities.require(capability).map_err(Into::into)
        }
    }

    fn require_phase(
        &mut self,
        expected: FramePhase,
        operation: &'static str,
    ) -> Result<(), FacadeError> {
        if self.phase == expected {
            Ok(())
        } else {
            self.record_failure(PluginFailureKind::AbiProtocol(format!(
                "{operation} is not valid in this callback phase"
            )));
            Err(FacadeError::Unavailable { operation })
        }
    }

    fn due_tick(&mut self, delay: u64) -> Result<Tick, FacadeError> {
        self.tick.checked_add(delay).ok_or_else(|| {
            self.record_failure(PluginFailureKind::TimerOverflow);
            FacadeError::InvalidInput {
                resource: "timer delay",
                reason: "absolute due tick overflowed",
            }
        })
    }

    fn diagnostic_inner(
        &mut self,
        level: DiagnosticLevel,
        message: &str,
    ) -> Result<(), FacadeError> {
        if !matches!(
            level,
            DiagnosticLevel::Error
                | DiagnosticLevel::Warn
                | DiagnosticLevel::Info
                | DiagnosticLevel::Debug
                | DiagnosticLevel::Trace
        ) {
            self.record_failure(PluginFailureKind::UnsupportedSemanticValue(
                "diagnostic level",
            ));
            return Err(FacadeError::Unavailable {
                operation: "future diagnostic level",
            });
        }
        if message.len() > MAX_DIAGNOSTIC_BYTES {
            self.record_failure(PluginFailureKind::AbiProtocol(
                "diagnostic exceeds the shared SDK byte limit".to_owned(),
            ));
            return Err(FacadeError::LimitExceeded {
                resource: "diagnostic",
                len: message.len(),
                max: MAX_DIAGNOSTIC_BYTES,
            });
        }
        let Some(total) = self.diagnostic_bytes.checked_add(message.len()) else {
            self.record_failure(PluginFailureKind::BufferFull);
            return Err(FacadeError::BufferFull);
        };
        if self.diagnostics.len() >= MAX_DIAGNOSTICS_PER_CALLBACK
            || total > MAX_DIAGNOSTIC_BYTES_PER_CALLBACK
        {
            self.record_failure(PluginFailureKind::BufferFull);
            return Err(FacadeError::BufferFull);
        }
        self.diagnostic_bytes = total;
        self.diagnostics.push(PluginDiagnostic::new(
            self.diagnostic_phase,
            level,
            message.to_owned(),
        ));
        Ok(())
    }
}

impl HostServices for CallbackFrame<'_> {
    fn capabilities(&self) -> CapabilityManifest {
        self.capabilities
    }

    fn subscribe_event(&mut self, kind: EventKind) -> Result<(), FacadeError> {
        self.require_phase(FramePhase::Load, "event subscription")?;
        self.require_capability(Capability::ReceiveEvents)?;
        self.admit(PluginEffect::SubscribeEvent(kind))
    }

    fn register_command(&mut self, command: &CommandDefinition) -> Result<(), FacadeError> {
        self.require_phase(FramePhase::Load, "command registration")?;
        self.require_capability(Capability::RegisterCommands)?;
        self.admit(PluginEffect::RegisterCommand(command.clone()))
    }

    fn is_chunk_loaded(&mut self, chunk: ChunkPos) -> Result<bool, FacadeError> {
        self.require_phase(FramePhase::Event, "world query")?;
        self.require_capability(Capability::ReadWorld)?;
        Ok(self.state.loaded_chunks.contains(&chunk))
    }

    fn block_state_id(&mut self, pos: BlockPos) -> Result<Option<u32>, FacadeError> {
        self.require_phase(FramePhase::Event, "world query")?;
        self.require_capability(Capability::ReadWorld)?;
        if !self.state.loaded_chunks.contains(&pos.to_chunk_pos()) {
            return Ok(None);
        }
        Ok(Some(self.state.blocks.get(&pos).copied().unwrap_or(0)))
    }

    fn player_position(&mut self, player: PlayerId) -> Result<Option<Vec3>, FacadeError> {
        self.require_phase(FramePhase::Event, "world query")?;
        self.require_capability(Capability::ReadWorld)?;
        Ok(self.state.player_positions.get(&player).copied())
    }

    fn submit_world_operation(&mut self, operation: WorldOperation) -> Result<(), FacadeError> {
        self.require_phase(FramePhase::Event, "world operation")?;
        self.require_capability(Capability::SubmitIntents)?;
        match operation {
            WorldOperation::SetBlock(operation) => self.admit(PluginEffect::SetBlock {
                pos: operation.pos(),
                block_state_id: operation.block_state_id(),
            }),
            WorldOperation::Teleport(operation) => self.admit(PluginEffect::Teleport(operation)),
            WorldOperation::Message(operation) => self.admit(PluginEffect::Message(operation)),
            _ => Err(FacadeError::Unavailable {
                operation: "future world operation",
            }),
        }
    }

    fn resolve_permission(
        &mut self,
        player: PlayerId,
        node: &PermissionNode,
    ) -> Result<Resolution, FacadeError> {
        self.require_phase(FramePhase::Event, "permission query")?;
        self.require_capability(Capability::ReadPermissions)?;
        Ok(permission_resolution(&self.state.permissions, player, node))
    }

    fn storage_get(&mut self, key: &str) -> Result<Option<Vec<u8>>, FacadeError> {
        self.require_capability(Capability::Storage)?;
        Ok(self.state.storage.get(key).cloned())
    }

    fn storage_put(&mut self, key: &str, value: &[u8]) -> Result<(), FacadeError> {
        self.require_capability(Capability::Storage)?;
        self.admit(PluginEffect::StoragePut {
            key: key.to_owned(),
            value: value.to_vec(),
        })
    }

    fn storage_delete(&mut self, key: &str) -> Result<(), FacadeError> {
        self.require_capability(Capability::Storage)?;
        self.admit(PluginEffect::StorageDelete {
            key: key.to_owned(),
        })
    }

    fn storage_keys(&mut self) -> Result<Vec<String>, FacadeError> {
        self.require_capability(Capability::Storage)?;
        Ok(self.state.storage.keys().cloned().collect())
    }

    fn schedule_timer(&mut self, id: TimerId, delay_ticks: u64) -> Result<(), FacadeError> {
        let due_tick = self.due_tick(delay_ticks)?;
        self.admit(PluginEffect::ScheduleTimer { id, due_tick })
    }

    fn cancel_timer(&mut self, id: TimerId) -> Result<(), FacadeError> {
        self.admit(PluginEffect::CancelTimer { id })
    }

    fn diagnostic(&mut self, level: DiagnosticLevel, message: &str) -> Result<(), FacadeError> {
        self.diagnostic_inner(level, message)
    }
}

impl AbiHostServices for CallbackFrame<'_> {
    fn call(&mut self, request: OwnedHostRequest) -> HostCallOutcome {
        let request = match codec::decode_request(
            &request,
            self.phase,
            self.capabilities,
            self.dimension_handle,
        ) {
            Ok(request) => request,
            Err((status, failure)) => {
                self.record_failure(failure);
                return HostCallOutcome::Status(status);
            }
        };
        match self.answer_request(request) {
            Ok(bytes) => HostCallOutcome::Response(bytes),
            Err((status, failure)) => {
                self.record_failure(failure);
                HostCallOutcome::Status(status)
            }
        }
    }

    fn emit(&mut self, command: OwnedCommand) -> FcStatus {
        let command = match codec::decode_command(
            &command,
            self.phase,
            self.capabilities,
            self.dimension_handle,
        ) {
            Ok(command) => command,
            Err((status, failure)) => {
                self.record_failure(failure);
                return status;
            }
        };
        let result = self.admit_abi_command(command);
        match result {
            Ok(()) => FC_OK,
            Err(failure @ PluginFailureKind::BufferFull) => {
                self.record_failure(failure);
                FC_COMMAND_BUFFER_FULL
            }
            Err(failure @ PluginFailureKind::CapabilityDenied(_)) => {
                self.record_failure(failure);
                FC_CAPABILITY_DENIED
            }
            Err(failure) => {
                self.record_failure(failure);
                FC_INVALID_ARGUMENT
            }
        }
    }

    fn diagnostic(&mut self, level: u32, message: String) -> FcStatus {
        let level = match level {
            FC_DIAGNOSTIC_ERROR => DiagnosticLevel::Error,
            FC_DIAGNOSTIC_WARN => DiagnosticLevel::Warn,
            FC_DIAGNOSTIC_INFO => DiagnosticLevel::Info,
            FC_DIAGNOSTIC_DEBUG => DiagnosticLevel::Debug,
            FC_DIAGNOSTIC_TRACE => DiagnosticLevel::Trace,
            _ => {
                self.record_failure(PluginFailureKind::AbiProtocol(
                    "diagnostic severity is unknown".to_owned(),
                ));
                return FC_INVALID_ARGUMENT;
            }
        };
        match self.diagnostic_inner(level, &message) {
            Ok(()) => FC_OK,
            Err(FacadeError::BufferFull) => FC_COMMAND_BUFFER_FULL,
            Err(_) => FC_INVALID_ARGUMENT,
        }
    }
}

impl CallbackFrame<'_> {
    fn answer_request(
        &mut self,
        request: AbiRequest,
    ) -> Result<Vec<u8>, (FcStatus, PluginFailureKind)> {
        match request {
            AbiRequest::Dimension => Ok(self.dimension_handle.to_le_bytes().to_vec()),
            AbiRequest::ChunkLoaded(chunk) => {
                Ok(vec![u8::from(self.state.loaded_chunks.contains(&chunk))])
            }
            AbiRequest::BlockState(pos) => {
                if !self.state.loaded_chunks.contains(&pos.to_chunk_pos()) {
                    return Err((
                        FC_ERROR,
                        PluginFailureKind::AbiProtocol(
                            "block-state request targeted an unloaded chunk".to_owned(),
                        ),
                    ));
                }
                Ok(self
                    .state
                    .blocks
                    .get(&pos)
                    .copied()
                    .unwrap_or(0)
                    .to_le_bytes()
                    .to_vec())
            }
            AbiRequest::PlayerPosition(player) => {
                let mut bytes = Vec::with_capacity(25);
                if let Some(position) = self.state.player_positions.get(&player) {
                    bytes.push(1);
                    codec::push_vec3(&mut bytes, *position);
                } else {
                    bytes.push(0);
                }
                Ok(bytes)
            }
            AbiRequest::Permission(player, node) => {
                Ok(vec![
                    match permission_resolution(&self.state.permissions, player, &node) {
                        Resolution::Unset => 0,
                        Resolution::Allowed => 1,
                        Resolution::Denied => 2,
                    },
                ])
            }
            AbiRequest::StorageGet(key) => {
                let mut bytes = Vec::new();
                if let Some(value) = self.state.storage.get(&key) {
                    bytes.push(1);
                    codec::push_bytes(&mut bytes, value).map_err(|reason| {
                        (FC_ERROR, PluginFailureKind::AbiProtocol(reason.to_owned()))
                    })?;
                } else {
                    bytes.push(0);
                }
                Ok(bytes)
            }
            AbiRequest::StorageKeys => {
                let mut bytes = Vec::new();
                let count = u32::try_from(self.state.storage.len()).map_err(|_| {
                    (
                        FC_ERROR,
                        PluginFailureKind::AbiProtocol(
                            "storage key count is not ABI-representable".to_owned(),
                        ),
                    )
                })?;
                bytes.extend_from_slice(&count.to_le_bytes());
                for key in self.state.storage.keys() {
                    codec::push_text(&mut bytes, key).map_err(|reason| {
                        (FC_ERROR, PluginFailureKind::AbiProtocol(reason.to_owned()))
                    })?;
                }
                Ok(bytes)
            }
        }
    }

    fn admit_abi_command(&mut self, command: AbiCommand) -> Result<(), PluginFailureKind> {
        match command {
            AbiCommand::Effect(effect) => self.admit(effect).map_err(|error| match error {
                FacadeError::BufferFull => PluginFailureKind::BufferFull,
                FacadeError::Capability(error) => {
                    PluginFailureKind::CapabilityDenied(error.capability())
                }
                _ => PluginFailureKind::AbiProtocol(error.to_string()),
            }),
            AbiCommand::ScheduleTimer { id, delay } => {
                let due_tick = self
                    .tick
                    .checked_add(delay)
                    .ok_or(PluginFailureKind::TimerOverflow)?;
                self.admit(PluginEffect::ScheduleTimer { id, due_tick })
                    .map_err(|_| PluginFailureKind::BufferFull)
            }
            AbiCommand::BlockDecision(decision) => {
                if self.decision_route != DecisionRoute::BlockPlace {
                    return Err(PluginFailureKind::WrongDecision);
                }
                if !self.capabilities.grants(Capability::VetoBlockEdits) {
                    return Err(PluginFailureKind::CapabilityDenied(
                        Capability::VetoBlockEdits,
                    ));
                }
                self.admit_decision(PluginEffect::BlockDecision(decision))
            }
            AbiCommand::EventDecision(decision) => match self.decision_route {
                DecisionRoute::BlockPlace => {
                    if !self.capabilities.grants(Capability::VetoBlockEdits) {
                        return Err(PluginFailureKind::CapabilityDenied(
                            Capability::VetoBlockEdits,
                        ));
                    }
                    let block = match decision {
                        EventDecision::Allow => BlockDecision::Allow,
                        EventDecision::Deny(feedback) => BlockDecision::Deny(feedback),
                        _ => return Err(PluginFailureKind::WrongDecision),
                    };
                    self.admit_decision(PluginEffect::BlockDecision(block))
                }
                DecisionRoute::Event(kind) => {
                    let required = match kind {
                        EventKind::BlockBreakAttempt => Capability::VetoBlockEdits,
                        EventKind::ChatAttempt | EventKind::InteractAttempt => {
                            Capability::VetoEvents
                        }
                        _ => return Err(PluginFailureKind::WrongDecision),
                    };
                    if !self.capabilities.grants(required) {
                        return Err(PluginFailureKind::CapabilityDenied(required));
                    }
                    self.admit_decision(PluginEffect::EventDecision { kind, decision })
                }
                DecisionRoute::None => Err(PluginFailureKind::WrongDecision),
            },
        }
    }

    fn validate_storage_projection(
        &mut self,
        put: Option<(&str, usize)>,
        delete: Option<&str>,
    ) -> Result<(), FacadeError> {
        let mut projected: BTreeMap<String, usize> = self
            .state
            .storage
            .iter()
            .map(|(key, value)| (key.clone(), value.len()))
            .collect();
        for effect in &self.staged {
            match effect {
                PluginEffect::StoragePut { key, value } => {
                    projected.insert(key.clone(), value.len());
                }
                PluginEffect::StorageDelete { key } => {
                    projected.remove(key);
                }
                _ => {}
            }
        }
        if let Some(key) = delete {
            projected.remove(key);
        }
        if let Some((key, value_len)) = put {
            projected.insert(key.to_owned(), value_len);
        }
        let key_bytes = projected
            .keys()
            .try_fold(0usize, |total, key| {
                total.checked_add(4)?.checked_add(key.len())
            })
            .ok_or_else(|| {
                self.record_failure(PluginFailureKind::BufferFull);
                FacadeError::BufferFull
            })?;
        if projected.len() > MAX_STORAGE_KEYS || key_bytes > MAX_STORAGE_KEY_LIST_BYTES {
            self.record_failure(PluginFailureKind::BufferFull);
            return Err(FacadeError::BufferFull);
        }
        Ok(())
    }
}

pub(crate) fn validate_seed_storage(key: &str, value: &[u8]) -> Result<(), String> {
    if key.is_empty() {
        return Err("key must not be empty".to_owned());
    }
    if key.len() > MAX_STORAGE_KEY_BYTES {
        return Err(format!(
            "key length {} exceeds {MAX_STORAGE_KEY_BYTES}",
            key.len()
        ));
    }
    if value.len() > MAX_STORAGE_VALUE_BYTES {
        return Err(format!(
            "value length {} exceeds {MAX_STORAGE_VALUE_BYTES}",
            value.len()
        ));
    }
    Ok(())
}

pub(crate) fn validate_seed_storage_set(
    storage: &BTreeMap<String, Vec<u8>>,
    key: &str,
    value: &[u8],
) -> Result<(), String> {
    validate_seed_storage(key, value)?;
    let mut count = storage.len();
    if !storage.contains_key(key) {
        count = count
            .checked_add(1)
            .ok_or_else(|| "storage key count overflowed".to_owned())?;
    }
    if count > MAX_STORAGE_KEYS {
        return Err(format!("storage key count exceeds {MAX_STORAGE_KEYS}"));
    }
    let mut bytes = storage
        .keys()
        .filter(|candidate| candidate.as_str() != key)
        .try_fold(0usize, |total, candidate| {
            total.checked_add(4)?.checked_add(candidate.len())
        })
        .ok_or_else(|| "storage key-list length overflowed".to_owned())?;
    bytes = bytes
        .checked_add(4)
        .and_then(|total| total.checked_add(key.len()))
        .ok_or_else(|| "storage key-list length overflowed".to_owned())?;
    if bytes > MAX_STORAGE_KEY_LIST_BYTES {
        return Err(format!(
            "storage key-list bytes exceed {MAX_STORAGE_KEY_LIST_BYTES}"
        ));
    }
    Ok(())
}

fn permission_resolution(
    settings: &[PermissionSetting],
    player: PlayerId,
    node: &PermissionNode,
) -> Resolution {
    settings
        .iter()
        .find(|setting| setting.player() == player && setting.node() == node)
        .map_or(Resolution::Unset, PermissionSetting::resolution)
}

fn known_command(command: &CommandDefinition) -> bool {
    command.nodes().iter().all(|node| {
        matches!(
            node.kind(),
            ferrumc_plugin_sdk::CommandNodeKind::Literal
                | ferrumc_plugin_sdk::CommandNodeKind::Word
                | ferrumc_plugin_sdk::CommandNodeKind::GreedyText
                | ferrumc_plugin_sdk::CommandNodeKind::Integer(_)
        )
    })
}

fn known_event_kind(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::PlayerJoin
            | EventKind::PlayerLeave
            | EventKind::BlockBreak
            | EventKind::AfterBlockPlace
            | EventKind::AfterBlockBreak
            | EventKind::PlayerMove
            | EventKind::BlockPlaceAttempt
            | EventKind::BlockBreakAttempt
            | EventKind::ChatAttempt
            | EventKind::InteractAttempt
            | EventKind::Command
            | EventKind::Timer
    )
}

fn known_decision_effect(effect: &PluginEffect) -> bool {
    match effect {
        PluginEffect::BlockDecision(decision) => matches!(
            decision,
            BlockDecision::Allow | BlockDecision::Deny(_) | BlockDecision::Replace(_)
        ),
        PluginEffect::EventDecision { decision, .. } => {
            matches!(decision, EventDecision::Allow | EventDecision::Deny(_))
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use ferrumc_plugin_abi::{
        FcCommandKind, FcResourceHandle, FC_CAPABILITY_DENIED, FC_COMMAND_BUFFER_FULL,
        FC_COMMAND_FLAGS_NONE, FC_DIAGNOSTIC_INFO, FC_INVALID_ARGUMENT, FC_OK,
    };
    use ferrumc_plugin_abi_sys::{HostServices as _, OwnedCommand};

    use super::*;

    fn state() -> RuntimeState {
        RuntimeState::from_seed(&SeedState::empty())
    }

    fn decision_command(kind: FcCommandKind, payload: Vec<u8>) -> OwnedCommand {
        OwnedCommand::new(
            kind,
            FC_COMMAND_FLAGS_NONE,
            FcResourceHandle::INVALID,
            payload,
        )
    }

    #[test]
    fn raw_block_allow_and_deny_are_disambiguated_by_event_route() {
        for (command, expected) in [
            (
                decision_command(FcCommandKind::DECISION_ALLOW, Vec::new()),
                BlockDecision::Allow,
            ),
            (
                {
                    let mut payload = Vec::new();
                    codec::push_text(&mut payload, "denied").expect("bounded feedback");
                    decision_command(FcCommandKind::DECISION_DENY, payload)
                },
                BlockDecision::Deny(Some(
                    ferrumc_plugin_sdk::Feedback::new("denied").expect("bounded feedback"),
                )),
            ),
        ] {
            let state = state();
            let capabilities = CapabilityManifest::empty().with(Capability::VetoBlockEdits);
            let mut frame = CallbackFrame::new(
                &state,
                capabilities,
                FramePhase::Event,
                Tick::new(1),
                4,
                DEFAULT_DIMENSION_HANDLE,
                Some(EventKind::BlockPlaceAttempt),
            );
            assert_eq!(frame.emit(command), FC_OK);
            assert_eq!(frame.validate_decision(), Ok(()));
            let (effects, _, failure) = frame.into_parts();
            assert_eq!(failure, None);
            assert_eq!(effects, [PluginEffect::BlockDecision(expected)]);
        }
    }

    #[test]
    fn raw_decisions_enforce_capability_and_exactly_one_slot() {
        let state = state();
        let mut denied = CallbackFrame::new(
            &state,
            CapabilityManifest::empty(),
            FramePhase::Event,
            Tick::new(1),
            4,
            DEFAULT_DIMENSION_HANDLE,
            Some(EventKind::BlockPlaceAttempt),
        );
        assert_eq!(
            denied.emit(decision_command(FcCommandKind::DECISION_ALLOW, Vec::new())),
            FC_CAPABILITY_DENIED
        );
        let (effects, _, failure) = denied.into_parts();
        assert!(effects.is_empty());
        assert_eq!(
            failure,
            Some(PluginFailureKind::CapabilityDenied(
                Capability::VetoBlockEdits
            ))
        );

        let capabilities = CapabilityManifest::empty().with(Capability::VetoBlockEdits);
        let mut duplicate = CallbackFrame::new(
            &state,
            capabilities,
            FramePhase::Event,
            Tick::new(1),
            4,
            DEFAULT_DIMENSION_HANDLE,
            Some(EventKind::BlockPlaceAttempt),
        );
        assert_eq!(
            duplicate.emit(decision_command(FcCommandKind::DECISION_ALLOW, Vec::new())),
            FC_OK
        );
        assert_eq!(
            duplicate.emit(decision_command(FcCommandKind::DECISION_ALLOW, Vec::new())),
            FC_INVALID_ARGUMENT
        );
        let (_, _, failure) = duplicate.into_parts();
        assert_eq!(failure, Some(PluginFailureKind::DuplicateDecision));

        let missing = CallbackFrame::new(
            &state,
            capabilities,
            FramePhase::Event,
            Tick::new(1),
            4,
            DEFAULT_DIMENSION_HANDLE,
            Some(EventKind::BlockPlaceAttempt),
        );
        assert_eq!(
            missing.validate_decision(),
            Err(PluginFailureKind::MissingDecision)
        );
    }

    #[test]
    fn raw_decision_on_a_nondecision_event_poison_the_frame() {
        let state = state();
        let capabilities = CapabilityManifest::empty().with(Capability::VetoEvents);
        let mut frame = CallbackFrame::new(
            &state,
            capabilities,
            FramePhase::Event,
            Tick::new(1),
            4,
            DEFAULT_DIMENSION_HANDLE,
            Some(EventKind::PlayerJoin),
        );
        assert_eq!(
            frame.emit(decision_command(FcCommandKind::DECISION_ALLOW, Vec::new())),
            FC_INVALID_ARGUMENT
        );
        let (effects, _, failure) = frame.into_parts();
        assert!(effects.is_empty());
        assert_eq!(failure, Some(PluginFailureKind::WrongDecision));
    }

    #[test]
    fn ignored_raw_diagnostics_poison_the_frame() {
        for (level, message) in [
            (FC_DIAGNOSTIC_INFO, "x".repeat(MAX_DIAGNOSTIC_BYTES + 1)),
            (u32::MAX, "unknown-level".to_owned()),
        ] {
            let state = state();
            let mut diagnostic = CallbackFrame::new(
                &state,
                CapabilityManifest::empty(),
                FramePhase::Event,
                Tick::new(1),
                4,
                DEFAULT_DIMENSION_HANDLE,
                None,
            );
            assert_eq!(
                AbiHostServices::diagnostic(&mut diagnostic, level, message),
                FC_INVALID_ARGUMENT
            );
            let (effects, diagnostics, failure) = diagnostic.into_parts();
            assert!(effects.is_empty());
            assert!(diagnostics.is_empty());
            assert!(matches!(failure, Some(PluginFailureKind::AbiProtocol(_))));
        }
    }

    #[test]
    fn raw_storage_commands_enforce_count_and_encoded_list_bounds() {
        let mut seed = SeedState::empty();
        for index in 0..MAX_STORAGE_KEYS {
            seed.storage.insert(format!("key-{index}"), Vec::new());
        }
        let state = RuntimeState::from_seed(&seed);
        let mut count = CallbackFrame::new(
            &state,
            CapabilityManifest::empty().with(Capability::Storage),
            FramePhase::Event,
            Tick::new(1),
            4,
            DEFAULT_DIMENSION_HANDLE,
            None,
        );
        let mut payload = Vec::new();
        codec::push_text(&mut payload, "one-more").expect("bounded key");
        codec::push_bytes(&mut payload, b"value").expect("bounded value");
        let command = OwnedCommand::new(
            FcCommandKind::STORAGE_PUT,
            FC_COMMAND_FLAGS_NONE,
            FcResourceHandle::INVALID,
            payload,
        );
        assert_eq!(count.emit(command), FC_COMMAND_BUFFER_FULL);
        let (effects, _, failure) = count.into_parts();
        assert!(effects.is_empty());
        assert_eq!(failure, Some(PluginFailureKind::BufferFull));

        let mut seed = SeedState::empty();
        for index in 0..252 {
            let key = format!("{index:03}{}", "x".repeat(253));
            assert_eq!(key.len(), MAX_STORAGE_KEY_BYTES);
            seed.storage.insert(key, Vec::new());
        }
        let state = RuntimeState::from_seed(&seed);
        let mut encoded_list = CallbackFrame::new(
            &state,
            CapabilityManifest::empty().with(Capability::Storage),
            FramePhase::Event,
            Tick::new(1),
            4,
            DEFAULT_DIMENSION_HANDLE,
            None,
        );
        let mut payload = Vec::new();
        codec::push_text(&mut payload, "123456789").expect("bounded key");
        codec::push_bytes(&mut payload, b"value").expect("bounded value");
        let command = OwnedCommand::new(
            FcCommandKind::STORAGE_PUT,
            FC_COMMAND_FLAGS_NONE,
            FcResourceHandle::INVALID,
            payload,
        );
        assert_eq!(
            encoded_list.emit(command),
            FC_COMMAND_BUFFER_FULL,
            "252 maximum-size keys consume 65,520 list bytes; a nine-byte key would exceed 65,532"
        );
        let (effects, _, failure) = encoded_list.into_parts();
        assert!(effects.is_empty());
        assert_eq!(failure, Some(PluginFailureKind::BufferFull));
    }

    #[test]
    fn timer_due_tick_overflow_poison_the_frame_without_staging() {
        let state = state();
        let mut frame = CallbackFrame::new(
            &state,
            CapabilityManifest::empty(),
            FramePhase::Event,
            Tick::new(u64::MAX),
            4,
            DEFAULT_DIMENSION_HANDLE,
            None,
        );
        let error =
            HostServices::schedule_timer(&mut frame, TimerId::new(1).expect("nonzero timer id"), 1);
        assert!(matches!(
            error,
            Err(FacadeError::InvalidInput {
                resource: "timer delay",
                ..
            })
        ));
        let (effects, _, failure) = frame.into_parts();
        assert!(effects.is_empty());
        assert_eq!(failure, Some(PluginFailureKind::TimerOverflow));
    }
}
