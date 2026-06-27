//! Per-connection protocol driver: handshake -> login -> configuration -> play.
//!
//! A handshake with `next_state == 1` instead takes the status branch: the
//! server replies with a server-list status response and a ping/pong echo, then
//! closes — this is what makes the server visible (and "COMPATIBLE") in a real
//! client's multiplayer list.
//!
//! Each accepted socket runs one [`handle_connection`] task. It drives the login
//! handshake by hand over the `ferrumc-net` framing primitives (the crate's own
//! `LoginServer` ends at a keepalive shell and exposes no post-play hook, so the
//! app wires the play handoff itself). The instant the client acknowledges
//! configuration, the connection joins the simulation and replays the shared
//! [`JoinKit`](crate::world::JoinKit): `JoinGame`, a position sync, then the
//! spawn-area chunk packets. From there it pumps serverbound play packets into
//! the simulation and clientbound outputs back to the socket.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};

use ferrumc_codec::{BoundedReader, BoundedString};
use ferrumc_command::CommandSource;
use ferrumc_core::{PlayerId, TextColor, TextComponent};
use ferrumc_math::{BlockPos, ChunkPos, Vec3};
use ferrumc_net::{
    offline_uuid, CompressionState, ConnectionLimits, ConnectionState, DecodeError,
    DisconnectReason, EnqueueOutcome, FrameDecodeError, InboundDecoder, InboundPacket,
    OutboundEncoder, OutboundPacket, OutboundPriority, PlayWriter, StatusInfo,
};
use ferrumc_observability::{
    CounterRegistry, MutationKind, MutationResult, PacketState, ServerClock, SessionDebug,
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
    AcknowledgeBlockChange, ClientboundKeepAlive, ClientboundPlayPacket, GameEvent,
    ServerboundPlayPacket, SetCenterChunk, SetDefaultSpawnPosition, SetPlayerPosition,
    SynchronizePlayerPosition, UnloadChunk,
};
use ferrumc_proto::generated::status::{
    ClientboundStatusPacket, PongResponse, ServerboundStatusPacket, StatusResponse,
};
use ferrumc_session::{net_event_to_input, NetEvent, PlayerSessionHandle};
use ferrumc_sim::GameInput;

use crate::command::{parse_gamemode, GAMEMODE_COMMAND, SPAWN_COMMAND};
use crate::driver::SimCommand;
use crate::observe;
use crate::plugins::PlayPolicy;
use crate::registries::ConfigRegistries;
use crate::world::JoinKit;

/// The `next_state` value in a handshake that selects the status branch
/// (server-list ping).
const NEXT_STATE_STATUS: i32 = 1;

/// The `next_state` value in a handshake that selects the login branch.
const NEXT_STATE_LOGIN: i32 = 2;

/// Human-readable version label shown in the client's multiplayer list.
const STATUS_VERSION_NAME: &str = "FerrumC 1.21.8";

/// Wire protocol number advertised in the status response. A client reporting
/// the same number (772, Minecraft 1.21.8) renders the server as compatible.
const STATUS_PROTOCOL_VERSION: i32 = 772;

/// MOTD text rendered as the status `description`.
const STATUS_MOTD: &str = "FerrumC";

/// Upper bound on the status-response JSON, matching the wire `StatusResponse`
/// string cap (chat-component max, 32767 chars).
const STATUS_JSON_MAX_CHARS: usize = 32_767;

/// Bytes read off the socket per `read` call before decoding.
const READ_CHUNK: usize = 4096;

/// `Game Event` reason `13`: "level chunks load start". Sent right after
/// `JoinGame` to tell the client the spawn chunks are on their way; without it
/// the client never leaves the "Loading terrain" screen.
const GAME_EVENT_LEVEL_CHUNKS_LOAD_START: u8 = 13;

/// `Game Event` reason `3`: "change game mode". The event `value` is the game-mode
/// id as a float; sending it switches the issuing client's mode (the carrier
/// `/gamemode` uses, since there is no dedicated set-game-mode packet).
const GAME_EVENT_CHANGE_GAMEMODE: u8 = 3;

/// Teleport id carried by the join `SynchronizePlayerPosition`.
///
/// Must be non-zero: a real client replies with a `ConfirmTeleportation` echoing
/// it, which the server decodes and ignores.
const JOIN_TELEPORT_ID: i32 = 1;

/// Maximum number of `ChunkDataAndLight` packets a single streaming evaluation
/// will request and send for one player.
///
/// Each chunk packet carries the full section + light payload (tens of KiB), so
/// an uncapped burst — a teleport that makes the whole view square new, or the
/// initial gap between the small spawn batch and a large view distance — could
/// dump megabytes onto one socket at once. Capping the per-update load count
/// paces the stream: the leftover chunks are picked up on the next position
/// update (the desired-vs-loaded diff is recomputed every move), nearest-first,
/// so the player always gets the chunks closest to them first. Unloads are tiny
/// (8 bytes) and are not capped. `16` keeps a normal single-chunk step fully
/// served in one update while bounding a teleport flood.
const MAX_CHUNK_LOADS_PER_UPDATE: usize = 16;

