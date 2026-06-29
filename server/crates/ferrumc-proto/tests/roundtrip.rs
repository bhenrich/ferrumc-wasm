//! Per-packet encode->decode round-trips plus the mandated malformed-input
//! tests (truncated, oversized string/array prefix, bad `VarInt`, trailing
//! bytes, unknown id) for the generated protocol 772 packet codecs.

use bytes::BytesMut;
use ferrumc_codec::{BoundedBytes, BoundedReader, BoundedString};
use uuid::Uuid;

use ferrumc_nbt::{NbtCompound, NbtTag};
use ferrumc_proto::generated::configuration::{
    AckFinishConfiguration, ClientInformation, ClientboundKnownPacks, FinishConfiguration,
    KnownPack, RegistryData, RegistryEntry, ServerboundKnownPacks,
};
use ferrumc_proto::generated::handshake::Handshake;
use ferrumc_proto::generated::login::{
    ClientboundLoginPacket, LoginAcknowledged, LoginDisconnect, LoginStart, LoginSuccess, Property,
    SetCompression,
};
use ferrumc_proto::generated::play::{
    BlockEntityData, BlockUpdate, BossBar, ChatCommand, ChunkBlockEntity, ChunkDataAndLight,
    ClientboundKeepAlive, ClientboundPlayPacket, ConfirmTeleportation, DeathLocation,
    DisplayObjective, EntityTeleport, EntityVelocity, GameEvent, Heightmap, JoinGame,
    OpenSignEditor, Particle, PlayerAction, PlayerInfoUpdate, RemoveEntities, RemovePlayerInfo,
    ServerboundKeepAlive, ServerboundPlayPacket, SetActionBarText, SetCenterChunk,
    SetDefaultSpawnPosition, SetHeadRotation, SetPlayerPosition, SetPlayerPositionAndRotation,
    SetPlayerTeam, SetSubtitleText, SetTitleAnimationTimes, SetTitleText, SoundEffect, SpawnEntity,
    SpawnInfo, SynchronizePlayerPosition, UnloadChunk, UpdateEntityPosition,
    UpdateEntityPositionAndRotation, UpdateEntityRotation, UpdateObjectives, UpdateScore,
    UpdateSign, UseItemOn,
};
use ferrumc_proto::generated::status::{
    PingRequest, PongResponse, ServerboundStatusPacket, StatusRequest, StatusResponse,
};
use ferrumc_proto::{BlockPosition, ProtoError};

