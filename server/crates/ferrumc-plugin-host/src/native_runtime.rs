//! Safe host-side adaptation for ABI-v1 native callbacks.
//!
//! A native callback never receives the caller's real command sink. Commands
//! are decoded into this module's bounded, ordered staging buffer first. The
//! caller may expose those effects only after [`NativeCallbackServices::complete`]
//! reports a committed callback.

use std::fmt;
use std::str;

use ferrumc_core::{PlayerId, TextComponent};
use ferrumc_math::{BlockPos, Vec3, WorldIntent};
use ferrumc_plugin_abi::{
    FcCommandKind, FcEventKind, FcHostRequestKind, FcResourceHandle, FcStatus, ABI_MAJOR,
    ABI_MINOR, FC_CAPABILITY_DENIED, FC_COMMAND_BUFFER_FULL, FC_COMMAND_FLAGS_NONE,
    FC_DIAGNOSTIC_DEBUG, FC_DIAGNOSTIC_ERROR, FC_DIAGNOSTIC_INFO, FC_DIAGNOSTIC_TRACE,
    FC_DIAGNOSTIC_WARN, FC_ERROR, FC_EVENT_FLAGS_NONE, FC_INVALID_ARGUMENT, FC_OK,
};
use ferrumc_plugin_api::{
    BlockBreakAttempt, BlockPlaceAttempt, Capability, CapabilityManifest, EventKind,
    PluginBlockDecision, PluginEvent, MAX_EMITTED_INTENTS,
};
use ferrumc_plugin_loader::{
    HostCallOutcome, HostServices, OwnedCommand, OwnedEvent, OwnedHostRequest,
};

use crate::host::NativeEventContext;

/// Total payload bytes retained by one callback's staged command buffer.
const MAX_BUFFERED_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Diagnostics retained from one callback.
const MAX_DIAGNOSTICS: usize = 64;

/// Total diagnostic UTF-8 bytes retained from one callback.
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;

/// Maximum staged effects for a decision callback.
///
/// The decision is mandatory protocol output, not one of the plugin's bounded
/// mutation intents, so a callback may stage all 64 intents plus one decision.
const MAX_DECISION_EFFECTS: usize = MAX_EMITTED_INTENTS + 1;

