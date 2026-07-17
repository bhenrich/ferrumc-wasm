use std::time::{Duration, Instant};

use bytes::{BufMut, BytesMut};

use ferrumc_codec::{write_var_int, BoundedString};
use ferrumc_net::{
    CompressionError, CompressionState, ConnectionLimits, ConnectionLiveness, ConnectionState,
    DecodeError, DisconnectReason, LivenessConfig, PacketBudget, PlayIngress, PlayIngressActivity,
    PlayIngressError, PlayIngressPoll, WireByteBudget,
};
use ferrumc_proto::generated::play::{ChatCommand, ServerboundKeepAlive, ServerboundPlayPacket};
use tokio::time::{advance, Instant as TokioInstant};

const GENEROUS_WIRE_BUDGET: usize = 1024 * 1024;

fn frame(body: &[u8]) -> Vec<u8> {
    let mut wire = Vec::new();
    write_var_int(
        &mut wire,
        i32::try_from(body.len()).expect("test frame body fits i32"),
    );
    wire.extend_from_slice(body);
    wire
}

fn keep_alive_body(id: i64) -> Vec<u8> {
    let mut body = Vec::new();
    write_var_int(&mut body, ServerboundKeepAlive::PACKET_ID);
    body.put_i64(id);
    body
}

fn compressed_frame(compression: CompressionState, body: &[u8]) -> Vec<u8> {
    let mut compressed = BytesMut::new();
    compression
        .compress(body, &mut compressed)
        .expect("test packet compresses");
    frame(&compressed)
}

fn ingress(
    now: Instant,
    limits: ConnectionLimits,
    compression: CompressionState,
    wire_burst: usize,
) -> PlayIngress {
    PlayIngress::new(
        limits,
        compression,
        PacketBudget::new(now, 300.0, 600.0),
        WireByteBudget::new(now, wire_burst as f64, wire_burst as f64),
    )
}

fn default_ingress(now: Instant, compression: CompressionState) -> PlayIngress {
    ingress(
        now,
        ConnectionLimits::default(),
        compression,
        GENEROUS_WIRE_BUDGET,
    )
}

fn expect_packet(poll: PlayIngressPoll) -> ferrumc_net::PlayIngressPacket {
    match poll {
        PlayIngressPoll::Packet(packet) => packet,
        other => panic!("expected a packet, got {other:?}"),
    }
}

#[test]
fn strict_play_ingress_rejects_trailing_packet_body() {
    let now = Instant::now();
    let mut body = keep_alive_body(7);
    body.extend_from_slice(&[0xDE, 0xAD]);
    let compression = CompressionState::enabled(0);
    let wire = compressed_frame(compression, &body);
    let mut ingress = default_ingress(now, compression);
    ingress.push(&wire).expect("wire is within the buffer cap");

    let error = ingress
        .poll(now)
        .expect_err("strict typed decode rejects trailing bytes");
    assert_eq!(
        error,
        PlayIngressError::Decode(DecodeError::TrailingBytes {
            state: ConnectionState::Play,
            trailing: 2,
        }),
    );
    assert_eq!(
        error.disconnect_reason(),
        DisconnectReason::ProtocolViolation
    );
    assert_eq!(
        ingress.wire_budget().admitted_bytes(),
        u64::try_from(wire.len()).expect("test length fits u64"),
    );
    assert_eq!(ingress.metrics().frames_decoded(), 0);
    assert!(ingress.is_terminated());
}

#[test]
fn wire_budget_is_charged_before_decompression() {
    let now = Instant::now();
    let producer = CompressionState::with_cap(Some(0), 8 * 1024);
    let bomb = compressed_frame(producer, &[0u8; 4 * 1024]);
    let victim = CompressionState::with_cap(Some(0), 1024);

    let mut rejected = ingress(now, ConnectionLimits::default(), victim, bomb.len() - 1);
    rejected
        .push(&bomb)
        .expect("compressed wire fits the frame buffer");
    let error = rejected
        .poll(now)
        .expect_err("wire admission must run before decompression");
    assert_eq!(
        error,
        PlayIngressError::WireBudgetExceeded {
            wire_bytes: bomb.len(),
        },
    );
    assert_eq!(error.disconnect_reason(), DisconnectReason::BudgetExceeded);
    assert_eq!(rejected.wire_budget().admitted_bytes(), 0);

    let mut admitted = ingress(now, ConnectionLimits::default(), victim, bomb.len());
    admitted
        .push(&bomb)
        .expect("compressed wire fits the frame buffer");
    let error = admitted
        .poll(now)
        .expect_err("the exact wire allowance reaches bounded decompression");
    assert_eq!(
        error,
        PlayIngressError::Compression(CompressionError::DeclaredTooLarge {
            declared: 4 * 1024,
            cap: 1024,
        }),
    );
    assert_eq!(
        admitted.wire_budget().admitted_bytes(),
        u64::try_from(bomb.len()).expect("test length fits u64"),
    );
}

