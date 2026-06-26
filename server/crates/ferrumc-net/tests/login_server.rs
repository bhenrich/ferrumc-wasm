//! Integration tests for the live Tokio login server (M11).
//!
//! Each test binds the real [`LoginServer`] to `127.0.0.1:0`, drives it with a
//! real `tokio::net::TcpStream`, and builds the serverbound wire bytes with
//! `ferrumc-proto`'s own packet encoders so the test exercises the actual codec
//! and (where enabled) the M10 compression framing.

use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use ferrumc_codec::{write_var_int, BoundedReader, BoundedString};
use ferrumc_net::{offline_uuid, CompressionState, LoginServer, LoginServerConfig};
use ferrumc_proto::generated::configuration::{
    AckFinishConfiguration, ClientboundConfigurationPacket, ServerboundKnownPacks,
};
use ferrumc_proto::generated::handshake::Handshake;
use ferrumc_proto::generated::login::{ClientboundLoginPacket, LoginAcknowledged, LoginStart};
use ferrumc_proto::generated::play::ClientboundPlayPacket;

/// Deadline for any single read/write so a hung server fails the test fast.
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Encodes a packet body to its `id + fields` bytes via its proto encoder.
fn id_body<F>(encode: F) -> BytesMut
where
    F: FnOnce(&mut BytesMut),
{
    let mut body = BytesMut::new();
    encode(&mut body);
    body
}

/// Wraps a raw `id + body` packet in the compressed framing for `compression`
/// (a verbatim pass-through when disabled), then the outer length prefix.
fn wire_frame(id_body: &[u8], compression: &CompressionState) -> Vec<u8> {
    let mut frame_body = BytesMut::new();
    compression
        .compress(id_body, &mut frame_body)
        .expect("compress");
    let mut out = Vec::new();
    write_var_int(
        &mut out,
        i32::try_from(frame_body.len()).expect("frame fits in i32"),
    );
    out.extend_from_slice(&frame_body);
    out
}

fn handshake_frame(next_state: i32) -> Vec<u8> {
    let body = id_body(|buf| {
        Handshake::new(
            772,
            BoundedString::<255>::new("localhost".to_string()).expect("address fits"),
            25565,
            next_state,
        )
        .encode(buf)
        .expect("handshake encodes");
    });
    // The handshake always precedes any compression negotiation.
    wire_frame(&body, &CompressionState::disabled())
}

fn login_start_frame(name: &str, compression: &CompressionState) -> Vec<u8> {
    let body = id_body(|buf| {
        LoginStart::new(
            BoundedString::<16>::new(name.to_string()).expect("name fits"),
            uuid::Uuid::nil(),
        )
        .encode(buf)
        .expect("login start encodes");
    });
    wire_frame(&body, compression)
}

fn login_ack_frame(compression: &CompressionState) -> Vec<u8> {
    let body = id_body(|buf| {
        LoginAcknowledged.encode(buf).expect("login ack encodes");
    });
    wire_frame(&body, compression)
}

fn known_packs_frame(compression: &CompressionState) -> Vec<u8> {
    let body = id_body(|buf| {
        ServerboundKnownPacks::new(Vec::new())
            .encode(buf)
            .expect("known packs encodes");
    });
    wire_frame(&body, compression)
}

fn ack_finish_frame(compression: &CompressionState) -> Vec<u8> {
    let body = id_body(|buf| {
        AckFinishConfiguration
            .encode(buf)
            .expect("ack finish encodes");
    });
    wire_frame(&body, compression)
}

/// Reads one `VarInt` off the stream (the frame-length prefix), byte by byte.
async fn read_varint(stream: &mut TcpStream) -> std::io::Result<usize> {
    let mut value: u32 = 0;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await?;
        value |= u32::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            break;
        }
        shift += 7;
        assert!(shift < 32, "frame-length VarInt too long");
    }
    Ok(value as usize)
}

/// Reads exactly one length-delimited frame body off the stream (still in its
/// on-wire, possibly compressed form).
async fn read_frame_body(stream: &mut TcpStream) -> Vec<u8> {
    let len = timeout(TEST_TIMEOUT, read_varint(stream))
        .await
        .expect("frame length arrives")
        .expect("read frame length");
    let mut body = vec![0u8; len];
    timeout(TEST_TIMEOUT, stream.read_exact(&mut body))
        .await
        .expect("frame body arrives")
        .expect("read frame body");
    body
}

