//! Checked ABI v1 binary payload codecs.

use ferrumc_plugin_abi::{FcEventKind, ABI_MAJOR, ABI_MINOR};
use ferrumc_plugin_sdk::{
    BlockEvent, BlockPlaceEvent, BlockPos, ChatAttempt, CommandArgument, CommandDefinition,
    CommandInvocation, CommandNodeKind, Direction, EntityId, Event, EventKind, FacadeError,
    Feedback, HandlerId, InteractHand, InteractTarget, InteractionAttempt, MoveEvent,
    PermissionNode, PlaceAttempt, PlayerEvent, PlayerId, TimerId, Vec3, MAX_CHAT_BYTES,
    MAX_COMMAND_ARGUMENTS, MAX_COMMAND_NAME_BYTES, MAX_COMMAND_NODES, MAX_COMMAND_TEXT_BYTES,
    MAX_STORAGE_KEYS, MAX_STORAGE_KEY_BYTES, MAX_STORAGE_VALUE_BYTES,
};
use uuid::Uuid;
const BLOCK_BREAK_RECORD_SIZE: u32 = 36;
const SET_BLOCK_RECORD_SIZE: u32 = 24;
const BLOCK_STATE_RECORD_SIZE: u32 = 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WireError {
    reason: &'static str,
}

impl WireError {
    pub(crate) const fn new(reason: &'static str) -> Self {
        Self { reason }
    }

    pub(crate) const fn reason(self) -> &'static str {
        self.reason
    }
}

pub(crate) fn decode_event(kind: FcEventKind, payload: &[u8]) -> Result<Event, WireError> {
    let mut reader = Reader::new(payload);
    let event = match kind.raw() {
        1 => Event::PlayerJoin(PlayerEvent::new(reader.player()?)),
        2 => Event::PlayerLeave(PlayerEvent::new(reader.player()?)),
        3 => Event::BlockBreak(decode_block_record(&mut reader)?),
        4 => Event::AfterBlockPlace(BlockPlaceEvent::new(
            reader.player()?,
            reader.block_pos()?,
            reader.u32()?,
        )),
        5 => Event::AfterBlockBreak(decode_block_record(&mut reader)?),
        6 => Event::PlayerMove(MoveEvent::new(
            reader.player()?,
            reader.block_pos()?,
            reader.block_pos()?,
        )),
        7 => Event::BlockPlaceAttempt(PlaceAttempt::new(
            reader.player()?,
            reader.block_pos()?,
            reader.u32()?,
        )),
        8 => Event::BlockBreakAttempt(decode_block_record(&mut reader)?),
        9 => {
            let player = reader.player()?;
            let message = reader.text(MAX_CHAT_BYTES)?;
            Event::ChatAttempt(
                ChatAttempt::new(player, message)
                    .map_err(|_| WireError::new("chat message violates SDK bounds"))?,
            )
        }
        10 => Event::InteractAttempt(decode_interaction(&mut reader)?),
        11 => Event::Command(decode_command_invocation(&mut reader)?),
        12 => {
            let timer = TimerId::new(reader.u64()?).ok_or(WireError::new("timer id is zero"))?;
            Event::Timer(timer)
        }
        _ => return Err(WireError::new("event kind is unknown")),
    };
    reader.finish()?;
    Ok(event)
}

fn decode_block_record(reader: &mut Reader<'_>) -> Result<BlockEvent, WireError> {
    let declared_size = reader.record_header(BLOCK_BREAK_RECORD_SIZE)?;
    let event = BlockEvent::new(reader.player()?, reader.block_pos()?);
    let extension_len = declared_size
        .checked_sub(36)
        .ok_or(WireError::new("versioned payload size underflowed"))?;
    let _extension = reader.take(extension_len)?;
    Ok(event)
}

fn decode_interaction(reader: &mut Reader<'_>) -> Result<InteractionAttempt, WireError> {
    let player = reader.player()?;
    let hand = match reader.u8()? {
        0 => InteractHand::Main,
        1 => InteractHand::Off,
        _ => return Err(WireError::new("interaction hand tag is unknown")),
    };
    let target_kind = reader.u8()?;
    if reader.u16()? != 0 {
        return Err(WireError::new("interaction reserved word is nonzero"));
    }
    let target = match target_kind {
        0 => InteractTarget::Air,
        1 => {
            let pos = reader.block_pos()?;
            let face = decode_direction(reader.u8()?)?;
            if reader.take(3)? != [0, 0, 0] {
                return Err(WireError::new(
                    "interaction block-target reserved bytes are nonzero",
                ));
            }
            InteractTarget::Block { pos, face }
        }
        2 => InteractTarget::Entity {
            entity: EntityId::new(reader.i32()?),
        },
        _ => return Err(WireError::new("interaction target tag is unknown")),
    };
    Ok(InteractionAttempt::new(player, hand, target))
}

