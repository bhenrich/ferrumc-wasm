//! Shared test client: a hand-driven 1.21.8 client over a real `TcpStream`.
//!
//! Drives a connection through handshake -> login -> configuration -> play using
//! the `ferrumc-proto` encoders and the `ferrumc-codec` framing primitives, then
//! exposes frame-level read/write so a test can assert on the clientbound play
//! packets the server sends. There are no wall-clock sleeps; every read awaits the
//! next frame and the caller wraps the flow in a timeout guard.
//!
//! The socket is split: a background task continuously drains the read half into
//! a bounded frame channel, while the writer half stays on the [`TestClient`].
//! Continuous draining is what a real client does, and it matters here because a
//! single `ChunkDataAndLight` packet carries ~70 KiB of section + light data — a
//! spawn area is hundreds of KiB, more than a socket buffer holds. Without a
//! background drainer, an idle client's socket fills and the server's join-kit
//! `write_all` blocks before it ever reaches its serverbound read loop.

use std::net::SocketAddr;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use uuid::Uuid;

use ferrumc_codec::{write_var_int, BoundedReader, BoundedString, CodecError, FrameLengthReader};
use ferrumc_proto::generated::configuration::{
    AckFinishConfiguration, ClientboundConfigurationPacket, ServerboundKnownPacks,
};
use ferrumc_proto::generated::handshake::Handshake;
use ferrumc_proto::generated::login::{ClientboundLoginPacket, LoginAcknowledged, LoginStart};
use ferrumc_proto::generated::play::ClientboundPlayPacket;

/// Protocol version for Minecraft 1.21.8.
const PROTOCOL_VERSION: i32 = 772;

/// `next_state` selecting the login branch in the handshake.
const NEXT_STATE_LOGIN: i32 = 2;

/// Capacity (in frames) of the channel the background drainer feeds.
///
/// Comfortably above the join kit plus the appearance/broadcast frames a test
/// reads before it next drains; tests consume promptly so it never fills.
const FRAME_CHANNEL_CAPACITY: usize = 1024;

/// The largest clientbound frame the drainer will accept, matching the server's
/// play frame cap with headroom.
const MAX_FRAME: usize = 4 * 1024 * 1024;

/// A length-delimited frame pipe over a real client socket.
pub struct TestClient {
    /// The write half of the socket, used to send serverbound frames.
    writer: OwnedWriteHalf,
    /// Complete clientbound frame bodies, fed by the background drainer.
    frames: mpsc::Receiver<Vec<u8>>,
    /// Canonical UUID observed in Login Success once login has completed.
    login_uuid: Option<Uuid>,
}

