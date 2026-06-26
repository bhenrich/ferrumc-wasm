//! Integration tests for the live Tokio status-ping server.
//!
//! Each test binds the real [`StatusServer`] to `127.0.0.1:0`, drives it with a
//! real `tokio::net::TcpStream`, and builds the serverbound wire bytes with
//! `ferrumc-proto`'s own packet encoders so the test exercises the actual codec.

use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use ferrumc_codec::{write_var_int, BoundedReader, BoundedString, FrameLengthReader};
use ferrumc_net::{StatusServer, StatusServerConfig, DEFAULT_HANDSHAKE_MAX_FRAME};
use ferrumc_proto::generated::handshake::Handshake;
use ferrumc_proto::generated::status::{ClientboundStatusPacket, PingRequest, StatusRequest};

/// Generous cap for parsing the server's clientbound frames in tests.
const PARSE_FRAME_CAP: usize = 1 << 20;

/// Wraps an encoded packet body in a `VarInt` length prefix, producing a frame.
fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_var_int(
        &mut out,
        i32::try_from(body.len()).expect("body fits in i32"),
    );
    out.extend_from_slice(body);
    out
}

/// Builds a length-prefixed serverbound handshake frame selecting `next_state`.
fn handshake_frame(next_state: i32) -> Vec<u8> {
    let mut body = BytesMut::new();
    Handshake::new(
        772,
        BoundedString::<255>::new("localhost".to_string()).expect("address fits"),
        25565,
        next_state,
    )
    .encode(&mut body)
    .expect("handshake encodes");
    frame(&body)
}

/// Builds a length-prefixed serverbound status-request frame.
fn status_request_frame() -> Vec<u8> {
    let mut body = BytesMut::new();
    StatusRequest
        .encode(&mut body)
        .expect("status request encodes");
    frame(&body)
}

/// Builds a length-prefixed serverbound ping-request frame carrying `payload`.
fn ping_frame(payload: i64) -> Vec<u8> {
    let mut body = BytesMut::new();
    PingRequest::new(payload)
        .encode(&mut body)
        .expect("ping encodes");
    frame(&body)
}

/// Reads one length-delimited frame from `reader` and decodes it as a
/// clientbound status packet.
fn next_status_packet(reader: &mut BoundedReader<'_>) -> ClientboundStatusPacket {
    let len = FrameLengthReader::new(PARSE_FRAME_CAP)
        .read_length(reader)
        .expect("frame length");
    let body = reader.read_bytes(len).expect("frame body present");
    let mut body_reader = BoundedReader::new(body);
    let id = body_reader.read_var_int().expect("packet id");
    ClientboundStatusPacket::decode(id, &mut body_reader).expect("status packet decodes")
}