/// Encodes `original` (which writes its packet id first), then decodes it back
/// and asserts equality, the expected id, and that nothing trails.
fn roundtrip<T: PartialEq + core::fmt::Debug>(
    original: &T,
    encode: fn(&T, &mut BytesMut) -> Result<(), ProtoError>,
    decode: fn(&mut BoundedReader<'_>) -> Result<T, ProtoError>,
    expected_id: i32,
) {
    let mut buf = BytesMut::new();
    encode(original, &mut buf).expect("encode");

    let mut reader = BoundedReader::new(&buf);
    let id = reader.read_var_int().expect("packet id");
    assert_eq!(id, expected_id, "encoded packet id");

    let decoded = decode(&mut reader).expect("decode");
    assert_eq!(&decoded, original, "round-trip mismatch");
    assert_eq!(reader.remaining(), 0, "unexpected trailing bytes");
}

/// Builds a bounded string, panicking in test code if it exceeds the limit.
fn s<const N: usize>(value: &str) -> BoundedString<N> {
    BoundedString::<N>::new(value.to_owned()).expect("string within limit")
}

#[test]
fn handshake_round_trips() {
    roundtrip(
        &Handshake::new(772, s::<255>("localhost"), 25565, 2),
        Handshake::encode,
        Handshake::decode,
        Handshake::PACKET_ID,
    );
}

#[test]
fn status_packets_round_trip() {
    roundtrip(
        &StatusRequest,
        StatusRequest::encode,
        StatusRequest::decode,
        StatusRequest::PACKET_ID,
    );
    roundtrip(
        &PingRequest::new(0x0123_4567_89ab_cdef),
        PingRequest::encode,
        PingRequest::decode,
        PingRequest::PACKET_ID,
    );
    roundtrip(
        &StatusResponse::new(s::<32767>("{\"version\":{}}")),
        StatusResponse::encode,
        StatusResponse::decode,
        StatusResponse::PACKET_ID,
    );
    roundtrip(
        &PongResponse::new(-42),
        PongResponse::encode,
        PongResponse::decode,
        PongResponse::PACKET_ID,
    );
}

#[test]
fn login_packets_round_trip() {
    roundtrip(
        &LoginStart::new(s::<16>("Notch"), Uuid::from_u128(0x1234)),
        LoginStart::encode,
        LoginStart::decode,
        LoginStart::PACKET_ID,
    );
    roundtrip(
        &LoginAcknowledged,
        LoginAcknowledged::encode,
        LoginAcknowledged::decode,
        LoginAcknowledged::PACKET_ID,
    );
    roundtrip(
        &LoginDisconnect::new(s::<262_144>("{\"text\":\"bye\"}")),
        LoginDisconnect::encode,
        LoginDisconnect::decode,
        LoginDisconnect::PACKET_ID,
    );
    roundtrip(
        &SetCompression::new(-1),
        SetCompression::encode,
        SetCompression::decode,
        SetCompression::PACKET_ID,
    );

    // Properties exercise both the prefixed array and the optional signature.
    let properties = vec![
        Property::new(
            s::<64>("textures"),
            s::<32767>("base64"),
            Some(s::<1024>("sig")),
        ),
        Property::new(s::<64>("cape"), s::<32767>("value"), None),
    ];
    roundtrip(
        &LoginSuccess::new(Uuid::from_u128(0xdead_beef), s::<16>("Notch"), properties),
        LoginSuccess::encode,
        LoginSuccess::decode,
        LoginSuccess::PACKET_ID,
    );
}

#[test]
fn configuration_packets_round_trip() {
    roundtrip(
        &ClientInformation::new(s::<16>("en_us"), 12, 0, true, 0x7F, 1, false, true, 2),
        ClientInformation::encode,
        ClientInformation::decode,
        ClientInformation::PACKET_ID,
    );
    roundtrip(
        &AckFinishConfiguration,
        AckFinishConfiguration::encode,
        AckFinishConfiguration::decode,
        AckFinishConfiguration::PACKET_ID,
    );
    roundtrip(
        &FinishConfiguration,
        FinishConfiguration::encode,
        FinishConfiguration::decode,
        FinishConfiguration::PACKET_ID,
    );

    let packs = vec![KnownPack::new(
        s::<32767>("minecraft"),
        s::<32767>("core"),
        s::<32767>("1.21.8"),
    )];
    roundtrip(
        &ServerboundKnownPacks::new(packs.clone()),
        ServerboundKnownPacks::encode,
        ServerboundKnownPacks::decode,
        ServerboundKnownPacks::PACKET_ID,
    );
    roundtrip(
        &ClientboundKnownPacks::new(packs),
        ClientboundKnownPacks::encode,
        ClientboundKnownPacks::decode,
        ClientboundKnownPacks::PACKET_ID,
    );
}

#[test]
fn registry_data_round_trips_with_embedded_nbt() {
    let mut compound = NbtCompound::new();
    compound.push("id", NbtTag::Int(7));
    compound.push("name", NbtTag::String("plains".to_owned()));
    let nbt = NbtTag::Compound(compound);

    let entries = vec![
        RegistryEntry::new(s::<32767>("minecraft:plains"), Some(nbt)),
        RegistryEntry::new(s::<32767>("minecraft:desert"), None),
    ];
    roundtrip(
        &RegistryData::new(s::<32767>("minecraft:worldgen/biome"), entries),
        RegistryData::encode,
        RegistryData::decode,
        RegistryData::PACKET_ID,
    );
}

#[test]
fn dispatch_decodes_and_reports_packet_id() {
    let original = ServerboundStatusPacket::PingRequest(PingRequest::new(99));
    let mut buf = BytesMut::new();
    original.encode(&mut buf).expect("encode");

    let mut reader = BoundedReader::new(&buf);
    let id = reader.read_var_int().expect("id");
    let decoded = ServerboundStatusPacket::decode(id, &mut reader).expect("decode");

    assert_eq!(decoded, original);
    assert_eq!(decoded.packet_id(), PingRequest::PACKET_ID);
    assert_eq!(reader.remaining(), 0);
}

#[test]
fn unknown_id_is_classified() {
    let mut reader = BoundedReader::new(&[]);
    let err = ServerboundStatusPacket::decode(0x7F, &mut reader).expect_err("unknown id");
    assert!(matches!(
        err,
        ProtoError::UnknownPacketId {
            id: 0x7F,
            state: ferrumc_proto::State::Status,
            direction: ferrumc_proto::Direction::Serverbound,
        }
    ));
}

#[test]
fn truncated_body_is_codec_error() {
    // PingRequest needs 8 bytes; give it 3.
    let buf = [0x01u8, 0x02, 0x03];
    let mut reader = BoundedReader::new(&buf);
    let err = PingRequest::decode(&mut reader).expect_err("truncated");
    assert!(matches!(err, ProtoError::Codec(_)));
}

#[test]
fn oversized_string_prefix_is_rejected() {
    // LoginStart starts with name: string(16). A prefix of 1000 exceeds the
    // 16*4 byte ceiling and is rejected before any body is read.
    let mut buf = BytesMut::new();
    ferrumc_codec::write_var_int(&mut buf, 1000);
    let mut reader = BoundedReader::new(&buf);
    let err = LoginStart::decode(&mut reader).expect_err("oversized string");
    assert!(matches!(err, ProtoError::Codec(_)));
}

#[test]
fn oversized_array_prefix_runs_into_eof() {
    // LoginSuccess: uuid(16) + empty name + a huge properties count with no
    // elements behind it. The element loop hits end-of-input.
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&[0u8; 16]); // uuid
    ferrumc_codec::write_var_int(&mut buf, 0); // name: empty string
    ferrumc_codec::write_var_int(&mut buf, 0x7FFF_FFFF); // absurd array count
    let mut reader = BoundedReader::new(&buf);
    let err = LoginSuccess::decode(&mut reader).expect_err("oversized array");
    assert!(matches!(err, ProtoError::Codec(_)));
}

