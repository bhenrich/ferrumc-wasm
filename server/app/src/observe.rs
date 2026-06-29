//! Observability glue: maps the app's concrete networking/protocol types onto
//! the proto-free [`ferrumc_observability`] vocabulary.
//!
//! `ferrumc-observability` is a leaf crate that knows nothing about `ferrumc-net`
//! or `ferrumc-proto`, so the app owns the mapping: connection-state translation,
//! the `&'static str` packet-name table (the generated proto enums expose
//! `packet_id()` but no `name()`, and generated files cannot be hand-edited), and
//! the decode-error label table. Everything here is allocation-free: a trace
//! carries a packet's name, id, and tick, never a re-encoded body.

use ferrumc_net::{
    CompressionState, ConnectionState, DecodeError, FrameDecodeError, InboundPacket, OutboundPacket,
};
use ferrumc_observability::{PacketState, PacketTrace, ServerClock};
use ferrumc_proto::generated::configuration::ServerboundConfigurationPacket;
use ferrumc_proto::generated::handshake::ServerboundHandshakePacket;
use ferrumc_proto::generated::login::ServerboundLoginPacket;
use ferrumc_proto::generated::play::{ClientboundPlayPacket, ServerboundPlayPacket};
use ferrumc_proto::generated::status::ServerboundStatusPacket;
use ferrumc_proto::ProtoError;

/// Maps a networking [`ConnectionState`] onto the observability [`PacketState`].
pub(crate) fn state_of(state: ConnectionState) -> PacketState {
    match state {
        ConnectionState::Handshaking => PacketState::Handshaking,
        ConnectionState::Status => PacketState::Status,
        ConnectionState::Login => PacketState::Login,
        ConnectionState::Configuration => PacketState::Configuration,
        ConnectionState::Play => PacketState::Play,
    }
}

/// A stable `&'static str` name for an inbound (serverbound) login-phase packet.
fn inbound_name(packet: &InboundPacket) -> &'static str {
    match packet {
        InboundPacket::Handshake(ServerboundHandshakePacket::Handshake(_)) => "handshake",
        InboundPacket::Status(status) => match status {
            ServerboundStatusPacket::StatusRequest(_) => "status_request",
            ServerboundStatusPacket::PingRequest(_) => "ping_request",
        },
        InboundPacket::Login(login) => match login {
            ServerboundLoginPacket::LoginStart(_) => "login_start",
            ServerboundLoginPacket::LoginAcknowledged(_) => "login_acknowledged",
        },
        InboundPacket::Configuration(config) => match config {
            ServerboundConfigurationPacket::ClientInformation(_) => "client_information",
            ServerboundConfigurationPacket::AckFinishConfiguration(_) => "ack_finish_configuration",
            ServerboundConfigurationPacket::ServerboundKnownPacks(_) => "serverbound_known_packs",
        },
        // Typed play packets are traced via `serverbound_play_name`; a raw play
        // body should never reach the login-phase trace path.
        InboundPacket::Play(_) => "play_raw",
    }
}

/// The wire packet id for an inbound login-phase packet (`-1` for a raw play
/// body, which is traced through the typed play path instead).
fn inbound_id(packet: &InboundPacket) -> i32 {
    match packet {
        InboundPacket::Handshake(p) => p.packet_id(),
        InboundPacket::Status(p) => p.packet_id(),
        InboundPacket::Login(p) => p.packet_id(),
        InboundPacket::Configuration(p) => p.packet_id(),
        InboundPacket::Play(_) => -1,
    }
}

/// A stable `&'static str` name for an outbound login/status/config packet.
fn outbound_name(packet: &OutboundPacket) -> &'static str {
    match packet {
        OutboundPacket::Status(status) => {
            use ferrumc_proto::generated::status::ClientboundStatusPacket as P;
            match status {
                P::StatusResponse(_) => "status_response",
                P::PongResponse(_) => "pong_response",
            }
        }
        OutboundPacket::Login(login) => {
            use ferrumc_proto::generated::login::ClientboundLoginPacket as P;
            match login {
                P::LoginDisconnect(_) => "login_disconnect",
                P::LoginSuccess(_) => "login_success",
                P::SetCompression(_) => "set_compression",
            }
        }
        OutboundPacket::Configuration(config) => {
            use ferrumc_proto::generated::configuration::ClientboundConfigurationPacket as P;
            match config {
                P::FinishConfiguration(_) => "finish_configuration",
                P::RegistryData(_) => "registry_data",
                P::ClientboundKnownPacks(_) => "clientbound_known_packs",
            }
        }
        // Play frames go out through `PlayWriter`, not `Connection::send`.
        OutboundPacket::Play(_) => "play_raw",
    }
}

/// The wire packet id for an outbound login/status/config packet (`-1` for a raw
/// play body).
fn outbound_id(packet: &OutboundPacket) -> i32 {
    match packet {
        OutboundPacket::Status(p) => p.packet_id(),
        OutboundPacket::Login(p) => p.packet_id(),
        OutboundPacket::Configuration(p) => p.packet_id(),
        OutboundPacket::Play(_) => -1,
    }
}

