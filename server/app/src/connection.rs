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
use ferrumc_command::{CommandSource, CommandTree};
use ferrumc_core::{GameMode, PlayerId, TextColor, TextComponent, Tick};
use ferrumc_items::UntrustedItemStack;
use ferrumc_math::{BlockPos, ChunkPos, Vec3, WorldIntent};
use ferrumc_net::{
    offline_uuid, CompressionState, ConnectionLimits, ConnectionState, Criticality, DecodeError,
    DisconnectReason, EnqueueOutcome, FrameDecodeError, InboundDecoder, InboundPacket,
    OutboundEncoder, OutboundPacket, OutboundPriority, PlayWriter, StatusInfo,
};
use ferrumc_observability::{
    CounterRegistry, MutationKind, MutationResult, PacketState, ServerClock, SessionDebug,
};
use ferrumc_plugin_api::{BlockBreakAttempt, BlockPlaceAttempt};
use ferrumc_plugin_host::ResolvedDecision;
use ferrumc_proto::generated::configuration::{
    ClientboundConfigurationPacket, ClientboundKnownPacks, FinishConfiguration,
    ServerboundConfigurationPacket,
};
use ferrumc_proto::generated::handshake::ServerboundHandshakePacket;
use ferrumc_proto::generated::login::{
    ClientboundLoginPacket, LoginSuccess, ServerboundLoginPacket, SetCompression,
};
use ferrumc_proto::generated::play::{
    AcknowledgeBlockChange, ClientboundKeepAlive, ClientboundPlayPacket, CommandSuggestionMatch,
    Commands, GameEvent, PlayerAbilities, ServerboundPlayPacket, ServerboundSetHeldItem,
    SetCenterChunk, SetContainerContent, SetContainerSlot, SetCreativeSlot,
    SetDefaultSpawnPosition, SetPlayerPosition, SynchronizePlayerPosition, TabCompleteResponse,
    UnloadChunk, UseItemOn, WindowClick,
};
use ferrumc_proto::generated::status::{
    ClientboundStatusPacket, PongResponse, ServerboundStatusPacket, StatusResponse,
};
use ferrumc_session::{net_event_to_input, use_item_on_target, NetEvent, PlayerSessionHandle};
use ferrumc_sim::{BlockStateId, GameInput};

use crate::command::{parse_gamemode, GAMEMODE_COMMAND, SPAWN_COMMAND};
use crate::driver::SimCommand;
use crate::inventory::{PlayerInventory, SLOT_COUNT, WINDOW_ID};
use crate::observe;
use crate::plugins::{BlockEventDispatcher, PermissionFacade, PlayPolicy};
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

/// Player Abilities `flags` bits sent to a creative client on join: invulnerable
/// (`0x01`) | allow flying (`0x04`) | instabuild/creative (`0x08`). Flying itself
/// (`0x02`) is left off so the player starts grounded but may take off.
const CREATIVE_ABILITY_FLAGS: i8 = 0x01 | 0x04 | 0x08;

/// Flying speed sent in Player Abilities (the vanilla creative default).
const ABILITY_FLYING_SPEED: f32 = 0.05;

/// Walking speed (field-of-view modifier base) sent in Player Abilities.
const ABILITY_WALKING_SPEED: f32 = 0.1;

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

/// Maximum number of relayed chat lines a connection may burst before the rate
/// limiter throttles it.
///
/// One client must not fan spam into every recipient's bounded outbound channel;
/// a burst of `8` covers normal conversation while capping a flood.
const CHAT_BURST: u32 = 8;

/// Server ticks that must elapse to refill one chat token.
///
/// At the 20 TPS target, `10` ticks is ~0.5 s, so the sustained relay rate is ~2
/// lines/second once the burst is spent.
const CHAT_TICKS_PER_TOKEN: u64 = 10;

/// A per-connection token bucket that paces relayed chat so one client cannot
/// saturate every recipient's bounded outbound channel.
///
/// Driven by the monotonic server tick ([`ServerClock`], no syscall): tokens
/// refill at [`CHAT_TICKS_PER_TOKEN`] and are capped at [`CHAT_BURST`]. This lives
/// in the connection task — which is allowed a non-deterministic time source — and
/// is never consulted from a deterministic sim/session tick path. It is a small,
/// pure struct so the policy is unit-testable without a live socket.
struct ChatRateLimiter {
    /// Tokens currently available; one is spent per relayed line.
    tokens: u32,
    /// The tick the token count was last reconciled against.
    last_tick: Tick,
}

impl ChatRateLimiter {
    /// Builds a full bucket as of `now`.
    fn new(now: Tick) -> Self {
        Self {
            tokens: CHAT_BURST,
            last_tick: now,
        }
    }