/// Upper bound applied to the configured view distance when streaming chunks.
///
/// The streamed view is a `(2 * r + 1)` square, so the per-player loaded-chunk
/// set is bounded by `(2 * r + 1)^2`. Clamping `r` here (to the vanilla view-
/// distance ceiling) keeps that set — and the work a single boundary crossing can
/// request — bounded even if the config carries an absurd view distance.
const STREAM_VIEW_DISTANCE_MAX: i32 = 32;

/// Per-connection chunk-streaming state: which chunk the client is centred on and
/// which chunk columns it currently holds.
///
/// This is connection/session bookkeeping, not world state: it records what has
/// been *sent to this client* so the stream never re-sends a chunk and knows what
/// to unload. The authoritative chunk data still lives in the simulation shard;
/// the connection only ever asks the driver (via [`SimCommand::StreamChunks`]) to
/// load-or-generate and never touches the chunk map itself.
struct ChunkStream {
    /// The chunk the client is currently centred on (the last `Set Center Chunk`).
    center: ChunkPos,
    /// The square radius, in chunks, of the streamed view (already clamped).
    view_distance: i32,
    /// The chunk columns the client currently holds (spawn batch + streamed in),
    /// bounded by `(2 * view_distance + 1)^2`.
    loaded: BTreeSet<ChunkPos>,
    /// The latest position reported by the client since the last evaluation, or
    /// `None` if nothing new — coalescing many move packets in one read into a
    /// single streaming pass.
    pending_position: Option<Vec3>,
}

impl ChunkStream {
    /// Seeds the stream from the join kit: centred on the spawn chunk and already
    /// holding the spawn-batch columns the client received at join.
    fn new(ctx: &ConnContext) -> Self {
        Self {
            center: ctx.join_kit.spawn_chunk(),
            view_distance: ctx.view_distance.clamp(0, STREAM_VIEW_DISTANCE_MAX),
            loaded: ctx.join_kit.chunk_positions().collect(),
            pending_position: None,
        }
    }

    /// Records the client's latest reported position for the next evaluation.
    fn observe(&mut self, position: Vec3) {
        self.pending_position = Some(position);
    }
}

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
    /// The prebuilt server-list status response, rendered once at startup and
    /// replayed for every status (`next_state == 1`) handshake. Behind an [`Arc`]
    /// so cloning the context per connection is a pointer bump, not a re-render.
    pub(crate) status_response: Arc<OutboundPacket>,
    /// The configured play view distance, in chunks: the square radius of chunks
    /// streamed around a player as they move (clamped to a sane maximum per
    /// connection, see [`STREAM_VIEW_DISTANCE_MAX`]).
    pub(crate) view_distance: i32,
    /// The shared metric registry every connection task feeds (chunk sends,
    /// decode errors, vetoed mutations, outbound queue depth).
    pub(crate) metrics: Arc<CounterRegistry>,
    /// The shared server clock (driver-written, connection-read) used to stamp
    /// packet traces with the current simulation tick.
    pub(crate) clock: ServerClock,
}

impl ConnContext {
    /// The active compression threshold (`>= 0`), or `None` when disabled.
    fn enabled_threshold(&self) -> Option<i32> {
        self.compression_threshold
            .filter(|threshold| *threshold >= 0)
    }
}

/// Builds the server-list status response advertised to clients that handshake
/// with `next_state == 1`.
///
/// The `{version, players, description}` JSON is rendered (and escaped) by
/// `ferrumc-net`'s [`StatusInfo`] so the shape matches the crate's own status
/// server: version `{"name": "FerrumC 1.21.8", "protocol": 772}` (772 marks the
/// server COMPATIBLE for a 1.21.8 client), `players` advertising `max_players`
/// with none online and an empty sample, and the MOTD as the description text.
///
/// Built once at startup and shared behind an [`Arc`]; each status request only
/// re-sends it.
///
/// # Errors
///
/// Returns an error if the rendered JSON exceeds the wire string bound — only
/// possible with an absurdly long MOTD or version label, neither of which the
/// server sets.
pub(crate) fn build_status_response(max_players: u32) -> anyhow::Result<OutboundPacket> {
    let info = StatusInfo::new(
        STATUS_VERSION_NAME,
        STATUS_PROTOCOL_VERSION,
        max_players,
        0,
        STATUS_MOTD,
    );
    let json = BoundedString::<STATUS_JSON_MAX_CHARS>::new(info.to_json())
        .map_err(|err| anyhow::anyhow!("status response JSON exceeds the wire bound: {err}"))?;
    Ok(OutboundPacket::Status(
        ClientboundStatusPacket::StatusResponse(StatusResponse::new(json)),
    ))
}

