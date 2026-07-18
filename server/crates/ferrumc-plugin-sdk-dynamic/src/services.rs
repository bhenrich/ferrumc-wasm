//! One-call ABI implementation of the shared SDK host-services seam.

use ferrumc_plugin_abi::{
    FcCommandKind, FcHostRequestKind, FcResourceHandle, FcStatus, FC_BUFFER_TOO_SMALL,
    FC_CAPABILITY_DENIED, FC_COMMAND_BUFFER_FULL, FC_ERROR, FC_INVALID_ARGUMENT, FC_PLUGIN_PANIC,
};
use ferrumc_plugin_abi_sys::{PluginCall, PluginCallError};
use ferrumc_plugin_sdk::{
    BlockDecision, BlockPos, Capability, CapabilityManifest, ChunkPos, CommandDefinition,
    DiagnosticLevel, EventDecision, EventKind, FacadeError, HostServices, PermissionNode, PlayerId,
    Resolution, TimerId, Vec3, WorldOperation,
};

use crate::codec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Load,
    Event,
    Unload,
}

pub(crate) struct AbiServices<'borrow, 'call> {
    call: &'borrow mut PluginCall<'call>,
    capabilities: CapabilityManifest,
    phase: Phase,
    dimension: Option<FcResourceHandle>,
}

impl<'borrow, 'call> AbiServices<'borrow, 'call> {
    pub(crate) fn load(
        call: &'borrow mut PluginCall<'call>,
        capabilities: CapabilityManifest,
    ) -> Self {
        Self::new(call, capabilities, Phase::Load)
    }

    pub(crate) fn event(
        call: &'borrow mut PluginCall<'call>,
        capabilities: CapabilityManifest,
    ) -> Self {
        Self::new(call, capabilities, Phase::Event)
    }

    pub(crate) fn unload(
        call: &'borrow mut PluginCall<'call>,
        capabilities: CapabilityManifest,
    ) -> Self {
        Self::new(call, capabilities, Phase::Unload)
    }

    fn new(
        call: &'borrow mut PluginCall<'call>,
        capabilities: CapabilityManifest,
        phase: Phase,
    ) -> Self {
        Self {
            call,
            capabilities,
            phase,
            dimension: None,
        }
    }

    pub(crate) fn emit_block_decision(
        &mut self,
        decision: &BlockDecision,
    ) -> Result<(), FacadeError> {
        self.require_event_phase("block decision")?;
        match decision {
            BlockDecision::Allow => self.emit(
                FcCommandKind::DECISION_ALLOW,
                FcResourceHandle::INVALID,
                &[],
                Some(Capability::VetoBlockEdits),
            ),
            BlockDecision::Deny(feedback) => {
                let payload = codec::encode_feedback(feedback.as_ref())?;
                self.emit(
                    FcCommandKind::DECISION_DENY,
                    FcResourceHandle::INVALID,
                    &payload,
                    Some(Capability::VetoBlockEdits),
                )
            }
            BlockDecision::Replace(block_state_id) => {
                let payload = codec::encode_u32(*block_state_id);
                self.emit(
                    FcCommandKind::DECISION_REPLACE_BLOCK,
                    FcResourceHandle::INVALID,
                    &payload,
                    Some(Capability::VetoBlockEdits),
                )
            }
            _ => Err(FacadeError::Unavailable {
                operation: "future block decision",
            }),
        }
    }

    pub(crate) fn emit_event_decision(
        &mut self,
        decision: &EventDecision,
        capability: Capability,
    ) -> Result<(), FacadeError> {
        self.require_event_phase("event decision")?;
        match decision {
            EventDecision::Allow => self.emit(
                FcCommandKind::DECISION_ALLOW,
                FcResourceHandle::INVALID,
                &[],
                Some(capability),
            ),
            EventDecision::Deny(feedback) => {
                let payload = codec::encode_feedback(feedback.as_ref())?;
                self.emit(
                    FcCommandKind::DECISION_DENY,
                    FcResourceHandle::INVALID,
                    &payload,
                    Some(capability),
                )
            }
            _ => Err(FacadeError::Unavailable {
                operation: "future event decision",
            }),
        }
    }