fn decode_direction(raw: u8) -> Result<Direction, WireError> {
    match raw {
        0 => Ok(Direction::Down),
        1 => Ok(Direction::Up),
        2 => Ok(Direction::North),
        3 => Ok(Direction::South),
        4 => Ok(Direction::West),
        5 => Ok(Direction::East),
        _ => Err(WireError::new("interaction face tag is unknown")),
    }
}

fn decode_command_invocation(reader: &mut Reader<'_>) -> Result<CommandInvocation, WireError> {
    let handler =
        HandlerId::new(reader.u64()?).ok_or(WireError::new("command handler id is zero"))?;
    let player = reader.player()?;
    let count = reader.count(
        MAX_COMMAND_ARGUMENTS,
        "command argument count exceeds SDK bound",
    )?;
    let mut arguments = Vec::with_capacity(count);
    for _ in 0..count {
        let name = reader.text(MAX_COMMAND_NAME_BYTES)?;
        let kind = reader.u8()?;
        if reader.take(3)? != [0, 0, 0] {
            return Err(WireError::new(
                "command argument reserved bytes are nonzero",
            ));
        }
        let argument = match kind {
            0 => CommandArgument::text(name, reader.text(MAX_COMMAND_TEXT_BYTES)?),
            1 => CommandArgument::integer(name, reader.i64()?),
            _ => return Err(WireError::new("command argument kind is unknown")),
        }
        .map_err(|_| WireError::new("command argument violates SDK bounds"))?;
        arguments.push(argument);
    }
    CommandInvocation::new(handler, player, arguments)
        .map_err(|_| WireError::new("command invocation violates SDK bounds"))
}

pub(crate) fn event_kind(kind: EventKind) -> Result<FcEventKind, FacadeError> {
    let raw = match kind {
        EventKind::PlayerJoin => 1,
        EventKind::PlayerLeave => 2,
        EventKind::BlockBreak => 3,
        EventKind::AfterBlockPlace => 4,
        EventKind::AfterBlockBreak => 5,
        EventKind::PlayerMove => 6,
        EventKind::BlockPlaceAttempt => 7,
        EventKind::BlockBreakAttempt => 8,
        EventKind::ChatAttempt => 9,
        EventKind::InteractAttempt => 10,
        EventKind::Command => 11,
        EventKind::Timer => 12,
        _ => {
            return Err(FacadeError::Unavailable {
                operation: "future event subscription kind",
            })
        }
    };
    Ok(FcEventKind::from_raw(raw))
}

pub(crate) fn encode_set_block(pos: BlockPos, block_state_id: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(24);
    push_record_header(&mut bytes, SET_BLOCK_RECORD_SIZE);
    push_block_pos(&mut bytes, pos);
    bytes.extend_from_slice(&block_state_id.to_le_bytes());
    bytes
}

pub(crate) fn encode_block_state_request(pos: BlockPos) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(20);
    push_record_header(&mut bytes, BLOCK_STATE_RECORD_SIZE);
    push_block_pos(&mut bytes, pos);
    bytes
}

pub(crate) fn encode_chunk(chunk: ferrumc_plugin_sdk::ChunkPos) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8);
    bytes.extend_from_slice(&chunk.x().to_le_bytes());
    bytes.extend_from_slice(&chunk.z().to_le_bytes());
    bytes
}

pub(crate) fn encode_player(player: PlayerId) -> Vec<u8> {
    player.as_uuid().into_bytes().to_vec()
}

pub(crate) fn encode_player_vec(player: PlayerId, position: Vec3) -> Vec<u8> {
    let mut bytes = encode_player(player);
    push_vec3(&mut bytes, position);
    bytes
}

pub(crate) fn encode_player_text(player: PlayerId, text: &str) -> Result<Vec<u8>, FacadeError> {
    let mut bytes = encode_player(player);
    push_text(&mut bytes, text)?;
    Ok(bytes)
}

pub(crate) fn encode_permission(
    player: PlayerId,
    permission: &PermissionNode,
) -> Result<Vec<u8>, FacadeError> {
    let mut bytes = encode_player(player);
    push_text(&mut bytes, permission.as_str())?;
    Ok(bytes)
}

pub(crate) fn encode_text(text: &str) -> Result<Vec<u8>, FacadeError> {
    let mut bytes = Vec::with_capacity(4usize.saturating_add(text.len()));
    push_text(&mut bytes, text)?;
    Ok(bytes)
}

pub(crate) fn encode_text_bytes(key: &str, value: &[u8]) -> Result<Vec<u8>, FacadeError> {
    let mut bytes =
        Vec::with_capacity(8usize.saturating_add(key.len()).saturating_add(value.len()));
    push_text(&mut bytes, key)?;
    push_bytes(&mut bytes, value)?;
    Ok(bytes)
}

