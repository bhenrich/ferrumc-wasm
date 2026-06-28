//! Golden-byte fixtures for the critical clientbound packets.
//!
//! Each test builds a fixed packet instance, runs it through the strict frame
//! oracle ([`ferrumc_testkit::assert_wire_frame`]), and pins the full
//! uncompressed wire frame `[VarInt len][VarInt id][body]` against a committed
//! `fixtures/golden/<name>.hex` snapshot. On top of byte equality, several tests
//! hand-assert the structural invariants a real Minecraft 1.21.8 client relies
//! on (heightmap array form, the post-1.21.5 prefix-less `PalettedContainer`, the
//! `PlayerInfoUpdate` add body shape, packed `BlockPosition`, top-bit-terminated
//! equipment, anonymous network NBT, fixed-point movement deltas).
//!
//! IMPORTANT: these goldens are a **drift-regression snapshot** generated from
//! the current encoders, NOT independent vanilla truth. The node-client smoke
//! test is the eventual independent oracle. See `fixtures/golden/README.md`.
//! Re-bless after an intentional wire change with:
//! `FERRUMC_BLESS_GOLDEN=1 cargo test -p ferrumc-testkit --test golden`.

use std::path::Path;

use bytes::BytesMut;

use ferrumc_codec::{write_var_int, BoundedBytes, BoundedReader, BoundedString};
use ferrumc_nbt::{NbtCompound, NbtTag};
use ferrumc_proto::generated::{configuration, login, play};
use ferrumc_proto::types::BlockPosition;
use ferrumc_proto::ProtoError;
use ferrumc_testkit::{assert_wire_frame, to_hex, HexFixture};
use uuid::Uuid;

/// A fixed UUID used across the UUID-bearing goldens so the bytes are stable.
const STEVE_UUID: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];

/// Builds a `BoundedString<16>` from a `&str`, panicking only in this test code.
fn bs16(text: &str) -> BoundedString<16> {
    BoundedString::<16>::new(text.to_string()).expect("fits in 16 code units")
}

/// Builds a `BoundedString<32_767>` from a `&str`.
fn bs32(text: &str) -> BoundedString<32_767> {
    BoundedString::<32_767>::new(text.to_string()).expect("fits in 32767 code units")
}

/// The absolute path to a committed golden fixture.
fn golden_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/golden")
        .join(format!("{name}.hex"))
}

/// Wraps a hex string at 64 columns (32 bytes/line) for reviewable diffs.
fn wrap_hex(hex: &str) -> String {
    let mut out = String::with_capacity(hex.len() + hex.len() / 64 + 1);
    for (i, ch) in hex.chars().enumerate() {
        if i != 0 && i % 64 == 0 {
            out.push('\n');
        }
        out.push(ch);
    }
    out.push('\n');
    out
}

/// Loads the golden for `name`, or, when `FERRUMC_BLESS_GOLDEN` is set in the
/// environment, (re)writes it from `framed` and returns it.
fn load_or_bless(name: &str, framed: &[u8]) -> HexFixture {
    let path = golden_path(name);
    if std::env::var_os("FERRUMC_BLESS_GOLDEN").is_some() {
        std::fs::write(&path, wrap_hex(&to_hex(framed)))
            .unwrap_or_else(|e| panic!("writing golden {name}: {e}"));
        HexFixture::from_bytes(framed.to_vec())
    } else {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading golden {name} ({}): {e}", path.display()));
        HexFixture::parse(&text).unwrap_or_else(|e| panic!("parsing golden {name}: {e}"))
    }
}