/// The login-handshake phase, one step finer than [`ConnectionState`] so an
/// out-of-order-but-valid packet can be rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginPhase {
    /// Awaiting the initial handshake.
    Handshaking,
    /// Status branch (`next_state == 1`): awaiting the Status Request and Ping
    /// Request that drive the server-list exchange.
    Status,
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
            Self::Status => ConnectionState::Status,
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
    /// Per-connection rolling packet-trace holder, dumped on disconnect or a
    /// decode error. Boxed so the large trace rings live on the heap rather than
    /// bloating the connection task's stack/async frame.
    debug: Box<SessionDebug>,
}

impl<'a> Connection<'a> {
    /// Builds the framing state for a freshly accepted `stream`.
    fn new(stream: TcpStream, ctx: &'a ConnContext) -> Self {
        // Label the session by peer address until login upgrades it to the name.
        let label = stream
            .peer_addr()
            .map_or_else(|_| "unknown".to_string(), |addr| addr.to_string());
        Self {
            stream,
            decoder: InboundDecoder::new(ctx.limits),
            encoder: OutboundEncoder::new(ctx.limits),
            compression: CompressionState::disabled(),
            ctx,
            debug: Box::new(SessionDebug::new(label)),
        }
    }

    /// Encodes and writes one clientbound packet, bounded by the I/O timeout.
    async fn send(&mut self, packet: &OutboundPacket) -> anyhow::Result<()> {
        let mut buf = BytesMut::new();
        self.encoder
            .encode_compressed(packet, &mut buf, &self.compression)?;
        // Record an outbound trace with the exact on-wire frame size. This covers
        // the login / status / configuration phases; play frames are traced where
        // they are enqueued into the `PlayWriter`.
        let trace = observe::trace_outbound(packet, buf.len(), &self.compression, &self.ctx.clock);
        self.debug.record_outbound(trace);
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
        match conn
            .decoder
            .next_packet_compressed(state, &conn.compression)
        {
            Ok(Some(packet)) => {
                // Record the inbound trace before dispatch. The login-phase
                // decoder does not surface the body length, so size is `0`
                // (documented); play-phase inbound records an exact size.
                let trace =
                    observe::trace_inbound_login(&packet, &conn.compression, &conn.ctx.clock);
                conn.debug.record_inbound(trace);
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
            // Buffer holds no complete frame yet: fall through and read more.
            Ok(None) => {}
            // A well-framed frame carrying an unmodelled packet id in login or
            // configuration is ignored, not fatal: the vanilla client sends
            // several configuration packets the slice does not react to (cookie
            // response 0x01, plugin message / brand 0x02, keep alive 0x04, pong
            // 0x05, resource pack response 0x06), and the spec requires a server
            // to skip unhandled configuration packets rather than disconnect. The
            // frame is length-delimited but `next_packet_compressed` leaves it
            // buffered on error, so it is explicitly skipped before continuing.
            // Only an unknown id is tolerated; a malformed *known* packet still
            // follows the disconnect policy below.
            Err(err) if is_ignorable_unknown_packet(state, &err) => {
                tracing::debug!(?state, %err, "ignoring unmodelled serverbound packet");
                conn.decoder.skip_frame(state)?;
                continue;
            }
            Err(err) => {
                // A fatal login/status/config decode error: count it and dump the
                // retained traces before propagating (acceptance: decode-error).
                conn.ctx.metrics.record_packet_decode_error(
                    observe::state_of(state),
                    observe::decode_error_label(&err),
                );
                conn.debug.dump("login_decode_error");
                return Err(err.into());
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

/// Whether `err` is a well-framed frame carrying an unknown (unmodelled) packet
/// id in a state where the server should ignore it rather than disconnect.
///
/// Tolerance is scoped to the login and configuration states: the vanilla client
/// legitimately sends configuration packets the slice does not model (e.g. the
/// `minecraft:brand` plugin message), and the protocol requires the server to
/// skip them. Handshaking and status remain strict — an unknown id in those
/// single-exchange states is a genuine protocol violation. A *malformed* known
/// packet ([`DecodeError::MalformedBody`] and friends) is never ignored here.
fn is_ignorable_unknown_packet(state: ConnectionState, err: &FrameDecodeError) -> bool {
    matches!(
        state,
        ConnectionState::Login | ConnectionState::Configuration
    ) && matches!(
        err,
        FrameDecodeError::Decode(DecodeError::UnknownPacket { .. })
    )
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
            match handshake.next_state() {
                NEXT_STATE_LOGIN => {
                    *phase = LoginPhase::Login;
                    Ok(LoginProgress::Continue)
                }
                NEXT_STATE_STATUS => {
                    *phase = LoginPhase::Status;
                    Ok(LoginProgress::Continue)
                }
                // Transfer (3) or anything else is out of scope: close cleanly.
                _ => Ok(LoginProgress::Close),
            }
        }
        (LoginPhase::Status, InboundPacket::Status(ServerboundStatusPacket::StatusRequest(_))) => {
            // Copy the shared context reference out so the prebuilt response borrow
            // does not collide with the `&mut self` `send` needs.
            let ctx = conn.ctx;
            conn.send(&ctx.status_response).await?;
            Ok(LoginProgress::Continue)
        }
        (LoginPhase::Status, InboundPacket::Status(ServerboundStatusPacket::PingRequest(req))) => {
            // Echo the client's payload, then close: the status exchange ends here.
            let reply = OutboundPacket::Status(ClientboundStatusPacket::PongResponse(
                PongResponse::new(req.payload()),
            ));
            conn.send(&reply).await?;
            Ok(LoginProgress::Close)
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
#[allow(clippy::too_many_lines)] // one cohesive lifecycle: join, replay, pump, dump
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
        mut debug,
        ..
    } = conn;
    // Upgrade the session label from the peer address to the player name now that
    // login has completed.
    debug.set_session(name.as_str());
    let player = PlayerId::offline(name.as_str());
    let position = ctx.join_kit.spawn_position();
    let mut handle = join_simulation(ctx, player, position).await?;

    // The client already holds the spawn batch after the join kit; stream tracks
    // it from there so it never re-sends a spawn chunk and knows what to unload.
    let mut chunk_stream = ChunkStream::new(ctx);

    // Replay the keystone payload, then drain any already-buffered play frames.
    let mut writer = PlayWriter::with_defaults(ctx.limits);
    send_join_kit(
        &mut writer,
        &mut stream,
        &compression,
        ctx,
        &mut debug,
        position,
    )
    .await?;
    pump_serverbound(
        &mut decoder,
        &compression,
        ctx,
        player,
        name.as_str(),
        &mut writer,
        &mut chunk_stream,
        &mut debug,
    )
    .await?;
    flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await?;
    observe_queue_len(&mut debug, ctx, &writer);

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
                let packet = ClientboundPlayPacket::ClientboundKeepAlive(
                    ClientboundKeepAlive::new(keep_alive_id),
                );
                enqueue_traced_classified(&mut writer, &mut debug, &compression, &ctx.clock, packet);
                if let Err(err) = flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await {
                    break Err(err);
                }
                observe_queue_len(&mut debug, ctx, &writer);
            }
            outbound = handle.recv() => match outbound {
                // Clientbound simulation output: queue and flush to the socket.
                Some(packet) => {
                    enqueue_traced_classified(&mut writer, &mut debug, &compression, &ctx.clock, packet);
                    if let Err(err) = flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await {
                        break Err(err);
                    }
                    observe_queue_len(&mut debug, ctx, &writer);
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
                        &mut chunk_stream,
                        &mut debug,
                        &read_buf[..n],
                    ).await,
                };
                if let Err(err) = outcome {
                    break Err(err);
                }
            },
        }
    };