pub(crate) fn encode_command_definition(
    command: &CommandDefinition,
) -> Result<Vec<u8>, FacadeError> {
    let node_count =
        u32::try_from(command.nodes().len()).map_err(|_| FacadeError::LimitExceeded {
            resource: "command node",
            len: command.nodes().len(),
            max: MAX_COMMAND_NODES,
        })?;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&node_count.to_le_bytes());
    for node in command.nodes() {
        let parent = match node.parent() {
            Some(parent) => u32::try_from(parent).map_err(|_| FacadeError::InvalidInput {
                resource: "command parent",
                reason: "parent index does not fit ABI v1",
            })?,
            None => u32::MAX,
        };
        bytes.extend_from_slice(&parent.to_le_bytes());
        let (kind, min, max) = match node.kind() {
            CommandNodeKind::Literal => (0, 0, 0),
            CommandNodeKind::Word => (1, 0, 0),
            CommandNodeKind::GreedyText => (2, 0, 0),
            CommandNodeKind::Integer(bounds) => (3, bounds.min(), bounds.max()),
            _ => {
                return Err(FacadeError::Unavailable {
                    operation: "future command node kind",
                })
            }
        };
        bytes.push(kind);
        bytes.push(u8::from(node.handler().is_some()));
        bytes.push(node.required_level().unwrap_or(0xff));
        bytes.push(0);
        bytes.extend_from_slice(&min.to_le_bytes());
        bytes.extend_from_slice(&max.to_le_bytes());
        push_text(&mut bytes, node.name())?;
        push_text(
            &mut bytes,
            node.required_permission()
                .map_or("", PermissionNode::as_str),
        )?;
        bytes.extend_from_slice(&node.handler().map_or(0, HandlerId::get).to_le_bytes());
    }
    Ok(bytes)
}

pub(crate) fn encode_u32(value: u32) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

pub(crate) fn encode_u64(value: u64) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

pub(crate) fn encode_timer(id: TimerId, delay_ticks: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&id.get().to_le_bytes());
    bytes.extend_from_slice(&delay_ticks.to_le_bytes());
    bytes
}

pub(crate) fn encode_feedback(feedback: Option<&Feedback>) -> Result<Vec<u8>, FacadeError> {
    feedback.map_or_else(|| Ok(Vec::new()), |value| encode_text(value.message()))
}

pub(crate) fn decode_dimension(bytes: &[u8]) -> Result<u64, WireError> {
    let mut reader = Reader::new(bytes);
    let handle = reader.u64()?;
    reader.finish()?;
    if handle == 0 {
        return Err(WireError::new("dimension handle is zero"));
    }
    Ok(handle)
}

pub(crate) fn decode_bool(bytes: &[u8]) -> Result<bool, WireError> {
    let mut reader = Reader::new(bytes);
    let value = match reader.u8()? {
        0 => false,
        1 => true,
        _ => return Err(WireError::new("boolean response tag is unknown")),
    };
    reader.finish()?;
    Ok(value)
}

pub(crate) fn decode_block_state(bytes: &[u8]) -> Result<u32, WireError> {
    let mut reader = Reader::new(bytes);
    let value = reader.u32()?;
    reader.finish()?;
    Ok(value)
}

pub(crate) fn decode_player_position(bytes: &[u8]) -> Result<Option<Vec3>, WireError> {
    let mut reader = Reader::new(bytes);
    let value = match reader.u8()? {
        0 => None,
        1 => {
            let position = reader.vec3()?;
            if !position.x.is_finite() || !position.y.is_finite() || !position.z.is_finite() {
                return Err(WireError::new("player position is non-finite"));
            }
            Some(position)
        }
        _ => return Err(WireError::new("player-position presence tag is unknown")),
    };
    reader.finish()?;
    Ok(value)
}

pub(crate) fn decode_resolution(bytes: &[u8]) -> Result<ferrumc_plugin_sdk::Resolution, WireError> {
    let mut reader = Reader::new(bytes);
    let value = match reader.u8()? {
        0 => ferrumc_plugin_sdk::Resolution::Unset,
        1 => ferrumc_plugin_sdk::Resolution::Allowed,
        2 => ferrumc_plugin_sdk::Resolution::Denied,
        _ => return Err(WireError::new("permission resolution tag is unknown")),
    };
    reader.finish()?;
    Ok(value)
}

pub(crate) fn decode_storage_value(bytes: &[u8]) -> Result<Option<Vec<u8>>, WireError> {
    let mut reader = Reader::new(bytes);
    let value = match reader.u8()? {
        0 => None,
        1 => Some(reader.bytes(MAX_STORAGE_VALUE_BYTES)?),
        _ => return Err(WireError::new("storage-value presence tag is unknown")),
    };
    reader.finish()?;
    Ok(value)
}