#[test]
fn bad_varint_is_rejected() {
    // SetCompression reads a VarInt threshold; six continuation bytes overrun
    // the 5-byte budget.
    let buf = [0x80u8, 0x80, 0x80, 0x80, 0x80, 0x00];
    let mut reader = BoundedReader::new(&buf);
    let err = SetCompression::decode(&mut reader).expect_err("bad varint");
    assert!(matches!(err, ProtoError::Codec(_)));
}

#[test]
fn trailing_bytes_are_detectable_after_decode() {
    // Decode succeeds on the leading bytes; the caller's finish() rejects the
    // extra junk, which is where trailing-byte enforcement lives.
    let mut buf = BytesMut::new();
    let original = PingRequest::new(7);
    original.encode(&mut buf).expect("encode");
    buf.extend_from_slice(&[0xAA, 0xBB]); // junk after the packet

    let mut reader = BoundedReader::new(&buf);
    let id = reader.read_var_int().expect("id");
    let decoded = ServerboundStatusPacket::decode(id, &mut reader).expect("decode");
    assert_eq!(decoded, ServerboundStatusPacket::PingRequest(original));
    assert_eq!(reader.remaining(), 2);
    assert!(
        reader.finish().is_err(),
        "finish must reject trailing bytes"
    );
}

#[test]
fn clientbound_login_dispatch_round_trips() {
    let original = ClientboundLoginPacket::SetCompression(SetCompression::new(256));
    let mut buf = BytesMut::new();
    original.encode(&mut buf).expect("encode");

    let mut reader = BoundedReader::new(&buf);
    let id = reader.read_var_int().expect("id");
    let decoded = ClientboundLoginPacket::decode(id, &mut reader).expect("decode");
    assert_eq!(decoded, original);
    assert_eq!(reader.remaining(), 0);
}