    fn require_event_phase(&self, operation: &'static str) -> Result<(), FacadeError> {
        if self.phase == Phase::Event {
            Ok(())
        } else {
            Err(FacadeError::Unavailable { operation })
        }
    }

    fn dimension_for_read(&mut self) -> Result<FcResourceHandle, FacadeError> {
        if let Some(dimension) = self.dimension {
            return Ok(dimension);
        }
        let response = self.request(
            FcHostRequestKind::DIMENSION,
            FcResourceHandle::INVALID,
            &[],
            Some(Capability::ReadWorld),
            "dimension",
        )?;
        let raw = codec::decode_dimension(&response)
            .map_err(|error| invalid_response("dimension", error.reason()))?;
        let handle = FcResourceHandle::from_raw(raw);
        self.dimension = Some(handle);
        Ok(handle)
    }

    fn dimension_for_set_block(&mut self) -> Result<FcResourceHandle, FacadeError> {
        if let Some(dimension) = self.dimension {
            return Ok(dimension);
        }
        let response =
            match self
                .call
                .request(FcHostRequestKind::DIMENSION, FcResourceHandle::INVALID, &[])
            {
                Ok(response) => response,
                Err(PluginCallError::HostStatus(status))
                    if status == FC_CAPABILITY_DENIED
                        && !self.capabilities.grants(Capability::ReadWorld) =>
                {
                    return Err(FacadeError::Unavailable {
                        operation: "set block dimension lookup without read-world host support",
                    })
                }
                Err(error) => {
                    return Err(map_call_error(
                        error,
                        Some(Capability::ReadWorld),
                        "dimension",
                    ))
                }
            };
        let raw = codec::decode_dimension(&response)
            .map_err(|error| invalid_response("dimension", error.reason()))?;
        let handle = FcResourceHandle::from_raw(raw);
        self.dimension = Some(handle);
        Ok(handle)
    }

    fn request(
        &mut self,
        kind: FcHostRequestKind,
        target: FcResourceHandle,
        payload: &[u8],
        capability: Option<Capability>,
        resource: &'static str,
    ) -> Result<Vec<u8>, FacadeError> {
        self.call
            .request(kind, target, payload)
            .map_err(|error| map_call_error(error, capability, resource))
    }

    fn emit(
        &mut self,
        kind: FcCommandKind,
        target: FcResourceHandle,
        payload: &[u8],
        capability: Option<Capability>,
    ) -> Result<(), FacadeError> {
        self.call
            .emit(kind, target, payload)
            .map_err(|error| map_call_error(error, capability, "command emission"))
    }
}

impl HostServices for AbiServices<'_, '_> {
    fn capabilities(&self) -> CapabilityManifest {
        self.capabilities
    }

    fn subscribe_event(&mut self, kind: EventKind) -> Result<(), FacadeError> {
        if self.phase != Phase::Load {
            return Err(FacadeError::Unavailable {
                operation: "event subscription outside load",
            });
        }
        let payload = codec::encode_u32(codec::event_kind(kind)?.raw());
        self.emit(
            FcCommandKind::SUBSCRIBE_EVENT,
            FcResourceHandle::INVALID,
            &payload,
            Some(Capability::ReceiveEvents),
        )
    }

    fn register_command(&mut self, command: &CommandDefinition) -> Result<(), FacadeError> {
        if self.phase != Phase::Load {
            return Err(FacadeError::Unavailable {
                operation: "command registration outside load",
            });
        }
        let payload = codec::encode_command_definition(command)?;
        self.emit(
            FcCommandKind::REGISTER_COMMAND,
            FcResourceHandle::INVALID,
            &payload,
            Some(Capability::RegisterCommands),
        )
    }

    fn is_chunk_loaded(&mut self, chunk: ChunkPos) -> Result<bool, FacadeError> {
        self.require_event_phase("world query outside event")?;
        let dimension = self.dimension_for_read()?;
        let payload = codec::encode_chunk(chunk);
        let response = self.request(
            FcHostRequestKind::CHUNK_LOADED,
            dimension,
            &payload,
            Some(Capability::ReadWorld),
            "chunk-loaded response",
        )?;
        codec::decode_bool(&response)
            .map_err(|error| invalid_response("chunk-loaded response", error.reason()))
    }

