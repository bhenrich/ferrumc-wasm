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
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};

use ferrumc_codec::{BoundedReader, BoundedString};
use ferrumc_command::CommandSource;
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
    ClientboundKeepAlive, ClientboundPlayPacket, GameEvent, ServerboundPlayPacket, SetCenterChunk,
    SetDefaultSpawnPosition, SetPlayerPosition, SynchronizePlayerPosition,
};
use ferrumc_session::{net_event_to_input, NetEvent, PlayerSessionHandle};
use ferrumc_sim::GameInput;

use crate::command::SPAWN_COMMAND;
use crate::driver::SimCommand;
use crate::plugins::PlayPolicy;
use crate::registries::ConfigRegistries;
use crate::world::JoinKit;

/// The `next_state` value in a handshake that selects the login branch.
const NEXT_STATE_LOGIN: i32 = 2;

/// Bytes read off the socket per `read` call before decoding.
const READ_CHUNK: usize = 4096;

/// `Game Event` reason `13`: "level chunks load start". Sent right after
/// `JoinGame` to tell the client the spawn chunks are on their way; without it
/// the client never leaves the "Loading terrain" screen.
const GAME_EVENT_LEVEL_CHUNKS_LOAD_START: u8 = 13;

/// Teleport id carried by the join `SynchronizePlayerPosition`.
///
/// Must be non-zero: a real client replies with a `ConfirmTeleportation` echoing
/// it, which the server decodes and ignores.
const JOIN_TELEPORT_ID: i32 = 1;

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
    /// The Known Packs advertisement and the registry packets sent during the
    /// configuration phase.
    pub(crate) config: Arc<ConfigRegistries>,
    /// Interval between clientbound play-phase Keep Alive pings.
    pub(crate) keep_alive_interval: Duration,
    /// Bounded channel to the simulation/session driver.
    pub(crate) commands: mpsc::Sender<SimCommand>,
    /// The shared play policy: spawn-protection veto, bypass permissions, and the
    /// command tree consulted for serverbound play packets.
    pub(crate) policy: Arc<PlayPolicy>,
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
    /// Awaiting the client's Known Packs echo (Clientbound Known Packs already
    /// sent); the registry data is not sent until it arrives.
    AwaitingKnownPacks,
    /// Awaiting Ack Finish Configuration.
    AwaitingFinishAck,
}

