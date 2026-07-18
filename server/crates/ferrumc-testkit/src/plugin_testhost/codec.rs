//! Strict ABI v1 semantic payload codecs for the deterministic testhost.

use ferrumc_permission::MAX_NODE_LEN;
use ferrumc_plugin_abi::{
    FcEventKind, FcResourceHandle, FcStatus, ABI_MAJOR, ABI_MINOR, FC_CAPABILITY_DENIED,
    FC_EVENT_FLAGS_NONE, FC_HOST_REQUEST_FLAGS_NONE, FC_INVALID_ARGUMENT,
};
use ferrumc_plugin_abi_sys::{OwnedCommand, OwnedEvent, OwnedHostRequest};
use ferrumc_plugin_sdk::{
    BlockDecision, BlockPos, Capability, CapabilityManifest, CommandArgumentValue,
    CommandDefinition, CommandNode, CommandNodeKind, Event, EventDecision, EventKind, Feedback,
    HandlerId, IntegerBounds, InteractHand, InteractTarget, MessageOperation, PermissionNode,
    PlayerId, TeleportOperation, TimerId, Vec3, MAX_COMMAND_NAME_BYTES, MAX_COMMAND_NODES,
    MAX_FEEDBACK_BYTES, MAX_MESSAGE_BYTES, MAX_STORAGE_KEY_BYTES, MAX_STORAGE_VALUE_BYTES,
};
use uuid::Uuid;

use super::backend::FramePhase;
use super::{PluginEffect, PluginFailureKind};

const BLOCK_EVENT_RECORD_SIZE: u32 = 36;
const SET_BLOCK_RECORD_SIZE: u32 = 24;
const BLOCK_STATE_RECORD_SIZE: u32 = 20;

pub(crate) enum AbiRequest {
    Dimension,
    ChunkLoaded(ferrumc_plugin_sdk::ChunkPos),
    BlockState(BlockPos),
    PlayerPosition(PlayerId),
    Permission(PlayerId, PermissionNode),
    StorageGet(String),
    StorageKeys,
}

pub(crate) enum AbiCommand {
    Effect(PluginEffect),
    ScheduleTimer { id: TimerId, delay: u64 },
    BlockDecision(BlockDecision),
    EventDecision(EventDecision),
}

#[allow(clippy::too_many_lines)]
pub(crate) fn encode_event(
    event: &Event,
    tick: u64,
    shard: u64,
) -> Result<OwnedEvent, PluginFailureKind> {
    let mut payload = Vec::new();
    let kind = match event {
        Event::PlayerJoin(event) => {
            push_player(&mut payload, event.player());
            FcEventKind::PLAYER_JOIN
        }
        Event::PlayerLeave(event) => {
            push_player(&mut payload, event.player());
            FcEventKind::PLAYER_LEAVE
        }
        Event::BlockBreak(event) => {
            push_block_event(&mut payload, event.player(), event.pos());
            FcEventKind::BLOCK_BREAK
        }
        Event::AfterBlockPlace(event) => {
            push_player(&mut payload, event.player());
            push_pos(&mut payload, event.pos());
            payload.extend_from_slice(&event.block_state_id().to_le_bytes());
            FcEventKind::AFTER_BLOCK_PLACE
        }
        Event::AfterBlockBreak(event) => {
            push_block_event(&mut payload, event.player(), event.pos());
            FcEventKind::AFTER_BLOCK_BREAK
        }
        Event::PlayerMove(event) => {
            push_player(&mut payload, event.player());
            push_pos(&mut payload, event.from());
            push_pos(&mut payload, event.to());
            FcEventKind::PLAYER_MOVE
        }
        Event::BlockPlaceAttempt(event) => {
            push_player(&mut payload, event.player());
            push_pos(&mut payload, event.pos());
            payload.extend_from_slice(&event.block_state_id().to_le_bytes());
            FcEventKind::BLOCK_PLACE_ATTEMPT
        }
        Event::BlockBreakAttempt(event) => {
            push_block_event(&mut payload, event.player(), event.pos());
            FcEventKind::BLOCK_BREAK_ATTEMPT
        }
        Event::ChatAttempt(event) => {
            push_player(&mut payload, event.player());
            push_text(&mut payload, event.message())
                .map_err(|reason| PluginFailureKind::AbiProtocol(reason.to_owned()))?;
            FcEventKind::CHAT_ATTEMPT
        }
        Event::InteractAttempt(event) => {
            push_player(&mut payload, event.player());
            payload.push(match event.hand() {
                InteractHand::Main => 0,
                InteractHand::Off => 1,
                _ => {
                    return Err(PluginFailureKind::AbiProtocol(
                        "future interaction hand is not ABI v1".to_owned(),
                    ))
                }
            });
            match event.target() {
                InteractTarget::Air => {
                    payload.push(0);
                    payload.extend_from_slice(&0u16.to_le_bytes());
                }
                InteractTarget::Block { pos, face } => {
                    payload.push(1);
                    payload.extend_from_slice(&0u16.to_le_bytes());
                    push_pos(&mut payload, pos);
                    payload.push(direction_tag(face));
                    payload.extend_from_slice(&[0; 3]);
                }
                InteractTarget::Entity { entity } => {
                    payload.push(2);
                    payload.extend_from_slice(&0u16.to_le_bytes());
                    payload.extend_from_slice(&entity.get().to_le_bytes());
                }
                _ => {
                    return Err(PluginFailureKind::AbiProtocol(
                        "future interaction target is not ABI v1".to_owned(),
                    ))
                }
            }
            FcEventKind::INTERACT_ATTEMPT
        }
        Event::Command(invocation) => {
            payload.extend_from_slice(&invocation.handler().get().to_le_bytes());
            push_player(&mut payload, invocation.player());
            let count = u32::try_from(invocation.arguments().len()).map_err(|_| {
                PluginFailureKind::AbiProtocol(
                    "command argument count is not ABI-representable".to_owned(),
                )
            })?;
            payload.extend_from_slice(&count.to_le_bytes());
            for argument in invocation.arguments() {
                push_text(&mut payload, argument.name())
                    .map_err(|reason| PluginFailureKind::AbiProtocol(reason.to_owned()))?;
                match argument.value() {
                    CommandArgumentValue::Text(value) => {
                        payload.push(0);
                        payload.extend_from_slice(&[0; 3]);
                        push_text(&mut payload, value)
                            .map_err(|reason| PluginFailureKind::AbiProtocol(reason.to_owned()))?;
                    }
                    CommandArgumentValue::Integer(value) => {
                        payload.push(1);
                        payload.extend_from_slice(&[0; 3]);
                        payload.extend_from_slice(&value.to_le_bytes());
                    }
                    _ => {
                        return Err(PluginFailureKind::AbiProtocol(
                            "future command argument is not ABI v1".to_owned(),
                        ))
                    }
                }
            }
            FcEventKind::COMMAND
        }
        Event::Timer(timer) => {
            payload.extend_from_slice(&timer.get().to_le_bytes());
            FcEventKind::TIMER
        }
        _ => {
            return Err(PluginFailureKind::AbiProtocol(
                "future SDK event is not ABI v1".to_owned(),
            ))
        }
    };
    Ok(OwnedEvent::new(
        kind,
        FC_EVENT_FLAGS_NONE,
        tick,
        FcResourceHandle::from_raw(shard),
        payload,
    ))
}