#[tokio::test(start_paused = true)]
async fn partial_frame_bytes_do_not_count_as_valid_activity() {
    let start = TokioInstant::now();
    let compression = CompressionState::enabled(0);
    let wire = compressed_frame(compression, &keep_alive_body(41));
    let mut ingress = default_ingress(start.into_std(), compression);
    let mut liveness = ConnectionLiveness::new(
        start,
        LivenessConfig::new(
            Duration::from_mins(1),
            Duration::from_secs(10),
            Duration::from_secs(15),
        ),
    );
    liveness
        .enter_state(ConnectionState::Play, start)
        .expect("open tracker enters play");
    let original_progress = liveness.next_deadline().expect("play deadline").at();
    let original_activity = liveness.last_valid_packet_at();

    let idle = ingress
        .poll(start.into_std())
        .expect("empty ingress is idle");
    assert_eq!(idle, PlayIngressPoll::Idle);
    assert_eq!(idle.valid_activity(), None);

    for (index, byte) in wire.iter().enumerate() {
        ingress.push(&[*byte]).expect("one byte remains bounded");
        let now = TokioInstant::now();
        let poll = ingress
            .poll(now.into_std())
            .expect("partial input is valid");
        if index + 1 < wire.len() {
            assert_eq!(poll, PlayIngressPoll::PartialFrame);
            assert_eq!(poll.valid_activity(), None);
            liveness
                .partial_frame_observed(now)
                .expect("partial activity starts but never refreshes its timer");
            assert_eq!(liveness.last_valid_packet_at(), original_activity);
            assert_eq!(ingress.wire_budget().admitted_bytes(), 0);
            assert_eq!(ingress.metrics().frames_decoded(), 0);
        } else {
            assert_eq!(
                poll.valid_activity(),
                Some(PlayIngressActivity::CompleteValidPacket),
            );
            liveness
                .valid_packet_observed(now)
                .expect("only the complete valid packet refreshes progress");
            assert_eq!(liveness.last_valid_packet_at(), Some(now));
            assert!(liveness.next_deadline().expect("refreshed deadline").at() > original_progress);
        }
        advance(Duration::from_millis(25)).await;
    }

    assert_eq!(ingress.buffered_len(), 0);
    assert_eq!(ingress.metrics().frames_decoded(), 1);
    assert_eq!(
        ingress.wire_budget().admitted_bytes(),
        u64::try_from(wire.len()).expect("test length fits u64"),
    );
}

#[test]
fn truncated_frame_is_partial_but_complete_truncated_packet_is_malformed() {
    let now = Instant::now();
    let valid_wire = frame(&keep_alive_body(5));
    let mut fragmented = default_ingress(now, CompressionState::disabled());
    fragmented
        .push(&valid_wire[..valid_wire.len() - 1])
        .expect("partial wire remains bounded");
    let poll = fragmented
        .poll(now)
        .expect("an incomplete frame is not fatal");
    assert_eq!(poll, PlayIngressPoll::PartialFrame);
    assert_eq!(poll.valid_activity(), None);
    assert_eq!(fragmented.wire_budget().admitted_bytes(), 0);
    assert!(!fragmented.is_terminated());

    let mut truncated_body = Vec::new();
    write_var_int(&mut truncated_body, ServerboundKeepAlive::PACKET_ID);
    truncated_body.extend_from_slice(&[0x00, 0x01, 0x02]);
    let wire = frame(&truncated_body);
    let mut complete = default_ingress(now, CompressionState::disabled());
    complete.push(&wire).expect("frame is complete on the wire");
    assert_eq!(
        complete
            .poll(now)
            .expect_err("a complete frame with a short packet body is malformed"),
        PlayIngressError::Decode(DecodeError::MalformedBody {
            state: ConnectionState::Play,
        }),
    );
    assert_eq!(
        complete.wire_budget().admitted_bytes(),
        u64::try_from(wire.len()).expect("test length fits u64"),
    );
}