impl TestClient {
    /// Connects to `addr`, splits the socket, and spawns the background drainer.
    pub async fn connect(addr: SocketAddr) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(addr).await?;
        let (reader, writer) = stream.into_split();
        let (tx, frames) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        tokio::spawn(drain_frames(reader, tx));
        Ok(Self {
            writer,
            frames,
            login_uuid: None,
        })
    }

    /// Writes one frame: a `VarInt` length prefix followed by `body` (id + fields).
    pub async fn send_frame(&mut self, body: &[u8]) -> anyhow::Result<()> {
        let mut framed: Vec<u8> = Vec::new();
        write_var_int(&mut framed, i32::try_from(body.len())?);
        framed.extend_from_slice(body);
        self.writer.write_all(&framed).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Writes several complete frames in one socket operation.
    ///
    /// This is used by fail-stop regressions: the malformed frame and its
    /// following sentinel must already be in one client write so a rejected
    /// first frame cannot race a separate write of the second.
    #[allow(dead_code)] // `common` is compiled separately into tests that do not pipeline frames.
    pub async fn send_frames(&mut self, bodies: &[&[u8]]) -> anyhow::Result<()> {
        let total_body_bytes = bodies.iter().map(|body| body.len()).sum::<usize>();
        let mut framed = Vec::with_capacity(total_body_bytes.saturating_add(bodies.len() * 5));
        for body in bodies {
            write_var_int(&mut framed, i32::try_from(body.len())?);
            framed.extend_from_slice(body);
        }
        self.writer.write_all(&framed).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Half-closes the client-to-server direction while keeping received frames
    /// readable.
    ///
    /// Login-boundary tests use this after a handshake-only write: a conforming
    /// server must produce its rejection from that handshake rather than waiting
    /// for a `LoginStart`. On the old behavior the server instead observes EOF,
    /// which closes the frame channel immediately and fails the regression
    /// without relying on a wall-clock delay.
    #[allow(dead_code)] // `common` is compiled separately into tests that never half-close.
    pub async fn finish_writes(&mut self) -> anyhow::Result<()> {
        self.writer.shutdown().await?;
        Ok(())
    }

    /// Awaits the next complete frame body (id + fields) from the drainer.
    pub async fn next_frame(&mut self) -> anyhow::Result<Vec<u8>> {
        self.frames.recv().await.ok_or_else(|| {
            anyhow::anyhow!("server closed the connection before the expected frame")
        })
    }

    /// Reads and decodes the next clientbound play packet.
    pub async fn next_play(&mut self) -> anyhow::Result<ClientboundPlayPacket> {
        let body = self.next_frame().await?;
        let mut reader = BoundedReader::new(&body);
        let id = reader.read_var_int()?;
        Ok(ClientboundPlayPacket::decode(id, &mut reader)?)
    }

    /// Reads the next clientbound Play packet, or reports that the socket closed.
    #[allow(dead_code)] // `common` is compiled separately into tests that only expect live frames.
    pub async fn next_play_or_closed(&mut self) -> anyhow::Result<Option<ClientboundPlayPacket>> {
        let Some(body) = self.frames.recv().await else {
            return Ok(None);
        };
        let mut reader = BoundedReader::new(&body);
        let id = reader.read_var_int()?;
        Ok(Some(ClientboundPlayPacket::decode(id, &mut reader)?))
    }

    /// Returns the UUID carried by Login Success after [`login_to_play`].
    #[allow(dead_code)] // This shared module is compiled into tests that do not inspect login identity.
    pub fn login_uuid(&self) -> Option<Uuid> {
        self.login_uuid
    }
}

/// Continuously reads `reader`, splitting the byte stream into length-delimited
/// frames and forwarding each body over `tx` until EOF, a framing error, or the
/// receiver is dropped.
async fn drain_frames<R>(mut reader: R, tx: mpsc::Sender<Vec<u8>>)
where
    R: AsyncReadExt + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        // Forward every complete frame currently buffered.
        loop {
            match take_frame(&buf) {
                Ok(Some((prefix, len))) => {
                    let body = buf[prefix..prefix + len].to_vec();
                    buf.drain(..prefix + len);
                    if tx.send(body).await.is_err() {
                        return; // The test dropped the client.
                    }
                }
                Ok(None) => break,
                Err(_) => return, // Malformed length prefix: stop draining.
            }
        }
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return, // EOF or socket error: close the channel.
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Locates the next complete frame in `buf`, returning `(prefix_len, body_len)`
/// when one is fully present, or `None` if more bytes are needed.
fn take_frame(buf: &[u8]) -> Result<Option<(usize, usize)>, CodecError> {
    let mut reader = BoundedReader::new(buf);
    let len = match FrameLengthReader::new(MAX_FRAME).read_length(&mut reader) {
        Ok(len) => len,
        Err(CodecError::UnexpectedEof { .. }) => return Ok(None),
        Err(err) => return Err(err),
    };
    let prefix = reader.position();
    if buf.len() < prefix + len {
        return Ok(None);
    }
    Ok(Some((prefix, len)))
}

/// Encodes a serverbound packet body (id + fields) via its `encode` method.
pub fn encode<F>(encode_body: F) -> Vec<u8>
where
    F: FnOnce(&mut BytesMut) -> Result<(), ferrumc_proto::ProtoError>,
{
    let mut body = BytesMut::new();
    encode_body(&mut body).expect("serverbound packet encodes");
    body.to_vec()
}

/// Connects, logs in offline as `name`, and reads up to and including the
/// `JoinGame` packet — confirming the client reached play and the server
/// registered its session. Any later play frames stay buffered for the caller.
pub async fn login_to_play(addr: SocketAddr, name: &str) -> anyhow::Result<TestClient> {
    let mut client = TestClient::connect(addr).await?;

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
    let player_name = BoundedString::<16>::new(name.to_string())?;
    client
        .send_frame(&encode(|buf| {
            LoginStart::new(player_name.clone(), uuid::Uuid::nil()).encode(buf)
        }))
        .await?;
    loop {
        let frame = client.next_frame().await?;
        let mut reader = BoundedReader::new(&frame);
        let id = reader.read_var_int()?;
        if let ClientboundLoginPacket::LoginSuccess(success) =
            ClientboundLoginPacket::decode(id, &mut reader)?
        {
            client.login_uuid = Some(success.uuid());
            break;
        }
    }

    // Login Acknowledged, then drive the Known Packs handshake: echo the server's
    // advertised packs so it sends the registries, and read through to Finish
    // Configuration (the registry data packets pass by).
    client
        .send_frame(&encode(|buf| LoginAcknowledged.encode(buf)))
        .await?;
    loop {
        let frame = client.next_frame().await?;
        let mut reader = BoundedReader::new(&frame);
        let id = reader.read_var_int()?;
        match ClientboundConfigurationPacket::decode(id, &mut reader)? {
            ClientboundConfigurationPacket::ClientboundKnownPacks(packs) => {
                let echo = ServerboundKnownPacks::new(packs.known_packs().to_vec());
                client.send_frame(&encode(|buf| echo.encode(buf))).await?;
            }
            ClientboundConfigurationPacket::FinishConfiguration(_) => break,
            ClientboundConfigurationPacket::RegistryData(_) => {}
        }
    }

    // Acknowledge configuration to enter play, then read through JoinGame.
    client
        .send_frame(&encode(|buf| AckFinishConfiguration.encode(buf)))
        .await?;
    loop {
        if matches!(
            client.next_play().await?,
            ClientboundPlayPacket::JoinGame(_)
        ) {
            break;
        }
    }

    Ok(client)
}
