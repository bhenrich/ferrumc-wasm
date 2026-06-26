//! End-to-end vertical-slice test.
//!
//! Starts the real server on an ephemeral port, connects a real
//! [`tokio::net::TcpStream`] acting as the client, and drives the protocol by
//! hand with the `ferrumc-proto` encoders: handshake(next=2) -> login ->
//! configuration -> play. It asserts the server transitions the client into play
//! and sends the keystone payload — a `JoinGame`, a `SynchronizePlayerPosition`,
//! and at least one `ChunkDataAndLight` — then asserts a clean shutdown.
//!
//! There are no wall-clock sleeps: every step awaits the next frame, and the
//! whole exchange (and the shutdown) is wrapped in a timeout guard so a hang
//! fails loudly instead of stalling the suite.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use ferrumc_codec::{write_var_int, BoundedReader, BoundedString, CodecError, FrameLengthReader};
use ferrumc_proto::generated::configuration::{
    AckFinishConfiguration, ClientboundConfigurationPacket,
};
use ferrumc_proto::generated::handshake::Handshake;
use ferrumc_proto::generated::login::{ClientboundLoginPacket, LoginAcknowledged, LoginStart};
use ferrumc_proto::generated::play::ClientboundPlayPacket;

use ferrumc_app::AppConfig;

/// Protocol version for Minecraft 1.21.8.
const PROTOCOL_VERSION: i32 = 772;

/// `next_state` selecting the login branch in the handshake.
const NEXT_STATE_LOGIN: i32 = 2;

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

/// Decodes a clientbound login packet from a frame body.
fn decode_login(body: &[u8]) -> ClientboundLoginPacket {
    let mut reader = BoundedReader::new(body);
    let id = reader.read_var_int().expect("login packet id");
    ClientboundLoginPacket::decode(id, &mut reader).expect("login packet decodes")
}

/// Decodes a clientbound configuration packet from a frame body.
fn decode_configuration(body: &[u8]) -> ClientboundConfigurationPacket {
    let mut reader = BoundedReader::new(body);
    let id = reader.read_var_int().expect("configuration packet id");
    ClientboundConfigurationPacket::decode(id, &mut reader).expect("configuration packet decodes")
}

/// Decodes a clientbound play packet from a frame body.
fn decode_play(body: &[u8]) -> ClientboundPlayPacket {
    let mut reader = BoundedReader::new(body);
    let id = reader.read_var_int().expect("play packet id");
    ClientboundPlayPacket::decode(id, &mut reader).expect("play packet decodes")
}

/// What the client observed once it reached and read the play phase.
struct PlayObservations {
    /// Whether a `JoinGame` packet arrived.
    join_game: bool,
    /// Whether a `SynchronizePlayerPosition` packet arrived.
    position: bool,
    /// Whether at least one `ChunkDataAndLight` packet arrived.
    chunk: bool,
}

/// Drives the full client flow and returns what it saw in play.
async fn drive_client(addr: SocketAddr) -> anyhow::Result<PlayObservations> {
    let mut client = FrameStream::connect(addr).await?;

    // Handshake -> Login.
    let address = BoundedString::<255>::new("127.0.0.1".to_string())?;
    client
        .send_frame(&encode(|buf| {
            Handshake::new(
                PROTOCOL_VERSION,
                address.clone(),
                addr.port(),
                NEXT_STATE_LOGIN,
            )
            .encode(buf)
        }))
        .await?;

    // Login Start, then wait for Login Success.
    let name = BoundedString::<16>::new("Saad".to_string())?;
    client
        .send_frame(&encode(|buf| {
            LoginStart::new(name.clone(), uuid::Uuid::nil()).encode(buf)
        }))
        .await?;
    loop {
        let frame = client.next_frame().await?;
        if matches!(
            decode_login(&frame),
            ClientboundLoginPacket::LoginSuccess(_)
        ) {
            break;
        }
    }

    // Login Acknowledged, then wait for the configuration to finish.
    client
        .send_frame(&encode(|buf| LoginAcknowledged.encode(buf)))
        .await?;
    loop {
        let frame = client.next_frame().await?;
        if matches!(
            decode_configuration(&frame),
            ClientboundConfigurationPacket::FinishConfiguration(_)
        ) {
            break;
        }
    }

    // Acknowledge configuration to enter play, then collect the keystone payload.
    client
        .send_frame(&encode(|buf| AckFinishConfiguration.encode(buf)))
        .await?;
    let mut seen = PlayObservations {
        join_game: false,
        position: false,
        chunk: false,
    };
    while !(seen.join_game && seen.position && seen.chunk) {
        let frame = client.next_frame().await?;
        match decode_play(&frame) {
            ClientboundPlayPacket::JoinGame(_) => seen.join_game = true,
            ClientboundPlayPacket::SynchronizePlayerPosition(_) => seen.position = true,
            ClientboundPlayPacket::ChunkDataAndLight(_) => seen.chunk = true,
            _ => {}
        }
    }
    Ok(seen)
}

#[tokio::test]
async fn client_reaches_play_and_receives_the_flat_world() {
    // Bind to an ephemeral port; a radius-1 spawn keeps the chunk payload small.
    let config = AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    let observed = timeout(GUARD, drive_client(addr))
        .await
        .expect("client flow finished within the timeout guard")
        .expect("client flow succeeded");

    assert!(observed.join_game, "server must send JoinGame");
    assert!(
        observed.position,
        "server must send a SynchronizePlayerPosition"
    );
    assert!(
        observed.chunk,
        "server must send at least one ChunkDataAndLight"
    );

    // Clean shutdown: the signal must wind the server down within the guard.
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the timeout guard")
        .expect("clean shutdown");
}
