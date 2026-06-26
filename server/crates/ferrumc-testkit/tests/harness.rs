//! End-to-end exercise of the harness against real `ferrumc-proto` packets:
//! `assert_packet_roundtrip` over handshake/status/ping-pong, a `HexFixture`
//! parse + mismatch diff, and a `ScriptedClient` flow recorded into a
//! `PacketScript` that round-trips through the transcript format.

use ferrumc_codec::BoundedString;
use ferrumc_proto::generated::handshake::Handshake;
use ferrumc_proto::generated::status::{PingRequest, PongResponse, StatusRequest, StatusResponse};

use ferrumc_testkit::{
    assert_packet_roundtrip, HexFixture, PacketScript, RoundtripError, ScriptedClient,
};

/// Builds a bounded string for test packets, failing the test if it is too long.
fn bs<const N: usize>(value: &str) -> BoundedString<N> {
    BoundedString::<N>::new(value.to_owned()).expect("string within bound")
}

#[test]
fn real_packets_round_trip() {
    let handshake = Handshake::new(772, bs::<255>("localhost"), 25565, 1);
    assert_packet_roundtrip(&handshake, Handshake::encode, Handshake::decode).expect("handshake");

    assert_packet_roundtrip(&StatusRequest, StatusRequest::encode, StatusRequest::decode)
        .expect("status request");

    let response = StatusResponse::new(bs::<32767>("{\"version\":{\"protocol\":772}}"));
    assert_packet_roundtrip(&response, StatusResponse::encode, StatusResponse::decode)
        .expect("status response");

    let ping_request = PingRequest::new(0x0102_0304_0506_0708);
    let pong_response = PongResponse::new(0x0102_0304_0506_0708);
    assert_packet_roundtrip(&ping_request, PingRequest::encode, PingRequest::decode).expect("ping");
    assert_packet_roundtrip(&pong_response, PongResponse::encode, PongResponse::decode)
        .expect("pong");
}

#[test]
fn roundtrip_mismatch_does_not_panic() {
    // A decode hook that returns a different payload must come back as an Err,
    // never a panic from inside the harness.
    let ping = PingRequest::new(7);
    let err = assert_packet_roundtrip(&ping, PingRequest::encode, |reader| {
        PingRequest::decode(reader).map(|_| PingRequest::new(8))
    })
    .expect_err("mismatch");
    assert!(matches!(err, RoundtripError::Mismatch { .. }));
}

#[test]
fn hex_fixture_parse_and_mismatch_diff() {
    // The on-wire encoding of PingRequest(1): id 0x01 then 8 big-endian bytes.
    let wire = assert_packet_roundtrip(
        &PingRequest::new(1),
        PingRequest::encode,
        PingRequest::decode,
    )
    .expect("ping wire");

    let fixture = HexFixture::parse("01 00 00 00 00 00 00 00 01").expect("valid hex");
    assert_eq!(fixture.as_bytes(), wire.as_slice());
    assert!(fixture.verify_eq(&wire).is_ok());

    // Flip the final byte and confirm the diff pinpoints offset 8.
    let mut corrupted = wire.clone();
    corrupted[8] ^= 0xff;
    let diff = fixture.diff(&corrupted).expect("should differ");
    assert_eq!(diff.first_diff(), Some(8));
    assert!(diff.to_string().contains("offset 8"));
}

#[test]
fn scripted_client_flow_records_and_replays() {
    // Encode a serverbound handshake + status request and a clientbound status
    // response + pong, then drive them through the in-memory pipe.
    let handshake_wire = assert_packet_roundtrip(
        &Handshake::new(772, bs::<255>("localhost"), 25565, 1),
        Handshake::encode,
        Handshake::decode,
    )
    .expect("handshake wire");
    let request_wire =
        assert_packet_roundtrip(&StatusRequest, StatusRequest::encode, StatusRequest::decode)
            .expect("request wire");
    let response_wire = assert_packet_roundtrip(
        &StatusResponse::new(bs::<32767>("{}")),
        StatusResponse::encode,
        StatusResponse::decode,
    )
    .expect("response wire");
    let pong_wire = assert_packet_roundtrip(
        &PongResponse::new(42),
        PongResponse::encode,
        PongResponse::decode,
    )
    .expect("pong wire");

    let mut client = ScriptedClient::new();
    // Client sends handshake then status request.
    client.send(&handshake_wire);
    client.send(&request_wire);
    // Server (the test) replies; client reads both replies as one stream.
    client.feed(&response_wire);
    client.feed(&pong_wire);
    let received = client.recv_all();

    let mut expected_inbound = response_wire.clone();
    expected_inbound.extend_from_slice(&pong_wire);
    assert_eq!(received, expected_inbound);

    // The recorded transcript matches an independently built expectation.
    let mut expected = PacketScript::new();
    expected.record_serverbound(handshake_wire.clone());
    expected.record_serverbound(request_wire.clone());
    expected.record_clientbound(expected_inbound.clone());
    client
        .verify_against(&expected)
        .expect("transcript matches");

    // The transcript serializes and parses back to an equal script (capture and
    // replay deterministically).
    let text = client.transcript().to_transcript();
    let reparsed = PacketScript::from_transcript(&text).expect("parse transcript");
    assert_eq!(&reparsed, client.transcript());

    // Replaying yields the recorded entries in order.
    let replayed: Vec<_> = reparsed.replay().map(|e| e.bytes().to_vec()).collect();
    assert_eq!(
        replayed,
        vec![handshake_wire, request_wire, expected_inbound]
    );
}