/// Maximum accepted byte length of plain-text decision feedback.
///
/// This matches the shared SDK bound, while keeping the host independent of
/// the author-facing SDK crate.
const MAX_DECISION_FEEDBACK_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeCallPhase {
    Initialization,
    Event,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativePayloadError {
    Truncated,
    TrailingBytes,
    InvalidUtf8,
    LengthOverflow,
    InvalidValue,
}

impl fmt::Display for NativePayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => formatter.write_str("payload is truncated"),
            Self::TrailingBytes => formatter.write_str("payload has trailing bytes"),
            Self::InvalidUtf8 => formatter.write_str("payload text is not UTF-8"),
            Self::LengthOverflow => formatter.write_str("payload length is not representable"),
            Self::InvalidValue => formatter.write_str("payload contains an invalid value"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NativeServiceError {
    CapabilityDenied {
        capability: Capability,
    },
    FacadeUnavailable {
        capability: Capability,
    },
    WrongPhase {
        command: u32,
        phase: NativeCallPhase,
    },
    InvalidFlags {
        kind: u32,
        flags: u32,
    },
    InvalidTarget {
        kind: u32,
        target: u64,
    },
    MalformedPayload {
        kind: u32,
        source: NativePayloadError,
    },
    UnsupportedCommand {
        kind: u32,
    },
    UnsupportedRequest {
        kind: u32,
    },
    MissingDecision,
    DuplicateDecision,
    UnexpectedDecision {
        kind: u32,
    },
    CommandBufferFull {
        maximum: usize,
    },
    PayloadBudgetExceeded {
        maximum: usize,
    },
    DiagnosticBufferFull {
        maximum: usize,
    },
    InvalidDiagnosticLevel {
        level: u32,
    },
}

impl NativeServiceError {
    pub(crate) fn status(&self) -> FcStatus {
        match self {
            Self::CapabilityDenied { .. } => FC_CAPABILITY_DENIED,
            Self::CommandBufferFull { .. }
            | Self::PayloadBudgetExceeded { .. }
            | Self::DiagnosticBufferFull { .. } => FC_COMMAND_BUFFER_FULL,
            Self::FacadeUnavailable { .. } => FC_ERROR,
            Self::WrongPhase { .. }
            | Self::InvalidFlags { .. }
            | Self::InvalidTarget { .. }
            | Self::MalformedPayload { .. }
            | Self::UnsupportedCommand { .. }
            | Self::UnsupportedRequest { .. }
            | Self::MissingDecision
            | Self::DuplicateDecision
            | Self::UnexpectedDecision { .. }
            | Self::InvalidDiagnosticLevel { .. } => FC_INVALID_ARGUMENT,
        }
    }
}

impl fmt::Display for NativeServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapabilityDenied { capability } => {
                write!(formatter, "native callback lacks capability {capability}")
            }
            Self::FacadeUnavailable { capability } => {
                write!(
                    formatter,
                    "native callback facade {capability} is unavailable"
                )
            }
            Self::WrongPhase { command, phase } => {
                write!(
                    formatter,
                    "native command {command} is invalid during {phase:?}"
                )
            }
            Self::InvalidFlags { kind, flags } => {
                write!(
                    formatter,
                    "native envelope {kind} has invalid flags {flags:#x}"
                )
            }
            Self::InvalidTarget { kind, target } => {
                write!(
                    formatter,
                    "native envelope {kind} has invalid target {target}"
                )
            }
            Self::MalformedPayload { kind, source } => {
                write!(formatter, "native envelope {kind} {source}")
            }
            Self::UnsupportedCommand { kind } => {
                write!(formatter, "native command kind {kind} is unsupported")
            }
            Self::UnsupportedRequest { kind } => {
                write!(formatter, "native host request kind {kind} is unsupported")
            }
            Self::MissingDecision => {
                formatter.write_str("native decision callback emitted no decision")
            }
            Self::DuplicateDecision => {
                formatter.write_str("native decision callback emitted more than one decision")
            }
            Self::UnexpectedDecision { kind } => {
                write!(
                    formatter,
                    "native decision command {kind} is invalid for this event"
                )
            }
            Self::CommandBufferFull { maximum } => {
                write!(formatter, "native command buffer reached {maximum} entries")
            }
            Self::PayloadBudgetExceeded { maximum } => {
                write!(
                    formatter,
                    "native command payload buffer exceeds {maximum} bytes"
                )
            }
            Self::DiagnosticBufferFull { maximum } => {
                write!(
                    formatter,
                    "native diagnostic buffer exceeds its bound of {maximum}"
                )
            }
            Self::InvalidDiagnosticLevel { level } => {
                write!(formatter, "native diagnostic level {level} is invalid")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum NativeEffect {
    Intent(WorldIntent),
    Subscribe(EventKind),
    BlockDecision(PluginBlockDecision),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeEventRoute {
    Notification,
    BlockPlaceDecision,
    BlockBreakDecision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeDiagnostic {
    level: u32,
    message: String,
}

impl NativeDiagnostic {
    pub(crate) const fn level(&self) -> u32 {
        self.level
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Debug)]
pub(crate) struct NativeCompletion {
    callback_status: FcStatus,
    committed: bool,
    effects: Vec<NativeEffect>,
    diagnostics: Vec<NativeDiagnostic>,
    first_error: Option<NativeServiceError>,
    capability_denial: Option<Capability>,
}

impl NativeCompletion {
    pub(crate) const fn callback_status(&self) -> FcStatus {
        self.callback_status
    }

    pub(crate) const fn is_committed(&self) -> bool {
        self.committed
    }

    #[cfg(test)]
    fn effects(&self) -> &[NativeEffect] {
        &self.effects
    }

    pub(crate) fn diagnostics(&self) -> &[NativeDiagnostic] {
        &self.diagnostics
    }

    pub(crate) const fn first_error(&self) -> Option<&NativeServiceError> {
        self.first_error.as_ref()
    }

    pub(crate) const fn capability_denial(&self) -> Option<Capability> {
        self.capability_denial
    }

    pub(crate) fn into_effects(self) -> Vec<NativeEffect> {
        self.effects
    }
}

pub(crate) const fn supported_native_capabilities() -> CapabilityManifest {
    CapabilityManifest::empty()
        .with(Capability::ReceiveEvents)
        .with(Capability::SubmitIntents)
        .with(Capability::VetoBlockEdits)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NativeEventError {
    UnsupportedEvent,
}

impl fmt::Display for NativeEventError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedEvent => formatter.write_str("plugin event has no ABI-v1 encoding"),
        }
    }
}

pub(crate) fn encode_event(
    event: &PluginEvent,
    tick: u64,
    shard: FcResourceHandle,
) -> Result<OwnedEvent, NativeEventError> {
    let mut payload = Vec::new();
    let kind = match event {
        PluginEvent::PlayerJoin { player } => {
            push_player(&mut payload, *player);
            FcEventKind::PLAYER_JOIN
        }
        PluginEvent::PlayerLeave { player } => {
            push_player(&mut payload, *player);
            FcEventKind::PLAYER_LEAVE
        }
        PluginEvent::BlockBreak { player, pos } => {
            push_block_break_payload(&mut payload, *player, *pos);
            FcEventKind::BLOCK_BREAK
        }
        PluginEvent::AfterBlockPlace {
            player,
            pos,
            block_state_id,
        } => {
            push_player(&mut payload, *player);
            push_block_pos(&mut payload, *pos);
            payload.extend_from_slice(&block_state_id.to_le_bytes());
            FcEventKind::AFTER_BLOCK_PLACE
        }
        PluginEvent::AfterBlockBreak { player, pos } => {
            push_block_break_payload(&mut payload, *player, *pos);
            FcEventKind::AFTER_BLOCK_BREAK
        }
        PluginEvent::PlayerMove { player, from, to } => {
            push_player(&mut payload, *player);
            push_block_pos(&mut payload, *from);
            push_block_pos(&mut payload, *to);
            FcEventKind::PLAYER_MOVE
        }
        _ => return Err(NativeEventError::UnsupportedEvent),
    };

    Ok(OwnedEvent::new(
        kind,
        FC_EVENT_FLAGS_NONE,
        tick,
        shard,
        payload,
    ))
}

pub(crate) fn encode_block_place_attempt(
    attempt: &BlockPlaceAttempt,
    tick: u64,
    shard: FcResourceHandle,
) -> OwnedEvent {
    let mut payload = Vec::with_capacity(32);
    push_player(&mut payload, attempt.player());
    push_block_pos(&mut payload, attempt.pos());
    payload.extend_from_slice(&attempt.block_state_id().to_le_bytes());
    OwnedEvent::new(
        FcEventKind::BLOCK_PLACE_ATTEMPT,
        FC_EVENT_FLAGS_NONE,
        tick,
        shard,
        payload,
    )
}

pub(crate) fn encode_block_break_attempt(
    attempt: &BlockBreakAttempt,
    tick: u64,
    shard: FcResourceHandle,
) -> OwnedEvent {
    let mut payload = Vec::with_capacity(36);
    push_block_break_payload(&mut payload, attempt.player(), attempt.pos());
    OwnedEvent::new(
        FcEventKind::BLOCK_BREAK_ATTEMPT,
        FC_EVENT_FLAGS_NONE,
        tick,
        shard,
        payload,
    )
}

fn push_block_break_payload(payload: &mut Vec<u8>, player: PlayerId, pos: BlockPos) {
    const STRUCT_SIZE: u32 = 36;
    push_record_header(payload, STRUCT_SIZE);
    push_player(payload, player);
    push_block_pos(payload, pos);
}

fn push_record_header(payload: &mut Vec<u8>, struct_size: u32) {
    payload.extend_from_slice(&struct_size.to_le_bytes());
    payload.extend_from_slice(&ABI_MAJOR.to_le_bytes());
    payload.extend_from_slice(&ABI_MINOR.to_le_bytes());
}

fn push_player(payload: &mut Vec<u8>, player: PlayerId) {
    payload.extend_from_slice(player.as_uuid().as_bytes());
}

fn push_block_pos(payload: &mut Vec<u8>, pos: BlockPos) {
    payload.extend_from_slice(&pos.x().to_le_bytes());
    payload.extend_from_slice(&pos.y().to_le_bytes());
    payload.extend_from_slice(&pos.z().to_le_bytes());
}

pub(crate) struct NativeCallbackServices {
    phase: NativeCallPhase,
    event_route: NativeEventRoute,
    capabilities: CapabilityManifest,
    effects: Vec<NativeEffect>,
    intent_count: usize,
    buffered_payload_bytes: usize,
    diagnostics: Vec<NativeDiagnostic>,
    diagnostic_bytes: usize,
    first_error: Option<NativeServiceError>,
    capability_denial: Option<Capability>,
    event_shard: Option<FcResourceHandle>,
    event_context: Option<NativeEventContext>,
    poisoned: bool,
}

impl NativeCallbackServices {
    pub(crate) fn for_initialization(capabilities: CapabilityManifest) -> Self {
        Self::new(
            NativeCallPhase::Initialization,
            NativeEventRoute::Notification,
            capabilities,
            None,
            None,
        )
    }

    pub(crate) fn for_event(
        capabilities: CapabilityManifest,
        event_shard: FcResourceHandle,
        event_context: NativeEventContext,
    ) -> Self {
        Self::for_event_route(
            capabilities,
            event_shard,
            event_context,
            NativeEventRoute::Notification,
        )
    }

    pub(crate) fn for_block_decision(
        capabilities: CapabilityManifest,
        event_shard: FcResourceHandle,
        event_context: NativeEventContext,
        event_route: NativeEventRoute,
    ) -> Self {
        Self::for_event_route(capabilities, event_shard, event_context, event_route)
    }

    fn for_event_route(
        capabilities: CapabilityManifest,
        event_shard: FcResourceHandle,
        event_context: NativeEventContext,
        event_route: NativeEventRoute,
    ) -> Self {
        Self::new(
            NativeCallPhase::Event,
            event_route,
            capabilities,
            Some(event_shard),
            Some(event_context),
        )
    }

    pub(crate) fn for_shutdown(capabilities: CapabilityManifest) -> Self {
        Self::new(
            NativeCallPhase::Shutdown,
            NativeEventRoute::Notification,
            capabilities,
            None,
            None,
        )
    }

    fn new(
        phase: NativeCallPhase,
        event_route: NativeEventRoute,
        capabilities: CapabilityManifest,
        event_shard: Option<FcResourceHandle>,
        event_context: Option<NativeEventContext>,
    ) -> Self {
        Self {
            phase,
            event_route,
            capabilities,
            effects: Vec::with_capacity(
                if matches!(
                    event_route,
                    NativeEventRoute::BlockPlaceDecision | NativeEventRoute::BlockBreakDecision
                ) {
                    MAX_DECISION_EFFECTS
                } else {
                    MAX_EMITTED_INTENTS
                },
            ),
            intent_count: 0,
            buffered_payload_bytes: 0,
            diagnostics: Vec::with_capacity(MAX_DIAGNOSTICS),
            diagnostic_bytes: 0,
            first_error: None,
            capability_denial: None,
            event_shard,
            event_context,
            poisoned: false,
        }
    }

    pub(crate) fn complete(mut self, callback_status: FcStatus) -> NativeCompletion {
        if callback_status == FC_OK
            && !self.poisoned
            && self.expects_decision()
            && !self
                .effects
                .iter()
                .any(|effect| matches!(effect, NativeEffect::BlockDecision(_)))
        {
            let _status = self.record_error(NativeServiceError::MissingDecision, true);
        }
        let committed = callback_status == FC_OK && !self.poisoned;
        if !committed {
            self.effects.clear();
        }
        NativeCompletion {
            callback_status,
            committed,
            effects: self.effects,
            diagnostics: self.diagnostics,
            first_error: self.first_error,
            capability_denial: self.capability_denial,
        }
    }

    const fn expects_decision(&self) -> bool {
        matches!(
            self.event_route,
            NativeEventRoute::BlockPlaceDecision | NativeEventRoute::BlockBreakDecision
        )
    }

    const fn poison_on_command_error(&self, kind: FcCommandKind) -> bool {
        self.expects_decision() || is_decision_command(kind)
    }

    const fn has_valid_event_scope(&self) -> bool {
        match (self.event_shard, self.event_context) {
            (Some(resource), Some(context)) => resource.is_valid() != context.is_connection_side(),
            _ => false,
        }
    }

    fn record_error(&mut self, error: NativeServiceError, poison: bool) -> FcStatus {
        let status = error.status();
        if let NativeServiceError::CapabilityDenied { capability } = &error {
            self.capability_denial.get_or_insert(*capability);
        }
        if self.first_error.is_none() {
            self.first_error = Some(error);
        }
        self.poisoned |= poison;
        status
    }

    fn require(&self, capability: Capability) -> Result<(), NativeServiceError> {
        if self.capabilities.grants(capability) {
            Ok(())
        } else {
            Err(NativeServiceError::CapabilityDenied { capability })
        }
    }

    fn validate_command_envelope(&self, command: &OwnedCommand) -> Result<(), NativeServiceError> {
        let kind = command.kind().raw();
        if command.flags() != FC_COMMAND_FLAGS_NONE {
            return Err(NativeServiceError::InvalidFlags {
                kind,
                flags: command.flags(),
            });
        }

        if command.kind() == FcCommandKind::MESSAGE {
            self.require_phase(command.kind(), NativeCallPhase::Event)?;
            require_target(command, FcResourceHandle::INVALID)?;
            return Ok(());
        }
        if command.kind() == FcCommandKind::TELEPORT {
            self.require_phase(command.kind(), NativeCallPhase::Event)?;
            require_target(command, FcResourceHandle::INVALID)?;
            return Ok(());
        }
        if command.kind() == FcCommandKind::SUBSCRIBE_EVENT {
            self.require_phase(command.kind(), NativeCallPhase::Initialization)?;
            require_target(command, FcResourceHandle::INVALID)?;
            return Ok(());
        }
        if is_decision_command(command.kind()) {
            self.require_phase(command.kind(), NativeCallPhase::Event)?;
            require_target(command, FcResourceHandle::INVALID)?;
            if !self.expects_decision()
                || (command.kind() == FcCommandKind::DECISION_REPLACE_BLOCK
                    && self.event_route != NativeEventRoute::BlockPlaceDecision)
            {
                return Err(NativeServiceError::UnexpectedDecision { kind });
            }
            return Ok(());
        }

        Err(NativeServiceError::UnsupportedCommand { kind })
    }

    fn decode_command(command: &OwnedCommand) -> Result<NativeEffect, NativeServiceError> {
        if command.kind() == FcCommandKind::MESSAGE {
            decode_message(command.payload()).map(NativeEffect::Intent)
        } else if command.kind() == FcCommandKind::TELEPORT {
            decode_teleport(command.payload()).map(NativeEffect::Intent)
        } else if command.kind() == FcCommandKind::SUBSCRIBE_EVENT {
            decode_subscription(command.payload()).map(NativeEffect::Subscribe)
        } else if command.kind() == FcCommandKind::DECISION_ALLOW {
            decode_allow(command.payload()).map(NativeEffect::BlockDecision)
        } else if command.kind() == FcCommandKind::DECISION_DENY {
            decode_deny(command.payload()).map(NativeEffect::BlockDecision)
        } else if command.kind() == FcCommandKind::DECISION_REPLACE_BLOCK {
            decode_replace(command.payload()).map(NativeEffect::BlockDecision)
        } else {
            Err(NativeServiceError::UnsupportedCommand {
                kind: command.kind().raw(),
            })
        }
    }

    fn require_phase(
        &self,
        command: FcCommandKind,
        expected: NativeCallPhase,
    ) -> Result<(), NativeServiceError> {
        if self.phase == expected {
            Ok(())
        } else {
            Err(NativeServiceError::WrongPhase {
                command: command.raw(),
                phase: self.phase,
            })
        }
    }

    fn stage(&mut self, command: &OwnedCommand) -> FcStatus {
        if let Some(capability) = required_command_capability(command.kind()) {
            if let Err(error) = self.require(capability) {
                return self.record_error(error, true);
            }
        }
        if self.phase == NativeCallPhase::Event && !self.has_valid_event_scope() {
            return self.record_error(
                NativeServiceError::FacadeUnavailable {
                    capability: Capability::ReceiveEvents,
                },
                true,
            );
        }
        if let Err(error) = self.validate_command_envelope(command) {
            return self.record_error(error, self.poison_on_command_error(command.kind()));
        }
        if is_decision_command(command.kind())
            && self
                .effects
                .iter()
                .any(|effect| matches!(effect, NativeEffect::BlockDecision(_)))
        {
            return self.record_error(NativeServiceError::DuplicateDecision, true);
        }

        let Some(buffered) = self
            .buffered_payload_bytes
            .checked_add(command.payload().len())
        else {
            return self.record_error(
                NativeServiceError::PayloadBudgetExceeded {
                    maximum: MAX_BUFFERED_PAYLOAD_BYTES,
                },
                self.expects_decision(),
            );
        };
        if buffered > MAX_BUFFERED_PAYLOAD_BYTES {
            return self.record_error(
                NativeServiceError::PayloadBudgetExceeded {
                    maximum: MAX_BUFFERED_PAYLOAD_BYTES,
                },
                self.expects_decision(),
            );
        }

        let effect = match Self::decode_command(command) {
            Ok(effect) => effect,
            Err(error) => {
                return self.record_error(error, self.poison_on_command_error(command.kind()));
            }
        };
        if matches!(effect, NativeEffect::Intent(_)) && self.intent_count >= MAX_EMITTED_INTENTS {
            return self.record_error(
                NativeServiceError::CommandBufferFull {
                    maximum: MAX_EMITTED_INTENTS,
                },
                self.expects_decision(),
            );
        }
        let effect_limit = if self.expects_decision() {
            MAX_DECISION_EFFECTS
        } else {
            MAX_EMITTED_INTENTS
        };
        if self.effects.len() >= effect_limit {
            return self.record_error(
                NativeServiceError::CommandBufferFull {
                    maximum: effect_limit,
                },
                self.expects_decision(),
            );
        }
        self.buffered_payload_bytes = buffered;
        if matches!(effect, NativeEffect::Intent(_)) {
            self.intent_count += 1;
        }
        self.effects.push(effect);
        FC_OK
    }

    fn host_call(&mut self, request: &OwnedHostRequest) -> HostCallOutcome {
        let kind = request.kind().raw();
        if let Some(capability) = required_request_capability(request.kind()) {
            if let Err(error) = self.require(capability) {
                let status = self.record_error(error, true);
                return HostCallOutcome::Status(status);
            }
        }
        if request.flags() != 0 {
            let status = self.record_error(
                NativeServiceError::InvalidFlags {
                    kind,
                    flags: request.flags(),
                },
                self.expects_decision(),
            );
            return HostCallOutcome::Status(status);
        }

        if request.kind() == FcHostRequestKind::DIMENSION {
            if self.phase != NativeCallPhase::Event {
                let status = self.record_error(
                    NativeServiceError::WrongPhase {
                        command: kind,
                        phase: self.phase,
                    },
                    self.expects_decision(),
                );
                return HostCallOutcome::Status(status);
            }
            if request.target() != FcResourceHandle::INVALID {
                let status = self.record_error(
                    NativeServiceError::InvalidTarget {
                        kind,
                        target: request.target().raw(),
                    },
                    self.expects_decision(),
                );
                return HostCallOutcome::Status(status);
            }
            if !request.payload().is_empty() {
                let status = self.record_error(
                    NativeServiceError::MalformedPayload {
                        kind,
                        source: NativePayloadError::TrailingBytes,
                    },
                    self.expects_decision(),
                );
                return HostCallOutcome::Status(status);
            }
            let status = self.record_error(
                NativeServiceError::FacadeUnavailable {
                    capability: Capability::ReadWorld,
                },
                true,
            );
            return HostCallOutcome::Status(status);
        }
        let status = self.record_error(
            NativeServiceError::UnsupportedRequest { kind },
            self.expects_decision(),
        );
        HostCallOutcome::Status(status)
    }

    fn record_diagnostic(&mut self, level: u32, message: String) -> FcStatus {
        if !valid_diagnostic_level(level) {
            return self.record_error(
                NativeServiceError::InvalidDiagnosticLevel { level },
                self.expects_decision(),
            );
        }
        if self.diagnostics.len() >= MAX_DIAGNOSTICS {
            return self.record_error(
                NativeServiceError::DiagnosticBufferFull {
                    maximum: MAX_DIAGNOSTICS,
                },
                self.expects_decision(),
            );
        }
        let Some(total) = self.diagnostic_bytes.checked_add(message.len()) else {
            return self.record_error(
                NativeServiceError::DiagnosticBufferFull {
                    maximum: MAX_DIAGNOSTIC_BYTES,
                },
                self.expects_decision(),
            );
        };
        if total > MAX_DIAGNOSTIC_BYTES {
            return self.record_error(
                NativeServiceError::DiagnosticBufferFull {
                    maximum: MAX_DIAGNOSTIC_BYTES,
                },
                self.expects_decision(),
            );
        }

        self.diagnostic_bytes = total;
        self.diagnostics.push(NativeDiagnostic { level, message });
        FC_OK
    }
}

fn required_command_capability(kind: FcCommandKind) -> Option<Capability> {
    if matches!(
        kind,
        FcCommandKind::SET_BLOCK | FcCommandKind::MESSAGE | FcCommandKind::TELEPORT
    ) {
        Some(Capability::SubmitIntents)
    } else if is_decision_command(kind) {
        Some(Capability::VetoBlockEdits)
    } else if kind == FcCommandKind::SUBSCRIBE_EVENT {
        Some(Capability::ReceiveEvents)
    } else if kind == FcCommandKind::REGISTER_COMMAND {
        Some(Capability::RegisterCommands)
    } else if matches!(
        kind,
        FcCommandKind::STORAGE_PUT | FcCommandKind::STORAGE_DELETE
    ) {
        Some(Capability::Storage)
    } else {
        None
    }
}

const fn is_decision_command(kind: FcCommandKind) -> bool {
    matches!(
        kind,
        FcCommandKind::DECISION_ALLOW
            | FcCommandKind::DECISION_DENY
            | FcCommandKind::DECISION_REPLACE_BLOCK
    )
}

fn required_request_capability(kind: FcHostRequestKind) -> Option<Capability> {
    if matches!(
        kind,
        FcHostRequestKind::DIMENSION
            | FcHostRequestKind::CHUNK_LOADED
            | FcHostRequestKind::BLOCK_STATE
            | FcHostRequestKind::PLAYER_POSITION
    ) {
        Some(Capability::ReadWorld)
    } else if kind == FcHostRequestKind::PERMISSION_RESOLVE {
        Some(Capability::ReadPermissions)
    } else if matches!(
        kind,
        FcHostRequestKind::STORAGE_GET | FcHostRequestKind::STORAGE_KEYS
    ) {
        Some(Capability::Storage)
    } else {
        None
    }
}

impl HostServices for NativeCallbackServices {
    fn call(&mut self, request: OwnedHostRequest) -> HostCallOutcome {
        self.host_call(&request)
    }

    fn emit(&mut self, command: OwnedCommand) -> FcStatus {
        self.stage(&command)
    }

    fn diagnostic(&mut self, level: u32, message: String) -> FcStatus {
        self.record_diagnostic(level, message)
    }
}

fn valid_diagnostic_level(level: u32) -> bool {
    matches!(
        level,
        FC_DIAGNOSTIC_ERROR
            | FC_DIAGNOSTIC_WARN
            | FC_DIAGNOSTIC_INFO
            | FC_DIAGNOSTIC_DEBUG
            | FC_DIAGNOSTIC_TRACE
    )
}

fn require_target(
    command: &OwnedCommand,
    expected: FcResourceHandle,
) -> Result<(), NativeServiceError> {
    if command.target() == expected {
        Ok(())
    } else {
        Err(NativeServiceError::InvalidTarget {
            kind: command.kind().raw(),
            target: command.target().raw(),
        })
    }
}

fn decode_message(payload: &[u8]) -> Result<WorldIntent, NativeServiceError> {
    let mut cursor = PayloadCursor::new(payload);
    let player = cursor
        .read_player()
        .map_err(|source| malformed(FcCommandKind::MESSAGE, source))?;
    let message = cursor
        .read_text(MAX_BUFFERED_PAYLOAD_BYTES)
        .map_err(|source| malformed(FcCommandKind::MESSAGE, source))?;
    cursor
        .finish()
        .map_err(|source| malformed(FcCommandKind::MESSAGE, source))?;
    Ok(WorldIntent::Message {
        player,
        message: TextComponent::text(message),
    })
}

fn decode_teleport(payload: &[u8]) -> Result<WorldIntent, NativeServiceError> {
    let mut cursor = PayloadCursor::new(payload);
    let player = cursor
        .read_player()
        .map_err(|source| malformed(FcCommandKind::TELEPORT, source))?;
    let x = cursor
        .read_f64()
        .map_err(|source| malformed(FcCommandKind::TELEPORT, source))?;
    let y = cursor
        .read_f64()
        .map_err(|source| malformed(FcCommandKind::TELEPORT, source))?;
    let z = cursor
        .read_f64()
        .map_err(|source| malformed(FcCommandKind::TELEPORT, source))?;
    cursor
        .finish()
        .map_err(|source| malformed(FcCommandKind::TELEPORT, source))?;
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        return Err(malformed(
            FcCommandKind::TELEPORT,
            NativePayloadError::InvalidValue,
        ));
    }
    Ok(WorldIntent::Teleport {
        player,
        position: Vec3::new(x, y, z),
    })
}

fn decode_allow(payload: &[u8]) -> Result<PluginBlockDecision, NativeServiceError> {
    if payload.is_empty() {
        Ok(PluginBlockDecision::Allow)
    } else {
        Err(malformed(
            FcCommandKind::DECISION_ALLOW,
            NativePayloadError::TrailingBytes,
        ))
    }
}

fn decode_deny(payload: &[u8]) -> Result<PluginBlockDecision, NativeServiceError> {
    if payload.is_empty() {
        return Ok(PluginBlockDecision::Deny { message: None });
    }
    let mut cursor = PayloadCursor::new(payload);
    let message = cursor
        .read_text(MAX_DECISION_FEEDBACK_BYTES)
        .map_err(|source| malformed(FcCommandKind::DECISION_DENY, source))?;
    cursor
        .finish()
        .map_err(|source| malformed(FcCommandKind::DECISION_DENY, source))?;
    Ok(PluginBlockDecision::Deny {
        message: Some(TextComponent::text(message)),
    })
}

fn decode_replace(payload: &[u8]) -> Result<PluginBlockDecision, NativeServiceError> {
    let mut cursor = PayloadCursor::new(payload);
    let block_state_id = cursor
        .read_u32()
        .map_err(|source| malformed(FcCommandKind::DECISION_REPLACE_BLOCK, source))?;
    cursor
        .finish()
        .map_err(|source| malformed(FcCommandKind::DECISION_REPLACE_BLOCK, source))?;
    Ok(PluginBlockDecision::Replace { block_state_id })
}

fn decode_subscription(payload: &[u8]) -> Result<EventKind, NativeServiceError> {
    let mut cursor = PayloadCursor::new(payload);
    let raw = cursor
        .read_u32()
        .map_err(|source| malformed(FcCommandKind::SUBSCRIBE_EVENT, source))?;
    cursor
        .finish()
        .map_err(|source| malformed(FcCommandKind::SUBSCRIBE_EVENT, source))?;
    let kind = FcEventKind::from_raw(raw);
    if kind == FcEventKind::PLAYER_JOIN {
        Ok(EventKind::PlayerJoin)
    } else if kind == FcEventKind::PLAYER_LEAVE {
        Ok(EventKind::PlayerLeave)
    } else if kind == FcEventKind::BLOCK_BREAK {
        Ok(EventKind::BlockBreak)
    } else if kind == FcEventKind::AFTER_BLOCK_PLACE {
        Ok(EventKind::AfterBlockPlace)
    } else if kind == FcEventKind::AFTER_BLOCK_BREAK {
        Ok(EventKind::AfterBlockBreak)
    } else if kind == FcEventKind::PLAYER_MOVE {
        Ok(EventKind::PlayerMove)
    } else {
        Err(malformed(
            FcCommandKind::SUBSCRIBE_EVENT,
            NativePayloadError::InvalidValue,
        ))
    }
}

fn malformed(kind: FcCommandKind, source: NativePayloadError) -> NativeServiceError {
    NativeServiceError::MalformedPayload {
        kind: kind.raw(),
        source,
    }
}

struct PayloadCursor<'a> {
    payload: &'a [u8],
    offset: usize,
}

impl<'a> PayloadCursor<'a> {
    const fn new(payload: &'a [u8]) -> Self {
        Self { payload, offset: 0 }
    }

    fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], NativePayloadError> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or(NativePayloadError::LengthOverflow)?;
        let bytes = self
            .payload
            .get(self.offset..end)
            .ok_or(NativePayloadError::Truncated)?;
        let array = <[u8; N]>::try_from(bytes).map_err(|_| NativePayloadError::Truncated)?;
        self.offset = end;
        Ok(array)
    }

    fn read_u32(&mut self) -> Result<u32, NativePayloadError> {
        Ok(u32::from_le_bytes(self.read_exact()?))
    }

    fn read_u64(&mut self) -> Result<u64, NativePayloadError> {
        Ok(u64::from_le_bytes(self.read_exact()?))
    }

    fn read_f64(&mut self) -> Result<f64, NativePayloadError> {
        Ok(f64::from_bits(self.read_u64()?))
    }

    fn read_player(&mut self) -> Result<PlayerId, NativePayloadError> {
        player_from_bytes(self.read_exact()?)
    }

    fn read_text(&mut self, maximum: usize) -> Result<String, NativePayloadError> {
        let length =
            usize::try_from(self.read_u32()?).map_err(|_| NativePayloadError::LengthOverflow)?;
        if length > maximum {
            return Err(NativePayloadError::LengthOverflow);
        }
        let end = self
            .offset
            .checked_add(length)
            .ok_or(NativePayloadError::LengthOverflow)?;
        let bytes = self
            .payload
            .get(self.offset..end)
            .ok_or(NativePayloadError::Truncated)?;
        let text = str::from_utf8(bytes).map_err(|_| NativePayloadError::InvalidUtf8)?;
        self.offset = end;
        Ok(text.to_owned())
    }

    fn finish(self) -> Result<(), NativePayloadError> {
        if self.offset == self.payload.len() {
            Ok(())
        } else {
            Err(NativePayloadError::TrailingBytes)
        }
    }
}