    // The play link has ended (clean close, EOF, shutdown, or error). Sample the
    // final outbound queue depth and dump the retained traces (acceptance:
    // disconnect).
    observe_queue_len(&mut debug, ctx, &writer);
    debug.dump("disconnect");

    // Release every chunk this connection had the client holding so its player
    // tickets stop pinning chunks resident after it leaves. Best-effort: a gone
    // driver just means the whole simulation is winding down anyway.
    if !chunk_stream.loaded.is_empty() {
        let positions = chunk_stream.loaded.iter().copied().collect();
        let _ = ctx
            .commands
            .send(SimCommand::ReleaseChunks { positions })
            .await;
    }

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
/// screen: `JoinGame`, `GameEvent(13)`, `SetCenterChunk`, `SetDefaultSpawnPosition`,
/// a non-zero `SynchronizePlayerPosition`, then the spawn-area chunks.
///
/// The position sync goes out *before* the chunks so the client's spawn point is
/// fixed first: the loading-screen gate releases on the chunk that contains the
/// player's position, and sending the sync first guarantees that chunk is among
/// the spawn-area column packets that follow, regardless of where spawn lands.
///
/// The sequence is flushed in two stages because the [`PlayWriter`] drains by
/// priority (State before World): flushing the framing-and-position packets, then
/// the chunks, guarantees the position sync lands ahead of the chunk column
/// rather than being reordered after it.
///
/// # Errors
///
/// Returns an error if any stage fails to encode or write to the socket.
async fn send_join_kit(
    writer: &mut PlayWriter,
    stream: &mut TcpStream,
    compression: &CompressionState,
    ctx: &ConnContext,
    debug: &mut SessionDebug,
    position: Vec3,
) -> anyhow::Result<()> {
    let kit = &ctx.join_kit;
    let clock = &ctx.clock;

    // Stage 1: enter play, cue the client to expect spawn chunks, fix the world
    // spawn, and teleport the player in — all before any chunk is sent.
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::JoinGame(kit.join_game().clone()),
    );
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::GameEvent(GameEvent::new(GAME_EVENT_LEVEL_CHUNKS_LOAD_START, 0.0)),
    );
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::SetCenterChunk(SetCenterChunk::new(
            kit.spawn_chunk().x(),
            kit.spawn_chunk().z(),
        )),
    );
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::SetDefaultSpawnPosition(SetDefaultSpawnPosition::new(
            kit.spawn_block(),
            0.0,
        )),
    );
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::SynchronizePlayerPosition(spawn_sync(JOIN_TELEPORT_ID, position)),
    );
    flush_writer(writer, stream, compression, ctx.io_timeout).await?;

    // Stage 2: the spawn-area chunk column packets (includes the player's chunk).
    for chunk in kit.chunks() {
        let outcome = enqueue_traced_classified(
            writer,
            debug,
            compression,
            clock,
            ClientboundPlayPacket::ChunkDataAndLight(chunk.clone()),
        );
        // Count only chunks that actually entered the queue; a tail-dropped chunk
        // never reaches the wire and must not inflate the counter.
        if outcome.is_enqueued() {
            ctx.metrics.incr_chunk_sent(1);
        }
    }
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
#[allow(clippy::too_many_arguments)] // one play step: framing + policy + I/O + trace state
async fn read_and_pump(
    decoder: &mut InboundDecoder,
    compression: &CompressionState,
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    stream: &mut TcpStream,
    chunk_stream: &mut ChunkStream,
    debug: &mut SessionDebug,
    bytes: &[u8],
) -> anyhow::Result<()> {
    decoder.push(bytes)?;
    pump_serverbound(
        decoder,
        compression,
        ctx,
        player,
        name,
        writer,
        chunk_stream,
        debug,
    )
    .await?;
    flush_writer(writer, stream, compression, ctx.io_timeout).await
}

