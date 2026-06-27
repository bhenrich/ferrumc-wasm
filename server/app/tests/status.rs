//! Server-list status-ping acceptance test.
//!
//! Starts the real server on an ephemeral port, connects a real
//! [`tokio::net::TcpStream`] acting as a 1.21.8 client, and drives the status
//! branch by hand with the `ferrumc-proto` encoders: handshake(next=1) ->
//! Status Request -> Ping Request.
//!
//! It asserts the exchange a real client performs to render the server in its
//! multiplayer list:
//! - **status response** — a `StatusResponse` whose JSON advertises protocol
//!   `772` (so the client shows the server as COMPATIBLE), the version name, and
//!   the configured player max.
//! - **ping/pong** — a `PongResponse` echoing the exact payload the client sent.
//!
//! This test is self-contained (it does not reach play), so it inlines a minimal
//! frame pipe rather than sharing the play-oriented `common` client. There are no
//! wall-clock sleeps: every step awaits the next frame and the whole exchange
//! (and the shutdown) is wrapped in a timeout guard so a regression fails loudly
//! instead of hanging the suite.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use ferrumc_codec::{write_var_int, BoundedReader, BoundedString, CodecError, FrameLengthReader};
use ferrumc_proto::generated::handshake::Handshake;
use ferrumc_proto::generated::status::{ClientboundStatusPacket, PingRequest, StatusRequest};

use ferrumc_app::AppConfig;

/// Protocol version for Minecraft 1.21.8.
const PROTOCOL_VERSION: i32 = 772;

/// `next_state` selecting the status branch in the handshake.
const NEXT_STATE_STATUS: i32 = 1;

/// Concurrent-connection ceiling the test configures; it doubles as the player
/// max advertised in the status JSON.
const MAX_CONNECTIONS: u32 = 7;

/// Arbitrary ping payload the server must echo verbatim in its pong.
const PING_PAYLOAD: i64 = 0x0123_4567_89ab_cdef;

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

/// A length-delimited frame pipe over a real client socket.
struct FrameStream {
    /// The connected client socket.
    stream: TcpStream,
    /// Accumulated clientbound bytes not yet consumed by a full frame.
    buf: Vec<u8>,
}

impl FrameStream {
    /// Connects to `addr` and wraps the socket.
    async fn connect(addr: SocketAddr) -> anyhow::Result<Self> {
        Ok(Self {
            stream: TcpStream::connect(addr).await?,
            buf: Vec::new(),
        })
    }

    /// Writes one frame: a `VarInt` length prefix followed by `body` (id + fields).
    async fn send_frame(&mut self, body: &[u8]) -> anyhow::Result<()> {
        let mut framed: Vec<u8> = Vec::new();
        write_var_int(&mut framed, i32::try_from(body.len())?);
        framed.extend_from_slice(body);
        self.stream.write_all(&framed).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Reads the next complete frame body (id + fields), reading from the socket
    /// as needed.
    async fn next_frame(&mut self) -> anyhow::Result<Vec<u8>> {
        loop {
            if let Some(body) = self.take_buffered_frame()? {
                return Ok(body);
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                anyhow::bail!("server closed the connection before the expected frame");
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Extracts one complete frame from the buffer, or `None` if more bytes are
    /// needed.
    fn take_buffered_frame(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        const MAX_FRAME: usize = 4 * 1024 * 1024;
        let mut reader = BoundedReader::new(&self.buf);
        let len = match FrameLengthReader::new(MAX_FRAME).read_length(&mut reader) {
            Ok(len) => len,
            Err(CodecError::UnexpectedEof { .. }) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let prefix = reader.position();
        if self.buf.len() < prefix + len {
            return Ok(None);
        }
        let body = self.buf[prefix..prefix + len].to_vec();
        self.buf.drain(..prefix + len);
        Ok(Some(body))
    }
}

/// Encodes a serverbound packet body (id + fields) via its `encode` method.
fn encode<F>(encode_body: F) -> Vec<u8>
where
    F: FnOnce(&mut BytesMut) -> Result<(), ferrumc_proto::ProtoError>,
{
    let mut body = BytesMut::new();
    encode_body(&mut body).expect("serverbound packet encodes");
    body.to_vec()
}

/// Decodes a clientbound status packet from a frame body.
fn decode_status(body: &[u8]) -> ClientboundStatusPacket {
    let mut reader = BoundedReader::new(body);
    let id = reader.read_var_int().expect("status packet id");
    ClientboundStatusPacket::decode(id, &mut reader).expect("status packet decodes")
}

/// Drives a handshake(next=1) + Status Request + Ping Request and asserts the
/// server's status response and pong echo.
async fn run_status_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let mut client = FrameStream::connect(addr).await?;

    // Handshake selecting the status branch.
    let address = BoundedString::<255>::new("127.0.0.1".to_string())?;
    client
        .send_frame(&encode(|buf| {
            Handshake::new(
                PROTOCOL_VERSION,
                address.clone(),
                addr.port(),
                NEXT_STATE_STATUS,
            )
            .encode(buf)
        }))
        .await?;

    // Status Request -> Status Response.
    client
        .send_frame(&encode(|buf| StatusRequest.encode(buf)))
        .await?;
    let ClientboundStatusPacket::StatusResponse(response) =
        decode_status(&client.next_frame().await?)
    else {
        anyhow::bail!("expected a StatusResponse first");
    };
    let json = response.json().as_str();
    assert!(
        json.contains("\"protocol\":772"),
        "status JSON must advertise protocol 772 (COMPATIBLE), got: {json}"
    );
    assert!(
        json.contains("FerrumC 1.21.8"),
        "status JSON must carry the version name, got: {json}"
    );
    assert!(
        json.contains(&format!("\"max\":{MAX_CONNECTIONS}")),
        "status JSON must advertise the configured player max, got: {json}"
    );

    // Ping Request -> Pong Response (exact payload echo).
    client
        .send_frame(&encode(|buf| PingRequest::new(PING_PAYLOAD).encode(buf)))
        .await?;
    let ClientboundStatusPacket::PongResponse(pong) = decode_status(&client.next_frame().await?)
    else {
        anyhow::bail!("expected a PongResponse after the ping");
    };
    assert_eq!(
        pong.payload(),
        PING_PAYLOAD,
        "pong must echo the exact ping payload"
    );

    Ok(())
}

#[tokio::test]
async fn server_answers_status_ping() {
    // Ephemeral port; a distinctive connection ceiling so the advertised player
    // max is checkable. Radius-1 spawn keeps startup cheap (status never joins).
    let config = AppConfig::from_toml_str(&format!(
        "bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1\nmax_connections = {MAX_CONNECTIONS}"
    ))
    .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, run_status_flow(addr))
        .await
        .expect("status flow finished within the timeout guard")
        .expect("status flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the timeout guard")
        .expect("clean shutdown");
}