pub(crate) fn decode_request(
    request: &OwnedHostRequest,
    phase: FramePhase,
    capabilities: CapabilityManifest,
    dimension_handle: u64,
) -> Result<AbiRequest, (FcStatus, PluginFailureKind)> {
    if request.flags() != FC_HOST_REQUEST_FLAGS_NONE {
        return invalid("host request flags are nonzero");
    }
    let kind = request.kind();
    let payload = request.payload();
    let target = request.target().raw();
    match kind.raw() {
        1 => {
            require_phase(phase, FramePhase::Event, "dimension request")?;
            require_capability(capabilities, Capability::ReadWorld)?;
            require_target(target, 0, "dimension request target")?;
            require_empty(payload, "dimension request payload")?;
            Ok(AbiRequest::Dimension)
        }
        2 => {
            require_phase(phase, FramePhase::Event, "chunk-loaded request")?;
            require_capability(capabilities, Capability::ReadWorld)?;
            require_target(target, dimension_handle, "chunk-loaded target")?;
            let mut reader = Reader::new(payload);
            let chunk = ferrumc_plugin_sdk::ChunkPos::new(reader.i32()?, reader.i32()?);
            reader.finish()?;
            Ok(AbiRequest::ChunkLoaded(chunk))
        }
        3 => {
            require_phase(phase, FramePhase::Event, "block-state request")?;
            require_capability(capabilities, Capability::ReadWorld)?;
            require_target(target, dimension_handle, "block-state target")?;
            let mut reader = Reader::new(payload);
            let declared = reader.record_header(BLOCK_STATE_RECORD_SIZE)?;
            let pos = reader.pos()?;
            reader.consume_extension(declared, BLOCK_STATE_RECORD_SIZE)?;
            reader.finish()?;
            Ok(AbiRequest::BlockState(pos))
        }
        4 => {
            require_phase(phase, FramePhase::Event, "player-position request")?;
            require_capability(capabilities, Capability::ReadWorld)?;
            require_target(target, 0, "player-position target")?;
            let mut reader = Reader::new(payload);
            let player = reader.player()?;
            reader.finish()?;
            Ok(AbiRequest::PlayerPosition(player))
        }
        5 => {
            require_phase(phase, FramePhase::Event, "permission request")?;
            require_capability(capabilities, Capability::ReadPermissions)?;
            require_target(target, 0, "permission target")?;
            let mut reader = Reader::new(payload);
            let player = reader.player()?;
            let raw = reader.text(MAX_NODE_LEN)?;
            reader.finish()?;
            let node = PermissionNode::parse(&raw)
                .map_err(|_| invalid_pair("permission node is invalid"))?;
            Ok(AbiRequest::Permission(player, node))
        }
        6 => {
            require_capability(capabilities, Capability::Storage)?;
            require_target(target, 0, "storage-get target")?;
            let mut reader = Reader::new(payload);
            let key = reader.text(MAX_STORAGE_KEY_BYTES)?;
            reader.finish()?;
            require_nonempty(&key, "storage key")?;
            Ok(AbiRequest::StorageGet(key))
        }
        7 => {
            require_capability(capabilities, Capability::Storage)?;
            require_target(target, 0, "storage-keys target")?;
            require_empty(payload, "storage-keys payload")?;
            Ok(AbiRequest::StorageKeys)
        }
        _ => invalid("host request kind is unknown"),
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn decode_command(
    command: &OwnedCommand,
    phase: FramePhase,
    capabilities: CapabilityManifest,
    dimension_handle: u64,
) -> Result<AbiCommand, (FcStatus, PluginFailureKind)> {
    if command.flags() != 0 {
        return invalid("command flags are nonzero");
    }
    let payload = command.payload();
    let target = command.target().raw();
    match command.kind().raw() {
        1 => {
            require_phase(phase, FramePhase::Event, "set-block command")?;
            require_capability(capabilities, Capability::SubmitIntents)?;
            require_target(target, dimension_handle, "set-block target")?;
            let mut reader = Reader::new(payload);
            let declared = reader.record_header(SET_BLOCK_RECORD_SIZE)?;
            let pos = reader.pos()?;
            let block_state_id = reader.u32()?;
            reader.consume_extension(declared, SET_BLOCK_RECORD_SIZE)?;
            reader.finish()?;
            Ok(AbiCommand::Effect(PluginEffect::SetBlock {
                pos,
                block_state_id,
            }))
        }
        2 => {
            require_phase(phase, FramePhase::Event, "teleport command")?;
            require_capability(capabilities, Capability::SubmitIntents)?;
            require_target(target, 0, "teleport target")?;
            let mut reader = Reader::new(payload);
            let player = reader.player()?;
            let position = reader.vec3()?;
            reader.finish()?;
            let operation = TeleportOperation::new(player, position)
                .map_err(|_| invalid_pair("teleport position is non-finite"))?;
            Ok(AbiCommand::Effect(PluginEffect::Teleport(operation)))
        }
        3 => {
            require_phase(phase, FramePhase::Event, "message command")?;
            require_capability(capabilities, Capability::SubmitIntents)?;
            require_target(target, 0, "message target")?;
            let mut reader = Reader::new(payload);
            let player = reader.player()?;
            let message = reader.text(MAX_MESSAGE_BYTES)?;
            reader.finish()?;
            let operation = MessageOperation::new(player, message)
                .map_err(|_| invalid_pair("message violates SDK bounds"))?;
            Ok(AbiCommand::Effect(PluginEffect::Message(operation)))
        }
        4 => {
            require_phase(phase, FramePhase::Load, "event subscription")?;
            require_capability(capabilities, Capability::ReceiveEvents)?;
            require_target(target, 0, "event-subscription target")?;
            let mut reader = Reader::new(payload);
            let kind = event_kind(reader.u32()?)?;
            reader.finish()?;
            Ok(AbiCommand::Effect(PluginEffect::SubscribeEvent(kind)))
        }
        5 => {
            require_phase(phase, FramePhase::Load, "command registration")?;
            require_capability(capabilities, Capability::RegisterCommands)?;
            require_target(target, 0, "command-registration target")?;
            Ok(AbiCommand::Effect(PluginEffect::RegisterCommand(
                decode_command_definition(payload)?,
            )))
        }
        6 => {
            require_capability(capabilities, Capability::Storage)?;
            require_target(target, 0, "storage-put target")?;
            let mut reader = Reader::new(payload);
            let key = reader.text(MAX_STORAGE_KEY_BYTES)?;
            require_nonempty(&key, "storage key")?;
            let value = reader.bytes(MAX_STORAGE_VALUE_BYTES)?;
            reader.finish()?;
            Ok(AbiCommand::Effect(PluginEffect::StoragePut { key, value }))
        }
        7 => {
            require_capability(capabilities, Capability::Storage)?;
            require_target(target, 0, "storage-delete target")?;
            let mut reader = Reader::new(payload);
            let key = reader.text(MAX_STORAGE_KEY_BYTES)?;
            require_nonempty(&key, "storage key")?;
            reader.finish()?;
            Ok(AbiCommand::Effect(PluginEffect::StorageDelete { key }))
        }
        8 => {
            require_target(target, 0, "schedule-timer target")?;
            let mut reader = Reader::new(payload);
            let id = nonzero_timer(reader.u64()?)?;
            let delay = reader.u64()?;
            if delay == 0 {
                return invalid("timer delay is zero");
            }
            reader.finish()?;
            Ok(AbiCommand::ScheduleTimer { id, delay })
        }
        9 => {
            require_target(target, 0, "cancel-timer target")?;
            let mut reader = Reader::new(payload);
            let id = nonzero_timer(reader.u64()?)?;
            reader.finish()?;
            Ok(AbiCommand::Effect(PluginEffect::CancelTimer { id }))
        }
        10 => {
            require_phase(phase, FramePhase::Event, "allow decision")?;
            require_target(target, 0, "allow-decision target")?;
            require_empty(payload, "allow-decision payload")?;
            Ok(AbiCommand::EventDecision(EventDecision::Allow))
        }
        11 => {
            require_phase(phase, FramePhase::Event, "deny decision")?;
            require_target(target, 0, "deny-decision target")?;
            let feedback = decode_optional_feedback(payload)?;
            Ok(AbiCommand::EventDecision(EventDecision::Deny(feedback)))
        }
        12 => {
            require_phase(phase, FramePhase::Event, "replace decision")?;
            require_target(target, 0, "replace-decision target")?;
            let mut reader = Reader::new(payload);
            let block_state_id = reader.u32()?;
            reader.finish()?;
            Ok(AbiCommand::BlockDecision(BlockDecision::Replace(
                block_state_id,
            )))
        }
        _ => invalid("command kind is unknown"),
    }
}

fn decode_command_definition(
    payload: &[u8],
) -> Result<CommandDefinition, (FcStatus, PluginFailureKind)> {
    let mut reader = Reader::new(payload);
    let count = reader.count(MAX_COMMAND_NODES, "command node count exceeds SDK bound")?;
    if count == 0 {
        return invalid("command tree is empty");
    }
    let mut nodes = Vec::with_capacity(count);
    for _ in 0..count {
        let parent_raw = reader.u32()?;
        let parent = if parent_raw == u32::MAX {
            None
        } else {
            Some(
                usize::try_from(parent_raw)
                    .map_err(|_| invalid_pair("command parent is not representable"))?,
            )
        };
        let kind_tag = reader.u8()?;
        let executable = match reader.u8()? {
            0 => false,
            1 => true,
            _ => return invalid("command executable tag is unknown"),
        };
        let level = match reader.u8()? {
            0xff => None,
            value @ 0..=4 => Some(value),
            _ => return invalid("command operator level is invalid"),
        };
        if reader.u8()? != 0 {
            return invalid("command reserved byte is nonzero");
        }
        let minimum = reader.i64()?;
        let maximum = reader.i64()?;
        let kind = match kind_tag {
            0 => {
                require_zero_bounds(minimum, maximum)?;
                CommandNodeKind::Literal
            }
            1 => {
                require_zero_bounds(minimum, maximum)?;
                CommandNodeKind::Word
            }
            2 => {
                require_zero_bounds(minimum, maximum)?;
                CommandNodeKind::GreedyText
            }
            3 => CommandNodeKind::Integer(
                IntegerBounds::new(minimum, maximum)
                    .map_err(|_| invalid_pair("command integer bounds are reversed"))?,
            ),
            _ => return invalid("command node kind is unknown"),
        };
        let name = reader.text(MAX_COMMAND_NAME_BYTES)?;
        let permission = reader.text(MAX_NODE_LEN)?;
        let handler_raw = reader.u64()?;
        let mut node = CommandNode::new(parent, kind, name)
            .map_err(|_| invalid_pair("command node violates SDK bounds"))?;
        if executable {
            let handler = HandlerId::new(handler_raw)
                .ok_or_else(|| invalid_pair("executable command handler is zero"))?;
            node = node.with_handler(handler);
        } else if handler_raw != 0 {
            return invalid("non-executable command handler is nonzero");
        }
        if let Some(level) = level {
            node = node
                .with_required_level(level)
                .map_err(|_| invalid_pair("command operator level is invalid"))?;
        }
        if !permission.is_empty() {
            let permission = PermissionNode::parse(&permission)
                .map_err(|_| invalid_pair("command permission node is invalid"))?;
            node = node.with_required_permission(permission);
        }
        nodes.push(node);
    }
    reader.finish()?;
    CommandDefinition::new(nodes)
        .map_err(|_| invalid_pair("command tree violates preorder or SDK bounds"))
}

fn decode_optional_feedback(
    payload: &[u8],
) -> Result<Option<Feedback>, (FcStatus, PluginFailureKind)> {
    if payload.is_empty() {
        return Ok(None);
    }
    let mut reader = Reader::new(payload);
    let message = reader.text(MAX_FEEDBACK_BYTES)?;
    reader.finish()?;
    Feedback::new(message)
        .map(Some)
        .map_err(|_| invalid_pair("decision feedback violates SDK bounds"))
}

fn event_kind(raw: u32) -> Result<EventKind, (FcStatus, PluginFailureKind)> {
    match raw {
        1 => Ok(EventKind::PlayerJoin),
        2 => Ok(EventKind::PlayerLeave),
        3 => Ok(EventKind::BlockBreak),
        4 => Ok(EventKind::AfterBlockPlace),
        5 => Ok(EventKind::AfterBlockBreak),
        6 => Ok(EventKind::PlayerMove),
        7 => Ok(EventKind::BlockPlaceAttempt),
        8 => Ok(EventKind::BlockBreakAttempt),
        9 => Ok(EventKind::ChatAttempt),
        10 => Ok(EventKind::InteractAttempt),
        11 => Ok(EventKind::Command),
        12 => Ok(EventKind::Timer),
        _ => invalid("event kind is unknown"),
    }
}

fn direction_tag(direction: ferrumc_plugin_sdk::Direction) -> u8 {
    match direction {
        ferrumc_plugin_sdk::Direction::Down => 0,
        ferrumc_plugin_sdk::Direction::Up => 1,
        ferrumc_plugin_sdk::Direction::North => 2,
        ferrumc_plugin_sdk::Direction::South => 3,
        ferrumc_plugin_sdk::Direction::West => 4,
        ferrumc_plugin_sdk::Direction::East => 5,
    }
}

fn require_phase(
    actual: FramePhase,
    expected: FramePhase,
    operation: &'static str,
) -> Result<(), (FcStatus, PluginFailureKind)> {
    if actual == expected {
        Ok(())
    } else {
        invalid(&format!("{operation} is invalid in this callback phase"))
    }
}

fn require_capability(
    capabilities: CapabilityManifest,
    capability: Capability,
) -> Result<(), (FcStatus, PluginFailureKind)> {
    if capabilities.grants(capability) {
        Ok(())
    } else {
        Err((
            FC_CAPABILITY_DENIED,
            PluginFailureKind::CapabilityDenied(capability),
        ))
    }
}

fn require_target(
    actual: u64,
    expected: u64,
    resource: &'static str,
) -> Result<(), (FcStatus, PluginFailureKind)> {
    if actual == expected {
        Ok(())
    } else {
        invalid(&format!("{resource} is invalid"))
    }
}

fn require_empty(
    payload: &[u8],
    resource: &'static str,
) -> Result<(), (FcStatus, PluginFailureKind)> {
    if payload.is_empty() {
        Ok(())
    } else {
        invalid(&format!("{resource} must be empty"))
    }
}

fn require_nonempty(
    value: &str,
    resource: &'static str,
) -> Result<(), (FcStatus, PluginFailureKind)> {
    if value.is_empty() {
        invalid(&format!("{resource} must not be empty"))
    } else {
        Ok(())
    }
}

fn require_zero_bounds(minimum: i64, maximum: i64) -> Result<(), (FcStatus, PluginFailureKind)> {
    if minimum == 0 && maximum == 0 {
        Ok(())
    } else {
        invalid("non-integer command node has nonzero bounds")
    }
}

fn nonzero_timer(raw: u64) -> Result<TimerId, (FcStatus, PluginFailureKind)> {
    TimerId::new(raw).ok_or_else(|| invalid_pair("timer id is zero"))
}

fn invalid<T>(reason: &str) -> Result<T, (FcStatus, PluginFailureKind)> {
    Err((
        FC_INVALID_ARGUMENT,
        PluginFailureKind::AbiProtocol(reason.to_owned()),
    ))
}

fn invalid_pair(reason: &str) -> (FcStatus, PluginFailureKind) {
    (
        FC_INVALID_ARGUMENT,
        PluginFailureKind::AbiProtocol(reason.to_owned()),
    )
}

fn push_block_event(bytes: &mut Vec<u8>, player: PlayerId, pos: BlockPos) {
    push_record_header(bytes, BLOCK_EVENT_RECORD_SIZE);
    push_player(bytes, player);
    push_pos(bytes, pos);
}

fn push_record_header(bytes: &mut Vec<u8>, size: u32) {
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&ABI_MAJOR.to_le_bytes());
    bytes.extend_from_slice(&ABI_MINOR.to_le_bytes());
}

fn push_player(bytes: &mut Vec<u8>, player: PlayerId) {
    bytes.extend_from_slice(&player.as_uuid().into_bytes());
}

fn push_pos(bytes: &mut Vec<u8>, pos: BlockPos) {
    bytes.extend_from_slice(&pos.x().to_le_bytes());
    bytes.extend_from_slice(&pos.y().to_le_bytes());
    bytes.extend_from_slice(&pos.z().to_le_bytes());
}

pub(crate) fn push_vec3(bytes: &mut Vec<u8>, position: Vec3) {
    bytes.extend_from_slice(&position.x.to_bits().to_le_bytes());
    bytes.extend_from_slice(&position.y.to_bits().to_le_bytes());
    bytes.extend_from_slice(&position.z.to_bits().to_le_bytes());
}

pub(crate) fn push_text(bytes: &mut Vec<u8>, value: &str) -> Result<(), &'static str> {
    push_bytes(bytes, value.as_bytes())
}

