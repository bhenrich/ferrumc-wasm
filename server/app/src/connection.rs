//! Per-connection protocol driver: handshake -> login -> configuration -> play.
//!
//! Each accepted socket runs one [`handle_connection`] task. It drives the login
//! handshake by hand over the `ferrumc-net` framing primitives (the crate's own
//! `LoginServer` ends at a keepalive shell and exposes no post-play hook, so the
//! app wires the play handoff itself). The instant the client acknowledges
//! configuration, the connection joins the simulation and replays the shared
//! [`JoinKit`](crate::world::JoinKit): `JoinGame`, a position sync, then the
//! spawn-area chunk packets. From there it pumps serverbound play packets into
//! the simulation and clientbound outputs back to the socket.

use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;

use ferrumc_codec::{BoundedReader, BoundedString};
use ferrumc_core::PlayerId;
use ferrumc_math::Vec3;
use ferrumc_net::{
    offline_uuid, CompressionState, ConnectionLimits, ConnectionState, DisconnectReason,
    InboundDecoder, InboundPacket, OutboundEncoder, OutboundPacket, PlayWriter,
};
use ferrumc_proto::generated::configuration::{
    ClientboundConfigurationPacket, ClientboundKnownPacks, FinishConfiguration,
    ServerboundConfigurationPacket,
};
use ferrumc_proto::generated::handshake::ServerboundHandshakePacket;
use ferrumc_proto::generated::login::{
    ClientboundLoginPacket, LoginSuccess, ServerboundLoginPacket, SetCompression,
};
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, ServerboundPlayPacket, SynchronizePlayerPosition,
};
use ferrumc_session::{NetEvent, PlayerSessionHandle};

use crate::driver::SimCommand;
use crate::world::JoinKit;

/// The `next_state` value in a handshake that selects the login branch.
const NEXT_STATE_LOGIN: i32 = 2;

/// Bytes read off the socket per `read` call before decoding.
const READ_CHUNK: usize = 4096;

/// Immutable context shared by every connection task.
///
/// Cloned cheaply (it is small and the [`JoinKit`] is behind an [`Arc`]) and
/// handed to each [`handle_connection`] call.
#[derive(Clone)]
pub(crate) struct ConnContext {
    /// Per-state hostile-input frame caps.
    pub(crate) limits: ConnectionLimits,
    /// Deadline applied to each socket read and write.
    pub(crate) io_timeout: Duration,
    /// Negotiated compression threshold, or `None` to leave compression off.
    pub(crate) compression_threshold: Option<i32>,
    /// The clientbound payload replayed when a client reaches play.
    pub(crate) join_kit: Arc<JoinKit>,
    /// Bounded channel to the simulation/session driver.
    pub(crate) commands: mpsc::Sender<SimCommand>,
}

impl ConnContext {
    /// The active compression threshold (`>= 0`), or `None` when disabled.
    fn enabled_threshold(&self) -> Option<i32> {
        self.compression_threshold
            .filter(|threshold| *threshold >= 0)
    }
}

/// The login-handshake phase, one step finer than [`ConnectionState`] so an
/// out-of-order-but-valid packet can be rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginPhase {
    /// Awaiting the initial handshake.
    Handshaking,
    /// Awaiting Login Start.
    Login,
    /// Awaiting Login Acknowledged (Login Success already sent).
    AwaitingLoginAck,
    /// Awaiting Ack Finish Configuration.
    AwaitingFinishAck,
}

impl LoginPhase {
    /// The connection state the decoder/encoder use for the next frame.
    fn connection_state(self) -> ConnectionState {
        match self {
            Self::Handshaking => ConnectionState::Handshaking,
            Self::Login | Self::AwaitingLoginAck => ConnectionState::Login,
            Self::AwaitingFinishAck => ConnectionState::Configuration,
        }
    }
}

/// The outcome of feeding one decoded packet to the login state machine.
enum LoginProgress {
    /// Stay in login; keep reading.
    Continue,
    /// Close the connection cleanly (a non-login handshake).
    Close,
    /// The client reached play.
    Play,
}

/// One connection's mutable framing state during the login handshake.
struct Connection<'a> {
    /// The accepted socket.
    stream: TcpStream,
    /// Accumulating serverbound frame decoder.
    decoder: InboundDecoder,
    /// Clientbound frame encoder.
    encoder: OutboundEncoder,
    /// Shared (read + write) compression state, off until negotiated.
    compression: CompressionState,
    /// Shared connection context.
    ctx: &'a ConnContext,
}