impl LoginPhase {
    /// The connection state the decoder/encoder use for the next frame.
    fn connection_state(self) -> ConnectionState {
        match self {
            Self::Handshaking => ConnectionState::Handshaking,
            Self::Login | Self::AwaitingLoginAck => ConnectionState::Login,
            Self::AwaitingKnownPacks | Self::AwaitingFinishAck => ConnectionState::Configuration,
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

    /// Advertises the built-in `minecraft:core` data pack so the client will
    /// accept NBT-omitted registry entries. The server then waits for the
    /// client's Known Packs echo before sending any registry data.
    async fn send_known_packs(&mut self) -> anyhow::Result<()> {
        let packs = self.ctx.config.known_packs().to_vec();
        self.send(&OutboundPacket::Configuration(
            ClientboundConfigurationPacket::ClientboundKnownPacks(ClientboundKnownPacks::new(
                packs,
            )),
        ))
        .await
    }

    /// Sends every enumerated registry (NBT omitted) in id-assignment order, then
    /// Finish Configuration to hand the client off to play.
    async fn send_registries_and_finish(&mut self) -> anyhow::Result<()> {
        // Detach the shared context reference so the registry borrow does not tie
        // up the `&mut self` each `send` needs.
        let ctx = self.ctx;
        for registry in ctx.config.registries() {
            self.send(&OutboundPacket::Configuration(
                ClientboundConfigurationPacket::RegistryData(registry.clone()),
            ))
            .await?;
        }
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
            conn.send_known_packs().await?;
            *phase = LoginPhase::AwaitingKnownPacks;
            Ok(LoginProgress::Continue)
        }
        (LoginPhase::AwaitingKnownPacks, InboundPacket::Configuration(config)) => match config {
            // The client's Known Packs echo is the cue to send the registries.
            ServerboundConfigurationPacket::ServerboundKnownPacks(_) => {
                conn.send_registries_and_finish().await?;
                *phase = LoginPhase::AwaitingFinishAck;
                Ok(LoginProgress::Continue)
            }
            // Client Information (and anything else) is accepted without a reply.
            _ => Ok(LoginProgress::Continue),
        },
        (LoginPhase::AwaitingFinishAck, InboundPacket::Configuration(config)) => match config {
            ServerboundConfigurationPacket::AckFinishConfiguration(_) => Ok(LoginProgress::Play),
            // Late client settings / known packs are accepted but need no reply.
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
    send_join_kit(&mut writer, &mut stream, &compression, ctx, position).await?;
    pump_serverbound(
        &mut decoder,
        &compression,
        ctx,
        player,
        name.as_str(),
        &mut writer,
    )
    .await?;
    flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await?;

    // Keep Alive: a real client disconnects if it hears nothing for 20 s. Ping on
    // an interval; the client echoes with a serverbound Keep Alive the play pump
    // decodes and ignores. The first tick fires one interval in, not immediately.
    let mut keep_alive = interval_at(
        Instant::now() + ctx.keep_alive_interval,
        ctx.keep_alive_interval,
    );
    keep_alive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut keep_alive_id: i64 = 0;

    let mut read_buf = [0u8; READ_CHUNK];
    let result = loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break Ok(()),
            _ = keep_alive.tick() => {
                keep_alive_id = keep_alive_id.wrapping_add(1);
                writer.enqueue_classified(ClientboundPlayPacket::ClientboundKeepAlive(
                    ClientboundKeepAlive::new(keep_alive_id),
                ));
                if let Err(err) = flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await {
                    break Err(err);
                }
            }
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
                    Ok(Ok(n)) => read_and_pump(
                        &mut decoder,
                        &compression,
                        ctx,
                        player,
                        name.as_str(),
                        &mut writer,
                        &mut stream,
                        &read_buf[..n],
                    ).await,
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

/// Sends the join kit in the order a real client needs to leave the loading
/// screen: `JoinGame`, `GameEvent(13)`, `SetCenterChunk`, the spawn-area chunks,
/// `SetDefaultSpawnPosition`, then a non-zero `SynchronizePlayerPosition`.
///
/// The sequence is flushed in three stages because the [`PlayWriter`] drains by
/// priority (State before World): flushing the framing packets, then the chunks,
/// then the spawn/position sync guarantees the chunks land *between* `SetCenterChunk`
/// and the position sync rather than being reordered after them.
///
/// # Errors
///
/// Returns an error if any stage fails to encode or write to the socket.
async fn send_join_kit(
    writer: &mut PlayWriter,
    stream: &mut TcpStream,
    compression: &CompressionState,
    ctx: &ConnContext,
    position: Vec3,
) -> anyhow::Result<()> {
    let kit = &ctx.join_kit;

    // Stage 1: enter play and cue the client to expect spawn chunks.
    writer.enqueue_classified(ClientboundPlayPacket::JoinGame(kit.join_game().clone()));
    writer.enqueue_classified(ClientboundPlayPacket::GameEvent(GameEvent::new(
        GAME_EVENT_LEVEL_CHUNKS_LOAD_START,
        0.0,
    )));
    writer.enqueue_classified(ClientboundPlayPacket::SetCenterChunk(SetCenterChunk::new(
        kit.spawn_chunk().x(),
        kit.spawn_chunk().z(),
    )));
    flush_writer(writer, stream, compression, ctx.io_timeout).await?;

    // Stage 2: the spawn-area chunk column packets (includes the player's chunk).
    for chunk in kit.chunks() {
        writer.enqueue_classified(ClientboundPlayPacket::ChunkDataAndLight(chunk.clone()));
    }
    flush_writer(writer, stream, compression, ctx.io_timeout).await?;

    // Stage 3: fix the world spawn, then teleport the player into it.
    writer.enqueue_classified(ClientboundPlayPacket::SetDefaultSpawnPosition(
        SetDefaultSpawnPosition::new(kit.spawn_block(), 0.0),
    ));
    writer.enqueue_classified(ClientboundPlayPacket::SynchronizePlayerPosition(
        spawn_sync(JOIN_TELEPORT_ID, position),
    ));
    flush_writer(writer, stream, compression, ctx.io_timeout).await
}

/// Builds an absolute position sync with the given teleport id (zero deltas,
/// orientation, and flags).
fn spawn_sync(teleport_id: i32, position: Vec3) -> SynchronizePlayerPosition {
    SynchronizePlayerPosition::new(
        teleport_id,
        position.x,
        position.y,
        position.z,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        0,
    )
}

/// Pushes freshly read bytes through the decoder, handles every complete play
/// frame, then flushes any clientbound responses queued while handling them.
#[allow(clippy::too_many_arguments)] // one play step: framing + policy + I/O state
async fn read_and_pump(
    decoder: &mut InboundDecoder,
    compression: &CompressionState,
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    stream: &mut TcpStream,
    bytes: &[u8],
) -> anyhow::Result<()> {
    decoder.push(bytes)?;
    pump_serverbound(decoder, compression, ctx, player, name, writer).await?;
    flush_writer(writer, stream, compression, ctx.io_timeout).await
}

/// Drains every buffered serverbound play frame and handles each: a
/// `ChatCommand` runs through the command tree (queuing any clientbound response
/// into `writer`), a spawn-protected break/place is vetoed, and anything else is
/// forwarded to the simulation as a [`NetEvent`].
async fn pump_serverbound(
    decoder: &mut InboundDecoder,
    compression: &CompressionState,
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
) -> anyhow::Result<()> {
    while let Some(packet) = decoder.next_packet_compressed(ConnectionState::Play, compression)? {
        let InboundPacket::Play(body) = packet else {
            anyhow::bail!("non-play frame received in the play phase");
        };
        handle_play_body(ctx, player, name, writer, &body).await?;
    }
    Ok(())
}

/// Handles one decoded serverbound play-frame body.
///
/// Unknown or malformed play packets are ignored (the slice models only a
/// subset), as are the teleport confirmation and the Keep Alive echo. A
/// `ChatCommand` is dispatched locally; every other modelled packet is forwarded
/// to the simulation unless spawn protection vetoes it.
async fn handle_play_body(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    body: &[u8],
) -> anyhow::Result<()> {
    let mut reader = BoundedReader::new(body);
    let Ok(id) = reader.read_var_int() else {
        return Ok(());
    };
    let Ok(packet) = ServerboundPlayPacket::decode(id, &mut reader) else {
        return Ok(());
    };

    match &packet {
        ServerboundPlayPacket::ChatCommand(command) => {
            let command = command.command().as_str().to_owned();
            return handle_command(ctx, player, name, writer, &command).await;
        }
        // The teleport confirmation (reply to the join position sync) and the
        // Keep Alive echo are accepted and need no action: the slice does not
        // validate teleport ids and the keep-alive timer is fire-and-forget.
        ServerboundPlayPacket::ConfirmTeleportation(_)
        | ServerboundPlayPacket::ServerboundKeepAlive(_) => return Ok(()),
        _ => {}
    }

    let event = NetEvent::play(player, packet);
    if is_vetoed(ctx, player, &event) {
        // Spawn protection: drop the edit so the world is never modified and no
        // BlockUpdate is broadcast. The actor's optimistic client-side change is
        // left uncorrected this slice (no clientbound carrier to restore it).
        return Ok(());
    }
    ctx.commands
        .send(SimCommand::Event(event))
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))
}

/// Returns whether `event` is a break/place that spawn protection should veto:
/// it targets a protected column and the actor lacks the bypass permission.
fn is_vetoed(ctx: &ConnContext, player: PlayerId, event: &NetEvent) -> bool {
    let Some(GameInput::BlockBreak { position, .. } | GameInput::BlockPlace { position, .. }) =
        net_event_to_input(event)
    else {
        return false;
    };
    ctx.policy
        .guard()
        .vetoes(position, ctx.policy.permissions().has_bypass(player))
}

/// Dispatches a `/command` for `player` and applies its side effect.
///
/// Dispatch goes through the shared command tree with the player's permission
/// level and a node-string checker backed by the permission registry. On a
/// successful `/spawn`, the player is teleported: a `SynchronizePlayerPosition`
/// is queued to their socket and a move is sent to the simulation so the
/// authoritative position updates and viewers see the teleport. Other commands
/// (notably `/gamemode`) have no clientbound carrier in this slice, so a
/// successful dispatch produces no packet. A rejected command is silently
/// dropped — there is no system-chat packet to report it to the client.
async fn handle_command(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    command: &str,
) -> anyhow::Result<()> {
    let policy = &ctx.policy;
    let source = CommandSource::for_player(player, name, policy.permission_level());
    let allowed = |node: &str| policy.permissions().is_allowed(player, node);
    let Ok(result) = policy
        .command_tree()
        .dispatch_with(command, &source, &allowed)
    else {
        return Ok(());
    };
    if !result.is_success() {
        return Ok(());
    }

    if command.split_whitespace().next() == Some(SPAWN_COMMAND) {
        let spawn = policy.spawn();
        writer.enqueue_classified(ClientboundPlayPacket::SynchronizePlayerPosition(
            spawn_sync(JOIN_TELEPORT_ID, spawn),
        ));
        let move_event = NetEvent::play(
            player,
            ServerboundPlayPacket::SetPlayerPosition(SetPlayerPosition::new(
                spawn.x, spawn.y, spawn.z, 0,
            )),
        );
        ctx.commands
            .send(SimCommand::Event(move_event))
            .await
            .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
    }
    Ok(())
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