    /// Refills any tokens earned since the last call, then spends one.
    ///
    /// Returns `true` if a token was available (the line may be relayed) or `false`
    /// if the sender is over budget (drop the line). `last_tick` advances only by
    /// whole refill intervals, so partial progress toward the next token is not
    /// lost.
    fn try_consume(&mut self, now: Tick) -> bool {
        let elapsed = now.get().saturating_sub(self.last_tick.get());
        if elapsed >= CHAT_TICKS_PER_TOKEN {
            let intervals = elapsed / CHAT_TICKS_PER_TOKEN;
            let refill = u32::try_from(intervals.min(u64::from(CHAT_BURST))).unwrap_or(CHAT_BURST);
            self.tokens = self.tokens.saturating_add(refill).min(CHAT_BURST);
            self.last_tick = Tick::new(self.last_tick.get() + intervals * CHAT_TICKS_PER_TOKEN);
        }
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }
}

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
    /// How often a standing player's chunk view is pumped toward the full
    /// advertised view distance, independent of movement packets (see
    /// [`AppConfig::chunk_stream_interval`](crate::config::AppConfig::chunk_stream_interval)).
    pub(crate) chunk_stream_interval: Duration,
    /// Bounded channel to the simulation/session driver.
    pub(crate) commands: mpsc::Sender<SimCommand>,
    /// The shared play policy: bypass permissions, the spawn position, and the
    /// command tree consulted for serverbound play packets.
    pub(crate) policy: Arc<PlayPolicy>,
    /// The shared block-event dispatcher: the long-lived plugin host the
    /// connection consults at the intent boundary for every block break/place.
    ///
    /// The plugins' `before_block_*` decision hooks run here (synchronously,
    /// panic-isolated, under a mutex with no lock held across an `.await`) — never
    /// inside the deterministic, plugin-free simulation tick.
    pub(crate) block_events: Arc<BlockEventDispatcher>,
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
    let mut handle = join_simulation(ctx, player, name.as_str(), position).await?;

    // The client already holds the spawn batch after the join kit; stream tracks
    // it from there so it never re-sends a spawn chunk and knows what to unload.
    let mut chunk_stream = ChunkStream::new(ctx);

    // Per-connection chat rate limiter, seeded at the current server tick. Lives
    // here (the connection task may use a non-deterministic time source) and is
    // never touched by the sim/session deterministic tick path.
    let mut chat_limiter = ChatRateLimiter::new(ctx.clock.now());

    // The authoritative server-side inventory for this connection. Seeded with the
    // creative starter kit and a creative game-mode mirror; the connection is the
    // sole writer of both, so the mirror cannot drift from the sim's mode. The
    // matching `SetGameMode` below makes the sim's authoritative mode agree.
    let mut inventory = PlayerInventory::with_creative_kit(GameMode::Creative);

    // The shard seeds every joiner's mode to the default (survival), but JoinGame
    // told the client creative — make the sim's authoritative mode creative too so
    // the creative-slot gate accepts this player and later enforcement is correct.
    ctx.commands
        .send(SimCommand::SetGameMode {
            player,
            mode: GameMode::Creative,
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;

    // Replay the keystone payload, then drain any already-buffered play frames.
    let mut writer = PlayWriter::with_defaults(ctx.limits);
    send_join_kit(
        &mut writer,
        &mut stream,
        &compression,
        ctx,
        &mut debug,
        player,
        name.as_str(),
        position,
        &inventory,
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
        &mut chat_limiter,
        &mut inventory,
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

    // Chunk-stream pump: advance a standing player's view toward the full
    // advertised view distance without waiting for a movement packet. The initial
    // fill already ran in the first `pump_serverbound` above; this drains the
    // remaining backlog one bounded batch per interval. `Delay` skips missed ticks
    // under load rather than bursting to catch up.
    let mut chunk_pump = interval_at(
        Instant::now() + ctx.chunk_stream_interval,
        ctx.chunk_stream_interval,
    );
    chunk_pump.set_missed_tick_behavior(MissedTickBehavior::Delay);

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
                let criticality = Criticality::for_packet(&packet);
                let outcome =
                    enqueue_traced_classified(&mut writer, &mut debug, &compression, &ctx.clock, packet);
                if is_mandatory_overflow(criticality, outcome) {
                    // A full Critical queue means the client cannot drain even
                    // keep-alives: the Layer-B mirror of the router's mandatory
                    // slow-client policy (DisconnectReason::OutboundOverflow).
                    break Err(anyhow::anyhow!(
                        "outbound overflow: a mandatory keep-alive was dropped at the connection writer"
                    ));
                }
                if let Err(err) = flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await {
                    break Err(err);
                }
                observe_queue_len(&mut debug, ctx, &writer);
            }
            _ = chunk_pump.tick() => {
                // Advance the view one bounded batch toward full view distance from
                // the current center, even if the player never moved. Bounded per
                // pump, so this paces the backlog out without flooding the socket.
                if let Err(err) =
                    pump_chunk_stream(ctx, &mut writer, &mut chunk_stream, &mut debug, &compression).await
                {
                    break Err(err);
                }
                if let Err(err) = flush_writer(&mut writer, &mut stream, &compression, ctx.io_timeout).await {
                    break Err(err);
                }
                observe_queue_len(&mut debug, ctx, &writer);
            }
            outbound = handle.recv() => match outbound {
                // Clientbound simulation output: queue and flush to the socket.
                Some(msg) => {
                    // The envelope carries the criticality AND priority the router
                    // (Layer A) assigned at the send site, so Layer B honors that
                    // intent instead of re-inferring it from packet type — which is
                    // wrong for context-dependent packets (an actor-resync BlockUpdate
                    // is mandatory, a viewer-broadcast BlockUpdate is droppable). The
                    // router already disconnects a slow client rather than silently
                    // drop a mandatory packet; this mirrors that here so a full
                    // priority queue can never silently drop a mandatory frame
                    // (despawn/spawn/ack/correction/resync) either.
                    let criticality = msg.criticality();
                    let priority = msg.priority();
                    let packet = msg.into_packet();
                    let outcome =
                        enqueue_traced(&mut writer, &mut debug, &compression, &ctx.clock, priority, packet);
                    if is_mandatory_overflow(criticality, outcome) {
                        break Err(anyhow::anyhow!(
                            "outbound overflow: a mandatory clientbound packet was dropped at the connection writer"
                        ));
                    }
                    // One channel message is enqueued and flushed per loop turn.
                    // The router's atomic resync+ack group relies on this: the FIFO
                    // channel yields the (Mandatory, State) resync before the ack, so
                    // batching several messages before a flush must NOT be introduced
                    // here without re-establishing that ordering, or a dropped resync
                    // could leave the ack behind.
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
                        &mut chat_limiter,
                        &mut inventory,
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
    name: &str,
    position: Vec3,
) -> anyhow::Result<PlayerSessionHandle> {
    let (reply_tx, reply_rx) = oneshot::channel();
    ctx.commands
        .send(SimCommand::Join {
            player,
            name: name.to_owned(),
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
/// the permission-filtered `Commands` graph, a non-zero `SynchronizePlayerPosition`,
/// then the spawn-area chunks.
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
#[allow(clippy::too_many_arguments)] // one cohesive step: framing + self player-info + I/O + trace state
#[allow(clippy::too_many_lines)] // one join sequence: framing, abilities, inventory, chunks
async fn send_join_kit(
    writer: &mut PlayWriter,
    stream: &mut TcpStream,
    compression: &CompressionState,
    ctx: &ConnContext,
    debug: &mut SessionDebug,
    player: PlayerId,
    name: &str,
    position: Vec3,
    inventory: &PlayerInventory,
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
    // Put the local player on their own tab list: a Player Info Update "Add
    // Player" for themselves. Other players' entries arrive from the session
    // router's join-visibility broadcast; this is the one entry the router cannot
    // send (a player is not in their own viewer set).
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ferrumc_session::player_info_add(player, name),
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
    // Declare the command graph so the client renders `/spawn` and `/gamemode` as
    // valid (not red) and offers autocomplete for them. The graph is filtered to
    // this player's permission level AND their granted permission nodes, so a
    // non-operator never receives the level-gated `/gamemode` subtree and a player
    // without a permission-gated command's node never receives that command.
    let allowed = |node: &str| ctx.policy.permissions().is_allowed(player, node);
    let command_body = ctx
        .policy
        .command_tree()
        .encode_commands_body(ctx.policy.permission_level(player), &allowed);
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::Commands(Commands::new(command_body)),
    );
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::SynchronizePlayerPosition(spawn_sync(JOIN_TELEPORT_ID, position)),
    );
    // Player Abilities: tell a creative client it may fly and instabuild, so the
    // flight + creative-reach UX matches the creative mode JoinGame advertised.
    send_mandatory(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::PlayerAbilities(PlayerAbilities::new(
            CREATIVE_ABILITY_FLAGS,
            ABILITY_FLYING_SPEED,
            ABILITY_WALKING_SPEED,
        )),
    )?;
    // Initialize window 0 with the full 46-slot inventory (the starter kit in the
    // hotbar) and an empty cursor. Mandatory: a dropped container-content leaves the
    // client's inventory view desynced.
    let container_payload = inventory
        .container_content_payload()
        .map_err(|err| anyhow::anyhow!("encoding join container content: {err}"))?;
    send_mandatory(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::SetContainerContent(SetContainerContent::new(
            WINDOW_ID,
            inventory.state_id(),
            container_payload,
        )),
    )?;
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
    chat_limiter: &mut ChatRateLimiter,
    inventory: &mut PlayerInventory,
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
        chat_limiter,
        inventory,
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
    chat_limiter: &mut ChatRateLimiter,
    inventory: &mut PlayerInventory,
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
            chat_limiter,
            inventory,
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
#[allow(clippy::too_many_lines)] // one dispatch: chat, inventory, place, movement fallthrough
async fn handle_play_body(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    chunk_stream: &mut ChunkStream,
    chat_limiter: &mut ChatRateLimiter,
    inventory: &mut PlayerInventory,
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
            return handle_command(
                ctx,
                player,
                name,
                writer,
                inventory,
                &command,
                debug,
                compression,
            )
            .await;
        }
        ServerboundPlayPacket::ChatMessage(chat) => {
            // Rate-limit at the SOURCE before relaying: one spammer must not fan
            // spam into every recipient's bounded outbound channel and starve legit
            // packets. The per-connection token bucket is driven by the server tick
            // (connection task, allowed a non-deterministic clock). Over budget ->
            // drop the line (logged, not relayed); the sender is not disconnected
            // for a transient burst.
            if !chat_limiter.try_consume(ctx.clock.now()) {
                tracing::debug!(player = name, "dropping over-budget chat line");
                return Ok(());
            }
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
        // A tab-complete request is answered locally from the command tree; it
        // never touches the simulation.
        ServerboundPlayPacket::TabCompleteRequest(req) => {
            handle_tab_complete(
                ctx,
                player,
                writer,
                req.transaction_id(),
                req.text().as_str(),
                debug,
                compression,
            );
            return Ok(());
        }
        // Place-with-held-item: resolve the held hotbar stack to a block-state and
        // route a place carrying it (or just ack, on empty hand / non-placeable /
        // veto). Handled here, not via the generic NetEvent path, because the place
        // needs the inventory the session layer cannot see.
        ServerboundPlayPacket::UseItemOn(p) => {
            return handle_use_item_on(ctx, player, writer, inventory, p, debug, compression).await;
        }
        // Set Creative Slot (untrusted): validate the hostile item bytes, store the
        // slot, and echo it back so the client view matches the server.
        ServerboundPlayPacket::SetCreativeSlot(p) => {
            return handle_set_creative_slot(ctx, name, writer, inventory, p, debug, compression);
        }
        // Set Held Item (serverbound): update the selected hotbar index.
        ServerboundPlayPacket::ServerboundSetHeldItem(p) => {
            handle_set_held_item(inventory, p);
            return Ok(());
        }
        // Click Container: the slice models no click logic, so any click on window
        // 0 triggers a safe resync of the authoritative inventory.
        ServerboundPlayPacket::WindowClick(p) => {
            return handle_window_click(ctx, writer, inventory, p, debug, compression);
        }
        // The teleport confirmation (reply to the join position sync) and the
        // Keep Alive echo are accepted and need no action: the slice does not
        // validate teleport ids and the keep-alive timer is fire-and-forget.
        ServerboundPlayPacket::ConfirmTeleportation(_)
        | ServerboundPlayPacket::ServerboundKeepAlive(_) => return Ok(()),
        _ => {}
    }

    let event = NetEvent::play(player, packet);
    // A block break crosses the plugin intent boundary: the loaded plugins'
    // `before_block_break` decision hooks decide whether (and how) it proceeds.
    // Every other event (movement, disconnect) carries no block decision and routes
    // straight to the simulation.
    if let Some(GameInput::BlockBreak {
        position, sequence, ..
    }) = net_event_to_input(&event)
    {
        return handle_block_break(
            ctx,
            player,
            writer,
            position,
            sequence,
            event,
            debug,
            compression,
        )
        .await;
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

/// Streams chunks to follow the client's latest reported position, then advances
/// the view toward the full view distance.
///
/// If the client reported a new position since the last call, this recenters on it
/// (sending `Set Center Chunk` on a chunk-boundary crossing). Either way it then
/// runs [`pump_chunk_stream`] against the current center, so the view advances even
/// when no position packet arrived (a freshly-joined or standing player). A chunk
/// already in the loaded set is never re-requested or re-sent.
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
    // Position path: recenter on the latest reported position, if any.
    if let Some(position) = chunk_stream.pending_position.take() {
        let new_center = chunk_of(position);
        if new_center != chunk_stream.center {
            chunk_stream.center = new_center;
            enqueue_traced_classified(
                writer,
                debug,
                compression,
                &ctx.clock,
                ClientboundPlayPacket::SetCenterChunk(SetCenterChunk::new(
                    new_center.x(),
                    new_center.z(),
                )),
            );
        }
    }

    // Always advance the view toward full view distance from the current center,
    // whether or not a position packet moved it.
    pump_chunk_stream(ctx, writer, chunk_stream, debug, compression).await
}

/// Advances the chunk view one bounded batch toward the full view distance around
/// the stream's current center, independent of any position packet.
///
/// Diffs the `(2 * view_distance + 1)` square against the per-player loaded set,
/// sends `Unload Chunk` for any column that left the radius, and asks the driver
/// (via [`SimCommand::StreamChunks`]) to load-or-generate the columns newly in
/// range — nearest-first and capped at [`MAX_CHUNK_LOADS_PER_UPDATE`] per call (see
/// [`next_chunk_batch`]), with the remainder caught on a later pump. Driven both
/// after a position update and on the standing-player pump interval, so a
/// non-moving joiner still fills out to the advertised view distance.
async fn pump_chunk_stream(
    ctx: &ConnContext,
    writer: &mut PlayWriter,
    chunk_stream: &mut ChunkStream,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let clock = &ctx.clock;
    let center = chunk_stream.center;

    let desired = desired_chunks(center, chunk_stream.view_distance);
    let to_unload: Vec<ChunkPos> = chunk_stream.loaded.difference(&desired).copied().collect();
    // Nearest-first, bounded batch of newly-in-range columns (the pure helper
    // computes the desired-vs-loaded diff, sorts center-out, and truncates).
    let to_load = next_chunk_batch(
        center,
        chunk_stream.view_distance,
        &chunk_stream.loaded,
        MAX_CHUNK_LOADS_PER_UPDATE,
    );
    if to_load.is_empty() && to_unload.is_empty() {
        return Ok(());
    }

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

/// The next bounded, nearest-first batch of chunk columns to load around `center`.
///
/// Diffs the `(2 * view_distance + 1)` desired square against the already-`loaded`
/// set, sorts the missing columns center-out by [`chebyshev_distance`] (with a
/// coordinate tiebreak for determinism), and truncates to `bound`. Pure (no I/O),
/// so each pump — whether driven by a position update or the standing-player
/// interval — makes bounded, center-out progress toward the full view distance, and
/// the policy is unit-testable without a live socket.
fn next_chunk_batch(
    center: ChunkPos,
    view_distance: i32,
    loaded: &BTreeSet<ChunkPos>,
    bound: usize,
) -> Vec<ChunkPos> {
    let desired = desired_chunks(center, view_distance);
    let mut to_load: Vec<ChunkPos> = desired.difference(loaded).copied().collect();
    to_load.sort_by_key(|pos| (chebyshev_distance(center, *pos), pos.x(), pos.z()));
    to_load.truncate(bound);
    to_load
}

/// Handles a block break at the plugin intent boundary.
///
/// Consults the loaded plugins' `before_block_break` hooks (off the tick, under
/// the host mutex with no lock held across an `.await`) and resolves the combined
/// decision:
///
/// - [`Deny`](ResolvedDecision::Deny): the edit is dropped (the world is never
///   modified), the rejected mutation is counted, and the actor's optimistic
///   client-side prediction is healed with an `AcknowledgeBlockChange` for the
///   edit's sequence (`endPredictionsUpTo`, which reverts the ghost block). If the
///   decision carries a message it is delivered as a system chat. (No authoritative
///   `BlockUpdate` is authored here: the net layer must not read world state, and a
///   veto changed nothing, so the pre-prediction state is already authoritative —
///   the ack alone is protocol-correct. A true sim-routed resync is a documented
///   follow-up.)
/// - [`Replace`](ResolvedDecision::Replace): the broken block is set to the
///   replacement state instead of air, by routing a [`SimCommand::PlaceBlock`] at
///   the break position.
/// - [`Allow`](ResolvedDecision::Allow): the original break routes to the
///   simulation as before.
///
/// Any emitted [`WorldIntent`]s are routed, and on a non-denied edit the
/// `after_block_break` notification fires.
#[allow(clippy::too_many_arguments)] // the connection threads its writer + trace context through
async fn handle_block_break(
    ctx: &ConnContext,
    player: PlayerId,
    writer: &mut PlayWriter,
    position: BlockPos,
    sequence: i32,
    event: NetEvent,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let perms = PermissionFacade::new(ctx.policy.permissions());
    let (decision, emitted) = ctx
        .block_events
        .before_block_break(&BlockBreakAttempt::new(player, position), &perms);

    match decision {
        ResolvedDecision::Deny { message } => {
            ctx.metrics
                .record_block_mutation(MutationKind::Break, MutationResult::Rejected);
            ack_sequence(writer, debug, compression, &ctx.clock, sequence)?;
            deliver_deny_message(ctx, writer, debug, compression, message);
            route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression)
                .await?;
            return Ok(());
        }
        ResolvedDecision::Replace { block_state_id } => {
            // Replace the broken block with the replacement state instead of air.
            ctx.commands
                .send(SimCommand::PlaceBlock {
                    player,
                    position,
                    sequence,
                    state: BlockStateId::new(block_state_id),
                })
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
        }
        _ => {
            ctx.commands
                .send(SimCommand::Event(event))
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
        }
    }

    route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression).await?;
    // The edit was accepted at the intent boundary and routed: notify after_*.
    let after = ctx.block_events.after_block_break(player, position, &perms);
    route_emitted_intents(ctx, player, writer, sequence, after, debug, compression).await
}

/// Delivers a plugin Deny message (if any) to the acting player as a system chat.
fn deliver_deny_message(
    ctx: &ConnContext,
    writer: &mut PlayWriter,
    debug: &mut SessionDebug,
    compression: &CompressionState,
    message: Option<TextComponent>,
) {
    if let Some(message) = message {
        enqueue_traced_classified(
            writer,
            debug,
            compression,
            &ctx.clock,
            ferrumc_session::system_chat(&message, false),
        );
    }
}

/// Routes the [`WorldIntent`]s a plugin emitted from a block decision (or an
/// after-* notification).
///
/// Mapping (best-effort; the emitted-intent surface is dev-only and bounded):
/// - [`WorldIntent::SetBlock`] -> [`SimCommand::PlaceBlock`] by the acting player.
/// - [`WorldIntent::Message`] -> a system chat to the acting player's own writer
///   when it targets them, otherwise a server-wide broadcast (the connection task
///   cannot reach another player's outbound channel directly).
/// - [`WorldIntent::Teleport`] -> not yet routed (logged); a documented follow-up.
async fn route_emitted_intents(
    ctx: &ConnContext,
    actor: PlayerId,
    writer: &mut PlayWriter,
    sequence: i32,
    intents: Vec<WorldIntent>,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    for intent in intents {
        match intent {
            WorldIntent::SetBlock {
                pos,
                block_state_id,
            } => {
                ctx.commands
                    .send(SimCommand::PlaceBlock {
                        player: actor,
                        position: pos,
                        sequence,
                        state: BlockStateId::new(block_state_id),
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
            }
            WorldIntent::Message { player, message } => {
                if player == actor {
                    enqueue_traced_classified(
                        writer,
                        debug,
                        compression,
                        &ctx.clock,
                        ferrumc_session::system_chat(&message, false),
                    );
                } else {
                    ctx.commands
                        .send(SimCommand::BroadcastSystemChat {
                            content: message,
                            overlay: false,
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
                }
            }
            _ => {
                tracing::debug!("plugin emitted an intent with no connection-side route; skipping");
            }
        }
    }
    Ok(())
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
#[allow(clippy::too_many_arguments)] // one command step: dispatch + feedback + side effects + I/O
async fn handle_command(
    ctx: &ConnContext,
    player: PlayerId,
    name: &str,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
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
            // The GameEvent only switches the CLIENT. Also mutate the authoritative
            // server-side mode (in the sim's PlayerState) so future enforcement
            // (creative no-decrement, break speed, flight) reads the right mode; the
            // visual switch and the authoritative state must not diverge.
            ctx.commands
                .send(SimCommand::SetGameMode { player, mode })
                .await
                .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
            // Keep the connection-local mirror in lockstep: it is the synchronous
            // source the creative-slot gate reads, and the connection is its sole
            // writer, so it must update here too.
            inventory.set_game_mode(mode);
        }
    }
    Ok(())
}

/// Handles a serverbound `UseItemOn`: place the held block at the targeted cell,
/// after consulting the loaded plugins at the intent boundary.
///
/// Resolves the held hotbar stack to a block-state. An empty hand, a non-placeable
/// item, or a malformed face places nothing but still acknowledges the
/// block-action sequence so the client's optimistic prediction ends. A placeable
/// block is offered to the plugins' `before_block_place` hooks (off the tick); the
/// combined decision is then resolved:
///
/// - [`Deny`](ResolvedDecision::Deny): nothing is placed, the rejection is
///   counted, the sequence is acked (healing the prediction), and any Deny message
///   is delivered as a system chat.
/// - [`Replace`](ResolvedDecision::Replace): the replacement block-state is placed
///   instead of the held one.
/// - [`Allow`](ResolvedDecision::Allow): the held block is placed (creative never
///   decrements the stack).
///
/// On a non-denied placement any emitted intents are routed and the
/// `after_block_place` notification fires.
async fn handle_use_item_on(
    ctx: &ConnContext,
    player: PlayerId,
    writer: &mut PlayWriter,
    inventory: &PlayerInventory,
    packet: &UseItemOn,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let sequence = packet.sequence();
    // A malformed face index yields no target: just ack so the prediction ends.
    let Some(position) = use_item_on_target(packet) else {
        ack_sequence(writer, debug, compression, &ctx.clock, sequence)?;
        return Ok(());
    };

    // Empty hand or non-placeable item: nothing to place, just ack. The plugins are
    // not consulted for a no-op placement.
    let Some(held_state) = inventory.held().placeable_block() else {
        ack_sequence(writer, debug, compression, &ctx.clock, sequence)?;
        return Ok(());
    };

    let perms = PermissionFacade::new(ctx.policy.permissions());
    let (decision, emitted) = ctx.block_events.before_block_place(
        &BlockPlaceAttempt::new(player, position, held_state),
        &perms,
    );

    // The state actually placed (the held block, or a plugin's replacement).
    let placed_state = match decision {
        ResolvedDecision::Deny { message } => {
            ctx.metrics
                .record_block_mutation(MutationKind::Place, MutationResult::Rejected);
            ack_sequence(writer, debug, compression, &ctx.clock, sequence)?;
            deliver_deny_message(ctx, writer, debug, compression, message);
            route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression)
                .await?;
            return Ok(());
        }
        ResolvedDecision::Replace { block_state_id } => block_state_id,
        _ => held_state,
    };

    // Creative: place the (possibly replaced) block; never touch the stack.
    ctx.commands
        .send(SimCommand::PlaceBlock {
            player,
            position,
            sequence,
            state: BlockStateId::new(placed_state),
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;

    route_emitted_intents(ctx, player, writer, sequence, emitted, debug, compression).await?;
    // Accepted at the intent boundary and routed: notify after_*.
    let after = ctx
        .block_events
        .after_block_place(player, position, placed_state, &perms);
    route_emitted_intents(ctx, player, writer, sequence, after, debug, compression).await
}

/// Handles a serverbound Set Creative Slot: validate the untrusted item bytes,
/// store the slot, and echo it.
///
/// Requires the player to be authoritatively creative (read from the connection's
/// drift-free game-mode mirror); a non-creative sender is ignored. The `slot` must
/// be in `0..=45` (a `-1` "drop outside" or any other out-of-range value is
/// ignored). The item bytes go through [`UntrustedItemStack::decode`] +
/// `into_validated` (clamping the count, stripping dangerous/unknown components); a
/// decode/validate error is logged and ignored, never fatal. On success the slot is
/// stored, the state id bumped, and a mandatory `SetContainerSlot` echoes the
/// authoritative slot back so the client view matches the server.
fn handle_set_creative_slot(
    ctx: &ConnContext,
    name: &str,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
    packet: &SetCreativeSlot,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    // Authoritative-creative gate: only a creative player may author slots.
    if inventory.game_mode() != GameMode::Creative {
        tracing::debug!(
            player = name,
            "ignoring set-creative-slot from non-creative player"
        );
        return Ok(());
    }
    // Bounds: -1 (drop) or anything outside 0..=45 is ignored.
    let Ok(index) = usize::try_from(packet.slot()) else {
        return Ok(());
    };
    if index >= SLOT_COUNT {
        return Ok(());
    }
    // Untrusted bytes -> validated stack; never trust the client's item bytes.
    let mut reader = BoundedReader::new(packet.item());
    let stack = match UntrustedItemStack::decode(&mut reader)
        .and_then(UntrustedItemStack::into_validated)
    {
        Ok(stack) => stack,
        Err(err) => {
            tracing::debug!(player = name, %err, "ignoring malformed creative slot");
            return Ok(());
        }
    };
    inventory.set_creative_slot(index, stack);

    // Echo the authoritative slot (mandatory) so the client view matches.
    let mut item_bytes = Vec::new();
    let Some(stored) = inventory.slot(index) else {
        return Ok(());
    };
    if let Err(err) = stored.encode_slot(&mut item_bytes) {
        tracing::warn!(player = name, %err, "failed to encode creative-slot echo");
        return Ok(());
    }
    send_mandatory(
        writer,
        debug,
        compression,
        &ctx.clock,
        ClientboundPlayPacket::SetContainerSlot(SetContainerSlot::new(
            WINDOW_ID,
            inventory.state_id(),
            packet.slot(),
            item_bytes,
        )),
    )
}

/// Handles a serverbound Set Held Item: update the selected hotbar index.
///
/// The wire slot is an `i16`; values outside `0..=8` are ignored (no clientbound
/// reply is needed — the client already moved its own selector).
fn handle_set_held_item(inventory: &mut PlayerInventory, packet: &ServerboundSetHeldItem) {
    if let Ok(slot) = u8::try_from(packet.slot()) {
        inventory.set_selected(slot);
    }
}

/// Handles a serverbound Click Container on window 0 with a conservative resync.
///
/// The slice models no click logic, so any click on the player inventory — a
/// state-id mismatch or otherwise — is answered by bumping the state id and
/// re-sending the full authoritative container content (mandatory). Clicks on any
/// other window are ignored. Never disconnects, never trusts the click, never
/// panics.
fn handle_window_click(
    ctx: &ConnContext,
    writer: &mut PlayWriter,
    inventory: &mut PlayerInventory,
    packet: &WindowClick,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    if packet.window_id() != WINDOW_ID {
        return Ok(());
    }
    // Bump first so the resync carries a fresh state id the client adopts.
    inventory.bump_state_id();
    let payload = match inventory.container_content_payload() {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(%err, "failed to encode container-content resync");
            return Ok(());
        }
    };
    send_mandatory(
        writer,
        debug,
        compression,
        &ctx.clock,
        ClientboundPlayPacket::SetContainerContent(SetContainerContent::new(
            WINDOW_ID,
            inventory.state_id(),
            payload,
        )),
    )
}

/// Enqueues an `AcknowledgeBlockChange` echoing `sequence`, ending the client's
/// optimistic prediction for that block action.
///
/// Sent as a *mandatory* frame (via [`send_mandatory`]): the ack is precisely the
/// packet that terminates the client's optimistic prediction, so a silent tail-drop
/// at a full outbound queue would strand the predicted (broken, placed, replaced,
/// or no-op) block as a ghost forever. Escalating a dropped ack to an outbound
/// overflow matches both the `Mandatory` criticality the router already tags onto
/// this packet ([`Criticality::for_packet`]) and the sim's own block-change
/// rejection path, which forces the same heal-ack mandatory for the same reason.
fn ack_sequence(
    writer: &mut PlayWriter,
    debug: &mut SessionDebug,
    compression: &CompressionState,
    clock: &ServerClock,
    sequence: i32,
) -> anyhow::Result<()> {
    send_mandatory(
        writer,
        debug,
        compression,
        clock,
        ClientboundPlayPacket::AcknowledgeBlockChange(AcknowledgeBlockChange::new(sequence)),
    )
}

/// Enqueues a mandatory clientbound packet, escalating a tail-drop at a full queue
/// to an outbound overflow (see [`is_mandatory_overflow`]).
///
/// The connection-originated inventory packets (join container content, the
/// creative-slot echo, the click resync) are authoritative state: a silent drop
/// would desync the client's inventory view, so a dropped mandatory frame here is
/// the same fatal condition the keep-alive and router paths already enforce. The
/// block-action heal-ack ([`ack_sequence`]) routes through here for the same
/// reason: dropping it strands the client's optimistic block prediction as a ghost.
fn send_mandatory(
    writer: &mut PlayWriter,
    debug: &mut SessionDebug,
    compression: &CompressionState,
    clock: &ServerClock,
    packet: ClientboundPlayPacket,
) -> anyhow::Result<()> {
    let criticality = Criticality::for_packet(&packet);
    let outcome = enqueue_traced_classified(writer, debug, compression, clock, packet);
    if is_mandatory_overflow(criticality, outcome) {
        return Err(anyhow::anyhow!(
            "outbound overflow: a mandatory inventory packet was dropped at the connection writer"
        ));
    }
    Ok(())
}

/// Answers a serverbound tab-complete request, enqueuing a `TabCompleteResponse`
/// built from the command tree's suggestion engine.
///
/// The request `text` is the full chat-box content including the leading `/`; the
/// slash is stripped before suggesting, and `start`/`length` are computed so the
/// client replaces exactly the in-progress token. Suggestions are filtered to the
/// literals the player's permission level *and* granted permission nodes allow
/// (matching the permission-filtered command graph the join kit declared), and
/// argument *hints* such as `<mode: 0..3>` are dropped — only concrete literal
/// completions are sent, never placeholder text the client would insert verbatim.
/// The offsets are character positions (the units the protocol's Command
/// Suggestions field expects), so a non-ASCII prefix is indexed correctly.
fn handle_tab_complete(
    ctx: &ConnContext,
    player: PlayerId,
    writer: &mut PlayWriter,
    transaction_id: i32,
    text: &str,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) {
    let level = ctx.policy.permission_level(player);
    let allowed = |node: &str| ctx.policy.permissions().is_allowed(player, node);
    let (start, length, suggestions) =
        tab_complete_reply(ctx.policy.command_tree(), level, &allowed, text);

    let matches: Vec<CommandSuggestionMatch> = suggestions
        .into_iter()
        .filter_map(|suggestion| {
            BoundedString::<32_767>::new(suggestion)
                .ok()
                // MVP: no hover tooltip (a single absent-flag byte on the wire).
                .map(|suggestion| CommandSuggestionMatch::new(suggestion, None))
        })
        .collect();

    let response = TabCompleteResponse::new(
        transaction_id,
        i32::try_from(start).unwrap_or(i32::MAX),
        i32::try_from(length).unwrap_or(i32::MAX),
        matches,
    );
    enqueue_traced_classified(
        writer,
        debug,
        compression,
        &ctx.clock,
        ClientboundPlayPacket::TabCompleteResponse(response),
    );
}

/// Computes the tab-complete reply for `text` at permission `level` against
/// `tree`, gating permission-node-declared commands through `is_allowed`: the
/// `(start, length)` *character* span of `text` the matches replace, and the
/// filtered list of literal completions.
///
/// Pure (no I/O), so it is unit-tested directly. The leading `/` is stripped
/// before suggesting; `start`/`length` delimit the in-progress token (from after
/// the last whitespace to the end of `text`). They are reported in character units
/// — what the protocol's Command Suggestions field expects — so a non-ASCII prefix
/// is indexed correctly rather than by UTF-8 byte offset. Matches are filtered to
/// the literals the player's `level` and granted permission nodes allow (the
/// declared graph is filtered the same way), and argument *hints* (which begin with
/// `<`) are dropped so the client is never sent placeholder text to insert verbatim.
fn tab_complete_reply(
    tree: &CommandTree,
    level: u8,
    is_allowed: &dyn Fn(&str) -> bool,
    text: &str,
) -> (usize, usize, Vec<String>) {
    let input = text.strip_prefix('/').unwrap_or(text);
    let offset = text.len() - input.len();
    // Byte index in `input` just past the last whitespace char (the token start).
    // Stepping by the whitespace char's UTF-8 width (not a bare `+ 1`) keeps the
    // index on a char boundary even for a multi-byte whitespace char, so the slices
    // below never panic on hostile input.
    let token_start = input.rfind(char::is_whitespace).map_or(0, |idx| {
        idx + input[idx..].chars().next().map_or(1, char::len_utf8)
    });
    let start_bytes = offset + token_start;
    // The protocol carries start/length as character positions, so convert the byte
    // offsets to char counts; for ASCII (the common case) the two coincide.
    let start = text[..start_bytes].chars().count();
    let length = text[start_bytes..].chars().count();

    let graph = tree.to_brigadier(level, is_allowed);
    let allowed: Vec<&str> = graph
        .nodes()
        .iter()
        .filter_map(|node| node.name())
        .collect();
    let matches = tree
        .suggest(input)
        .into_iter()
        .filter(|suggestion| !suggestion.starts_with('<'))
        .filter(|suggestion| allowed.contains(&suggestion.as_str()))
        .collect();
    (start, length, matches)
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

/// Whether a Layer-B (connection writer) enqueue `outcome` for a packet of the
/// given `criticality` must escalate to a
/// [`DisconnectReason::OutboundOverflow`](ferrumc_net::DisconnectReason::OutboundOverflow).
///
/// The per-player outbound *channel* (Layer A, the session router) already
/// guarantees mandatory packets are delivered-or-disconnect, but the connection
/// writer ([`PlayWriter`], Layer B) tail-drops a full priority queue silently.
/// This is the backstop that turns a dropped *mandatory* frame — a despawn, spawn,
/// ack, correction, or the keep-alive — into the documented outbound overflow
/// rather than a silent drop that would ghost an entity, leave an invisible body,
/// or strand a prediction. Droppable frames (movement, chunks, chat) may still
/// shed without disconnecting.
///
/// The `criticality` is the one the router tagged onto the packet's
/// [`OutboundMessage`](ferrumc_session::OutboundMessage) envelope at the send
/// site, **not** [`Criticality::for_packet`](ferrumc_net::Criticality::for_packet).
/// So a context-dependent frame is escalated exactly when the router meant it to
/// be: an actor-resync `BlockUpdate` carries `Mandatory` and is escalated here,
/// while the same packet type sent as a viewer broadcast carries `Droppable` and
/// is allowed to shed.
fn is_mandatory_overflow(criticality: Criticality, outcome: EnqueueOutcome) -> bool {
    outcome.is_dropped() && matches!(criticality, Criticality::Mandatory)
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

#[cfg(test)]
mod tests {
    use ferrumc_core::Tick;

    use super::{tab_complete_reply, ChatRateLimiter, CHAT_BURST, CHAT_TICKS_PER_TOKEN};
    use crate::command::{build_command_tree, GAMEMODE_COMMAND, SPAWN_COMMAND};

    const OP_LEVEL: u8 = 4;
    const MEMBER_LEVEL: u8 = 0;

    #[test]
    fn chat_rate_limiter_allows_a_burst_then_throttles_until_refill() {
        let mut limiter = ChatRateLimiter::new(Tick::new(0));
        // The full burst is allowed within a single tick.
        for _ in 0..CHAT_BURST {
            assert!(
                limiter.try_consume(Tick::new(0)),
                "burst line within budget"
            );
        }
        // The next line at the same tick is over budget and dropped.
        assert!(
            !limiter.try_consume(Tick::new(0)),
            "over-budget line throttled"
        );
        // One refill interval later, exactly one more line is allowed.
        assert!(limiter.try_consume(Tick::new(CHAT_TICKS_PER_TOKEN)));
        assert!(!limiter.try_consume(Tick::new(CHAT_TICKS_PER_TOKEN)));
    }

    #[test]
    fn chat_rate_limiter_refill_is_capped_at_the_burst() {
        let mut limiter = ChatRateLimiter::new(Tick::new(0));
        // Spend the whole burst.
        for _ in 0..CHAT_BURST {
            assert!(limiter.try_consume(Tick::new(0)));
        }
        // A long idle gap refills at most CHAT_BURST tokens, never more.
        let far = Tick::new(CHAT_TICKS_PER_TOKEN * u64::from(CHAT_BURST) * 100);
        for _ in 0..CHAT_BURST {
            assert!(limiter.try_consume(far), "refilled up to the burst cap");
        }
        assert!(!limiter.try_consume(far), "cannot exceed the burst cap");
    }

    #[test]
    fn mandatory_layer_b_drop_escalates_droppable_does_not() {
        use ferrumc_net::{Criticality, EnqueueOutcome, OutboundPriority};

        use super::is_mandatory_overflow;

        let dropped = EnqueueOutcome::Dropped {
            priority: OutboundPriority::State,
        };
        let enqueued = EnqueueOutcome::Enqueued { depth: 1 };

        // A dropped mandatory frame is an outbound overflow; a dropped droppable
        // frame is tolerated, and a successfully enqueued frame never escalates.
        assert!(is_mandatory_overflow(Criticality::Mandatory, dropped));
        assert!(!is_mandatory_overflow(Criticality::Droppable, dropped));
        assert!(!is_mandatory_overflow(Criticality::Mandatory, enqueued));
        assert!(!is_mandatory_overflow(Criticality::Droppable, enqueued));
    }

    #[test]
    fn actor_resync_envelope_escalates_at_layer_b_despite_droppable_type() {
        // Acceptance 5a: the actor-resync `BlockUpdate` rides a (Mandatory, State)
        // envelope, so Layer B escalates a dropped resync to an outbound overflow —
        // never silently dropped while its ack survives — even though the packet
        // TYPE defaults to (Droppable, World). This is the seam the envelope closes.
        use ferrumc_net::{Criticality, EnqueueOutcome, OutboundPriority};
        use ferrumc_proto::generated::play::{BlockUpdate, ClientboundPlayPacket};
        use ferrumc_proto::types::BlockPosition;
        use ferrumc_session::OutboundMessage;

        use super::is_mandatory_overflow;

        let resync = OutboundMessage::new(
            ClientboundPlayPacket::BlockUpdate(BlockUpdate::new(BlockPosition::new(8, 63, 8), 1)),
            Criticality::Mandatory,
            OutboundPriority::State,
        );

        // The carried criticality is Mandatory while the type default is Droppable:
        // re-inferring from the type (the old Layer-B bug) would mis-drop the resync.
        assert_eq!(resync.criticality(), Criticality::Mandatory);
        assert_eq!(
            Criticality::for_packet(resync.packet()),
            Criticality::Droppable
        );

        // With the carried criticality, a dropped resync at a full State queue is an
        // outbound overflow (disconnect), not a silent drop.
        let dropped_state = EnqueueOutcome::Dropped {
            priority: OutboundPriority::State,
        };
        assert!(is_mandatory_overflow(resync.criticality(), dropped_state));
    }

    #[test]
    fn tab_complete_offers_literal_completion_for_a_prefix() {
        let tree = build_command_tree();
        // "/sp" -> the in-progress token "sp" (char 1..3) completes to "spawn".
        let (start, length, matches) = tab_complete_reply(&tree, OP_LEVEL, &|_| true, "/sp");
        assert_eq!((start, length), (1, 2));
        assert_eq!(matches, vec![SPAWN_COMMAND.to_string()]);
    }

    #[test]
    fn tab_complete_lists_all_commands_after_the_slash() {
        let tree = build_command_tree();
        let (start, length, matches) = tab_complete_reply(&tree, OP_LEVEL, &|_| true, "/");
        assert_eq!((start, length), (1, 0));
        assert!(matches.contains(&SPAWN_COMMAND.to_string()));
        assert!(matches.contains(&GAMEMODE_COMMAND.to_string()));
    }

    #[test]
    fn tab_complete_hides_gated_commands_from_low_level_players() {
        let tree = build_command_tree();
        // A level-0 player gets `/spawn` but never `/gamemode`.
        let (_, _, op_matches) = tab_complete_reply(&tree, OP_LEVEL, &|_| true, "/ga");
        assert_eq!(op_matches, vec![GAMEMODE_COMMAND.to_string()]);
        let (_, _, member_matches) = tab_complete_reply(&tree, MEMBER_LEVEL, &|_| true, "/ga");
        assert!(member_matches.is_empty());
    }

    #[test]
    fn tab_complete_drops_argument_hints() {
        let tree = build_command_tree();
        // After "/gamemode " the only candidate is the `<mode: 0..3>` hint, which is
        // display-only and must not be sent as an insertable match.
        let (start, length, matches) = tab_complete_reply(&tree, OP_LEVEL, &|_| true, "/gamemode ");
        assert_eq!((start, length), (10, 0));
        assert!(matches.is_empty());
    }

    #[test]
    fn chunk_pump_makes_center_out_progress_without_a_position_packet() {
        use std::collections::BTreeSet;

        use ferrumc_math::ChunkPos;

        use super::{
            chebyshev_distance, desired_chunks, next_chunk_batch, MAX_CHUNK_LOADS_PER_UPDATE,
        };

        let center = ChunkPos::new(0, 0);
        let view_distance = 10;
        // A fresh joiner holds only the spawn batch (radius-2 square, 25 columns)
        // before sending any movement packet.
        let mut loaded: BTreeSet<ChunkPos> = desired_chunks(center, 2);

        // The first batch is the nearest ring (center-out): every column sits at the
        // closest missing Chebyshev distance — 3, just outside the spawn batch.
        let first = next_chunk_batch(center, view_distance, &loaded, MAX_CHUNK_LOADS_PER_UPDATE);
        assert_eq!(first.len(), MAX_CHUNK_LOADS_PER_UPDATE);
        assert!(first
            .iter()
            .all(|pos| chebyshev_distance(center, *pos) == 3));

        // Driving the pump repeatedly — with NO position packet ever — fills the
        // whole advertised view square. Each call simulates the driver loading the
        // returned batch.
        let desired = desired_chunks(center, view_distance);
        let mut iterations = 0;
        loop {
            let batch =
                next_chunk_batch(center, view_distance, &loaded, MAX_CHUNK_LOADS_PER_UPDATE);
            if batch.is_empty() {
                break;
            }
            for pos in batch {
                loaded.insert(pos);
            }
            iterations += 1;
            assert!(iterations < 1000, "the pump must terminate");
        }
        assert!(
            desired.is_subset(&loaded),
            "the pump fills out to the full advertised view distance"
        );
        // 441 desired - 25 seeded = 416 columns, in bounded batches of 16 -> 26 pumps.
        assert_eq!(iterations, 26);
    }

    #[test]
    fn tab_complete_range_is_in_char_units_for_non_ascii() {
        let tree = build_command_tree();
        // "/éa": the accented `é` is two UTF-8 bytes, so the in-progress token "éa"
        // begins at character 1 (right after the slash) and is two characters long.
        // The old byte computation would report length 3, mis-indexing the client.
        let (start, length, _) = tab_complete_reply(&tree, OP_LEVEL, &|_| true, "/\u{e9}a");
        assert_eq!((start, length), (1, 2));
    }
}
