//! Regression test for the configuration-phase "unknown packet id" kick.
//!
//! A real 1.21.8 client always sends configuration packets the slice does not
//! model — most notably the `minecraft:brand` Plugin Message (serverbound
//! configuration id `0x02`), plus cookie responses, keep alives, pongs, and
//! resource-pack responses. The protocol requires the server to *ignore* these,
//! but the connection driver previously treated any unmodelled configuration id
//! as a fatal "unknown packet id" error and dropped the client.
//!
//! This drives the real-join flow with a Plugin Message (`0x02`) frame
//! interleaved before and after Client Information (and once more after the
//! Known Packs echo), and asserts the server silently skips every one and the
//! client still reaches play with the full keystone join sequence — the same one
//! `vertical_slice` checks. There are no wall-clock sleeps; the whole exchange is
//! wrapped in a timeout guard so a regression fails loudly instead of hanging.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use ferrumc_codec::{write_var_int, BoundedReader, BoundedString, CodecError, FrameLengthReader};
use ferrumc_proto::generated::configuration::{
    AckFinishConfiguration, ClientInformation, ClientboundConfigurationPacket,
    ServerboundKnownPacks,
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

/// `Game Event` reason `13` (level chunks load start), the cue that releases the
/// loading screen once the player is in a loaded chunk.
const LEVEL_CHUNKS_LOAD_START: u8 = 13;

/// Serverbound configuration `custom_payload` (Plugin Message) id — what the
/// vanilla client uses for `minecraft:brand`. The slice models no such packet,
/// so a frame carrying it must be skipped, not treated as fatal.
const SERVERBOUND_PLUGIN_MESSAGE_ID: i32 = 0x02;

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

/// Builds a serverbound configuration Plugin Message frame body: the
/// `custom_payload` packet id followed by an arbitrary payload. No typed packet
/// models it, so it is hand-encoded; the exact payload bytes are irrelevant
/// because the server skips the whole frame on the unknown id.
fn plugin_message_body() -> Vec<u8> {
    let mut body = Vec::new();
    write_var_int(&mut body, SERVERBOUND_PLUGIN_MESSAGE_ID);
    body.extend_from_slice(b"minecraft:brand\x07FerrumC"); // arbitrary brand payload
    body
}

/// A plausible Client Information (id `0x00`) the client sends during config; the
/// server accepts it without a reply.
fn client_information() -> ClientInformation {
    ClientInformation::new(
        BoundedString::<16>::new("en_us".to_string()).expect("locale within bound"),
        10,    // view distance
        0,     // chat mode: enabled
        true,  // chat colors
        0x7f,  // displayed skin parts: all
        1,     // main hand: right
        false, // text filtering off
        true,  // server listings allowed
        0,     // particle status: all
    )
}

/// Drives the real-join flow, interleaving unmodelled Plugin Message frames
/// through configuration, and returns the keystone play packet order it observed.
async fn drive_client_with_unknown_config(addr: SocketAddr) -> anyhow::Result<Vec<&'static str>> {
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

    // Login Acknowledged moves the server into configuration. Immediately pipeline
    // an unmodelled Plugin Message BEFORE Client Information, the Client
    // Information itself, then another Plugin Message AFTER it — every 0x02 frame
    // must be silently skipped while the 0x00 is accepted.
    client
        .send_frame(&encode(|buf| LoginAcknowledged.encode(buf)))
        .await?;
    client.send_frame(&plugin_message_body()).await?;
    client
        .send_frame(&encode(|buf| client_information().encode(buf)))
        .await?;
    client.send_frame(&plugin_message_body()).await?;

    // Drive the Known Packs handshake and read through to Finish Configuration,
    // slipping one more unmodelled Plugin Message in after the echo (the server is
    // now awaiting the finish ack — it must skip this too).
    loop {
        let frame = client.next_frame().await?;
        match decode_configuration(&frame) {
            ClientboundConfigurationPacket::ClientboundKnownPacks(packs) => {
                let echo = ServerboundKnownPacks::new(packs.known_packs().to_vec());
                client.send_frame(&encode(|buf| echo.encode(buf))).await?;
                client.send_frame(&plugin_message_body()).await?;
            }
            ClientboundConfigurationPacket::RegistryData(_) => {}
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
            ClientboundPlayPacket::SynchronizePlayerPosition(_)
                if !play_order.contains(&"sync") =>
            {
                // Record the first sync only, to capture its position in the order.
                play_order.push("sync");
            }
            ClientboundPlayPacket::ChunkDataAndLight(_) if !play_order.contains(&"chunk") => {
                // Record the first chunk only, to capture its position in the order.
                play_order.push("chunk");
            }
            _ => {}
        }
        // The position sync precedes the chunk column; stop once both arrive.
        if play_order.contains(&"sync") && play_order.contains(&"chunk") {
            break;
        }
    }

    Ok(play_order)
}

#[tokio::test]
async fn unknown_configuration_packets_are_ignored_and_client_reaches_play() {
    // Ephemeral port; radius-1 spawn keeps the chunk payload small.
    let config = AppConfig::from_toml_str(
        "bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1\nkeep_alive_interval_ms = 100",
    )
    .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    let play_order = timeout(GUARD, drive_client_with_unknown_config(addr))
        .await
        .expect("client flow finished within the timeout guard")
        .expect("client flow succeeded despite unmodelled configuration packets");

    // The server skipped every Plugin Message and still drove the full join.
    assert_eq!(
        play_order,
        vec!["join", "game_event", "center", "sync", "chunk"],
        "join sequence out of order after ignoring unknown configuration packets",
    );

    // Clean shutdown within the guard.
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the timeout guard")
        .expect("clean shutdown");
}