/// Drains every buffered serverbound play frame and handles each: a
/// `ChatCommand` runs through the command tree (queuing any clientbound response
/// into `writer`), a spawn-protected break/place is vetoed, and anything else is
/// forwarded to the simulation as a [`NetEvent`].
///
/// After the whole buffered batch is drained, a single chunk-streaming pass runs
/// against the latest position the batch reported, so many coalesced move packets
/// trigger at most one streaming evaluation per read.
#[allow(clippy::too_many_arguments)] // one play drain: framing + policy + trace state
async fn pump_serverbound(
    decoder: &mut InboundDecoder,
    compression: &CompressionState,
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    chunk_stream: &mut ChunkStream,
    debug: &mut SessionDebug,
) -> anyhow::Result<()> {
    loop {
        let next = match decoder.next_packet_compressed(ConnectionState::Play, compression) {
            Ok(next) => next,
            Err(err) => {
                // A frame/compression-level decode error during play: count it and
                // dump the retained traces before propagating (acceptance:
                // decode-error).
                ctx.metrics.record_packet_decode_error(
                    PacketState::Play,
                    observe::decode_error_label(&err),
                );
                debug.dump("play_decode_error");
                return Err(err.into());
            }
        };
        let Some(packet) = next else {
            break;
        };
        let InboundPacket::Play(body) = packet else {
            anyhow::bail!("non-play frame received in the play phase");
        };
        handle_play_body(
            ctx,
            player,
            name,
            writer,
            chunk_stream,
            &body,
            debug,
            compression,
        )
        .await?;
    }
    apply_chunk_stream(ctx, writer, chunk_stream, debug, compression).await
}

