//! Per-packet encode->decode round-trips plus the mandated malformed-input
//! tests (truncated, oversized string/array prefix, bad `VarInt`, trailing
//! bytes, unknown id) for the generated protocol 772 packet codecs.

use bytes::BytesMut;
use ferrumc_codec::{BoundedReader, BoundedString};
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
use ferrumc_proto::generated::status::{
    PingRequest, PongResponse, ServerboundStatusPacket, StatusRequest, StatusResponse,
};
use ferrumc_proto::ProtoError;

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
