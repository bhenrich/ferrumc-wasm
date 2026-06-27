//! End-to-end real-client-join acceptance test.
//!
//! Starts the real server on an ephemeral port, connects a real
//! [`tokio::net::TcpStream`] acting as a 1.21.8 client, and drives the protocol
//! by hand with the `ferrumc-proto` encoders: handshake(next=2) -> login ->
//! configuration -> play.
//!
//! It asserts the full real-join flow:
//! - **configuration** — the server advertises Known Packs, the client echoes
//!   them, and the server then sends at least the 11 enumerated `RegistryData`
//!   packets followed by Finish Configuration.
//! - **play join sequence** — the keystone packets arrive in the order a real
//!   client needs to leave the loading screen: `JoinGame`, `GameEvent(13)`,
//!   `SetCenterChunk`, a `SynchronizePlayerPosition`, then at least one
//!   `ChunkDataAndLight`.
//! - **keep alive** — a clientbound `KeepAlive` arrives within the (short,
//!   test-configured) timer window.
//!
//! There are no wall-clock sleeps: every step awaits the next frame, the
//! keep-alive interval is driven short via config (not a sleep), and the whole
//! exchange (and the shutdown) is wrapped in a timeout guard so a hang fails
//! loudly instead of stalling the suite.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use ferrumc_codec::{write_var_int, BoundedReader, BoundedString, CodecError, FrameLengthReader};
use ferrumc_proto::generated::configuration::{
    AckFinishConfiguration, ClientboundConfigurationPacket, ServerboundKnownPacks,
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

/// `Game Event` reason `13` (level chunks load start), the cue that releases the
/// loading screen once the player is in a loaded chunk.
const LEVEL_CHUNKS_LOAD_START: u8 = 13;

/// The minimum number of `RegistryData` packets a real client needs to leave
/// configuration (one per enumerated registry).
const MIN_REGISTRIES: usize = 11;

/// What the client observed across the real-join flow.
struct JoinObservations {
    /// The number of `RegistryData` packets received in configuration.
    registry_count: usize,
    /// The order the keystone play packets arrived in, as short tags.
    play_order: Vec<&'static str>,
    /// Whether a clientbound `KeepAlive` arrived within the timer window.
    keep_alive: bool,
}

/// Drives the full real-client-join flow and returns what it saw.
async fn drive_client(addr: SocketAddr) -> anyhow::Result<JoinObservations> {
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

    // Login Acknowledged, then drive the Known Packs handshake: echo the server's
    // advertised packs so it sends the registries, counting the RegistryData
    // packets up to Finish Configuration.
    client
        .send_frame(&encode(|buf| LoginAcknowledged.encode(buf)))
        .await?;
    let mut registry_count = 0;
    loop {
        let frame = client.next_frame().await?;
        match decode_configuration(&frame) {
            ClientboundConfigurationPacket::ClientboundKnownPacks(packs) => {
                let echo = ServerboundKnownPacks::new(packs.known_packs().to_vec());
                client.send_frame(&encode(|buf| echo.encode(buf))).await?;
            }
            ClientboundConfigurationPacket::RegistryData(_) => registry_count += 1,
            ClientboundConfigurationPacket::FinishConfiguration(_) => break,
        }
    }

    // Acknowledge configuration to enter play, then record the keystone play
    // packets in arrival order, up to and including the first spawn chunk.
    client
        .send_frame(&encode(|buf| AckFinishConfiguration.encode(buf)))
        .await?;
    let mut play_order: Vec<&'static str> = Vec::new();
    loop {
        match decode_play(&client.next_frame().await?) {
            ClientboundPlayPacket::JoinGame(_) => play_order.push("join"),
            ClientboundPlayPacket::GameEvent(event)
                if event.reason() == LEVEL_CHUNKS_LOAD_START =>
            {
                play_order.push("game_event");
            }
            ClientboundPlayPacket::SetCenterChunk(_) => play_order.push("center"),
            ClientboundPlayPacket::SynchronizePlayerPosition(_) => {
                // Record the first sync only, to capture its position in the order.
                if !play_order.contains(&"sync") {
                    play_order.push("sync");
                }
            }
            ClientboundPlayPacket::ChunkDataAndLight(_) => {
                // Record the first chunk only, to capture its position in the order.
                if !play_order.contains(&"chunk") {
                    play_order.push("chunk");
                }
            }
            _ => {}
        }
        // The position sync precedes the chunk column; stop once both keystones
        // have arrived so their relative order is captured.
        if play_order.contains(&"sync") && play_order.contains(&"chunk") {
            break;
        }
    }

    // The keep-alive timer is configured short; one must arrive promptly.
    let mut keep_alive = false;
    for _ in 0..64 {
        if matches!(
            decode_play(&client.next_frame().await?),
            ClientboundPlayPacket::ClientboundKeepAlive(_)
        ) {
            keep_alive = true;
            break;
        }
    }

    Ok(JoinObservations {
        registry_count,
        play_order,
        keep_alive,
    })
}

#[tokio::test]
async fn client_reaches_play_and_receives_the_flat_world() {
    // Ephemeral port; a radius-1 spawn keeps the chunk payload small, and a short
    // keep-alive interval lets the test observe a ping without a wall-clock sleep.
    let config = AppConfig::from_toml_str(
        "bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1\nkeep_alive_interval_ms = 100",
    )
    .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    let observed = timeout(GUARD, drive_client(addr))
        .await
        .expect("client flow finished within the timeout guard")
        .expect("client flow succeeded");

    // Configuration enumerated every registry the client needs.
    assert!(
        observed.registry_count >= MIN_REGISTRIES,
        "server must send at least {MIN_REGISTRIES} RegistryData packets, saw {}",
        observed.registry_count,
    );

    // The keystone play packets arrived in the loading-screen-releasing order:
    // the position sync precedes the spawn-area chunk column.
    assert_eq!(
        observed.play_order,
        vec!["join", "game_event", "center", "sync", "chunk"],
        "join sequence out of order",
    );

    // A keep-alive landed within the timer window.
    assert!(
        observed.keep_alive,
        "server must send a clientbound KeepAlive"
    );

    // Clean shutdown: the signal must wind the server down within the guard.
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the timeout guard")
        .expect("clean shutdown");
}