#[test]
fn end_of_input_classifies_an_incomplete_frame_as_truncated() {
    let now = Instant::now();
    let mut ingress = default_ingress(now, CompressionState::disabled());
    ingress
        .push(&[0x08, 0x1B, 0x00])
        .expect("declared frame prefix and partial body are bounded");
    assert_eq!(
        ingress.poll(now).expect("the transport may provide more"),
        PlayIngressPoll::PartialFrame,
    );

    let error = ingress
        .end_of_input()
        .expect_err("EOF makes the retained partial frame terminal");
    assert_eq!(error, PlayIngressError::TruncatedFrame { buffered: 3 });
    assert_eq!(error.disconnect_reason(), DisconnectReason::MalformedPacket);
    assert!(ingress.is_terminated());
    assert_eq!(ingress.wire_budget().admitted_bytes(), 0);

    let mut clean = default_ingress(now, CompressionState::disabled());
    assert_eq!(clean.end_of_input(), Ok(()));
    assert!(!clean.is_terminated());

    let mut partial_prefix = default_ingress(now, CompressionState::disabled());
    partial_prefix
        .push(&[0x80])
        .expect("one continuation byte remains bounded");
    assert_eq!(
        partial_prefix
            .poll(now)
            .expect("an incomplete length prefix may receive more bytes"),
        PlayIngressPoll::PartialFrame,
    );
    assert_eq!(
        partial_prefix
            .end_of_input()
            .expect_err("EOF truncates the outer length prefix"),
        PlayIngressError::TruncatedFrame { buffered: 1 },
    );
}

#[test]
fn frame_length_boundaries_are_typed() {
    let now = Instant::now();

    let mut bad_varint = default_ingress(now, CompressionState::disabled());
    bad_varint
        .push(&[0x80; 5])
        .expect("five prefix bytes fit the buffer");
    assert_eq!(
        bad_varint
            .poll(now)
            .expect_err("unterminated five-byte length is malformed"),
        PlayIngressError::Decode(DecodeError::BadLengthVarInt),
    );
    assert_eq!(bad_varint.wire_budget().admitted_bytes(), 0);

    let mut negative_wire = Vec::new();
    write_var_int(&mut negative_wire, -1);
    let mut negative = default_ingress(now, CompressionState::disabled());
    negative
        .push(&negative_wire)
        .expect("negative prefix bytes fit the buffer");
    assert_eq!(
        negative
            .poll(now)
            .expect_err("negative frame lengths are invalid"),
        PlayIngressError::Decode(DecodeError::NegativeLength { length: -1 }),
    );
    assert_eq!(negative.wire_budget().admitted_bytes(), 0);

    let limits = ConnectionLimits::new(4, 4, 4, 4, 4);
    let mut oversized_prefix = Vec::new();
    write_var_int(&mut oversized_prefix, 5);
    let mut oversized = ingress(
        now,
        limits,
        CompressionState::disabled(),
        GENEROUS_WIRE_BUDGET,
    );
    oversized
        .push(&oversized_prefix)
        .expect("prefix alone fits the accumulation buffer");
    assert_eq!(
        oversized
            .poll(now)
            .expect_err("cap plus one is rejected before buffering its body"),
        PlayIngressError::Decode(DecodeError::FrameTooLarge {
            state: ConnectionState::Play,
            length: 5,
            max: 4,
        }),
    );
    assert_eq!(oversized.wire_budget().admitted_bytes(), 0);

    let body = keep_alive_body(11);
    let exact_limits = ConnectionLimits::new(4096, 4096, 4096, 4096, body.len());
    let wire = frame(&body);
    let mut exact = ingress(now, exact_limits, CompressionState::disabled(), wire.len());
    exact.push(&wire).expect("exact-cap frame is bufferable");
    let packet = expect_packet(exact.poll(now).expect("exact-cap frame is valid"));
    assert_eq!(
        packet.packet(),
        &ServerboundPlayPacket::ServerboundKeepAlive(ServerboundKeepAlive::new(11)),
    );
    assert_eq!(packet.wire_bytes(), wire.len());
}

#[test]
fn zero_length_frame_is_typed_malformed() {
    let now = Instant::now();

    let mut plain = default_ingress(now, CompressionState::disabled());
    plain.push(&[0x00]).expect("zero-length frame is complete");
    assert_eq!(
        plain
            .poll(now)
            .expect_err("a plain empty frame has no packet id"),
        PlayIngressError::Decode(DecodeError::MalformedBody {
            state: ConnectionState::Play,
        }),
    );
    assert_eq!(plain.wire_budget().admitted_bytes(), 1);

    let mut compressed = default_ingress(now, CompressionState::enabled(0));
    compressed
        .push(&[0x00])
        .expect("zero-length compressed frame is complete");
    assert_eq!(
        compressed
            .poll(now)
            .expect_err("a compressed frame needs a data-length prefix"),
        PlayIngressError::Compression(CompressionError::BadDataLength),
    );
    assert_eq!(compressed.wire_budget().admitted_bytes(), 1);
}