/// Spawns a server with `config`, returning its address and a shutdown trigger.
async fn spawn_server(
    config: StatusServerConfig,
) -> (
    std::net::SocketAddr,
    oneshot::Sender<()>,
    JoinHandle<std::io::Result<()>>,
) {
    let server = StatusServer::bind("127.0.0.1:0", config)
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
async fn status_request_and_ping_round_trip() {
    let (addr, shutdown_tx, handle) = spawn_server(StatusServerConfig::default()).await;

    let mut client = TcpStream::connect(addr).await.expect("connect");
    let mut wire = handshake_frame(1);
    wire.extend_from_slice(&status_request_frame());
    let payload = 0x0102_0304_0506_0708_i64;
    wire.extend_from_slice(&ping_frame(payload));
    client.write_all(&wire).await.expect("write request");

    // The server closes after the pong, so reading to EOF yields exactly the
    // status response frame followed by the pong frame.
    let mut response = Vec::new();
    timeout(Duration::from_secs(5), client.read_to_end(&mut response))
        .await
        .expect("response arrives before timeout")
        .expect("read response");

    let mut reader = BoundedReader::new(&response);
    match next_status_packet(&mut reader) {
        ClientboundStatusPacket::StatusResponse(resp) => {
            let json = resp.json().as_str();
            assert!(json.contains("\"version\""), "json: {json}");
            assert!(json.contains("\"players\""), "json: {json}");
            assert!(json.contains("\"description\""), "json: {json}");
        }
        ClientboundStatusPacket::PongResponse(other) => {
            panic!("expected status response, got pong {other:?}")
        }
    }
    match next_status_packet(&mut reader) {
        ClientboundStatusPacket::PongResponse(pong) => {
            assert_eq!(pong.payload(), payload);
        }
        ClientboundStatusPacket::StatusResponse(other) => {
            panic!("expected pong response, got status {other:?}")
        }
    }
    assert_eq!(reader.remaining(), 0, "no trailing bytes after the pong");

    let _ = shutdown_tx.send(());
    handle.await.expect("join").expect("run ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_connection_is_closed_by_read_timeout() {
    let config = StatusServerConfig::default().with_io_timeout(Duration::from_millis(150));
    let (addr, shutdown_tx, handle) = spawn_server(config).await;

    let mut client = TcpStream::connect(addr).await.expect("connect");
    // A single continuation byte: a frame prefix that never completes, so the
    // server blocks on a read it will never satisfy and must time out.
    client.write_all(&[0x80]).await.expect("write partial");

    let mut buf = Vec::new();
    let n = timeout(Duration::from_secs(5), client.read_to_end(&mut buf))
        .await
        .expect("server closes before the test timeout")
        .expect("read");
    assert_eq!(n, 0, "server timed out and closed without replying");

    let _ = shutdown_tx.send(());
    handle.await.expect("join").expect("run ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_handshake_frame_is_rejected() {
    let (addr, shutdown_tx, handle) = spawn_server(StatusServerConfig::default()).await;

    let mut client = TcpStream::connect(addr).await.expect("connect");
    // Declare a handshake frame one byte past the handshake cap; the body never
    // needs to arrive, the length prefix alone is rejected.
    let mut oversized = Vec::new();
    write_var_int(
        &mut oversized,
        i32::try_from(DEFAULT_HANDSHAKE_MAX_FRAME + 1).expect("cap fits in i32"),
    );
    client.write_all(&oversized).await.expect("write prefix");

    let mut buf = Vec::new();
    let n = timeout(Duration::from_secs(5), client.read_to_end(&mut buf))
        .await
        .expect("server closes before the test timeout")
        .expect("read");
    assert_eq!(n, 0, "oversized frame is rejected with no reply");

    let _ = shutdown_tx.send(());
    handle.await.expect("join").expect("run ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_status_handshake_is_closed_cleanly() {
    let (addr, shutdown_tx, handle) = spawn_server(StatusServerConfig::default()).await;

    let mut client = TcpStream::connect(addr).await.expect("connect");
    // next_state 2 selects login, which this milestone does not serve: the
    // server must close without sending anything rather than error noisily.
    client
        .write_all(&handshake_frame(2))
        .await
        .expect("write handshake");

    let mut buf = Vec::new();
    let n = timeout(Duration::from_secs(5), client.read_to_end(&mut buf))
        .await
        .expect("server closes before the test timeout")
        .expect("read");
    assert_eq!(n, 0, "login handshake is closed with no reply");

    let _ = shutdown_tx.send(());
    handle.await.expect("join").expect("run ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_signal_stops_the_acceptor() {
    let (addr, shutdown_tx, handle) = spawn_server(StatusServerConfig::default()).await;

    // Trigger shutdown before connecting; the acceptor must wind down and return.
    let _ = shutdown_tx.send(());
    handle
        .await
        .expect("join")
        .expect("run returns ok after shutdown");

    // After shutdown the listener is dropped, so new connections are refused
    // (connect fails, or connects then immediately sees EOF).
    if let Ok(mut late) = TcpStream::connect(addr).await {
        let mut buf = Vec::new();
        let _ = timeout(Duration::from_secs(5), late.read_to_end(&mut buf)).await;
        assert!(buf.is_empty(), "a closed server sends nothing");
    }
}