impl<'a> Connection<'a> {
    /// Builds the framing state for a freshly accepted `stream`.
    fn new(stream: TcpStream, ctx: &'a ConnContext) -> Self {
        Self {
            stream,
            decoder: InboundDecoder::new(ctx.limits),
            encoder: OutboundEncoder::new(ctx.limits),
            compression: CompressionState::disabled(),
            ctx,
        }
    }

    /// Encodes and writes one clientbound packet, bounded by the I/O timeout.
    async fn send(&mut self, packet: &OutboundPacket) -> anyhow::Result<()> {
        let mut buf = BytesMut::new();
        self.encoder
            .encode_compressed(packet, &mut buf, &self.compression)?;
        write_all(&mut self.stream, &buf, self.ctx.io_timeout).await
    }

    /// Sends the (optional) Set Compression and the Login Success for `name`.
    async fn send_login_success(&mut self, name: &BoundedString<16>) -> anyhow::Result<()> {
        if let Some(threshold) = self.ctx.enabled_threshold() {
            self.send(&OutboundPacket::Login(
                ClientboundLoginPacket::SetCompression(SetCompression::new(threshold)),
            ))
            .await?;
            // Set Compression itself goes out uncompressed; every later frame is
            // framed with the negotiated zlib threshold.
            self.compression = CompressionState::enabled(threshold as usize);
        }
        let uuid = offline_uuid(name.as_str());
        self.send(&OutboundPacket::Login(
            ClientboundLoginPacket::LoginSuccess(LoginSuccess::new(uuid, name.clone(), Vec::new())),
        ))
        .await
    }

    /// Advertises the (empty) known packs and finishes configuration.
    async fn send_enter_configuration(&mut self) -> anyhow::Result<()> {
        self.send(&OutboundPacket::Configuration(
            ClientboundConfigurationPacket::ClientboundKnownPacks(ClientboundKnownPacks::new(
                Vec::new(),
            )),
        ))
        .await?;
        self.send(&OutboundPacket::Configuration(
            ClientboundConfigurationPacket::FinishConfiguration(FinishConfiguration),
        ))
        .await
    }
}

/// Drives one accepted socket through login and, on success, play.
///
/// # Errors
///
/// Returns an error on a protocol violation, a framing/encode failure, a socket
/// I/O error or timeout, or loss of the simulation driver. A clean close before
/// play (a status handshake, EOF, or shutdown) is `Ok(())`.
pub(crate) async fn handle_connection(
    stream: TcpStream,
    ctx: &ConnContext,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut conn = Connection::new(stream, ctx);
    match run_login(&mut conn, &mut shutdown).await? {
        Some(name) => enter_play(conn, name, &mut shutdown).await,
        None => Ok(()),
    }
}

/// Runs the login handshake, returning the player name once play is reached, or
/// `None` if the connection closed cleanly first.
async fn run_login(
    conn: &mut Connection<'_>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<Option<BoundedString<16>>> {
    let mut phase = LoginPhase::Handshaking;
    let mut name: Option<BoundedString<16>> = None;
    let mut read_buf = [0u8; READ_CHUNK];

    loop {
        let state = phase.connection_state();
        // Drain everything buffered before blocking on another read so pipelined
        // frames progress without an extra round trip.
        if let Some(packet) = conn
            .decoder
            .next_packet_compressed(state, &conn.compression)?
        {
            match advance(conn, &mut phase, &mut name, &packet).await? {
                LoginProgress::Continue => continue,
                LoginProgress::Close => return Ok(None),
                LoginProgress::Play => {
                    return name
                        .take()
                        .map(Some)
                        .ok_or_else(|| anyhow::anyhow!("reached play without a login name"));
                }
            }
        }

        let io_timeout = conn.ctx.io_timeout;
        match read_more(
            &mut conn.stream,
            &mut conn.decoder,
            &mut read_buf,
            io_timeout,
            shutdown,
        )
        .await?
        {
            ReadOutcome::Data => {}
            ReadOutcome::Eof | ReadOutcome::Shutdown => return Ok(None),
        }
    }
}