#[test]
fn compression_bomb_is_bounded() {
    let now = Instant::now();
    let producer = CompressionState::with_cap(Some(0), 8 * 1024);
    let wire = compressed_frame(producer, &[0u8; 4 * 1024]);
    assert!(wire.len() < 256, "the hostile frame stays tiny on the wire");

    let victim = CompressionState::with_cap(Some(0), 1024);
    let mut ingress = default_ingress(now, victim);
    ingress
        .push(&wire)
        .expect("small compressed frame fits the wire buffer");
    let error = ingress
        .poll(now)
        .expect_err("declared output is capped before allocation");
    assert_eq!(
        error,
        PlayIngressError::Compression(CompressionError::DeclaredTooLarge {
            declared: 4 * 1024,
            cap: 1024,
        }),
    );
    assert_eq!(error.disconnect_reason(), DisconnectReason::FrameTooLarge);
    assert_eq!(ingress.metrics().frames_decoded(), 0);
}

#[test]
fn corrupt_compressed_payload_is_typed_after_wire_admission() {
    let now = Instant::now();
    let mut compressed_body = Vec::new();
    write_var_int(&mut compressed_body, 128);
    compressed_body.extend_from_slice(&[0xFF, 0x00, 0x13, 0x37]);
    let wire = frame(&compressed_body);
    let mut ingress = default_ingress(now, CompressionState::enabled(0));
    ingress.push(&wire).expect("wire is bounded");

    let error = ingress
        .poll(now)
        .expect_err("invalid zlib is a typed compression failure");
    assert_eq!(
        error,
        PlayIngressError::Compression(CompressionError::MalformedZlib),
    );
    assert_eq!(error.disconnect_reason(), DisconnectReason::MalformedPacket);
    assert_eq!(
        ingress.wire_budget().admitted_bytes(),
        u64::try_from(wire.len()).expect("test length fits u64"),
    );
    assert_eq!(ingress.metrics().frames_decoded(), 0);
}

#[test]
fn uncompressed_threshold_boundary_is_strict() {
    let now = Instant::now();
    let body = keep_alive_body(13);
    let mut uncompressed_frame_body = Vec::new();
    write_var_int(&mut uncompressed_frame_body, 0);
    uncompressed_frame_body.extend_from_slice(&body);
    let wire = frame(&uncompressed_frame_body);

    let mut exact = default_ingress(now, CompressionState::enabled(body.len()));
    exact.push(&wire).expect("wire is bounded");
    let error = exact
        .poll(now)
        .expect_err("an uncompressed packet at the threshold is invalid");
    assert_eq!(
        error,
        PlayIngressError::Compression(CompressionError::UncompressedAtOrAboveThreshold {
            actual: body.len(),
            threshold: body.len(),
        }),
    );
    assert_eq!(
        error.disconnect_reason(),
        DisconnectReason::ProtocolViolation
    );

    let mut below = default_ingress(now, CompressionState::enabled(body.len() + 1));
    below.push(&wire).expect("wire is bounded");
    let packet = expect_packet(
        below
            .poll(now)
            .expect("an uncompressed packet below the threshold is valid"),
    );
    assert_eq!(
        packet.packet(),
        &ServerboundPlayPacket::ServerboundKeepAlive(ServerboundKeepAlive::new(13)),
    );
}

#[test]
fn inbound_buffer_overflow_is_atomic_and_terminal() {
    let now = Instant::now();
    let limits = ConnectionLimits::new(4, 4, 4, 4, 4);
    let mut ingress = ingress(
        now,
        limits,
        CompressionState::disabled(),
        GENEROUS_WIRE_BUDGET,
    );

    let error = ingress
        .push(&[0u8; 10])
        .expect_err("the maximum buffer is four body bytes plus five prefix bytes");
    assert_eq!(
        error,
        PlayIngressError::Decode(DecodeError::BufferOverflow {
            buffered: 10,
            max: 9,
        }),
    );
    assert_eq!(ingress.buffered_len(), 0);
    assert_eq!(ingress.wire_budget().admitted_bytes(), 0);
    assert!(ingress.is_terminated());
    assert_eq!(
        ingress
            .poll(now)
            .expect_err("a fatal push poisons the strict ingress"),
        PlayIngressError::Terminated {
            reason: DisconnectReason::FrameTooLarge,
        },
    );
}