/// A stable `&'static str` name for a clientbound play packet.
pub(crate) fn clientbound_play_name(packet: &ClientboundPlayPacket) -> &'static str {
    match packet {
        ClientboundPlayPacket::SpawnEntity(_) => "spawn_entity",
        ClientboundPlayPacket::AcknowledgeBlockChange(_) => "acknowledge_block_change",
        ClientboundPlayPacket::BlockUpdate(_) => "block_update",
        ClientboundPlayPacket::UnloadChunk(_) => "unload_chunk",
        ClientboundPlayPacket::GameEvent(_) => "game_event",
        ClientboundPlayPacket::ClientboundKeepAlive(_) => "clientbound_keep_alive",
        ClientboundPlayPacket::ChunkDataAndLight(_) => "chunk_data_and_light",
        ClientboundPlayPacket::JoinGame(_) => "join_game",
        ClientboundPlayPacket::PlayerInfoUpdate(_) => "player_info_update",
        ClientboundPlayPacket::SynchronizePlayerPosition(_) => "synchronize_player_position",
        ClientboundPlayPacket::SetCenterChunk(_) => "set_center_chunk",
        ClientboundPlayPacket::SetDefaultSpawnPosition(_) => "set_default_spawn_position",
        ClientboundPlayPacket::SystemChat(_) => "system_chat",
        ClientboundPlayPacket::EntityTeleport(_) => "entity_teleport",
        ClientboundPlayPacket::UpdateEntityPosition(_) => "update_entity_position",
        ClientboundPlayPacket::UpdateEntityPositionAndRotation(_) => {
            "update_entity_position_and_rotation"
        }
        ClientboundPlayPacket::UpdateEntityRotation(_) => "update_entity_rotation",
        ClientboundPlayPacket::SetHeadRotation(_) => "set_head_rotation",
        ClientboundPlayPacket::RemoveEntities(_) => "remove_entities",
        ClientboundPlayPacket::RemovePlayerInfo(_) => "remove_player_info",
        ClientboundPlayPacket::Commands(_) => "commands",
        ClientboundPlayPacket::TabCompleteResponse(_) => "tab_complete_response",
        ClientboundPlayPacket::SetContainerContent(_) => "set_container_content",
        ClientboundPlayPacket::SetContainerSlot(_) => "set_container_slot",
        ClientboundPlayPacket::ClientboundSetHeldItem(_) => "clientbound_set_held_item",
        ClientboundPlayPacket::PlayerAbilities(_) => "player_abilities",
        ClientboundPlayPacket::SetEquipment(_) => "set_equipment",
        ClientboundPlayPacket::SetTitleText(_) => "set_title_text",
        ClientboundPlayPacket::SetSubtitleText(_) => "set_subtitle_text",
        ClientboundPlayPacket::SetActionBarText(_) => "set_action_bar_text",
        ClientboundPlayPacket::SetTitleAnimationTimes(_) => "set_title_animation_times",
        ClientboundPlayPacket::UpdateTime(_) => "update_time",
        ClientboundPlayPacket::SoundEffect(_) => "sound_effect",
        ClientboundPlayPacket::Particle(_) => "particle",
        ClientboundPlayPacket::UpdateObjectives(_) => "update_objectives",
        ClientboundPlayPacket::DisplayObjective(_) => "display_objective",
        ClientboundPlayPacket::UpdateScore(_) => "update_score",
        ClientboundPlayPacket::SetPlayerTeam(_) => "set_player_team",
        ClientboundPlayPacket::BossBar(_) => "boss_bar",
        ClientboundPlayPacket::BlockEntityData(_) => "block_entity_data",
        ClientboundPlayPacket::OpenSignEditor(_) => "open_sign_editor",
        ClientboundPlayPacket::OpenScreen(_) => "open_screen",
    }
}

/// A stable `&'static str` name for a serverbound play packet.
fn serverbound_play_name(packet: &ServerboundPlayPacket) -> &'static str {
    match packet {
        ServerboundPlayPacket::ConfirmTeleportation(_) => "confirm_teleportation",
        ServerboundPlayPacket::ChatCommand(_) => "chat_command",
        ServerboundPlayPacket::ServerboundKeepAlive(_) => "serverbound_keep_alive",
        ServerboundPlayPacket::SetPlayerPosition(_) => "set_player_position",
        ServerboundPlayPacket::SetPlayerPositionAndRotation(_) => {
            "set_player_position_and_rotation"
        }
        ServerboundPlayPacket::SetPlayerRotation(_) => "set_player_rotation",
        ServerboundPlayPacket::PlayerAction(_) => "player_action",
        ServerboundPlayPacket::UseItemOn(_) => "use_item_on",
        ServerboundPlayPacket::ChatMessage(_) => "chat_message",
        ServerboundPlayPacket::TabCompleteRequest(_) => "tab_complete_request",
        ServerboundPlayPacket::ServerboundSetHeldItem(_) => "serverbound_set_held_item",
        ServerboundPlayPacket::SetCreativeSlot(_) => "set_creative_slot",
        ServerboundPlayPacket::WindowClick(_) => "window_click",
        ServerboundPlayPacket::CloseContainer(_) => "close_container",
        ServerboundPlayPacket::UpdateSign(_) => "update_sign",
    }
}