/// Feeds one decoded packet to the login state machine, sending replies.
async fn advance(
    conn: &mut Connection<'_>,
    phase: &mut LoginPhase,
    name: &mut Option<BoundedString<16>>,
    packet: &InboundPacket,
) -> anyhow::Result<LoginProgress> {
    match (*phase, packet) {
        (
            LoginPhase::Handshaking,
            InboundPacket::Handshake(ServerboundHandshakePacket::Handshake(handshake)),
        ) => {
            if handshake.next_state() == NEXT_STATE_LOGIN {
                *phase = LoginPhase::Login;
                Ok(LoginProgress::Continue)
            } else {
                // Status / transfer: this slice serves only the login branch.
                Ok(LoginProgress::Close)
            }
        }
        (LoginPhase::Login, InboundPacket::Login(ServerboundLoginPacket::LoginStart(start))) => {
            let player_name = start.name().clone();
            conn.send_login_success(&player_name).await?;
            *name = Some(player_name);
            *phase = LoginPhase::AwaitingLoginAck;
            Ok(LoginProgress::Continue)
        }
        (
            LoginPhase::AwaitingLoginAck,
            InboundPacket::Login(ServerboundLoginPacket::LoginAcknowledged(_)),
        ) => {
            conn.send_enter_configuration().await?;
            *phase = LoginPhase::AwaitingFinishAck;
            Ok(LoginProgress::Continue)
        }
        (LoginPhase::AwaitingFinishAck, InboundPacket::Configuration(config)) => match config {
            ServerboundConfigurationPacket::AckFinishConfiguration(_) => Ok(LoginProgress::Play),
            // Client settings / known packs are accepted but need no reply.
            _ => Ok(LoginProgress::Continue),
        },
        _ => anyhow::bail!("unexpected {:?} packet during {:?}", packet.state(), *phase),
    }
}

/// Joins the simulation and replays the join kit, then pumps the play link until
/// the client disconnects or the server shuts down.
async fn enter_play(
    conn: Connection<'_>,
    name: BoundedString<16>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let Connection {
        mut stream,
        mut decoder,
        compression,
        ctx,
        ..
    } = conn;
    let player = PlayerId::offline(name.as_str());
    let position = ctx.join_kit.spawn_position();
    let mut handle = join_simulation(ctx, player, position).await?;

    // Replay the keystone payload, then drain any already-buffered play frames.
    let mut writer = PlayWriter::with_defaults(ctx.limits);
    enqueue_join_kit(&mut writer, ctx, position);
    flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await?;
    forward_serverbound(&mut decoder, &compression, ctx, player).await?;

    let mut read_buf = [0u8; READ_CHUNK];
    let result = loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break Ok(()),
            outbound = handle.recv() => match outbound {
                // Clientbound simulation output: queue and flush to the socket.
                Some(packet) => {
                    writer.enqueue_classified(packet);
                    if let Err(err) = flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await {
                        break Err(err);
                    }
                }
                // The router dropped the session.
                None => break Ok(()),
            },
            read = timeout(ctx.io_timeout, stream.read(&mut read_buf)) => {
                let outcome = match read {
                    Err(_) => break Err(anyhow::anyhow!("play socket read timed out")),
                    Ok(Err(err)) => break Err(err.into()),
                    Ok(Ok(0)) => break Ok(()),
                    Ok(Ok(n)) => decode_and_forward(&mut decoder, &compression, ctx, player, &read_buf[..n]).await,
                };
                if let Err(err) = outcome {
                    break Err(err);
                }
            },
        }
    };

    // Best-effort despawn notice regardless of how the link ended.
    let _ = ctx
        .commands
        .send(SimCommand::Event(NetEvent::disconnected(
            player,
            DisconnectReason::ServerShutdown,
        )))
        .await;
    result
}