fn player_from_bytes(bytes: [u8; 16]) -> Result<PlayerId, NativePayloadError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(36);
    for (index, byte) in bytes.into_iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            encoded.push('-');
        }
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
        .parse()
        .map_err(|_| NativePayloadError::InvalidValue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_plugin_abi::{
        FC_CAPABILITY_DENIED, FC_COMMAND_BUFFER_FULL, FC_DIAGNOSTIC_INFO, FC_PLUGIN_PANIC,
    };

    fn player() -> PlayerId {
        PlayerId::offline("NativeFixture")
    }

    fn event_context() -> NativeEventContext {
        NativeEventContext::new(
            ferrumc_core::Tick::new(37),
            ferrumc_core::WorldId::new(0),
            ferrumc_core::DimensionId::new(0),
            ferrumc_math::ShardPos::new(0, 0),
        )
    }

    fn message_payload(message: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        push_player(&mut payload, player());
        payload.extend_from_slice(
            &u32::try_from(message.len())
                .expect("test message length fits")
                .to_le_bytes(),
        );
        payload.extend_from_slice(message);
        payload
    }

    fn teleport_payload(x: f64, y: f64, z: f64) -> Vec<u8> {
        let mut payload = Vec::new();
        push_player(&mut payload, player());
        for component in [x, y, z] {
            payload.extend_from_slice(&component.to_bits().to_le_bytes());
        }
        payload
    }

    fn command(kind: FcCommandKind, target: FcResourceHandle, payload: Vec<u8>) -> OwnedCommand {
        OwnedCommand::new(kind, FC_COMMAND_FLAGS_NONE, target, payload)
    }

    fn feedback_payload(message: &[u8]) -> Vec<u8> {
        let mut payload = u32::try_from(message.len())
            .expect("test feedback length fits")
            .to_le_bytes()
            .to_vec();
        payload.extend_from_slice(message);
        payload
    }

    #[test]
    fn event_encoder_maps_current_events_to_binary_abi_payloads() {
        let pos = BlockPos::new(-3, 72, 41);
        let shard = FcResourceHandle::from_raw(91);
        let event = encode_event(
            &PluginEvent::BlockBreak {
                player: player(),
                pos,
            },
            37,
            shard,
        )
        .expect("current event encodes");

        assert_eq!(event.kind(), FcEventKind::BLOCK_BREAK);
        assert_eq!(event.flags(), FC_EVENT_FLAGS_NONE);
        assert_eq!(event.tick(), 37);
        assert_eq!(event.shard(), shard);
        assert_eq!(event.payload().len(), 36);
        assert_eq!(&event.payload()[0..4], &36_u32.to_le_bytes());
        assert_eq!(&event.payload()[4..6], &ABI_MAJOR.to_le_bytes());
        assert_eq!(&event.payload()[6..8], &ABI_MINOR.to_le_bytes());
        assert_eq!(&event.payload()[8..24], player().as_uuid().as_bytes());
        assert_eq!(&event.payload()[24..28], &pos.x().to_le_bytes());
        assert_eq!(&event.payload()[28..32], &pos.y().to_le_bytes());
        assert_eq!(&event.payload()[32..36], &pos.z().to_le_bytes());

        let joined = encode_event(
            &PluginEvent::PlayerJoin { player: player() },
            38,
            FcResourceHandle::from_raw(92),
        )
        .expect("join event encodes");
        assert_eq!(joined.kind(), FcEventKind::PLAYER_JOIN);
        assert_eq!(joined.payload(), player().as_uuid().as_bytes());
    }

    #[test]
    fn block_attempt_encoders_use_exact_wire_bytes_and_connection_sentinels() {
        let context = NativeEventContext::connection_side();
        assert_eq!(context.tick(), ferrumc_core::Tick::ZERO);
        assert_eq!(context.world().get(), 0);
        assert_eq!(context.dimension().get(), 0);
        assert_eq!(context.shard(), ferrumc_math::ShardPos::new(0, 0));

        let pos = BlockPos::new(-17, 72, 41);
        let shard = FcResourceHandle::INVALID;
        let place = encode_block_place_attempt(
            &BlockPlaceAttempt::new(player(), pos, 0x1020_3040),
            context.tick().get(),
            shard,
        );
        assert_eq!(place.kind(), FcEventKind::BLOCK_PLACE_ATTEMPT);
        assert_eq!(place.flags(), FC_EVENT_FLAGS_NONE);
        assert_eq!(place.tick(), 0);
        assert_eq!(place.shard(), shard);
        assert_eq!(place.payload().len(), 32);
        assert_eq!(&place.payload()[0..16], player().as_uuid().as_bytes());
        assert_eq!(&place.payload()[16..20], &pos.x().to_le_bytes());
        assert_eq!(&place.payload()[20..24], &pos.y().to_le_bytes());
        assert_eq!(&place.payload()[24..28], &pos.z().to_le_bytes());
        assert_eq!(&place.payload()[28..32], &0x1020_3040_u32.to_le_bytes());

        let block_break = encode_block_break_attempt(
            &BlockBreakAttempt::new(player(), pos),
            context.tick().get(),
            shard,
        );
        assert_eq!(block_break.kind(), FcEventKind::BLOCK_BREAK_ATTEMPT);
        assert_eq!(block_break.flags(), FC_EVENT_FLAGS_NONE);
        assert_eq!(block_break.tick(), 0);
        assert_eq!(block_break.shard(), shard);
        assert_eq!(block_break.payload().len(), 36);
        assert_eq!(&block_break.payload()[0..4], &36_u32.to_le_bytes());
        assert_eq!(&block_break.payload()[4..6], &ABI_MAJOR.to_le_bytes());
        assert_eq!(&block_break.payload()[6..8], &ABI_MINOR.to_le_bytes());
        assert_eq!(&block_break.payload()[8..24], player().as_uuid().as_bytes());
        assert_eq!(&block_break.payload()[24..28], &pos.x().to_le_bytes());
        assert_eq!(&block_break.payload()[28..32], &pos.y().to_le_bytes());
        assert_eq!(&block_break.payload()[32..36], &pos.z().to_le_bytes());
    }

    #[test]
    fn event_scope_requires_resource_provenance_to_match_context() {
        let veto = CapabilityManifest::empty().with(Capability::VetoBlockEdits);
        let decision = || {
            command(
                FcCommandKind::DECISION_ALLOW,
                FcResourceHandle::INVALID,
                Vec::new(),
            )
        };

        let mut connection_side = NativeCallbackServices::for_block_decision(
            veto,
            FcResourceHandle::INVALID,
            NativeEventContext::connection_side(),
            NativeEventRoute::BlockPlaceDecision,
        );
        assert_eq!(connection_side.emit(decision()), FC_OK);
        assert!(connection_side.complete(FC_OK).is_committed());

        let mut fabricated_connection_resource = NativeCallbackServices::for_block_decision(
            veto,
            FcResourceHandle::from_raw(1),
            NativeEventContext::connection_side(),
            NativeEventRoute::BlockPlaceDecision,
        );
        assert_eq!(fabricated_connection_resource.emit(decision()), FC_ERROR);
        assert!(!fabricated_connection_resource
            .complete(FC_OK)
            .is_committed());

        let exact_context = event_context();
        let mut exact = NativeCallbackServices::for_block_decision(
            veto,
            FcResourceHandle::from_raw(1),
            exact_context,
            NativeEventRoute::BlockPlaceDecision,
        );
        assert_eq!(exact.emit(decision()), FC_OK);
        assert!(exact.complete(FC_OK).is_committed());

        let mut missing_exact_resource = NativeCallbackServices::for_block_decision(
            veto,
            FcResourceHandle::INVALID,
            exact_context,
            NativeEventRoute::BlockPlaceDecision,
        );
        assert_eq!(missing_exact_resource.emit(decision()), FC_ERROR);
        assert!(!missing_exact_resource.complete(FC_OK).is_committed());
    }

    #[test]
    fn block_decision_commands_decode_and_commit_exactly_once() {
        let caps = CapabilityManifest::empty()
            .with(Capability::VetoBlockEdits)
            .with(Capability::SubmitIntents);
        assert!(supported_native_capabilities().grants(Capability::VetoBlockEdits));

        let mut denied = NativeCallbackServices::for_block_decision(
            caps,
            FcResourceHandle::from_raw(1),
            event_context(),
            NativeEventRoute::BlockPlaceDecision,
        );
        assert_eq!(
            denied.emit(command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::INVALID,
                message_payload(b"staged with decision"),
            )),
            FC_OK
        );
        assert_eq!(
            denied.emit(command(
                FcCommandKind::DECISION_DENY,
                FcResourceHandle::INVALID,
                feedback_payload(b"protected"),
            )),
            FC_OK
        );
        let denied = denied.complete(FC_OK);
        assert!(denied.is_committed());
        assert_eq!(
            denied.effects(),
            &[
                NativeEffect::Intent(WorldIntent::Message {
                    player: player(),
                    message: TextComponent::text("staged with decision"),
                }),
                NativeEffect::BlockDecision(PluginBlockDecision::Deny {
                    message: Some(TextComponent::text("protected")),
                }),
            ]
        );

        let mut replaced = NativeCallbackServices::for_block_decision(
            caps,
            FcResourceHandle::from_raw(2),
            event_context(),
            NativeEventRoute::BlockPlaceDecision,
        );
        assert_eq!(
            replaced.emit(command(
                FcCommandKind::DECISION_REPLACE_BLOCK,
                FcResourceHandle::INVALID,
                731_u32.to_le_bytes().to_vec(),
            )),
            FC_OK
        );
        assert_eq!(
            replaced.complete(FC_OK).effects(),
            &[NativeEffect::BlockDecision(PluginBlockDecision::Replace {
                block_state_id: 731,
            })]
        );

        let mut allowed = NativeCallbackServices::for_block_decision(
            caps,
            FcResourceHandle::from_raw(3),
            event_context(),
            NativeEventRoute::BlockBreakDecision,
        );
        assert_eq!(
            allowed.emit(command(
                FcCommandKind::DECISION_ALLOW,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_OK
        );
        assert_eq!(
            allowed.complete(FC_OK).effects(),
            &[NativeEffect::BlockDecision(PluginBlockDecision::Allow)]
        );
    }

    #[test]
    fn block_decisions_fail_closed_when_missing_duplicate_or_ungranted() {
        let veto = CapabilityManifest::empty().with(Capability::VetoBlockEdits);
        let missing = NativeCallbackServices::for_block_decision(
            veto,
            FcResourceHandle::from_raw(1),
            event_context(),
            NativeEventRoute::BlockPlaceDecision,
        )
        .complete(FC_OK);
        assert!(!missing.is_committed());
        assert!(missing.effects().is_empty());
        assert_eq!(
            missing.first_error(),
            Some(&NativeServiceError::MissingDecision)
        );

        let mut duplicate = NativeCallbackServices::for_block_decision(
            veto,
            FcResourceHandle::from_raw(1),
            event_context(),
            NativeEventRoute::BlockPlaceDecision,
        );
        assert_eq!(
            duplicate.emit(command(
                FcCommandKind::DECISION_ALLOW,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_OK
        );
        assert_eq!(
            duplicate.emit(command(
                FcCommandKind::DECISION_DENY,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_INVALID_ARGUMENT
        );
        let duplicate = duplicate.complete(FC_OK);
        assert!(!duplicate.is_committed());
        assert!(duplicate.effects().is_empty());
        assert_eq!(
            duplicate.first_error(),
            Some(&NativeServiceError::DuplicateDecision)
        );

        let mut ungranted = NativeCallbackServices::for_block_decision(
            CapabilityManifest::empty(),
            FcResourceHandle::from_raw(1),
            event_context(),
            NativeEventRoute::BlockPlaceDecision,
        );
        assert_eq!(
            ungranted.emit(command(
                FcCommandKind::DECISION_ALLOW,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_CAPABILITY_DENIED
        );
        let ungranted = ungranted.complete(FC_OK);
        assert!(!ungranted.is_committed());
        assert!(ungranted.effects().is_empty());
        assert_eq!(
            ungranted.capability_denial(),
            Some(Capability::VetoBlockEdits)
        );
    }

    #[test]
    fn malformed_and_misrouted_block_decisions_are_rejected_transactionally() {
        let veto = CapabilityManifest::empty().with(Capability::VetoBlockEdits);
        let malformed = [
            (FcCommandKind::DECISION_ALLOW, vec![0]),
            (FcCommandKind::DECISION_DENY, vec![1, 0, 0]),
            (FcCommandKind::DECISION_DENY, feedback_payload(&[0xff])),
            (
                FcCommandKind::DECISION_DENY,
                (u32::try_from(MAX_DECISION_FEEDBACK_BYTES).expect("feedback bound fits u32") + 1)
                    .to_le_bytes()
                    .to_vec(),
            ),
            (FcCommandKind::DECISION_REPLACE_BLOCK, Vec::new()),
            (FcCommandKind::DECISION_REPLACE_BLOCK, vec![1, 0, 0, 0, 0]),
        ];
        for (kind, payload) in malformed {
            let mut services = NativeCallbackServices::for_block_decision(
                veto,
                FcResourceHandle::from_raw(1),
                event_context(),
                NativeEventRoute::BlockPlaceDecision,
            );
            assert_eq!(
                services.emit(command(kind, FcResourceHandle::INVALID, payload)),
                FC_INVALID_ARGUMENT,
                "malformed decision kind {}",
                kind.raw()
            );
            let completion = services.complete(FC_OK);
            assert!(!completion.is_committed());
            assert!(completion.effects().is_empty());
            assert!(matches!(
                completion.first_error(),
                Some(NativeServiceError::MalformedPayload { .. })
            ));
        }

        let mut replace_on_break = NativeCallbackServices::for_block_decision(
            veto,
            FcResourceHandle::from_raw(1),
            event_context(),
            NativeEventRoute::BlockBreakDecision,
        );
        assert_eq!(
            replace_on_break.emit(command(
                FcCommandKind::DECISION_REPLACE_BLOCK,
                FcResourceHandle::INVALID,
                1_u32.to_le_bytes().to_vec(),
            )),
            FC_INVALID_ARGUMENT
        );
        assert!(!replace_on_break.complete(FC_OK).is_committed());

        let mut notification =
            NativeCallbackServices::for_event(veto, FcResourceHandle::from_raw(1), event_context());
        assert_eq!(
            notification.emit(command(
                FcCommandKind::DECISION_ALLOW,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_INVALID_ARGUMENT
        );
        assert!(!notification.complete(FC_OK).is_committed());

        let mut poisoned_after_decision = NativeCallbackServices::for_block_decision(
            veto.with(Capability::SubmitIntents),
            FcResourceHandle::from_raw(1),
            event_context(),
            NativeEventRoute::BlockPlaceDecision,
        );
        assert_eq!(
            poisoned_after_decision.emit(command(
                FcCommandKind::DECISION_ALLOW,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_OK
        );
        assert_eq!(
            poisoned_after_decision.emit(command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_INVALID_ARGUMENT
        );
        let poisoned_after_decision = poisoned_after_decision.complete(FC_OK);
        assert!(!poisoned_after_decision.is_committed());
        assert!(poisoned_after_decision.effects().is_empty());
    }

    #[test]
    fn decision_stage_accepts_the_full_intent_budget_plus_one_decision() {
        let caps = CapabilityManifest::empty()
            .with(Capability::SubmitIntents)
            .with(Capability::VetoBlockEdits);
        let mut services = NativeCallbackServices::for_block_decision(
            caps,
            FcResourceHandle::from_raw(1),
            event_context(),
            NativeEventRoute::BlockPlaceDecision,
        );
        for _ in 0..MAX_EMITTED_INTENTS {
            assert_eq!(
                services.emit(command(
                    FcCommandKind::MESSAGE,
                    FcResourceHandle::INVALID,
                    message_payload(b"bounded"),
                )),
                FC_OK
            );
        }
        assert_eq!(
            services.emit(command(
                FcCommandKind::DECISION_ALLOW,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_OK
        );

        let completion = services.complete(FC_OK);
        assert!(completion.is_committed());
        assert_eq!(completion.effects().len(), MAX_DECISION_EFFECTS);
        assert_eq!(
            completion
                .effects()
                .iter()
                .filter(|effect| matches!(effect, NativeEffect::Intent(_)))
                .count(),
            MAX_EMITTED_INTENTS
        );
        assert!(matches!(
            completion.effects().last(),
            Some(NativeEffect::BlockDecision(PluginBlockDecision::Allow))
        ));
    }

    #[test]
    fn decision_stage_intent_overflow_rolls_back_every_effect() {
        let caps = CapabilityManifest::empty()
            .with(Capability::SubmitIntents)
            .with(Capability::VetoBlockEdits);
        let mut services = NativeCallbackServices::for_block_decision(
            caps,
            FcResourceHandle::from_raw(1),
            event_context(),
            NativeEventRoute::BlockPlaceDecision,
        );
        assert_eq!(
            services.emit(command(
                FcCommandKind::DECISION_ALLOW,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_OK
        );
        for _ in 0..MAX_EMITTED_INTENTS {
            assert_eq!(
                services.emit(command(
                    FcCommandKind::MESSAGE,
                    FcResourceHandle::INVALID,
                    message_payload(b"bounded"),
                )),
                FC_OK
            );
        }
        assert_eq!(
            services.emit(command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::INVALID,
                message_payload(b"overflow"),
            )),
            FC_COMMAND_BUFFER_FULL
        );

        let completion = services.complete(FC_OK);
        assert!(!completion.is_committed());
        assert!(completion.effects().is_empty());
        assert!(matches!(
            completion.first_error(),
            Some(NativeServiceError::CommandBufferFull { .. })
        ));
    }

    #[test]
    fn successful_callback_exposes_staged_message_only_at_completion() {
        let caps = CapabilityManifest::empty().with(Capability::SubmitIntents);
        let mut services =
            NativeCallbackServices::for_event(caps, FcResourceHandle::from_raw(1), event_context());
        assert_eq!(
            services.emit(command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::INVALID,
                message_payload(b"hello"),
            )),
            FC_OK
        );

        let completion = services.complete(FC_OK);
        assert!(completion.is_committed());
        assert_eq!(completion.effects().len(), 1);
        assert_eq!(
            completion.effects(),
            &[NativeEffect::Intent(WorldIntent::Message {
                player: player(),
                message: TextComponent::text("hello"),
            })]
        );
    }

    #[test]
    fn callback_failure_discards_every_staged_effect() {
        let caps = CapabilityManifest::empty().with(Capability::SubmitIntents);
        let mut services =
            NativeCallbackServices::for_event(caps, FcResourceHandle::from_raw(1), event_context());
        assert_eq!(
            services.emit(command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::INVALID,
                message_payload(b"discard me"),
            )),
            FC_OK
        );

        let completion = services.complete(FC_PLUGIN_PANIC);
        assert!(!completion.is_committed());
        assert!(completion.effects().is_empty());
        assert_eq!(completion.callback_status(), FC_PLUGIN_PANIC);
    }

    #[test]
    fn capability_denial_poison_discards_a_prior_valid_command() {
        let caps = CapabilityManifest::empty().with(Capability::SubmitIntents);
        let mut services =
            NativeCallbackServices::for_event(caps, FcResourceHandle::from_raw(1), event_context());
        assert_eq!(
            services.emit(command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::INVALID,
                message_payload(b"must not escape"),
            )),
            FC_OK
        );

        let outcome = services.call(OwnedHostRequest::new(
            FcHostRequestKind::DIMENSION,
            0,
            FcResourceHandle::INVALID,
            Vec::new(),
        ));
        assert_eq!(outcome, HostCallOutcome::Status(FC_CAPABILITY_DENIED));

        let completion = services.complete(FC_OK);
        assert!(!completion.is_committed());
        assert!(completion.effects().is_empty());
        assert_eq!(completion.capability_denial(), Some(Capability::ReadWorld));
    }

    #[test]
    fn capability_denial_is_not_masked_by_an_earlier_validation_error() {
        let caps = CapabilityManifest::empty().with(Capability::SubmitIntents);
        let mut services =
            NativeCallbackServices::for_event(caps, FcResourceHandle::from_raw(1), event_context());
        assert_eq!(
            services.emit(command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::INVALID,
                message_payload(b"must be discarded"),
            )),
            FC_OK
        );
        assert_eq!(
            services.emit(command(
                FcCommandKind::from_raw(u32::MAX),
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_INVALID_ARGUMENT
        );
        assert_eq!(
            services.call(OwnedHostRequest::new(
                FcHostRequestKind::DIMENSION,
                0,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            HostCallOutcome::Status(FC_CAPABILITY_DENIED)
        );

        let completion = services.complete(FC_OK);
        assert!(!completion.is_committed());
        assert!(completion.effects().is_empty());
        assert_eq!(completion.capability_denial(), Some(Capability::ReadWorld));
        assert!(matches!(
            completion.first_error(),
            Some(NativeServiceError::UnsupportedCommand { .. })
        ));
    }

    #[test]
    fn known_operations_check_capability_before_other_validation() {
        let mut services = NativeCallbackServices::for_event(
            CapabilityManifest::empty(),
            FcResourceHandle::from_raw(1),
            event_context(),
        );
        let invalid_message = OwnedCommand::new(
            FcCommandKind::MESSAGE,
            1,
            FcResourceHandle::from_raw(99),
            Vec::new(),
        );
        assert_eq!(services.emit(invalid_message), FC_CAPABILITY_DENIED);
        assert_eq!(
            services.call(OwnedHostRequest::new(
                FcHostRequestKind::DIMENSION,
                1,
                FcResourceHandle::from_raw(99),
                vec![1],
            )),
            HostCallOutcome::Status(FC_CAPABILITY_DENIED)
        );
        let completion = services.complete(FC_OK);
        assert!(!completion.is_committed());
        assert_eq!(
            completion.capability_denial(),
            Some(Capability::SubmitIntents)
        );

        let mut set_block = NativeCallbackServices::for_event(
            CapabilityManifest::empty(),
            FcResourceHandle::from_raw(1),
            event_context(),
        );
        assert_eq!(
            set_block.emit(command(
                FcCommandKind::SET_BLOCK,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_CAPABILITY_DENIED
        );

        for (kind, capability) in [
            (FcCommandKind::SUBSCRIBE_EVENT, Capability::ReceiveEvents),
            (
                FcCommandKind::REGISTER_COMMAND,
                Capability::RegisterCommands,
            ),
            (FcCommandKind::STORAGE_PUT, Capability::Storage),
            (FcCommandKind::STORAGE_DELETE, Capability::Storage),
        ] {
            let mut services = NativeCallbackServices::for_event(
                CapabilityManifest::empty(),
                FcResourceHandle::from_raw(1),
                event_context(),
            );
            assert_eq!(
                services.emit(OwnedCommand::new(
                    kind,
                    1,
                    FcResourceHandle::from_raw(99),
                    Vec::new(),
                )),
                FC_CAPABILITY_DENIED
            );
            assert_eq!(
                services.complete(FC_OK).capability_denial(),
                Some(capability)
            );
        }

        for (kind, capability) in [
            (FcHostRequestKind::CHUNK_LOADED, Capability::ReadWorld),
            (FcHostRequestKind::BLOCK_STATE, Capability::ReadWorld),
            (FcHostRequestKind::PLAYER_POSITION, Capability::ReadWorld),
            (
                FcHostRequestKind::PERMISSION_RESOLVE,
                Capability::ReadPermissions,
            ),
            (FcHostRequestKind::STORAGE_GET, Capability::Storage),
            (FcHostRequestKind::STORAGE_KEYS, Capability::Storage),
        ] {
            let mut services = NativeCallbackServices::for_event(
                CapabilityManifest::empty(),
                FcResourceHandle::from_raw(1),
                event_context(),
            );
            assert_eq!(
                services.call(OwnedHostRequest::new(
                    kind,
                    1,
                    FcResourceHandle::from_raw(99),
                    vec![1],
                )),
                HostCallOutcome::Status(FC_CAPABILITY_DENIED)
            );
            assert_eq!(
                services.complete(FC_OK).capability_denial(),
                Some(capability)
            );
        }
    }

    #[test]
    fn initialization_stages_only_valid_subscriptions() {
        let caps = CapabilityManifest::empty().with(Capability::ReceiveEvents);
        let mut services = NativeCallbackServices::for_initialization(caps);
        assert_eq!(
            services.emit(command(
                FcCommandKind::SUBSCRIBE_EVENT,
                FcResourceHandle::INVALID,
                FcEventKind::BLOCK_BREAK.raw().to_le_bytes().to_vec(),
            )),
            FC_OK
        );
        assert_eq!(
            services.emit(command(
                FcCommandKind::SUBSCRIBE_EVENT,
                FcResourceHandle::INVALID,
                FcEventKind::BLOCK_BREAK_ATTEMPT
                    .raw()
                    .to_le_bytes()
                    .to_vec(),
            )),
            FC_INVALID_ARGUMENT
        );

        let completion = services.complete(FC_OK);
        assert!(completion.is_committed());
        assert_eq!(
            completion.effects(),
            &[NativeEffect::Subscribe(EventKind::BlockBreak)]
        );
    }

    #[test]
    fn command_buffer_rejects_newest_without_discarding_prior_entries() {
        let caps = CapabilityManifest::empty().with(Capability::SubmitIntents);
        let mut services =
            NativeCallbackServices::for_event(caps, FcResourceHandle::from_raw(1), event_context());
        for _ in 0..MAX_EMITTED_INTENTS {
            assert_eq!(
                services.emit(command(
                    FcCommandKind::MESSAGE,
                    FcResourceHandle::INVALID,
                    message_payload(b"bounded"),
                )),
                FC_OK
            );
        }
        assert_eq!(
            services.emit(command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::INVALID,
                message_payload(b"rejected"),
            )),
            FC_COMMAND_BUFFER_FULL
        );

        let completion = services.complete(FC_OK);
        assert!(completion.is_committed());
        assert_eq!(completion.effects().len(), MAX_EMITTED_INTENTS);
        assert!(matches!(
            completion.first_error(),
            Some(NativeServiceError::CommandBufferFull { .. })
        ));

        let mut invalid_services =
            NativeCallbackServices::for_event(caps, FcResourceHandle::from_raw(1), event_context());
        for _ in 0..MAX_EMITTED_INTENTS {
            assert_eq!(
                invalid_services.emit(command(
                    FcCommandKind::MESSAGE,
                    FcResourceHandle::INVALID,
                    message_payload(b"bounded"),
                )),
                FC_OK
            );
        }
        assert_eq!(
            invalid_services.emit(command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::from_raw(9),
                message_payload(b"wrong target"),
            )),
            FC_INVALID_ARGUMENT,
            "envelope validation precedes command-buffer admission"
        );
        assert_eq!(
            invalid_services.emit(command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::INVALID,
                Vec::new(),
            )),
            FC_INVALID_ARGUMENT,
            "payload validation precedes command-buffer admission"
        );
    }

    #[test]
    fn malformed_message_payloads_are_rejected_without_panicking() {
        let caps = CapabilityManifest::empty().with(Capability::SubmitIntents);
        for payload in [
            Vec::new(),
            vec![0; 15],
            player().as_uuid().as_bytes().to_vec(),
            {
                let mut oversized = player().as_uuid().as_bytes().to_vec();
                oversized.extend_from_slice(&u32::MAX.to_le_bytes());
                oversized
            },
            message_payload(&[0xff]),
            {
                let mut trailing = message_payload(b"valid");
                trailing.push(0);
                trailing
            },
        ] {
            let mut services = NativeCallbackServices::for_event(
                caps,
                FcResourceHandle::from_raw(1),
                event_context(),
            );
            assert_eq!(
                services.emit(command(
                    FcCommandKind::MESSAGE,
                    FcResourceHandle::INVALID,
                    payload,
                )),
                FC_INVALID_ARGUMENT
            );
            assert!(services.complete(FC_OK).effects().is_empty());
        }
    }

    #[test]
    fn malformed_teleport_payloads_are_rejected_without_panicking() {
        let caps = CapabilityManifest::empty().with(Capability::SubmitIntents);
        let valid = teleport_payload(1.0, 2.0, 3.0);
        for payload in [
            Vec::new(),
            valid[..valid.len() - 1].to_vec(),
            {
                let mut trailing = valid.clone();
                trailing.push(0);
                trailing
            },
            teleport_payload(f64::NAN, 2.0, 3.0),
            teleport_payload(1.0, f64::INFINITY, 3.0),
        ] {
            let mut services = NativeCallbackServices::for_event(
                caps,
                FcResourceHandle::from_raw(1),
                event_context(),
            );
            assert_eq!(
                services.emit(command(
                    FcCommandKind::TELEPORT,
                    FcResourceHandle::INVALID,
                    payload,
                )),
                FC_INVALID_ARGUMENT
            );
            assert!(services.complete(FC_OK).effects().is_empty());
        }
    }

    #[test]
    fn malformed_subscriptions_and_envelopes_are_rejected() {
        let receive = CapabilityManifest::empty().with(Capability::ReceiveEvents);
        let valid = FcEventKind::BLOCK_BREAK.raw().to_le_bytes();
        for payload in [
            Vec::new(),
            valid[..3].to_vec(),
            {
                let mut trailing = valid.to_vec();
                trailing.push(0);
                trailing
            },
            u32::MAX.to_le_bytes().to_vec(),
        ] {
            let mut services = NativeCallbackServices::for_initialization(receive);
            assert_eq!(
                services.emit(command(
                    FcCommandKind::SUBSCRIBE_EVENT,
                    FcResourceHandle::INVALID,
                    payload,
                )),
                FC_INVALID_ARGUMENT
            );
            assert!(services.complete(FC_OK).effects().is_empty());
        }

        let submit = CapabilityManifest::empty().with(Capability::SubmitIntents);
        for invalid in [
            OwnedCommand::new(
                FcCommandKind::MESSAGE,
                1,
                FcResourceHandle::INVALID,
                message_payload(b"flags"),
            ),
            command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::from_raw(9),
                message_payload(b"target"),
            ),
        ] {
            let mut services = NativeCallbackServices::for_event(
                submit,
                FcResourceHandle::from_raw(1),
                event_context(),
            );
            assert_eq!(services.emit(invalid), FC_INVALID_ARGUMENT);
            assert!(services.complete(FC_OK).effects().is_empty());
        }

        let mut wrong_phase = NativeCallbackServices::for_initialization(submit);
        assert_eq!(
            wrong_phase.emit(command(
                FcCommandKind::MESSAGE,
                FcResourceHandle::INVALID,
                message_payload(b"phase"),
            )),
            FC_INVALID_ARGUMENT
        );
        let mut subscribe_during_event = NativeCallbackServices::for_event(
            receive,
            FcResourceHandle::from_raw(1),
            event_context(),
        );
        assert_eq!(
            subscribe_during_event.emit(command(
                FcCommandKind::SUBSCRIBE_EVENT,
                FcResourceHandle::INVALID,
                valid.to_vec(),
            )),
            FC_INVALID_ARGUMENT
        );
    }

    #[test]
    fn diagnostics_are_bounded_and_retained_on_callback_failure() {
        let mut services = NativeCallbackServices::for_shutdown(CapabilityManifest::empty());
        assert_eq!(
            services.diagnostic(FC_DIAGNOSTIC_INFO, "fixture detail".to_owned()),
            FC_OK
        );

        let completion = services.complete(FC_PLUGIN_PANIC);
        assert_eq!(completion.diagnostics().len(), 1);
        assert_eq!(completion.diagnostics()[0].level(), FC_DIAGNOSTIC_INFO);
        assert_eq!(completion.diagnostics()[0].message(), "fixture detail");
    }
}