// --- Play state ---------------------------------------------------------------

#[test]
fn play_serverbound_packets_round_trip() {
    roundtrip(
        &ServerboundKeepAlive::new(0x0102_0304_0506_0708),
        ServerboundKeepAlive::encode,
        ServerboundKeepAlive::decode,
        ServerboundKeepAlive::PACKET_ID,
    );
    roundtrip(
        &SetPlayerPosition::new(1.5, -64.0, 2048.25, 0x01),
        SetPlayerPosition::encode,
        SetPlayerPosition::decode,
        SetPlayerPosition::PACKET_ID,
    );
    roundtrip(
        &SetPlayerPositionAndRotation::new(1.5, -64.0, 2048.25, 90.0, -45.0, 0x03),
        SetPlayerPositionAndRotation::encode,
        SetPlayerPositionAndRotation::decode,
        SetPlayerPositionAndRotation::PACKET_ID,
    );
    roundtrip(
        &PlayerAction::new(0, BlockPosition::new(100, -60, 200), 1, 5),
        PlayerAction::encode,
        PlayerAction::decode,
        PlayerAction::PACKET_ID,
    );
    roundtrip(
        &UseItemOn::new(
            0,
            BlockPosition::new(1, 2, 3),
            1,
            0.5,
            0.25,
            0.75,
            false,
            true,
            9,
        ),
        UseItemOn::encode,
        UseItemOn::decode,
        UseItemOn::PACKET_ID,
    );
    roundtrip(
        &ChatCommand::new(s::<256>("gamemode creative")),
        ChatCommand::encode,
        ChatCommand::decode,
        ChatCommand::PACKET_ID,
    );
    roundtrip(
        &ConfirmTeleportation::new(1),
        ConfirmTeleportation::encode,
        ConfirmTeleportation::decode,
        ConfirmTeleportation::PACKET_ID,
    );
}

#[test]
fn play_clientbound_simple_packets_round_trip() {
    roundtrip(
        &ClientboundKeepAlive::new(-1),
        ClientboundKeepAlive::encode,
        ClientboundKeepAlive::decode,
        ClientboundKeepAlive::PACKET_ID,
    );
    roundtrip(
        &BlockUpdate::new(BlockPosition::new(10, 64, -5), 2),
        BlockUpdate::encode,
        BlockUpdate::decode,
        BlockUpdate::PACKET_ID,
    );
    roundtrip(
        &SpawnEntity::new(
            1,
            Uuid::from_u128(0xfeed_face),
            70,
            8.5,
            65.0,
            -16.0,
            1,
            2,
            3,
            0,
            EntityVelocity::new(10, -20, 30),
        ),
        SpawnEntity::encode,
        SpawnEntity::decode,
        SpawnEntity::PACKET_ID,
    );
    roundtrip(
        &SynchronizePlayerPosition::new(7, 1.0, 2.0, 3.0, 0.0, 0.0, 0.0, 180.0, -90.0, 0x1F),
        SynchronizePlayerPosition::encode,
        SynchronizePlayerPosition::decode,
        SynchronizePlayerPosition::PACKET_ID,
    );
    // The Player Info Update entry list is carried as an opaque trailing blob.
    roundtrip(
        &PlayerInfoUpdate::new(0x09, vec![0xDE, 0xAD, 0xBE, 0xEF]),
        PlayerInfoUpdate::encode,
        PlayerInfoUpdate::decode,
        PlayerInfoUpdate::PACKET_ID,
    );
    // Game Event: reason 13 (level_chunks_load_start) leaves the loading screen.
    roundtrip(
        &GameEvent::new(13, 0.0),
        GameEvent::encode,
        GameEvent::decode,
        GameEvent::PACKET_ID,
    );
    roundtrip(
        &SetCenterChunk::new(0, -7),
        SetCenterChunk::encode,
        SetCenterChunk::decode,
        SetCenterChunk::PACKET_ID,
    );
    roundtrip(
        &SetDefaultSpawnPosition::new(BlockPosition::new(8, 64, 8), 90.0),
        SetDefaultSpawnPosition::encode,
        SetDefaultSpawnPosition::decode,
        SetDefaultSpawnPosition::PACKET_ID,
    );
    // Unload Chunk: note the wire order is Z then X (a protocol quirk), so the
    // constructor arguments here are (chunk_z, chunk_x).
    roundtrip(
        &UnloadChunk::new(-7, 3),
        UnloadChunk::encode,
        UnloadChunk::decode,
        UnloadChunk::PACKET_ID,
    );
}