pub(crate) fn push_bytes(bytes: &mut Vec<u8>, value: &[u8]) -> Result<(), &'static str> {
    let len = u32::try_from(value.len()).map_err(|_| "field length exceeds ABI v1")?;
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

struct Reader<'bytes> {
    bytes: &'bytes [u8],
    offset: usize,
}

impl<'bytes> Reader<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'bytes [u8], (FcStatus, PluginFailureKind)> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| invalid_pair("payload offset overflowed"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| invalid_pair("payload is truncated"))?;
        self.offset = end;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], (FcStatus, PluginFailureKind)> {
        let mut value = [0; N];
        value.copy_from_slice(self.take(N)?);
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, (FcStatus, PluginFailureKind)> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| invalid_pair("payload is truncated"))
    }

    fn u16(&mut self) -> Result<u16, (FcStatus, PluginFailureKind)> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, (FcStatus, PluginFailureKind)> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, (FcStatus, PluginFailureKind)> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, (FcStatus, PluginFailureKind)> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, (FcStatus, PluginFailureKind)> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn f64(&mut self) -> Result<f64, (FcStatus, PluginFailureKind)> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn player(&mut self) -> Result<PlayerId, (FcStatus, PluginFailureKind)> {
        Ok(PlayerId::from_uuid(Uuid::from_bytes(self.array()?)))
    }

    fn pos(&mut self) -> Result<BlockPos, (FcStatus, PluginFailureKind)> {
        Ok(BlockPos::new(self.i32()?, self.i32()?, self.i32()?))
    }

    fn vec3(&mut self) -> Result<Vec3, (FcStatus, PluginFailureKind)> {
        Ok(Vec3::new(self.f64()?, self.f64()?, self.f64()?))
    }

    fn text(&mut self, maximum: usize) -> Result<String, (FcStatus, PluginFailureKind)> {
        let bytes = self.field(maximum)?;
        let text =
            core::str::from_utf8(bytes).map_err(|_| invalid_pair("text field is not UTF-8"))?;
        Ok(text.to_owned())
    }

    fn bytes(&mut self, maximum: usize) -> Result<Vec<u8>, (FcStatus, PluginFailureKind)> {
        Ok(self.field(maximum)?.to_vec())
    }

    fn field(&mut self, maximum: usize) -> Result<&'bytes [u8], (FcStatus, PluginFailureKind)> {
        let len = usize::try_from(self.u32()?)
            .map_err(|_| invalid_pair("field length is not representable"))?;
        if len > maximum {
            return invalid("field length exceeds SDK bound");
        }
        self.take(len)
    }

    fn count(
        &mut self,
        maximum: usize,
        reason: &'static str,
    ) -> Result<usize, (FcStatus, PluginFailureKind)> {
        let count =
            usize::try_from(self.u32()?).map_err(|_| invalid_pair("count is not representable"))?;
        if count > maximum {
            return invalid(reason);
        }
        Ok(count)
    }

    fn record_header(&mut self, minimum: u32) -> Result<usize, (FcStatus, PluginFailureKind)> {
        let size = self.u32()?;
        let major = self.u16()?;
        let _minor = self.u16()?;
        if size < minimum {
            return invalid("versioned record is too short");
        }
        if major != ABI_MAJOR {
            return invalid("versioned record has incompatible ABI major");
        }
        usize::try_from(size).map_err(|_| invalid_pair("record size is not representable"))
    }

    fn consume_extension(
        &mut self,
        declared: usize,
        known: u32,
    ) -> Result<(), (FcStatus, PluginFailureKind)> {
        let known = usize::try_from(known)
            .map_err(|_| invalid_pair("known record size is not representable"))?;
        let extension = declared
            .checked_sub(known)
            .ok_or_else(|| invalid_pair("record extension length underflowed"))?;
        let _extension = self.take(extension)?;
        Ok(())
    }

    fn finish(self) -> Result<(), (FcStatus, PluginFailureKind)> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            invalid("payload contains trailing bytes")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_plugin_abi::{
        FcCommandKind, FcHostRequestKind, FC_CAPABILITIES_V1, FC_COMMAND_FLAGS_NONE,
    };

    fn all_caps() -> CapabilityManifest {
        CapabilityManifest::from_bits_truncate(FC_CAPABILITIES_V1 as u32)
    }

    fn command(kind: FcCommandKind, target: u64, payload: Vec<u8>) -> OwnedCommand {
        OwnedCommand::new(
            kind,
            FC_COMMAND_FLAGS_NONE,
            FcResourceHandle::from_raw(target),
            payload,
        )
    }

    fn request(kind: FcHostRequestKind, target: u64, payload: Vec<u8>) -> OwnedHostRequest {
        OwnedHostRequest::new(
            kind,
            FC_HOST_REQUEST_FLAGS_NONE,
            FcResourceHandle::from_raw(target),
            payload,
        )
    }

    fn assert_invalid<T>(result: Result<T, (FcStatus, PluginFailureKind)>, expected_reason: &str) {
        match result {
            Err((status, PluginFailureKind::AbiProtocol(reason))) => {
                assert_eq!(status, FC_INVALID_ARGUMENT);
                assert_eq!(reason, expected_reason);
            }
            Err((status, failure)) => {
                panic!("expected ABI protocol failure, got status {status:?} and {failure:?}")
            }
            Ok(_) => panic!("expected strict ABI decoding to fail"),
        }
    }

    fn push_command_node(bytes: &mut Vec<u8>, parent: Option<u32>, name: &str, permission: &str) {
        bytes.extend_from_slice(&parent.unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0xff, 0]);
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        push_text(bytes, name).expect("bounded command name");
        push_text(bytes, permission).expect("bounded permission test input");
        bytes.extend_from_slice(&0u64.to_le_bytes());
    }

    #[test]
    fn strict_command_decoder_rejects_empty_truncated_and_trailing_payloads() {
        let empty = command(FcCommandKind::SET_BLOCK, 7, Vec::new());
        assert!(decode_command(&empty, FramePhase::Event, all_caps(), 7).is_err());

        let mut valid = Vec::new();
        push_record_header(&mut valid, SET_BLOCK_RECORD_SIZE);
        push_pos(&mut valid, BlockPos::ORIGIN);
        valid.extend_from_slice(&1u32.to_le_bytes());
        for len in 0..valid.len() {
            let truncated = command(FcCommandKind::SET_BLOCK, 7, valid[..len].to_vec());
            assert!(decode_command(&truncated, FramePhase::Event, all_caps(), 7).is_err());
        }
        valid.push(0);
        let trailing = command(FcCommandKind::SET_BLOCK, 7, valid);
        assert!(decode_command(&trailing, FramePhase::Event, all_caps(), 7).is_err());
    }

    #[test]
    fn strict_decoders_reject_bad_flags_targets_tags_and_record_headers() {
        let flagged_command = OwnedCommand::new(
            FcCommandKind::DECISION_ALLOW,
            FC_COMMAND_FLAGS_NONE + 1,
            FcResourceHandle::INVALID,
            Vec::new(),
        );
        assert_invalid(
            decode_command(&flagged_command, FramePhase::Event, all_caps(), 7),
            "command flags are nonzero",
        );

        let flagged_request = OwnedHostRequest::new(
            FcHostRequestKind::DIMENSION,
            FC_HOST_REQUEST_FLAGS_NONE + 1,
            FcResourceHandle::INVALID,
            Vec::new(),
        );
        assert_invalid(
            decode_request(&flagged_request, FramePhase::Event, all_caps(), 7),
            "host request flags are nonzero",
        );

        let wrong_target = command(FcCommandKind::MESSAGE, 9, Vec::new());
        assert!(decode_command(&wrong_target, FramePhase::Event, all_caps(), 7).is_err());

        let unknown = command(FcCommandKind::from_raw(999), 0, Vec::new());
        assert!(decode_command(&unknown, FramePhase::Event, all_caps(), 7).is_err());

        let mut header = Vec::new();
        push_record_header(&mut header, SET_BLOCK_RECORD_SIZE - 1);
        header.resize(
            usize::try_from(SET_BLOCK_RECORD_SIZE).expect("small constant"),
            0,
        );
        let short_record = command(FcCommandKind::SET_BLOCK, 7, header);
        assert!(decode_command(&short_record, FramePhase::Event, all_caps(), 7).is_err());

        let bad_request = request(FcHostRequestKind::from_raw(999), 0, Vec::new());
        assert!(decode_request(&bad_request, FramePhase::Event, all_caps(), 7).is_err());
    }

    #[test]
    fn versioned_records_reject_bad_major_and_truncated_extensions() {
        let mut bad_major = Vec::new();
        push_record_header(&mut bad_major, SET_BLOCK_RECORD_SIZE);
        bad_major[4..6].copy_from_slice(&ABI_MAJOR.wrapping_add(1).to_le_bytes());
        push_pos(&mut bad_major, BlockPos::ORIGIN);
        bad_major.extend_from_slice(&1u32.to_le_bytes());
        assert_invalid(
            decode_command(
                &command(FcCommandKind::SET_BLOCK, 7, bad_major),
                FramePhase::Event,
                all_caps(),
                7,
            ),
            "versioned record has incompatible ABI major",
        );

        let mut extension = Vec::new();
        push_record_header(&mut extension, SET_BLOCK_RECORD_SIZE + 1);
        push_pos(&mut extension, BlockPos::ORIGIN);
        extension.extend_from_slice(&1u32.to_le_bytes());
        assert_invalid(
            decode_command(
                &command(FcCommandKind::SET_BLOCK, 7, extension.clone()),
                FramePhase::Event,
                all_caps(),
                7,
            ),
            "payload is truncated",
        );

        extension.push(0xa5);
        assert!(matches!(
            decode_command(
                &command(FcCommandKind::SET_BLOCK, 7, extension),
                FramePhase::Event,
                all_caps(),
                7,
            ),
            Ok(AbiCommand::Effect(PluginEffect::SetBlock {
                pos: BlockPos::ORIGIN,
                block_state_id: 1
            }))
        ));
    }

    #[test]
    fn strict_field_decoder_rejects_invalid_utf8_oversize_and_bad_reserved_bytes() {
        let mut invalid_utf8 = Vec::new();
        invalid_utf8.extend_from_slice(&1u32.to_le_bytes());
        invalid_utf8.push(0xff);
        let delete = command(FcCommandKind::STORAGE_DELETE, 0, invalid_utf8);
        assert!(decode_command(&delete, FramePhase::Load, all_caps(), 7).is_err());

        let mut oversized = Vec::new();
        oversized.extend_from_slice(
            &u32::try_from(MAX_STORAGE_KEY_BYTES + 1)
                .expect("SDK bound fits u32")
                .to_le_bytes(),
        );
        let delete = command(FcCommandKind::STORAGE_DELETE, 0, oversized);
        assert!(decode_command(&delete, FramePhase::Load, all_caps(), 7).is_err());

        let mut tree = Vec::new();
        tree.extend_from_slice(&1u32.to_le_bytes());
        tree.extend_from_slice(&u32::MAX.to_le_bytes());
        tree.extend_from_slice(&[0, 0, 0xff, 1]);
        tree.extend_from_slice(&0i64.to_le_bytes());
        tree.extend_from_slice(&0i64.to_le_bytes());
        push_text(&mut tree, "x").expect("bounded");
        push_text(&mut tree, "").expect("bounded");
        tree.extend_from_slice(&0u64.to_le_bytes());
        let registration = command(FcCommandKind::REGISTER_COMMAND, 0, tree);
        assert!(decode_command(&registration, FramePhase::Load, all_caps(), 7).is_err());
    }

    #[test]
    fn permission_fields_accept_255_bytes_and_reject_declared_256() {
        let player = PlayerId::offline("PermissionBoundary");
        let exact = "a".repeat(MAX_NODE_LEN);

        let mut request_payload = Vec::new();
        push_player(&mut request_payload, player);
        push_text(&mut request_payload, &exact).expect("permission is ABI-representable");
        match decode_request(
            &request(FcHostRequestKind::PERMISSION_RESOLVE, 0, request_payload),
            FramePhase::Event,
            all_caps(),
            7,
        ) {
            Ok(AbiRequest::Permission(actual_player, node)) => {
                assert_eq!(actual_player, player);
                assert_eq!(node.as_str(), exact);
            }
            _ => panic!("exact-bound permission request must decode"),
        }

        let mut oversized_request = Vec::new();
        push_player(&mut oversized_request, player);
        oversized_request.extend_from_slice(
            &u32::try_from(MAX_NODE_LEN + 1)
                .expect("permission bound fits u32")
                .to_le_bytes(),
        );
        assert_invalid(
            decode_request(
                &request(FcHostRequestKind::PERMISSION_RESOLVE, 0, oversized_request),
                FramePhase::Event,
                all_caps(),
                7,
            ),
            "field length exceeds SDK bound",
        );

        let mut registration = Vec::new();
        registration.extend_from_slice(&1u32.to_le_bytes());
        push_command_node(&mut registration, None, "root", &exact);
        match decode_command(
            &command(FcCommandKind::REGISTER_COMMAND, 0, registration),
            FramePhase::Load,
            all_caps(),
            7,
        ) {
            Ok(AbiCommand::Effect(PluginEffect::RegisterCommand(definition))) => {
                assert_eq!(
                    definition.nodes()[0]
                        .required_permission()
                        .map(PermissionNode::as_str),
                    Some(exact.as_str())
                );
            }
            _ => panic!("exact-bound command permission must decode"),
        }

        let mut oversized_registration = Vec::new();
        oversized_registration.extend_from_slice(&1u32.to_le_bytes());
        oversized_registration.extend_from_slice(&u32::MAX.to_le_bytes());
        oversized_registration.extend_from_slice(&[0, 0, 0xff, 0]);
        oversized_registration.extend_from_slice(&0i64.to_le_bytes());
        oversized_registration.extend_from_slice(&0i64.to_le_bytes());
        push_text(&mut oversized_registration, "root").expect("bounded command name");
        oversized_registration.extend_from_slice(
            &u32::try_from(MAX_NODE_LEN + 1)
                .expect("permission bound fits u32")
                .to_le_bytes(),
        );
        assert_invalid(
            decode_command(
                &command(FcCommandKind::REGISTER_COMMAND, 0, oversized_registration),
                FramePhase::Load,
                all_caps(),
                7,
            ),
            "field length exceeds SDK bound",
        );
    }

    #[test]
    fn command_node_count_accepts_the_exact_limit_and_rejects_one_more() {
        let mut exact = Vec::new();
        exact.extend_from_slice(
            &u32::try_from(MAX_COMMAND_NODES)
                .expect("command-node bound fits u32")
                .to_le_bytes(),
        );
        for index in 0..MAX_COMMAND_NODES {
            let parent = if index == 0 {
                None
            } else {
                Some(u32::try_from(index - 1).expect("bounded node index"))
            };
            push_command_node(&mut exact, parent, "n", "");
        }
        match decode_command(
            &command(FcCommandKind::REGISTER_COMMAND, 0, exact),
            FramePhase::Load,
            all_caps(),
            7,
        ) {
            Ok(AbiCommand::Effect(PluginEffect::RegisterCommand(definition))) => {
                assert_eq!(definition.nodes().len(), MAX_COMMAND_NODES);
            }
            _ => panic!("exact-bound command tree must decode"),
        }

        let mut oversized = Vec::new();
        oversized.extend_from_slice(
            &u32::try_from(MAX_COMMAND_NODES + 1)
                .expect("command-node bound fits u32")
                .to_le_bytes(),
        );
        assert_invalid(
            decode_command(
                &command(FcCommandKind::REGISTER_COMMAND, 0, oversized),
                FramePhase::Load,
                all_caps(),
                7,
            ),
            "command node count exceeds SDK bound",
        );
    }

    #[test]
    fn strict_request_decoder_rejects_capability_phase_and_handle_mismatches() {
        let dimension = request(FcHostRequestKind::DIMENSION, 0, Vec::new());
        assert!(decode_request(
            &dimension,
            FramePhase::Event,
            CapabilityManifest::empty(),
            7
        )
        .is_err());
        assert!(decode_request(&dimension, FramePhase::Load, all_caps(), 7).is_err());

        let mut chunk = Vec::new();
        chunk.extend_from_slice(&0i32.to_le_bytes());
        chunk.extend_from_slice(&0i32.to_le_bytes());
        let wrong_handle = request(FcHostRequestKind::CHUNK_LOADED, 8, chunk);
        assert!(decode_request(&wrong_handle, FramePhase::Event, all_caps(), 7).is_err());
    }
}