/// Handles one decoded serverbound play-frame body.
///
/// Unknown or malformed play packets are ignored (the slice models only a
/// subset), as are the teleport confirmation and the Keep Alive echo. A
/// `ChatCommand` is dispatched locally; every other modelled packet is forwarded
/// to the simulation unless spawn protection vetoes it. A position packet is also
/// recorded on the chunk stream so the post-drain pass can react to a boundary
/// crossing.
#[allow(clippy::too_many_arguments)] // one play step: framing + policy + trace state
async fn handle_play_body(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    chunk_stream: &mut ChunkStream,
    body: &[u8],
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let mut reader = BoundedReader::new(body);
    let Ok(id) = reader.read_var_int() else {
        // A play frame whose body has no readable packet id is malformed.
        ctx.metrics
            .record_packet_decode_error(PacketState::Play, "bad_packet_id");
        debug.dump("play_packet_decode_error");
        return Ok(());
    };
    let packet = match ServerboundPlayPacket::decode(id, &mut reader) {
        Ok(packet) => packet,
        Err(err) => {
            // An unknown id is an expected unmodelled packet (counted, not dumped);
            // a malformed body is a genuine decode error (counted and dumped).
            let (label, dump) = observe::play_decode_error(&err);
            ctx.metrics
                .record_packet_decode_error(PacketState::Play, label);
            if dump {
                debug.dump("play_packet_decode_error");
            }
            return Ok(());
        }
    };

    // Record the inbound play trace with the exact frame-body size.
    debug.record_inbound(observe::trace_inbound_play(
        &packet,
        body.len(),
        compression,
        &ctx.clock,
    ));

    // Track the client's reported position for chunk streaming. The packet is
    // still forwarded to the simulation below — this only mirrors the position the
    // stream centres on; the simulation stays authoritative.
    if let Some(position) = reported_position(&packet) {
        chunk_stream.observe(position);
    }

    match &packet {
        ServerboundPlayPacket::ChatCommand(command) => {
            let command = command.command().as_str().to_owned();
            return handle_command(ctx, player, name, writer, &command, debug, compression).await;
        }
        ServerboundPlayPacket::ChatMessage(chat) => {
            // Relay unsigned player chat to everyone as a system message: format it
            // "<name> message" and hand it to the driver, the only owner of every
            // player's outbound channel. enforces_secure_chat = false, so no 1.19
            // signing apparatus is needed (the signature tail was decoded into the
            // ignored `rest` field). The relay reaches the sender via its own
            // session outbound channel, so it is NOT also enqueued on the writer.
            //
            // Strip legacy section-sign (§, U+00A7) codes from the untrusted
            // message first: a client still interprets `§k`/`§l`/§<colour> inside
            // a component's `text`, so leaving them in would let a player inject
            // colour/obfuscation formatting into the relayed line. The name is
            // not user-controlled (usernames are `[A-Za-z0-9_]`), so only the
            // message body needs sanitising.
            let sanitized = chat.message().as_str().replace('\u{00A7}', "");
            let line = format!("<{name}> {sanitized}");
            let content = TextComponent::text(line);
            return ctx
                .commands
                .send(SimCommand::BroadcastSystemChat {
                    content,
                    overlay: false,
                })
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"));
        }
        // The teleport confirmation (reply to the join position sync) and the
        // Keep Alive echo are accepted and need no action: the slice does not
        // validate teleport ids and the keep-alive timer is fire-and-forget.
        ServerboundPlayPacket::ConfirmTeleportation(_)
        | ServerboundPlayPacket::ServerboundKeepAlive(_) => return Ok(()),
        _ => {}
    }

    let event = NetEvent::play(player, packet);
    if let Some((kind, sequence)) = veto_kind(ctx, player, &event) {
        // Spawn protection: drop the edit so the world is never modified and no
        // BlockUpdate is broadcast. Count the rejected mutation, then heal the
        // actor's optimistic client-side prediction: an `AcknowledgeBlockChange`
        // for the edit's sequence ends the client's pending prediction
        // (endPredictionsUpTo) and reverts the ghost block. The veto changed
        // nothing, so the client's pre-prediction state is already authoritative —
        // the ack alone heals it, and the net layer has no world access to read a
        // `BlockUpdate` anyway. Without the ack a real 1.21.8 client would keep the
        // ghost block until some later sequence happened to be acked.
        ctx.metrics
            .record_block_mutation(kind, MutationResult::Rejected);
        enqueue_traced_classified(
            writer,
            debug,
            compression,
            &ctx.clock,
            ClientboundPlayPacket::AcknowledgeBlockChange(AcknowledgeBlockChange::new(sequence)),
        );
        return Ok(());
    }
    ctx.commands
        .send(SimCommand::Event(event))
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))
}

/// The absolute position a serverbound play packet reports, if any.
///
/// Both absolute-move packets carry a position; the rotation-only and other
/// packets do not move the player and so report nothing.
fn reported_position(packet: &ServerboundPlayPacket) -> Option<Vec3> {
    match packet {
        ServerboundPlayPacket::SetPlayerPosition(p) => Some(Vec3::new(p.x(), p.y(), p.z())),
        ServerboundPlayPacket::SetPlayerPositionAndRotation(p) => {
            Some(Vec3::new(p.x(), p.y(), p.z()))
        }
        _ => None,
    }
}

