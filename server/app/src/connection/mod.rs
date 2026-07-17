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

use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio::time::timeout;

use ferrumc_codec::BoundedString;
use ferrumc_core::PlayerId;
use ferrumc_math::Vec3;
use ferrumc_net::{
    CompressionState, ConnectionState, DecodeError, FrameDecodeError, InboundDecoder,
    InboundPacket, OutboundEncoder, OutboundPacket,
};
use ferrumc_observability::SessionDebug;
use ferrumc_proto::generated::configuration::{
    ClientboundConfigurationPacket, ClientboundKnownPacks, FinishConfiguration,
    ServerboundConfigurationPacket,
};
use ferrumc_proto::generated::handshake::ServerboundHandshakePacket;
use ferrumc_proto::generated::login::{
    ClientboundLoginPacket, LoginSuccess, ServerboundLoginPacket, SetCompression,
};
use ferrumc_proto::generated::play::SynchronizePlayerPosition;
use ferrumc_proto::generated::status::{
    ClientboundStatusPacket, PongResponse, ServerboundStatusPacket,
};

use crate::observe;

mod chunk_stream;
mod context;
mod handlers;
mod join;
mod outbound;
mod play;
mod rate_limiter;
mod serverbound_budget;

pub(crate) use context::{build_status_response, ConnContext};

use play::enter_play;

/// The `next_state` value in a handshake that selects the status branch
/// (server-list ping).
const NEXT_STATE_STATUS: i32 = 1;

/// The `next_state` value in a handshake that selects the login branch.
const NEXT_STATE_LOGIN: i32 = 2;

/// Bytes read off the socket per `read` call before decoding.
const READ_CHUNK: usize = 4096;

/// Teleport id carried by the join `SynchronizePlayerPosition`.
///
/// Must be non-zero: a real client replies with a `ConfirmTeleportation` echoing
/// it, which the server decodes and ignores.
const JOIN_TELEPORT_ID: i32 = 1;

/// `Game Event` reason `3`: "change game mode". The event `value` is the game-mode
/// id as a float; sending it switches the issuing client's mode (the carrier
/// `/gamemode` and the rejoin restore use, since there is no dedicated
/// set-game-mode packet). Shared by the join restore and the `/gamemode` handler.
pub(super) const GAME_EVENT_CHANGE_GAMEMODE: u8 = 3;

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

/// The canonical identity established once from Login Start and carried into
/// every later app-owned identity surface.
struct LoginIdentity {
    /// The exact, case-sensitive username supplied by the client.
    name: BoundedString<16>,
    /// The canonical offline-mode player identity derived from `name`.
    player: PlayerId,
}

impl LoginIdentity {
    /// Derives the canonical offline identity once for a wire-bounded name.
    fn offline(name: BoundedString<16>) -> Self {
        let player = PlayerId::offline(name.as_str());
        Self { name, player }
    }

    /// Returns the UUID exposed in Login Success and player-facing packets.
    fn uuid(&self) -> uuid::Uuid {
        self.player.as_uuid()
    }
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

    /// Sends the optional Set Compression and Login Success for `identity`.
    async fn send_login_success(&mut self, identity: &LoginIdentity) -> anyhow::Result<()> {
        if let Some(threshold) = self.ctx.enabled_threshold() {
            self.send(&OutboundPacket::Login(
                ClientboundLoginPacket::SetCompression(SetCompression::new(threshold)),
            ))
            .await?;
            // Set Compression itself goes out uncompressed; every later frame is
            // framed with the negotiated zlib threshold.
            self.compression = CompressionState::enabled(threshold as usize);
        }
        self.send(&OutboundPacket::Login(
            ClientboundLoginPacket::LoginSuccess(LoginSuccess::new(
                identity.uuid(),
                identity.name.clone(),
                Vec::new(),
            )),
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
        Some(identity) => enter_play(conn, identity.name, identity.player, &mut shutdown).await,
        None => Ok(()),
    }
}

/// Runs the login handshake, returning the canonical identity once play is
/// reached, or `None` if the connection closed cleanly first.
async fn run_login(
    conn: &mut Connection<'_>,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<Option<LoginIdentity>> {
    let mut phase = LoginPhase::Handshaking;
    let mut identity: Option<LoginIdentity> = None;
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
                match advance(conn, &mut phase, &mut identity, &packet).await? {
                    LoginProgress::Continue => continue,
                    LoginProgress::Close => return Ok(None),
                    LoginProgress::Play => {
                        return identity.take().map(Some).ok_or_else(|| {
                            anyhow::anyhow!("reached play without a login identity")
                        });
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
    identity: &mut Option<LoginIdentity>,
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
            let player_identity = LoginIdentity::offline(start.name().clone());
            // Beta-gate: reject banned / non-whitelisted players before login
            // completes (single additive access check; see ConnContext::login_denial).
            if let Some(disconnect) = conn
                .ctx
                .login_denial(player_identity.name.as_str(), player_identity.player)?
            {
                conn.send(&disconnect).await?;
                return Ok(LoginProgress::Close);
            }
            conn.send_login_success(&player_identity).await?;
            *identity = Some(player_identity);
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

/// Builds an absolute position sync with the given teleport id, `yaw`, and `pitch`
/// (zero velocity deltas and flags), used both to spawn a joiner in at their
/// restored look and to snap a `/spawn`'d player back.
fn spawn_sync(teleport_id: i32, position: Vec3, yaw: f32, pitch: f32) -> SynchronizePlayerPosition {
    SynchronizePlayerPosition::new(
        teleport_id,
        position.x,
        position.y,
        position.z,
        0.0,
        0.0,
        0.0,
        yaw,
        pitch,
        0,
    )
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
    use super::*;

    #[test]
    fn login_identity_is_derived_once() {
        let name = BoundedString::<16>::new("IdentityProbe".to_string())
            .expect("test username is within the protocol bound");
        let identity = LoginIdentity::offline(name);

        assert_eq!(identity.player, PlayerId::offline("IdentityProbe"));
        assert_eq!(identity.uuid(), identity.player.as_uuid());
        assert_eq!(identity.name.as_str(), "IdentityProbe");
    }
}