/// Round-trips the entity movement / despawn packets backing remote-player
/// visibility, including the two new `prefixed_array` element-type usages
/// (`<varint>` ids and `<uuid>`).
#[test]
fn play_clientbound_entity_packets_round_trip() {
    // Entity movement / despawn packets (remote-player visibility).
    roundtrip(
        &EntityTeleport::new(42, 1.5, 64.0, -2.5, 0.0, 0.0, 0.0, 90.0, -45.0, true),
        EntityTeleport::encode,
        EntityTeleport::decode,
        EntityTeleport::PACKET_ID,
    );
    roundtrip(
        &UpdateEntityPosition::new(42, 4096, -2048, 8192, true),
        UpdateEntityPosition::encode,
        UpdateEntityPosition::decode,
        UpdateEntityPosition::PACKET_ID,
    );
    roundtrip(
        &UpdateEntityPositionAndRotation::new(42, 1, -1, 2, 64, -32, false),
        UpdateEntityPositionAndRotation::encode,
        UpdateEntityPositionAndRotation::decode,
        UpdateEntityPositionAndRotation::PACKET_ID,
    );
    roundtrip(
        &UpdateEntityRotation::new(42, 64, -32, true),
        UpdateEntityRotation::encode,
        UpdateEntityRotation::decode,
        UpdateEntityRotation::PACKET_ID,
    );
    roundtrip(
        &SetHeadRotation::new(42, -100),
        SetHeadRotation::encode,
        SetHeadRotation::decode,
        SetHeadRotation::PACKET_ID,
    );
    // Prefixed array of varint entity ids (a new generator element type usage).
    roundtrip(
        &RemoveEntities::new(vec![2, 3, 5, 8]),
        RemoveEntities::encode,
        RemoveEntities::decode,
        RemoveEntities::PACKET_ID,
    );
    // Prefixed array of UUIDs (a new generator element type usage).
    roundtrip(
        &RemovePlayerInfo::new(vec![Uuid::from_u128(0xfeed_face), Uuid::from_u128(0x1234)]),
        RemovePlayerInfo::encode,
        RemovePlayerInfo::decode,
        RemovePlayerInfo::PACKET_ID,
    );
}

#[test]
fn join_game_round_trips_with_nested_spawn_info() {
    let spawn = SpawnInfo::new(
        0,
        s::<32767>("minecraft:overworld"),
        0x0123_4567_89ab_cdef,
        1,   // gamemode: creative
        255, // previous gamemode: "none"
        false,
        true,
        Some(DeathLocation::new(
            s::<32767>("minecraft:the_nether"),
            BlockPosition::new(1, 2, 3),
        )),
        0,
        63,
    );
    let join = JoinGame::new(
        42,
        false,
        vec![
            s::<32767>("minecraft:overworld"),
            s::<32767>("minecraft:the_nether"),
        ],
        20,
        10,
        10,
        false,
        true,
        false,
        spawn,
        true,
    );
    roundtrip(
        &join,
        JoinGame::encode,
        JoinGame::decode,
        JoinGame::PACKET_ID,
    );
}