/// Classifies a serverbound play-body decode failure into a metric label and
/// whether it warrants a full session dump.
///
/// An unknown packet id is *expected* — the slice models only a subset of
/// serverbound play packets — so it is counted but not dumped (dumping on every
/// unmodelled client packet would flood the logs). A byte- or NBT-level failure
/// on a frame is a genuine decode error: counted and dumped.
pub(crate) fn play_decode_error(err: &ProtoError) -> (&'static str, bool) {
    match err {
        ProtoError::UnknownPacketId { .. } => ("unknown_play", false),
        ProtoError::Nbt(_) => ("malformed_play_nbt", true),
        // A byte-level codec failure, plus any future `#[non_exhaustive]` variant,
        // is treated as a genuine decode error.
        _ => ("malformed_play", true),
    }
}

/// Maps a frame decode failure onto a stable `&'static str` label for
/// `ferrumc_packet_decode_error_total{state,packet}`.
pub(crate) fn decode_error_label(err: &FrameDecodeError) -> &'static str {
    match err {
        FrameDecodeError::Decode(decode) => match decode {
            DecodeError::FrameTooLarge { .. } => "frame_too_large",
            DecodeError::BufferOverflow { .. } => "buffer_overflow",
            DecodeError::BadLengthVarInt => "bad_length_varint",
            DecodeError::NegativeLength { .. } => "negative_length",
            DecodeError::UnknownPacket { .. } => "unknown_packet",
            DecodeError::MalformedBody { .. } => "malformed_body",
            DecodeError::TrailingBytes { .. } => "trailing_bytes",
            // `DecodeError` is `#[non_exhaustive]`.
            _ => "decode",
        },
        FrameDecodeError::Compression(_) => "compression",
        // `FrameDecodeError` is `#[non_exhaustive]`.
        _ => "frame_decode",
    }
}

/// Builds an outbound trace for a login/status/config packet, with the exact
/// on-wire `size` from the encoder.
pub(crate) fn trace_outbound(
    packet: &OutboundPacket,
    size: usize,
    compression: &CompressionState,
    clock: &ServerClock,
) -> PacketTrace {
    PacketTrace::outbound(
        state_of(packet.state()),
        outbound_id(packet),
        outbound_name(packet),
        size,
        compression.is_enabled(),
        clock.now(),
    )
}

/// Builds an outbound trace for a clientbound play packet.
///
/// `size` is recorded as `0`. The on-wire body length is only known after the
/// [`PlayWriter`](ferrumc_net::PlayWriter) encodes the packet at drain time, and
/// re-encoding it here purely to fill the trace would serialize every clientbound
/// frame twice on the hottest path — chunk streaming sends tens-of-KiB
/// `ChunkDataAndLight` packets. The trace's value is the packet's name, id, tick,
/// and ordering, not its byte size; inbound login records `0` for the same reason
/// (see [`trace_inbound_login`]).
pub(crate) fn trace_outbound_play(
    packet: &ClientboundPlayPacket,
    compression: &CompressionState,
    clock: &ServerClock,
) -> PacketTrace {
    PacketTrace::outbound(
        PacketState::Play,
        packet.packet_id(),
        clientbound_play_name(packet),
        0,
        compression.is_enabled(),
        clock.now(),
    )
}

/// Builds an inbound trace for a login/status/config packet.
///
/// The login-phase decoder does not surface the body length, so `size` is
/// recorded as `0`; only play-phase inbound (the acceptance-critical phase)
/// records an exact size (see [`trace_inbound_play`]).
pub(crate) fn trace_inbound_login(
    packet: &InboundPacket,
    compression: &CompressionState,
    clock: &ServerClock,
) -> PacketTrace {
    PacketTrace::inbound(
        state_of(packet.state()),
        inbound_id(packet),
        inbound_name(packet),
        0,
        compression.is_enabled(),
        clock.now(),
    )
}

/// Builds an inbound trace for a serverbound play packet, with the exact frame
/// body `size`.
pub(crate) fn trace_inbound_play(
    packet: &ServerboundPlayPacket,
    size: usize,
    compression: &CompressionState,
    clock: &ServerClock,
) -> PacketTrace {
    PacketTrace::inbound(
        PacketState::Play,
        packet.packet_id(),
        serverbound_play_name(packet),
        size,
        compression.is_enabled(),
        clock.now(),
    )
}