/// Sends a join request to the driver and awaits the session handle.
async fn join_simulation(
    ctx: &ConnContext,
    player: PlayerId,
    position: Vec3,
) -> anyhow::Result<PlayerSessionHandle> {
    let (reply_tx, reply_rx) = oneshot::channel();
    ctx.commands
        .send(SimCommand::Join {
            player,
            position,
            reply: reply_tx,
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver dropped the join reply"))?
        .map_err(|err| anyhow::anyhow!("join rejected: {err}"))
}

/// Queues the join kit: `JoinGame`, the spawn position sync, then the chunks.
fn enqueue_join_kit(writer: &mut PlayWriter, ctx: &ConnContext, position: Vec3) {
    writer.enqueue_classified(ClientboundPlayPacket::JoinGame(
        ctx.join_kit.join_game().clone(),
    ));
    writer.enqueue_classified(ClientboundPlayPacket::SynchronizePlayerPosition(
        spawn_sync(position),
    ));
    for chunk in ctx.join_kit.chunks() {
        writer.enqueue_classified(ClientboundPlayPacket::ChunkDataAndLight(chunk.clone()));
    }
}

/// Builds the absolute spawn-position sync packet.
fn spawn_sync(position: Vec3) -> SynchronizePlayerPosition {
    // Teleport id 0; absolute position with zero deltas, orientation, and flags.
    SynchronizePlayerPosition::new(
        0, position.x, position.y, position.z, 0.0, 0.0, 0.0, 0.0, 0.0, 0,
    )
}

/// Pushes freshly read bytes through the decoder and forwards complete play
/// packets to the simulation.
async fn decode_and_forward(
    decoder: &mut InboundDecoder,
    compression: &CompressionState,
    ctx: &ConnContext,
    player: PlayerId,
    bytes: &[u8],
) -> anyhow::Result<()> {
    decoder.push(bytes)?;
    forward_serverbound(decoder, compression, ctx, player).await
}

/// Drains every buffered serverbound play frame and forwards it as a
/// [`NetEvent`].
async fn forward_serverbound(
    decoder: &mut InboundDecoder,
    compression: &CompressionState,
    ctx: &ConnContext,
    player: PlayerId,
) -> anyhow::Result<()> {
    while let Some(packet) = decoder.next_packet_compressed(ConnectionState::Play, compression)? {
        let InboundPacket::Play(body) = packet else {
            anyhow::bail!("non-play frame received in the play phase");
        };
        if let Some(event) = decode_play_event(player, &body) {
            ctx.commands
                .send(SimCommand::Event(event))
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
        }
    }
    Ok(())
}

/// Decodes a raw play-frame body into a typed serverbound event, or `None` if it
/// is not a packet the simulation models.
fn decode_play_event(player: PlayerId, body: &[u8]) -> Option<NetEvent> {
    let mut reader = BoundedReader::new(body);
    let id = reader.read_var_int().ok()?;
    let packet = ServerboundPlayPacket::decode(id, &mut reader).ok()?;
    Some(NetEvent::play(player, packet))
}

/// Drains the writer into back-to-back batches and writes each to the socket.
async fn flush_writer(
    writer: &mut PlayWriter,
    stream: &mut TcpStream,
    compression: &CompressionState,
    io_timeout: Duration,
) -> anyhow::Result<()> {
    loop {
        let batch = writer.drain_batch(compression)?;
        if batch.is_empty() {
            break;
        }
        write_all(stream, batch.bytes(), io_timeout).await?;
    }
    Ok(())
}

/// The result of a single socket read during login.
enum ReadOutcome {
    /// Bytes were read and appended to the decoder.
    Data,
    /// The peer half-closed the connection.
    Eof,
    /// A shutdown was signalled while waiting to read.
    Shutdown,
}

/// Reads one chunk of bytes into the decoder, honouring shutdown and the timeout.
async fn read_more(
    stream: &mut TcpStream,
    decoder: &mut InboundDecoder,
    read_buf: &mut [u8],
    io_timeout: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<ReadOutcome> {
    let read = tokio::select! {
        biased;
        _ = shutdown.changed() => return Ok(ReadOutcome::Shutdown),
        result = timeout(io_timeout, stream.read(read_buf)) => result,
    };
    let n = read.map_err(|_| anyhow::anyhow!("login socket read timed out"))??;
    if n == 0 {
        return Ok(ReadOutcome::Eof);
    }
    decoder.push(&read_buf[..n])?;
    Ok(ReadOutcome::Data)
}

/// Writes every byte of `bytes` and flushes, each bounded by `io_timeout`.
async fn write_all(
    stream: &mut TcpStream,
    bytes: &[u8],
    io_timeout: Duration,
) -> anyhow::Result<()> {
    timeout(io_timeout, stream.write_all(bytes))
        .await
        .map_err(|_| anyhow::anyhow!("socket write timed out"))??;
    timeout(io_timeout, stream.flush())
        .await
        .map_err(|_| anyhow::anyhow!("socket flush timed out"))??;
    Ok(())
}
