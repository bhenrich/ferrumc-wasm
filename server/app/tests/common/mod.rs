//! Shared test client: a hand-driven 1.21.8 client over a real `TcpStream`.
//!
//! Drives a connection through handshake -> login -> configuration -> play using
//! the `ferrumc-proto` encoders and the `ferrumc-codec` framing primitives, then
//! exposes frame-level read/write so a test can assert on the clientbound play
//! packets the server sends. There are no wall-clock sleeps; every read awaits the
//! next frame and the caller wraps the flow in a timeout guard.

use std::net::SocketAddr;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use ferrumc_codec::{write_var_int, BoundedReader, BoundedString, CodecError, FrameLengthReader};
use ferrumc_proto::generated::configuration::{
    AckFinishConfiguration, ClientboundConfigurationPacket,
};
use ferrumc_proto::generated::handshake::Handshake;
use ferrumc_proto::generated::login::{ClientboundLoginPacket, LoginAcknowledged, LoginStart};
use ferrumc_proto::generated::play::ClientboundPlayPacket;

/// Protocol version for Minecraft 1.21.8.
const PROTOCOL_VERSION: i32 = 772;

/// `next_state` selecting the login branch in the handshake.
const NEXT_STATE_LOGIN: i32 = 2;

/// A length-delimited frame pipe over a real client socket.
pub struct TestClient {
    /// The connected client socket.
    stream: TcpStream,
    /// Accumulated clientbound bytes not yet consumed by a full frame.
    buf: Vec<u8>,
}

impl TestClient {
    /// Connects to `addr` and wraps the socket.
    pub async fn connect(addr: SocketAddr) -> anyhow::Result<Self> {
        Ok(Self {
            stream: TcpStream::connect(addr).await?,
            buf: Vec::new(),
        })
    }

    /// Writes one frame: a `VarInt` length prefix followed by `body` (id + fields).
    pub async fn send_frame(&mut self, body: &[u8]) -> anyhow::Result<()> {
        let mut framed: Vec<u8> = Vec::new();
        write_var_int(&mut framed, i32::try_from(body.len())?);
        framed.extend_from_slice(body);
        self.stream.write_all(&framed).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Reads the next complete frame body (id + fields), reading from the socket
    /// as needed.
    pub async fn next_frame(&mut self) -> anyhow::Result<Vec<u8>> {
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

    /// Reads and decodes the next clientbound play packet.
    pub async fn next_play(&mut self) -> anyhow::Result<ClientboundPlayPacket> {
        let body = self.next_frame().await?;
        let mut reader = BoundedReader::new(&body);
        let id = reader.read_var_int()?;
        Ok(ClientboundPlayPacket::decode(id, &mut reader)?)
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
        if matches!(
            ClientboundLoginPacket::decode(id, &mut reader)?,
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
        let mut reader = BoundedReader::new(&frame);
        let id = reader.read_var_int()?;
        if matches!(
            ClientboundConfigurationPacket::decode(id, &mut reader)?,
            ClientboundConfigurationPacket::FinishConfiguration(_)
        ) {
            break;
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