#[test]
fn fatal_ingress_error_never_reaches_a_pipelined_following_frame() {
    let now = Instant::now();
    let mut unknown_body = Vec::new();
    write_var_int(&mut unknown_body, 0x77);
    let malformed = frame(&unknown_body);
    let valid = frame(&keep_alive_body(18));
    let mut pipelined = malformed.clone();
    pipelined.extend_from_slice(&valid);

    let mut ingress = default_ingress(now, CompressionState::disabled());
    ingress
        .push(&pipelined)
        .expect("both frames fit the bounded buffer");
    assert_eq!(
        ingress
            .poll(now)
            .expect_err("the first malformed frame is fatal"),
        PlayIngressError::Decode(DecodeError::UnknownPacket {
            state: ConnectionState::Play,
            id: 0x77,
        }),
    );
    assert_eq!(ingress.buffered_len(), valid.len());
    assert_eq!(ingress.metrics().frames_decoded(), 0);
    assert_eq!(
        ingress
            .poll(now)
            .expect_err("strict ingress exposes no recovery parser"),
        PlayIngressError::Terminated {
            reason: DisconnectReason::ProtocolViolation,
        },
    );
    assert_eq!(
        ingress
            .push(&[])
            .expect_err("terminal ingress accepts no more bytes"),
        PlayIngressError::Terminated {
            reason: DisconnectReason::ProtocolViolation,
        },
    );
    assert_eq!(ingress.metrics().frames_decoded(), 0);
    assert_eq!(
        ingress.wire_budget().admitted_bytes(),
        u64::try_from(malformed.len()).expect("test length fits u64"),
    );
}

#[test]
fn pipelined_frames_are_charged_one_wire_span_at_a_time() {
    let now = Instant::now();
    let first = frame(&keep_alive_body(21));
    let second = frame(&keep_alive_body(22));
    assert_eq!(first.len(), second.len());
    let mut wire = first.clone();
    wire.extend_from_slice(&second);

    let mut ingress = ingress(
        now,
        ConnectionLimits::default(),
        CompressionState::disabled(),
        first.len(),
    );
    ingress.push(&wire).expect("both frames fit the buffer");
    let first_packet = expect_packet(ingress.poll(now).expect("first frame is admitted"));
    assert_eq!(first_packet.wire_bytes(), first.len());
    assert_eq!(ingress.buffered_len(), second.len());
    assert_eq!(
        ingress
            .poll(now)
            .expect_err("the second span receives a distinct byte charge"),
        PlayIngressError::WireBudgetExceeded {
            wire_bytes: second.len(),
        },
    );
    assert_eq!(
        ingress.wire_budget().admitted_bytes(),
        u64::try_from(first.len()).expect("test length fits u64"),
    );
}

#[test]
fn multi_byte_length_prefix_is_included_in_wire_charge() {
    let now = Instant::now();
    let command =
        BoundedString::<256>::new("x".repeat(200)).expect("test command is within its bound");
    let expected_packet = ChatCommand::new(command);
    let mut body = BytesMut::new();
    expected_packet
        .encode(&mut body)
        .expect("generated test packet encodes");
    assert!(body.len() > 127, "the outer length needs two bytes");

    let wire = frame(&body);
    assert_eq!(
        wire.len(),
        body.len() + 2,
        "the complete wire span includes both length-prefix bytes",
    );

    let mut exact = ingress(
        now,
        ConnectionLimits::default(),
        CompressionState::disabled(),
        wire.len(),
    );
    exact.push(&wire).expect("wire fits the accumulation cap");
    let packet = expect_packet(exact.poll(now).expect("exact wire allowance admits frame"));
    assert_eq!(
        packet.packet(),
        &ServerboundPlayPacket::ChatCommand(expected_packet),
    );
    assert_eq!(packet.wire_bytes(), wire.len());
    assert_eq!(
        exact.wire_budget().admitted_bytes(),
        u64::try_from(wire.len()).expect("test length fits u64"),
    );

    let mut short = ingress(
        now,
        ConnectionLimits::default(),
        CompressionState::disabled(),
        wire.len() - 1,
    );
    short.push(&wire).expect("wire fits the accumulation cap");
    assert_eq!(
        short
            .poll(now)
            .expect_err("body-only allowance omits one prefix byte"),
        PlayIngressError::WireBudgetExceeded {
            wire_bytes: wire.len(),
        },
    );
    assert_eq!(short.wire_budget().admitted_bytes(), 0);
}