pub(crate) fn decode_storage_keys(bytes: &[u8]) -> Result<Vec<String>, WireError> {
    let mut reader = Reader::new(bytes);
    let count = reader.count(MAX_STORAGE_KEYS, "storage key count exceeds SDK bound")?;
    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        let key = reader.text(MAX_STORAGE_KEY_BYTES)?;
        if key.is_empty() {
            return Err(WireError::new("storage key is empty"));
        }
        keys.push(key);
    }
    reader.finish()?;
    Ok(keys)
}

fn push_record_header(bytes: &mut Vec<u8>, size: u32) {
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&ABI_MAJOR.to_le_bytes());
    bytes.extend_from_slice(&ABI_MINOR.to_le_bytes());
}

fn push_block_pos(bytes: &mut Vec<u8>, pos: BlockPos) {
    bytes.extend_from_slice(&pos.x().to_le_bytes());
    bytes.extend_from_slice(&pos.y().to_le_bytes());
    bytes.extend_from_slice(&pos.z().to_le_bytes());
}

fn push_vec3(bytes: &mut Vec<u8>, value: Vec3) {
    bytes.extend_from_slice(&value.x.to_bits().to_le_bytes());
    bytes.extend_from_slice(&value.y.to_bits().to_le_bytes());
    bytes.extend_from_slice(&value.z.to_bits().to_le_bytes());
}

fn push_text(bytes: &mut Vec<u8>, value: &str) -> Result<(), FacadeError> {
    push_field(bytes, value.as_bytes(), "text field")
}

fn push_bytes(bytes: &mut Vec<u8>, value: &[u8]) -> Result<(), FacadeError> {
    push_field(bytes, value, "byte field")
}

fn push_field(
    bytes: &mut Vec<u8>,
    value: &[u8],
    resource: &'static str,
) -> Result<(), FacadeError> {
    let len = u32::try_from(value.len()).map_err(|_| FacadeError::LimitExceeded {
        resource,
        len: value.len(),
        max: match usize::try_from(u32::MAX) {
            Ok(maximum) => maximum,
            Err(_) => usize::MAX,
        },
    })?;
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], WireError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(WireError::new("payload offset overflowed"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(WireError::new("payload is truncated"))?;
        self.offset = end;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let mut value = [0; N];
        value.copy_from_slice(self.take(N)?);
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, WireError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or(WireError::new("payload is truncated"))
    }

    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, WireError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, WireError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn f64(&mut self) -> Result<f64, WireError> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn player(&mut self) -> Result<PlayerId, WireError> {
        Ok(PlayerId::from_uuid(Uuid::from_bytes(self.array()?)))
    }

    fn block_pos(&mut self) -> Result<BlockPos, WireError> {
        Ok(BlockPos::new(self.i32()?, self.i32()?, self.i32()?))
    }

    fn vec3(&mut self) -> Result<Vec3, WireError> {
        Ok(Vec3::new(self.f64()?, self.f64()?, self.f64()?))
    }

    fn text(&mut self, maximum: usize) -> Result<String, WireError> {
        let bytes = self.field(maximum)?;
        let text =
            core::str::from_utf8(bytes).map_err(|_| WireError::new("text field is not UTF-8"))?;
        Ok(text.to_owned())
    }

    fn bytes(&mut self, maximum: usize) -> Result<Vec<u8>, WireError> {
        Ok(self.field(maximum)?.to_vec())
    }

    fn field(&mut self, maximum: usize) -> Result<&'a [u8], WireError> {
        let len = usize::try_from(self.u32()?)
            .map_err(|_| WireError::new("field length is not representable"))?;
        if len > maximum {
            return Err(WireError::new("field length exceeds its SDK bound"));
        }
        self.take(len)
    }

    fn count(&mut self, maximum: usize, reason: &'static str) -> Result<usize, WireError> {
        let count = usize::try_from(self.u32()?)
            .map_err(|_| WireError::new("count is not representable"))?;
        if count > maximum {
            return Err(WireError::new(reason));
        }
        Ok(count)
    }

    fn record_header(&mut self, expected_size: u32) -> Result<usize, WireError> {
        let size = self.u32()?;
        let major = self.u16()?;
        let _minor = self.u16()?;
        // This adapter consumes ABI minor zero, so every same-major producer
        // minor covers its known prefix.
        if size < expected_size || major != ABI_MAJOR {
            return Err(WireError::new("versioned payload header is incompatible"));
        }
        usize::try_from(size).map_err(|_| WireError::new("record size is not representable"))
    }

    fn finish(self) -> Result<(), WireError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(WireError::new("payload contains trailing bytes"))
        }
    }
}