#[test]
fn chunk_data_round_trips_with_opaque_payload_and_block_entity() {
    let mut be_nbt = NbtCompound::new();
    be_nbt.push("id", NbtTag::String("minecraft:chest".to_owned()));
    let block_entity = ChunkBlockEntity::new(0x12, 70, 5, NbtTag::Compound(be_nbt));

    let chunk = ChunkDataAndLight::new(
        3,
        -7,
        vec![Heightmap::new(
            4,
            vec![0x0102_0304_0506_0708, 0x1122_3344_5566_7788],
        )],
        BoundedBytes::<2_097_152>::new(vec![0xAB; 512]).expect("chunk blob within cap"),
        vec![block_entity],
        vec![0x0F],
        vec![],
        vec![],
        vec![],
        vec![BoundedBytes::<2048>::new(vec![0x77; 2048]).expect("light array within cap")],
        vec![],
    );
    roundtrip(
        &chunk,
        ChunkDataAndLight::encode,
        ChunkDataAndLight::decode,
        ChunkDataAndLight::PACKET_ID,
    );
}

#[test]
fn play_dispatch_round_trips_both_directions() {
    let sb = ServerboundPlayPacket::SetPlayerPosition(SetPlayerPosition::new(1.0, 2.0, 3.0, 0x01));
    let mut buf = BytesMut::new();
    sb.encode(&mut buf).expect("encode");
    let mut reader = BoundedReader::new(&buf);
    let id = reader.read_var_int().expect("id");
    let decoded = ServerboundPlayPacket::decode(id, &mut reader).expect("decode");
    assert_eq!(decoded, sb);
    assert_eq!(decoded.packet_id(), SetPlayerPosition::PACKET_ID);
    assert_eq!(reader.remaining(), 0);

    let cb = ClientboundPlayPacket::BlockUpdate(BlockUpdate::new(BlockPosition::new(0, 0, 0), 1));
    let mut buf = BytesMut::new();
    cb.encode(&mut buf).expect("encode");
    let mut reader = BoundedReader::new(&buf);
    let id = reader.read_var_int().expect("id");
    let decoded = ClientboundPlayPacket::decode(id, &mut reader).expect("decode");
    assert_eq!(decoded, cb);
    assert_eq!(decoded.packet_id(), BlockUpdate::PACKET_ID);
    assert_eq!(reader.remaining(), 0);
}

#[test]
fn play_unknown_id_is_classified() {
    let mut reader = BoundedReader::new(&[]);
    let err = ClientboundPlayPacket::decode(0x77, &mut reader).expect_err("unknown id");
    assert!(matches!(
        err,
        ProtoError::UnknownPacketId {
            id: 0x77,
            state: ferrumc_proto::State::Play,
            direction: ferrumc_proto::Direction::Clientbound,
        }
    ));
}

#[test]
fn block_position_round_trips_through_packet() {
    // Min/max field values must survive the 26/26/12-bit packing.
    let pos = BlockPosition::new(33_554_431, 2047, -33_554_432);
    let mut buf = BytesMut::new();
    BlockUpdate::new(pos, 9).encode(&mut buf).expect("encode");
    let mut reader = BoundedReader::new(&buf);
    let _id = reader.read_var_int().expect("id");
    let decoded = BlockUpdate::decode(&mut reader).expect("decode");
    assert_eq!(decoded.location(), pos);
}

#[test]
fn player_action_truncated_position_is_codec_error() {
    // status VarInt (1 byte), then only 4 of the 8 position bytes.
    let buf = [0x00u8, 0x01, 0x02, 0x03, 0x04];
    let mut reader = BoundedReader::new(&buf);
    let err = PlayerAction::decode(&mut reader).expect_err("truncated position");
    assert!(matches!(err, ProtoError::Codec(_)));
}