/// Recovers the inflated `id + body` from a frame body via `compression`.
fn inflate(frame_body: &[u8], compression: &CompressionState) -> Vec<u8> {
    compression
        .decompress(frame_body)
        .expect("decompress frame")
}

fn decode_login(frame_body: &[u8], compression: &CompressionState) -> ClientboundLoginPacket {
    let inner = inflate(frame_body, compression);
    let mut reader = BoundedReader::new(&inner);
    let id = reader.read_var_int().expect("packet id");
    ClientboundLoginPacket::decode(id, &mut reader).expect("login packet decodes")
}

fn decode_configuration(
    frame_body: &[u8],
    compression: &CompressionState,
) -> ClientboundConfigurationPacket {
    let inner = inflate(frame_body, compression);
    let mut reader = BoundedReader::new(&inner);
    let id = reader.read_var_int().expect("packet id");
    ClientboundConfigurationPacket::decode(id, &mut reader).expect("config packet decodes")
}

fn decode_play(frame_body: &[u8], compression: &CompressionState) -> ClientboundPlayPacket {
    let inner = inflate(frame_body, compression);
    let mut reader = BoundedReader::new(&inner);
    let id = reader.read_var_int().expect("packet id");
    ClientboundPlayPacket::decode(id, &mut reader).expect("play packet decodes")
}