/// Runs the strict frame oracle on `packet` and pins the frame against the
/// committed golden `name`, returning the full uncompressed frame.
///
/// The first call (golden `None`) runs every structural check and yields the
/// bytes; those are loaded/blessed into a fixture; the second call drives the
/// oracle's byte-exact golden comparison end to end.
fn pin<T, E, D>(name: &str, packet: &T, encode: E, decode: D, id: i32) -> Vec<u8>
where
    T: PartialEq + std::fmt::Debug,
    E: Fn(&T, &mut BytesMut) -> Result<(), ProtoError> + Copy,
    D: Fn(&mut BoundedReader<'_>) -> Result<T, ProtoError> + Copy,
{
    let framed = assert_wire_frame(packet, encode, decode, id, None)
        .unwrap_or_else(|e| panic!("[{name}] oracle rejected the frame: {e}"));
    let fixture = load_or_bless(name, &framed);
    assert_wire_frame(packet, encode, decode, id, Some(&fixture))
        .unwrap_or_else(|e| panic!("[{name}] {e}"));
    framed
}

/// Strips the frame and packet-id prefixes, returning just the body bytes.
fn frame_body(framed: &[u8]) -> Vec<u8> {
    let mut reader = BoundedReader::new(framed);
    let _len = reader.read_var_int_len().expect("frame length");
    let _id = reader.read_var_int().expect("packet id");
    let remaining = reader.remaining();
    reader.read_bytes(remaining).expect("body").to_vec()
}

// ---------------------------------------------------------------------------
// Login / configuration state
// ---------------------------------------------------------------------------

#[test]
fn login_success() {
    let packet = login::LoginSuccess::new(Uuid::from_bytes(STEVE_UUID), bs16("Steve"), Vec::new());
    let body = frame_body(&pin(
        "login_success",
        &packet,
        login::LoginSuccess::encode,
        login::LoginSuccess::decode,
        login::LoginSuccess::PACKET_ID,
    ));
    // body = uuid(16) + name(VarInt len 5 + "Steve") + properties VarInt count(0).
    assert_eq!(&body[0..16], &STEVE_UUID);
    assert_eq!(body[16], 0x05);
    assert_eq!(&body[17..22], b"Steve");
    assert_eq!(body[22], 0x00, "empty properties array");
    assert_eq!(body.len(), 23);
}

#[test]
fn clientbound_known_packs() {
    let pack = configuration::KnownPack::new(bs32("minecraft"), bs32("core"), bs32("1.21.8"));
    let packet = configuration::ClientboundKnownPacks::new(vec![pack]);
    let body = frame_body(&pin(
        "clientbound_known_packs",
        &packet,
        configuration::ClientboundKnownPacks::encode,
        configuration::ClientboundKnownPacks::decode,
        configuration::ClientboundKnownPacks::PACKET_ID,
    ));
    // VarInt count(1) then three length-prefixed strings.
    assert_eq!(body[0], 0x01, "one known pack");
}

// ---------------------------------------------------------------------------
// Play state — join / world
// ---------------------------------------------------------------------------

#[test]
fn join_game() {
    let spawn = play::SpawnInfo::new(
        0,
        bs32("minecraft:overworld"),
        0,
        1,
        255,
        false,
        true,
        None,
        0,
        63,
    );
    let packet = play::JoinGame::new(
        1,
        false,
        vec![bs32("minecraft:overworld")],
        20,
        10,
        10,
        false,
        true,
        false,
        spawn,
        false,
    );
    pin(
        "join_game",
        &packet,
        play::JoinGame::encode,
        play::JoinGame::decode,
        play::JoinGame::PACKET_ID,
    );
}

#[test]
fn game_event() {
    let packet = play::GameEvent::new(13, 0.0);
    let body = frame_body(&pin(
        "game_event",
        &packet,
        play::GameEvent::encode,
        play::GameEvent::decode,
        play::GameEvent::PACKET_ID,
    ));
    // u8 reason then big-endian f32 value.
    assert_eq!(body, vec![0x0d, 0x00, 0x00, 0x00, 0x00]);
}

#[test]
fn set_center_chunk() {
    let packet = play::SetCenterChunk::new(0, 0);
    let body = frame_body(&pin(
        "set_center_chunk",
        &packet,
        play::SetCenterChunk::encode,
        play::SetCenterChunk::decode,
        play::SetCenterChunk::PACKET_ID,
    ));
    // Two VarInts, both zero.
    assert_eq!(body, vec![0x00, 0x00]);
}

#[test]
fn chunk_data_and_light() {
    // A hand-built single-section chunk-data blob: i16 block_count, a single-value
    // block-states PalettedContainer (bits_per_entry 0 + VarInt value, no longs),
    // then a single-value biomes PalettedContainer. Crucially there is NO VarInt
    // length prefix between the two containers (the 1.21.5+ format dropped it).
    let mut blob = Vec::new();
    blob.extend_from_slice(&10i16.to_be_bytes()); // block_count
    blob.push(0x00); // block-states bits_per_entry = 0 (single value)
    write_var_int(&mut blob, 1); // block-states single value (stone)
    blob.push(0x00); // biomes bits_per_entry = 0 (single value)
    write_var_int(&mut blob, 0); // biomes single value (plains)
    let chunk_data = BoundedBytes::<2_097_152>::new(blob).expect("blob within cap");

    // The 1.21.5 heightmap array form: a VarInt kind (4 = MOTION_BLOCKING) then a
    // VarInt-counted i64 array.
    let heightmap = play::Heightmap::new(4, vec![128, 256]);

    let packet = play::ChunkDataAndLight::new(
        0,
        0,
        vec![heightmap],
        chunk_data,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let body = frame_body(&pin(
        "chunk_data_and_light",
        &packet,
        play::ChunkDataAndLight::encode,
        play::ChunkDataAndLight::decode,
        play::ChunkDataAndLight::PACKET_ID,
    ));

    // Decode the body the way a client does and assert the structural invariants.
    let mut reader = BoundedReader::new(&body);
    let decoded = play::ChunkDataAndLight::decode(&mut reader).expect("decode chunk");
    assert_eq!(reader.remaining(), 0, "no trailing bytes in the chunk body");
    assert_eq!(decoded.heightmaps().len(), 1);
    assert_eq!(
        decoded.heightmaps()[0].kind(),
        4,
        "MOTION_BLOCKING heightmap kind"
    );
    assert_eq!(
        decoded.heightmaps()[0].data().len(),
        2,
        "heightmap is the 1.21.5 i64 array form"
    );
    // chunk_data is exactly one VarInt-length-prefixed blob; the blob itself has
    // the two single-value containers back to back with nothing between them.
    assert_eq!(
        decoded.chunk_data().as_slice(),
        &[0x00, 0x0a, 0x00, 0x01, 0x00, 0x00],
        "single section: block_count + block container + biome container, no inner length prefix"
    );
}

#[test]
fn unload_chunk() {
    // NOTE: encode order is chunk_z then chunk_x, both i32 (not VarInt).
    let packet = play::UnloadChunk::new(0, 0);
    let body = frame_body(&pin(
        "unload_chunk",
        &packet,
        play::UnloadChunk::encode,
        play::UnloadChunk::decode,
        play::UnloadChunk::PACKET_ID,
    ));
    assert_eq!(body, vec![0; 8], "two big-endian i32 zeros");
}

#[test]
fn block_update() {
    let packet = play::BlockUpdate::new(BlockPosition::new(1, 2, 3), 1);
    let body = frame_body(&pin(
        "block_update",
        &packet,
        play::BlockUpdate::encode,
        play::BlockUpdate::decode,
        play::BlockUpdate::PACKET_ID,
    ));
    // BlockPosition packs as a single big-endian i64: x<<38 | z<<12 | y.
    let packed_position: i64 = (1i64 << 38) | (3i64 << 12) | 2;
    assert_eq!(&body[0..8], &packed_position.to_be_bytes());
    assert_eq!(body[8], 0x01, "block_state VarInt (stone)");
    assert_eq!(body.len(), 9);
}

#[test]
fn acknowledge_block_change() {
    let packet = play::AcknowledgeBlockChange::new(1);
    let body = frame_body(&pin(
        "acknowledge_block_change",
        &packet,
        play::AcknowledgeBlockChange::encode,
        play::AcknowledgeBlockChange::decode,
        play::AcknowledgeBlockChange::PACKET_ID,
    ));
    assert_eq!(body, vec![0x01], "single VarInt sequence");
}

// ---------------------------------------------------------------------------
// Play state — tab list / chat / commands
// ---------------------------------------------------------------------------

#[test]
fn player_info_update_add() {
    // Action 0x09 = Add Player (0x01) | Update Listed (0x08). The entries blob is
    // hand-built to mirror the session-layer encoder exactly.
    const ADD_AND_LISTED: u8 = 0x09;
    let name = b"Steve";
    let mut entries = Vec::new();
    write_var_int(&mut entries, 1); // entry count
    entries.extend_from_slice(&STEVE_UUID); // 16-byte UUID
    write_var_int(
        &mut entries,
        i32::try_from(name.len()).expect("name len fits"),
    );
    entries.extend_from_slice(name);
    write_var_int(&mut entries, 0); // properties count
    entries.push(0x01); // Update Listed boolean = true

    let packet = play::PlayerInfoUpdate::new(ADD_AND_LISTED, entries);
    let body = frame_body(&pin(
        "player_info_update_add",
        &packet,
        play::PlayerInfoUpdate::encode,
        play::PlayerInfoUpdate::decode,
        play::PlayerInfoUpdate::PACKET_ID,
    ));
    // body = action(1) + count(1) + uuid(16) + name(1 + 5) + properties(1) + listed(1).
    assert_eq!(body[0], ADD_AND_LISTED, "action byte");
    assert_eq!(body[1], 0x01, "one entry");
    assert_eq!(&body[2..18], &STEVE_UUID);
    assert_eq!(body[18], 0x05, "name length");
    assert_eq!(&body[19..24], b"Steve");
    assert_eq!(body[24], 0x00, "empty properties");
    assert_eq!(body[25], 0x01, "listed = true");
    assert_eq!(body.len(), 26);
}

#[test]
fn remove_player_info() {
    let packet = play::RemovePlayerInfo::new(vec![Uuid::from_bytes(STEVE_UUID)]);
    let body = frame_body(&pin(
        "remove_player_info",
        &packet,
        play::RemovePlayerInfo::encode,
        play::RemovePlayerInfo::decode,
        play::RemovePlayerInfo::PACKET_ID,
    ));
    // body = VarInt count(1) + one bare 16-byte UUID (no per-UUID prefix).
    assert_eq!(body[0], 0x01, "one player");
    assert_eq!(&body[1..17], &STEVE_UUID);
    assert_eq!(body.len(), 17);
}

#[test]
fn system_chat() {
    let mut content = NbtCompound::new();
    content.push("text", NbtTag::String("hi".to_string()));
    let packet = play::SystemChat::new(NbtTag::Compound(content), false);
    let body = frame_body(&pin(
        "system_chat",
        &packet,
        play::SystemChat::encode,
        play::SystemChat::decode,
        play::SystemChat::PACKET_ID,
    ));
    // content is anonymous network NBT: the first body byte is TAG_Compound (0x0a)
    // with NO name length following it; the final body byte is the overlay bool.
    assert_eq!(body[0], 0x0a, "anonymous TAG_Compound (no name length)");
    assert_eq!(body[body.len() - 1], 0x00, "overlay = false");
}

#[test]
fn commands() {
    // A minimal command graph: node count(1), one root node (flags 0x00, 0 children),
    // root index 0. Opaque to proto, carried as a raw byte run.
    let packet = play::Commands::new(vec![0x01, 0x00, 0x00]);
    let body = frame_body(&pin(
        "commands",
        &packet,
        play::Commands::encode,
        play::Commands::decode,
        play::Commands::PACKET_ID,
    ));
    assert_eq!(body, vec![0x01, 0x00, 0x00]);
}

// ---------------------------------------------------------------------------
// Play state — inventory / equipment
// ---------------------------------------------------------------------------

#[test]
fn set_container_content() {
    // payload = VarInt item count(1) + one empty Slot(0x00) + carried empty Slot(0x00).
    let payload = vec![0x01, 0x00, 0x00];
    let packet = play::SetContainerContent::new(0, 1, payload);
    let body = frame_body(&pin(
        "set_container_content",
        &packet,
        play::SetContainerContent::encode,
        play::SetContainerContent::decode,
        play::SetContainerContent::PACKET_ID,
    ));
    // window_id VarInt(0) + state_id VarInt(1) + payload.
    assert_eq!(body, vec![0x00, 0x01, 0x01, 0x00, 0x00]);
}

#[test]
fn set_container_slot() {
    let packet = play::SetContainerSlot::new(0, 1, 0, vec![0x00]);
    let body = frame_body(&pin(
        "set_container_slot",
        &packet,
        play::SetContainerSlot::encode,
        play::SetContainerSlot::decode,
        play::SetContainerSlot::PACKET_ID,
    ));
    // window_id(0) + state_id(1) + slot i16(0) + empty Slot(0x00).
    assert_eq!(body, vec![0x00, 0x01, 0x00, 0x00, 0x00]);
}

#[test]
fn set_equipment() {
    // A single-entry top-bit-terminated array: the leading slot byte has the high
    // bit CLEAR (< 0x80), marking it the last/only entry, then an empty Slot.
    let packet = play::SetEquipment::new(1, vec![0x00, 0x00]);
    let body = frame_body(&pin(
        "set_equipment",
        &packet,
        play::SetEquipment::encode,
        play::SetEquipment::decode,
        play::SetEquipment::PACKET_ID,
    ));
    // entity_id VarInt(1) + slot byte + empty Slot.
    assert_eq!(body[0], 0x01, "entity id");
    assert!(body[1] < 0x80, "slot high bit clear marks the last entry");
    assert_eq!(body, vec![0x01, 0x00, 0x00]);
}

// ---------------------------------------------------------------------------
// Play state — entity movement / rotation
// ---------------------------------------------------------------------------

#[test]
fn update_entity_position() {
    let packet = play::UpdateEntityPosition::new(1, 256, 0, 0, true);
    let body = frame_body(&pin(
        "update_entity_position",
        &packet,
        play::UpdateEntityPosition::encode,
        play::UpdateEntityPosition::decode,
        play::UpdateEntityPosition::PACKET_ID,
    ));
    // entity_id VarInt(1) + three i16 deltas + on_ground bool.
    assert_eq!(body[0], 0x01, "entity id");
    assert_eq!(
        &body[1..3],
        &256i16.to_be_bytes(),
        "delta_x is a 2-byte i16"
    );
    assert_eq!(body[body.len() - 1], 0x01, "on_ground = true");
    assert_eq!(body.len(), 1 + 2 + 2 + 2 + 1);
}

#[test]
fn update_entity_position_and_rotation() {
    let packet = play::UpdateEntityPositionAndRotation::new(1, 256, 0, 0, 16, -16, true);
    let body = frame_body(&pin(
        "update_entity_position_and_rotation",
        &packet,
        play::UpdateEntityPositionAndRotation::encode,
        play::UpdateEntityPositionAndRotation::decode,
        play::UpdateEntityPositionAndRotation::PACKET_ID,
    ));
    // entity_id(1) + dx/dy/dz i16 + yaw i8 + pitch i8 + on_ground.
    assert_eq!(&body[1..3], &256i16.to_be_bytes(), "delta_x i16");
    assert_eq!(body[7], 0x10, "yaw is a single i8 (16)");
    assert_eq!(body[8], 0xf0, "pitch is a single i8 (-16)");
    assert_eq!(body.len(), 1 + 2 + 2 + 2 + 1 + 1 + 1);
}

#[test]
fn update_entity_rotation() {
    let packet = play::UpdateEntityRotation::new(1, 16, -16, true);
    let body = frame_body(&pin(
        "update_entity_rotation",
        &packet,
        play::UpdateEntityRotation::encode,
        play::UpdateEntityRotation::decode,
        play::UpdateEntityRotation::PACKET_ID,
    ));
    // entity_id(1) + yaw i8 + pitch i8 + on_ground.
    assert_eq!(body, vec![0x01, 0x10, 0xf0, 0x01]);
}

#[test]
fn set_head_rotation() {
    let packet = play::SetHeadRotation::new(1, 64);
    let body = frame_body(&pin(
        "set_head_rotation",
        &packet,
        play::SetHeadRotation::encode,
        play::SetHeadRotation::decode,
        play::SetHeadRotation::PACKET_ID,
    ));
    // entity_id VarInt(1) + head_yaw i8(64).
    assert_eq!(body, vec![0x01, 0x40]);
}