#[test]
fn chunk_data_oversized_blob_prefix_is_rejected() {
    // x, z, zero heightmaps, then a chunk_data length prefix above the 2 MiB cap.
    let mut buf = BytesMut::new();
    buf.extend_from_slice(&3i32.to_be_bytes()); // x
    buf.extend_from_slice(&(-7i32).to_be_bytes()); // z
    ferrumc_codec::write_var_int(&mut buf, 0); // heightmaps count
    ferrumc_codec::write_var_int(&mut buf, 3_000_000); // chunk_data len > cap
    let mut reader = BoundedReader::new(&buf);
    let err = ChunkDataAndLight::decode(&mut reader).expect_err("oversized chunk blob");
    assert!(matches!(err, ProtoError::Codec(_)));
}

#[test]
fn chat_command_oversized_string_prefix_is_rejected() {
    // BoundedString<256> caps at 256*4 bytes; a 5000-byte prefix is rejected
    // before any body is read.
    let mut buf = BytesMut::new();
    ferrumc_codec::write_var_int(&mut buf, 5000);
    let mut reader = BoundedReader::new(&buf);
    let err = ChatCommand::decode(&mut reader).expect_err("oversized command");
    assert!(matches!(err, ProtoError::Codec(_)));
}

#[test]
fn confirm_teleportation_bad_varint_is_rejected() {
    // teleport_id is a VarInt; six continuation bytes overrun the 5-byte budget.
    let buf = [0x80u8, 0x80, 0x80, 0x80, 0x80, 0x00];
    let mut reader = BoundedReader::new(&buf);
    let err = ConfirmTeleportation::decode(&mut reader).expect_err("bad varint");
    assert!(matches!(err, ProtoError::Codec(_)));
}

#[test]
fn game_event_truncated_value_is_codec_error() {
    // reason (1 byte) is present, but only 2 of the value f32's 4 bytes follow.
    let buf = [0x0Du8, 0x00, 0x00];
    let mut reader = BoundedReader::new(&buf);
    let err = GameEvent::decode(&mut reader).expect_err("truncated f32");
    assert!(matches!(err, ProtoError::Codec(_)));
}

#[test]
fn set_center_chunk_bad_varint_is_rejected() {
    // chunk_x is a VarInt; six continuation bytes overrun the 5-byte budget.
    let buf = [0x80u8, 0x80, 0x80, 0x80, 0x80, 0x00];
    let mut reader = BoundedReader::new(&buf);
    let err = SetCenterChunk::decode(&mut reader).expect_err("bad varint");
    assert!(matches!(err, ProtoError::Codec(_)));
}

#[test]
fn set_default_spawn_position_truncated_is_codec_error() {
    // The packed position needs 8 bytes; supply only 4 before the angle.
    let buf = [0x00u8, 0x01, 0x02, 0x03];
    let mut reader = BoundedReader::new(&buf);
    let err = SetDefaultSpawnPosition::decode(&mut reader).expect_err("truncated position");
    assert!(matches!(err, ProtoError::Codec(_)));
}

// --- Reserved play packets (title / sound / particle / scoreboard / team /
// boss bar / block entity / sign) ---------------------------------------------

/// Builds a network-form text component (an anonymous-root NBT compound with a
/// single `text` string), the shape the title / action-bar packets carry.
fn text_component(text: &str) -> NbtTag {
    let mut compound = NbtCompound::new();
    compound.push("text", NbtTag::String(text.to_owned()));
    NbtTag::Compound(compound)
}