/// Streams chunks to follow the client's latest reported position.
///
/// Does nothing until the client has reported a new position. On a chunk-boundary
/// crossing it sends `Set Center Chunk`; either way it diffs the square of chunks
/// within view distance against the per-player loaded set, sends `Unload Chunk`
/// for any column that left the radius, and asks the driver (via
/// [`SimCommand::StreamChunks`]) to load-or-generate the columns newly in range —
/// nearest-first and capped at [`MAX_CHUNK_LOADS_PER_UPDATE`] per call, with the
/// remainder caught on a later position update. A chunk already in the loaded set
/// is never re-requested or re-sent.
///
/// The connection never generates chunks itself: it only decides the desired set
/// and renders the packets the driver returns.
async fn apply_chunk_stream(
    ctx: &ConnContext,
    writer: &mut PlayWriter,
    chunk_stream: &mut ChunkStream,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let Some(position) = chunk_stream.pending_position.take() else {
        return Ok(());
    };
    let clock = &ctx.clock;

    let new_center = chunk_of(position);
    if new_center != chunk_stream.center {
        chunk_stream.center = new_center;
        enqueue_traced_classified(
            writer,
            debug,
            compression,
            clock,
            ClientboundPlayPacket::SetCenterChunk(SetCenterChunk::new(
                new_center.x(),
                new_center.z(),
            )),
        );
    }

    let desired = desired_chunks(new_center, chunk_stream.view_distance);
    let to_unload: Vec<ChunkPos> = chunk_stream.loaded.difference(&desired).copied().collect();
    let mut to_load: Vec<ChunkPos> = desired.difference(&chunk_stream.loaded).copied().collect();
    if to_load.is_empty() && to_unload.is_empty() {
        return Ok(());
    }

    // Nearest-first so the player always receives the chunks closest to them when
    // the per-update cap defers the rest; the coordinate tiebreak keeps it
    // deterministic.
    to_load.sort_by_key(|pos| (chebyshev_distance(new_center, *pos), pos.x(), pos.z()));
    to_load.truncate(MAX_CHUNK_LOADS_PER_UPDATE);

    // Drop the departed columns from the client now (tiny packets) and from the
    // tracked set; the driver releases their tickets via the command below.
    for pos in &to_unload {
        let outcome = enqueue_traced(
            writer,
            debug,
            compression,
            clock,
            OutboundPriority::World,
            ClientboundPlayPacket::UnloadChunk(UnloadChunk::new(pos.z(), pos.x())),
        );
        // Count only unloads that actually reach the queue. The column always
        // leaves the tracked set, though: the driver releases its ticket via the
        // `StreamChunks` command below regardless of whether the packet queued.
        if outcome.is_enqueued() {
            ctx.metrics.incr_chunk_unloaded(1);
        }
        chunk_stream.loaded.remove(pos);
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    ctx.commands
        .send(SimCommand::StreamChunks {
            load: to_load,
            unload: to_unload,
            reply: reply_tx,
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
    let packets = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver dropped the chunk-stream reply"))?;

    // Only the chunks the driver actually built come back; record exactly those so
    // a skipped chunk is retried on a later update rather than treated as sent.
    for packet in packets {
        let pos = ChunkPos::new(packet.x(), packet.z());
        // Track the column unconditionally: the driver already took a player ticket
        // for it, and `loaded` is what the disconnect/unload path uses to release
        // that ticket. Gating this on the enqueue would leak the ticket on a
        // tail-drop. Only the send *counter* is gated on a real enqueue.
        chunk_stream.loaded.insert(pos);
        let outcome = enqueue_traced(
            writer,
            debug,
            compression,
            clock,
            OutboundPriority::World,
            ClientboundPlayPacket::ChunkDataAndLight(packet),
        );
        if outcome.is_enqueued() {
            ctx.metrics.incr_chunk_sent(1);
        }
    }
    Ok(())
}

/// The chunk column the world `position` falls in (flooring so negatives land on
/// the correct column).
fn chunk_of(position: Vec3) -> ChunkPos {
    BlockPos::new(
        position.x.floor() as i32,
        position.y.floor() as i32,
        position.z.floor() as i32,
    )
    .to_chunk_pos()
}

/// The `(2 * radius + 1)` square of chunk columns centred on `center`.
///
/// Coordinates are added saturating so a centre at the edge of the coordinate
/// range can never overflow; any realistic position is exact.
fn desired_chunks(center: ChunkPos, radius: i32) -> BTreeSet<ChunkPos> {
    let mut set = BTreeSet::new();
    for dz in -radius..=radius {
        for dx in -radius..=radius {
            set.insert(ChunkPos::new(
                center.x().saturating_add(dx),
                center.z().saturating_add(dz),
            ));
        }
    }
    set
}

/// The Chebyshev (square/king-move) chunk distance between `a` and `b`.
///
/// Computed in `i64` so the difference cannot overflow at the coordinate
/// extremes.
fn chebyshev_distance(a: ChunkPos, b: ChunkPos) -> i64 {
    let dx = (i64::from(a.x()) - i64::from(b.x())).abs();
    let dz = (i64::from(a.z()) - i64::from(b.z())).abs();
    dx.max(dz)
}

/// Returns the mutation kind and block-action sequence if `event` is a
/// break/place that spawn protection should veto (it targets a protected column
/// and the actor lacks the bypass permission), or `None` if it is not a vetoed
/// edit.
///
/// Returning the [`MutationKind`] lets the caller record the rejected mutation
/// against `ferrumc_block_mutation_total{kind,result=rejected}`; the `sequence`
/// lets it acknowledge the vetoed action so the client's prediction heals.
fn veto_kind(ctx: &ConnContext, player: PlayerId, event: &NetEvent) -> Option<(MutationKind, i32)> {
    let (position, sequence, kind) = match net_event_to_input(event) {
        Some(GameInput::BlockBreak {
            position, sequence, ..
        }) => (position, sequence, MutationKind::Break),
        Some(GameInput::BlockPlace {
            position, sequence, ..
        }) => (position, sequence, MutationKind::Place),
        _ => return None,
    };
    if ctx
        .policy
        .guard()
        .vetoes(position, ctx.policy.permissions().has_bypass(player))
    {
        Some((kind, sequence))
    } else {
        None
    }
}

/// Dispatches a `/command` for `player`, reports the outcome to them, and applies
/// its side effect.
///
/// Dispatch goes through the shared command tree with the player's per-player
/// permission level and a node-string checker backed by the permission registry.
/// Every outcome is now reported to the issuer as a `SystemChat` on their writer:
/// a dispatch failure (unknown command, bad argument, permission denied) becomes a
/// red error line and the command stops there, while a handler that ran reports
/// its [`CommandResult`](ferrumc_command::CommandResult) feedback (for success or
/// logical failure).
///
/// On a successful `/spawn` the player is also teleported: a
/// `SynchronizePlayerPosition` is queued to their socket and a move is sent to the
/// simulation so the authoritative position updates and viewers see the teleport.
/// On a successful `/gamemode <id>` a `GameEvent` with reason `3`
/// (`change_game_mode`) carrying the mode id is queued so the client actually
/// switches mode.
async fn handle_command(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    command: &str,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let policy = &ctx.policy;
    let source = CommandSource::for_player(player, name, policy.permission_level(player));
    let allowed = |node: &str| policy.permissions().is_allowed(player, node);
    let result = match policy
        .command_tree()
        .dispatch_with(command, &source, &allowed)
    {
        Ok(result) => result,
        Err(err) => {
            // The handler never ran (unknown command / bad argument / permission
            // denied): report why to the issuer as a red system-chat line.
            let message = TextComponent::text(err.to_string()).with_color(TextColor::Red);
            enqueue_traced_classified(
                writer,
                debug,
                compression,
                &ctx.clock,
                ferrumc_session::system_chat(&message, false),
            );
            return Ok(());
        }
    };

    // The handler ran: show its feedback to the issuer (covers both a success and a
    // `CommandResult::failure`).
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        &ctx.clock,
        ferrumc_session::system_chat(result.feedback(), false),
    );
    if !result.is_success() {
        return Ok(());
    }

    let first_token = command.split_whitespace().next();
    if first_token == Some(SPAWN_COMMAND) {
        let spawn = policy.spawn();
        enqueue_traced_classified(
            writer,
            debug,
            compression,
            &ctx.clock,
            ClientboundPlayPacket::SynchronizePlayerPosition(spawn_sync(JOIN_TELEPORT_ID, spawn)),
        );
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
    } else if first_token == Some(GAMEMODE_COMMAND) {
        // Make the mode change observable: a GameEvent (reason 3 = change_game_mode)
        // carrying the mode id switches the client's mode. The argument is parsed
        // the same way the handler validated it so the two always agree.
        if let Some(mode) = parse_gamemode(command) {
            enqueue_traced_classified(
                writer,
                debug,
                compression,
                &ctx.clock,
                ClientboundPlayPacket::GameEvent(GameEvent::new(
                    GAME_EVENT_CHANGE_GAMEMODE,
                    f32::from(mode.as_id()),
                )),
            );
        }
    }
    Ok(())
}

/// Enqueues `packet` at its default priority, recording an outbound trace only
/// when it is actually queued.
///
/// Returns the [`EnqueueOutcome`] so the caller can gate per-packet counters
/// (e.g. `ferrumc_chunk_sent_total`) on a real enqueue. A tail-dropped packet
/// (queue at capacity) is neither traced nor counted: the disconnect dump and the
/// send counters then reflect what entered the outbound pipeline rather than
/// intent, so backpressure cannot inflate them.
fn enqueue_traced_classified(
    writer: &mut PlayWriter,
    debug: &mut SessionDebug,
    compression: &CompressionState,
    clock: &ServerClock,
    packet: ClientboundPlayPacket,
) -> EnqueueOutcome {
    // Build the trace before the packet is moved into the queue; recording it is
    // deferred until the enqueue is known to have succeeded.
    let trace = observe::trace_outbound_play(&packet, compression, clock);
    let outcome = writer.enqueue_classified(packet);
    if outcome.is_enqueued() {
        debug.record_outbound(trace);
    }
    outcome
}

/// Enqueues `packet` at an explicit priority, recording an outbound trace only
/// when it is actually queued (see [`enqueue_traced_classified`] for the
/// drop-vs-trace policy).
fn enqueue_traced(
    writer: &mut PlayWriter,
    debug: &mut SessionDebug,
    compression: &CompressionState,
    clock: &ServerClock,
    priority: OutboundPriority,
    packet: ClientboundPlayPacket,
) -> EnqueueOutcome {
    let trace = observe::trace_outbound_play(&packet, compression, clock);
    let outcome = writer.enqueue(priority, packet);
    if outcome.is_enqueued() {
        debug.record_outbound(trace);
    }
    outcome
}

/// Samples the writer's outbound queue depth into both the per-session dump and
/// the `ferrumc_session_outbound_queue_len{session}` aggregate gauge.
fn observe_queue_len(debug: &mut SessionDebug, ctx: &ConnContext, writer: &PlayWriter) {
    let depth = writer.total_queued();
    debug.observe_outbound_queue_len(depth);
    ctx.metrics.observe_outbound_queue_len(depth);
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