/// Spawns a server with `config`, returning its address and a shutdown trigger.
async fn spawn_server(
    config: LoginServerConfig,
) -> (
    std::net::SocketAddr,
    oneshot::Sender<()>,
    JoinHandle<std::io::Result<()>>,
) {
    let server = LoginServer::bind("127.0.0.1:0", config)
        .await
        .expect("bind succeeds");
    let addr = server.local_addr();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        server
            .run(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });
    (addr, shutdown_tx, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_login_configuration_reaches_play() {
    let (addr, shutdown_tx, handle) = spawn_server(LoginServerConfig::default()).await;
    let mut client = TcpStream::connect(addr).await.expect("connect");
    // No compression negotiated in this flow.
    let codec = CompressionState::disabled();

    // Handshake (login) + Login Start.
    client
        .write_all(&handshake_frame(2))
        .await
        .expect("write handshake");
    client
        .write_all(&login_start_frame("Saad", &codec))
        .await
        .expect("write login start");

    // The server emits Login Success carrying the offline UUID and echoed name.
    match decode_login(&read_frame_body(&mut client).await, &codec) {
        ClientboundLoginPacket::LoginSuccess(success) => {
            assert_eq!(success.name().as_str(), "Saad");
            assert_eq!(success.uuid(), offline_uuid("Saad"));
        }
        other => panic!("expected Login Success, got {other:?}"),
    }

    // Acknowledge login -> the server drives the configuration phase.
    client
        .write_all(&login_ack_frame(&codec))
        .await
        .expect("write login ack");

    // Known Packs then Finish Configuration.
    assert!(matches!(
        decode_configuration(&read_frame_body(&mut client).await, &codec),
        ClientboundConfigurationPacket::ClientboundKnownPacks(_)
    ));
    assert!(
        matches!(
            decode_configuration(&read_frame_body(&mut client).await, &codec),
            ClientboundConfigurationPacket::FinishConfiguration(_)
        ),
        "server must emit Finish Configuration"
    );

    // Reply with our known packs and the finish ack -> the server enters play.
    client
        .write_all(&known_packs_frame(&codec))
        .await
        .expect("write known packs");
    client
        .write_all(&ack_finish_frame(&codec))
        .await
        .expect("write ack finish");

    // The keepalive shell proves the server transitioned to the play state.
    match decode_play(&read_frame_body(&mut client).await, &codec) {
        ClientboundPlayPacket::ClientboundKeepAlive(keepalive) => {
            assert_eq!(
                keepalive.keep_alive_id(),
                ferrumc_net::DEFAULT_KEEP_ALIVE_ID
            );
        }
        other => panic!("expected play KeepAlive, got {other:?}"),
    }

    let _ = shutdown_tx.send(());
    handle.await.expect("join").expect("run ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_through_play_with_compression() {
    // Threshold 0 compresses every post-negotiation packet, exercising the M10
    // zlib framing end to end.
    let config = LoginServerConfig::default().with_compression_threshold(Some(0));
    let (addr, shutdown_tx, handle) = spawn_server(config).await;
    let mut client = TcpStream::connect(addr).await.expect("connect");

    // Compression is off until the server sends Set Compression.
    let mut codec = CompressionState::disabled();

    client
        .write_all(&handshake_frame(2))
        .await
        .expect("write handshake");
    client
        .write_all(&login_start_frame("Saad", &codec))
        .await
        .expect("write login start");

    // Set Compression arrives uncompressed; after it, both sides switch.
    match decode_login(&read_frame_body(&mut client).await, &codec) {
        ClientboundLoginPacket::SetCompression(set) => {
            assert_eq!(set.threshold(), 0);
            codec = CompressionState::enabled(0);
        }
        other => panic!("expected Set Compression, got {other:?}"),
    }

    // Login Success now arrives compressed.
    match decode_login(&read_frame_body(&mut client).await, &codec) {
        ClientboundLoginPacket::LoginSuccess(success) => {
            assert_eq!(success.uuid(), offline_uuid("Saad"));
        }
        other => panic!("expected Login Success, got {other:?}"),
    }

    client
        .write_all(&login_ack_frame(&codec))
        .await
        .expect("write login ack");

    assert!(matches!(
        decode_configuration(&read_frame_body(&mut client).await, &codec),
        ClientboundConfigurationPacket::ClientboundKnownPacks(_)
    ));
    assert!(matches!(
        decode_configuration(&read_frame_body(&mut client).await, &codec),
        ClientboundConfigurationPacket::FinishConfiguration(_)
    ));

    client
        .write_all(&ack_finish_frame(&codec))
        .await
        .expect("write ack finish");

    assert!(matches!(
        decode_play(&read_frame_body(&mut client).await, &codec),
        ClientboundPlayPacket::ClientboundKeepAlive(_)
    ));

    let _ = shutdown_tx.send(());
    handle.await.expect("join").expect("run ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_login_packet_disconnects_without_login_success() {
    let (addr, shutdown_tx, handle) = spawn_server(LoginServerConfig::default()).await;
    let mut client = TcpStream::connect(addr).await.expect("connect");

    client
        .write_all(&handshake_frame(2))
        .await
        .expect("write handshake");
    // 0x7f is not a serverbound login packet id: a protocol violation that must
    // close the connection before any Login Success is sent.
    let mut bad = Vec::new();
    write_var_int(&mut bad, 1); // frame length
    bad.push(0x7f); // bogus packet id
    client.write_all(&bad).await.expect("write bad frame");

    let mut buf = Vec::new();
    let n = timeout(TEST_TIMEOUT, client.read_to_end(&mut buf))
        .await
        .expect("server closes before the test timeout")
        .expect("read");
    assert_eq!(n, 0, "malformed login is rejected with no reply");

    let _ = shutdown_tx.send(());
    handle.await.expect("join").expect("run ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_login_handshake_is_closed_cleanly() {
    let (addr, shutdown_tx, handle) = spawn_server(LoginServerConfig::default()).await;
    let mut client = TcpStream::connect(addr).await.expect("connect");

    // next_state 1 selects status, which the login server does not serve.
    client
        .write_all(&handshake_frame(1))
        .await
        .expect("write handshake");

    let mut buf = Vec::new();
    let n = timeout(TEST_TIMEOUT, client.read_to_end(&mut buf))
        .await
        .expect("server closes before the test timeout")
        .expect("read");
    assert_eq!(n, 0, "a non-login handshake is closed with no reply");

    let _ = shutdown_tx.send(());
    handle.await.expect("join").expect("run ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_order_login_ack_disconnects() {
    let (addr, shutdown_tx, handle) = spawn_server(LoginServerConfig::default()).await;
    let mut client = TcpStream::connect(addr).await.expect("connect");
    let codec = CompressionState::disabled();

    client
        .write_all(&handshake_frame(2))
        .await
        .expect("write handshake");
    // Login Acknowledged before Login Start: valid packet, wrong phase.
    client
        .write_all(&login_ack_frame(&codec))
        .await
        .expect("write premature ack");

    let mut buf = Vec::new();
    let n = timeout(TEST_TIMEOUT, client.read_to_end(&mut buf))
        .await
        .expect("server closes before the test timeout")
        .expect("read");
    assert_eq!(
        n, 0,
        "an out-of-order login packet is rejected with no reply"
    );

    let _ = shutdown_tx.send(());
    handle.await.expect("join").expect("run ok");
}