/// Round-trips the fully-typed reserved packets: the title / action-bar text
/// components, the title animation times, the block-entity NBT payload, and the
/// sign open/update packets.
#[test]
fn play_reserved_typed_packets_round_trip() {
    roundtrip(
        &SetTitleText::new(text_component("Welcome")),
        SetTitleText::encode,
        SetTitleText::decode,
        SetTitleText::PACKET_ID,
    );
    roundtrip(
        &SetSubtitleText::new(text_component("to the server")),
        SetSubtitleText::encode,
        SetSubtitleText::decode,
        SetSubtitleText::PACKET_ID,
    );
    roundtrip(
        &SetActionBarText::new(text_component("low on health")),
        SetActionBarText::encode,
        SetActionBarText::decode,
        SetActionBarText::PACKET_ID,
    );
    roundtrip(
        &SetTitleAnimationTimes::new(10, 70, 20),
        SetTitleAnimationTimes::encode,
        SetTitleAnimationTimes::decode,
        SetTitleAnimationTimes::PACKET_ID,
    );

    // Display Objective: slot 1 = sidebar.
    roundtrip(
        &DisplayObjective::new(1, s::<32_767>("health")),
        DisplayObjective::encode,
        DisplayObjective::decode,
        DisplayObjective::PACKET_ID,
    );

    let mut be_nbt = NbtCompound::new();
    be_nbt.push("id", NbtTag::String("minecraft:sign".to_owned()));
    roundtrip(
        &BlockEntityData::new(BlockPosition::new(1, 64, -3), 7, NbtTag::Compound(be_nbt)),
        BlockEntityData::encode,
        BlockEntityData::decode,
        BlockEntityData::PACKET_ID,
    );

    roundtrip(
        &OpenSignEditor::new(BlockPosition::new(8, 65, 8), true),
        OpenSignEditor::encode,
        OpenSignEditor::decode,
        OpenSignEditor::PACKET_ID,
    );
    roundtrip(
        &UpdateSign::new(
            BlockPosition::new(8, 65, 8),
            true,
            s::<384>("line one"),
            s::<384>("line two"),
            s::<384>(""),
            s::<384>(""),
        ),
        UpdateSign::encode,
        UpdateSign::decode,
        UpdateSign::PACKET_ID,
    );
}

/// Round-trips the reserved opaque-tail / opaque-body packets, mirroring the
/// `PlayerInfoUpdate` precedent: the stable leading fields are typed and the
/// variant tail (or, for the sound packets, the whole body) is an opaque blob the
/// feature lane hand-encodes.
#[test]
fn play_reserved_opaque_packets_round_trip() {
    // Sound: the leading ItemSoundHolder union forces the whole body opaque.
    roundtrip(
        &SoundEffect::new(vec![0x01, 0x02, 0x03, 0x04]),
        SoundEffect::encode,
        SoundEffect::decode,
        SoundEffect::PACKET_ID,
    );

    // Particle: typed leading fields, then a typed particle id and an opaque
    // per-type data tail.
    roundtrip(
        &Particle::new(
            true,
            false,
            1.0,
            2.0,
            3.0,
            0.1,
            0.2,
            0.3,
            0.5,
            64,
            13, // dust
            vec![0xDE, 0xAD, 0xBE, 0xEF],
        ),
        Particle::encode,
        Particle::decode,
        Particle::PACKET_ID,
    );

    // Update Objectives: mode 0 (create) with an opaque display-data tail.
    roundtrip(
        &UpdateObjectives::new(s::<32_767>("health"), 0, vec![0x00, 0x01]),
        UpdateObjectives::encode,
        UpdateObjectives::decode,
        UpdateObjectives::PACKET_ID,
    );
    // Update Score: typed names + value, two `false` flag bytes as the tail.
    roundtrip(
        &UpdateScore::new(
            s::<32_767>("Notch"),
            s::<32_767>("health"),
            20,
            vec![0x00, 0x00],
        ),
        UpdateScore::encode,
        UpdateScore::decode,
        UpdateScore::PACKET_ID,
    );
    // Set Player Team: method 1 (remove) carries no tail.
    roundtrip(
        &SetPlayerTeam::new(s::<32_767>("red"), 1, vec![]),
        SetPlayerTeam::encode,
        SetPlayerTeam::decode,
        SetPlayerTeam::PACKET_ID,
    );
    // Boss Bar: action 1 (remove) carries no tail.
    roundtrip(
        &BossBar::new(Uuid::from_u128(0xfeed_face), 1, vec![]),
        BossBar::encode,
        BossBar::decode,
        BossBar::PACKET_ID,
    );
}