    fn block_state_id(&mut self, pos: BlockPos) -> Result<Option<u32>, FacadeError> {
        if !self.is_chunk_loaded(pos.to_chunk_pos())? {
            return Ok(None);
        }
        let dimension = self.dimension_for_read()?;
        let payload = codec::encode_block_state_request(pos);
        let response = self.request(
            FcHostRequestKind::BLOCK_STATE,
            dimension,
            &payload,
            Some(Capability::ReadWorld),
            "block-state response",
        )?;
        codec::decode_block_state(&response)
            .map(Some)
            .map_err(|error| invalid_response("block-state response", error.reason()))
    }

    fn player_position(&mut self, player: PlayerId) -> Result<Option<Vec3>, FacadeError> {
        self.require_event_phase("world query outside event")?;
        let payload = codec::encode_player(player);
        let response = self.request(
            FcHostRequestKind::PLAYER_POSITION,
            FcResourceHandle::INVALID,
            &payload,
            Some(Capability::ReadWorld),
            "player-position response",
        )?;
        codec::decode_player_position(&response)
            .map_err(|error| invalid_response("player-position response", error.reason()))
    }

    fn submit_world_operation(&mut self, operation: WorldOperation) -> Result<(), FacadeError> {
        self.require_event_phase("world operation outside event")?;
        match operation {
            WorldOperation::SetBlock(operation) => {
                let dimension = self.dimension_for_set_block()?;
                let payload = codec::encode_set_block(operation.pos(), operation.block_state_id());
                self.emit(
                    FcCommandKind::SET_BLOCK,
                    dimension,
                    &payload,
                    Some(Capability::SubmitIntents),
                )
            }
            WorldOperation::Teleport(operation) => {
                let payload = codec::encode_player_vec(operation.player(), operation.position());
                self.emit(
                    FcCommandKind::TELEPORT,
                    FcResourceHandle::INVALID,
                    &payload,
                    Some(Capability::SubmitIntents),
                )
            }
            WorldOperation::Message(operation) => {
                let payload = codec::encode_player_text(operation.player(), operation.message())?;
                self.emit(
                    FcCommandKind::MESSAGE,
                    FcResourceHandle::INVALID,
                    &payload,
                    Some(Capability::SubmitIntents),
                )
            }
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
        self.require_event_phase("permission query outside event")?;
        let payload = codec::encode_permission(player, node)?;
        let response = self.request(
            FcHostRequestKind::PERMISSION_RESOLVE,
            FcResourceHandle::INVALID,
            &payload,
            Some(Capability::ReadPermissions),
            "permission response",
        )?;
        codec::decode_resolution(&response)
            .map_err(|error| invalid_response("permission response", error.reason()))
    }

    fn storage_get(&mut self, key: &str) -> Result<Option<Vec<u8>>, FacadeError> {
        let payload = codec::encode_text(key)?;
        let response = self.request(
            FcHostRequestKind::STORAGE_GET,
            FcResourceHandle::INVALID,
            &payload,
            Some(Capability::Storage),
            "storage-get response",
        )?;
        codec::decode_storage_value(&response)
            .map_err(|error| invalid_response("storage-get response", error.reason()))
    }

    fn storage_put(&mut self, key: &str, value: &[u8]) -> Result<(), FacadeError> {
        let payload = codec::encode_text_bytes(key, value)?;
        self.emit(
            FcCommandKind::STORAGE_PUT,
            FcResourceHandle::INVALID,
            &payload,
            Some(Capability::Storage),
        )
    }

    fn storage_delete(&mut self, key: &str) -> Result<(), FacadeError> {
        let payload = codec::encode_text(key)?;
        self.emit(
            FcCommandKind::STORAGE_DELETE,
            FcResourceHandle::INVALID,
            &payload,
            Some(Capability::Storage),
        )
    }

    fn storage_keys(&mut self) -> Result<Vec<String>, FacadeError> {
        let response = self.request(
            FcHostRequestKind::STORAGE_KEYS,
            FcResourceHandle::INVALID,
            &[],
            Some(Capability::Storage),
            "storage-keys response",
        )?;
        codec::decode_storage_keys(&response)
            .map_err(|error| invalid_response("storage-keys response", error.reason()))
    }

    fn schedule_timer(&mut self, id: TimerId, delay_ticks: u64) -> Result<(), FacadeError> {
        let payload = codec::encode_timer(id, delay_ticks);
        self.emit(
            FcCommandKind::SCHEDULE_TIMER,
            FcResourceHandle::INVALID,
            &payload,
            None,
        )
    }

    fn cancel_timer(&mut self, id: TimerId) -> Result<(), FacadeError> {
        let payload = codec::encode_u64(id.get());
        self.emit(
            FcCommandKind::CANCEL_TIMER,
            FcResourceHandle::INVALID,
            &payload,
            None,
        )
    }

    fn diagnostic(&mut self, level: DiagnosticLevel, message: &str) -> Result<(), FacadeError> {
        let raw = match level {
            DiagnosticLevel::Error => ferrumc_plugin_abi::FC_DIAGNOSTIC_ERROR,
            DiagnosticLevel::Warn => ferrumc_plugin_abi::FC_DIAGNOSTIC_WARN,
            DiagnosticLevel::Info => ferrumc_plugin_abi::FC_DIAGNOSTIC_INFO,
            DiagnosticLevel::Debug => ferrumc_plugin_abi::FC_DIAGNOSTIC_DEBUG,
            DiagnosticLevel::Trace => ferrumc_plugin_abi::FC_DIAGNOSTIC_TRACE,
            _ => {
                return Err(FacadeError::Unavailable {
                    operation: "future diagnostic level",
                })
            }
        };
        self.call
            .diagnostic(raw, message)
            .map_err(|error| map_call_error(error, None, "diagnostic"))
    }
}

fn map_call_error(
    error: PluginCallError,
    capability: Option<Capability>,
    resource: &'static str,
) -> FacadeError {
    match error {
        PluginCallError::PayloadTooLong => FacadeError::InvalidInput {
            resource: "ABI payload",
            reason: "length is not representable by ABI v1",
        },
        PluginCallError::InvalidHostOutput => {
            invalid_response(resource, "output buffer protocol was violated")
        }
        PluginCallError::HostStatus(status) if status == FC_COMMAND_BUFFER_FULL => {
            FacadeError::BufferFull
        }
        PluginCallError::HostStatus(status) if status == FC_CAPABILITY_DENIED => capability
            .map_or_else(
                || FacadeError::Host("host denied an unscoped packaging service".to_owned()),
                capability_denied,
            ),
        PluginCallError::HostStatus(status) if status == FC_BUFFER_TOO_SMALL => {
            invalid_response(resource, "host output buffer was too small")
        }
        PluginCallError::HostStatus(status) if status == FC_INVALID_ARGUMENT => {
            FacadeError::Rejected("host rejected the ABI argument".to_owned())
        }
        PluginCallError::HostStatus(status) if status == FC_PLUGIN_PANIC => {
            FacadeError::Host("host callback reported a panic status".to_owned())
        }
        PluginCallError::HostStatus(status) if status == FC_ERROR => {
            FacadeError::Host("host callback failed".to_owned())
        }
        PluginCallError::HostStatus(status) => FacadeError::Host(status_message(status)),
    }
}

fn capability_denied(capability: Capability) -> FacadeError {
    match CapabilityManifest::empty().require(capability) {
        Err(error) => FacadeError::Capability(error),
        Ok(()) => FacadeError::Host("host denied an unknown capability".to_owned()),
    }
}

fn status_message(status: FcStatus) -> String {
    format!("host returned unknown ABI status {}", status.code())
}

fn invalid_response(resource: &'static str, reason: &'static str) -> FacadeError {
    FacadeError::InvalidHostResponse { resource, reason }
}
