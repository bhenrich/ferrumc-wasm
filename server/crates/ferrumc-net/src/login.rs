//! The live Tokio TCP login server (M11): offline-mode login through
//! configuration into the play state.
//!
//! [`LoginServer`] binds a [`tokio::net::TcpListener`] and runs a connection task
//! per accepted socket, reusing the shared bounded acceptor ([`crate::accept`])
//! and the sync framing types ([`InboundDecoder`]/[`OutboundEncoder`], M08) over
//! the wire. Each connection is driven by a sync [`LoginFlow`] state machine that
//! walks the protocol:
//!
//! ```text
//! Handshake(next=2) ──▶ Login Start ──▶ [Set Compression] ──▶ Login Success
//!   ──▶ Login Acknowledged ──▶ Configuration
//!   ──▶ (Known Packs + Finish Configuration) ──▶ Ack Finish Configuration
//!   ──▶ Play (keepalive shell)
//! ```
//!
//! The server is offline-mode only: it never contacts Mojang and assigns each
//! player a deterministic UUID via [`crate::offline_uuid`]. No world or
//! simulation runs; entering play sends a single clientbound `KeepAlive` (the
//! "keepalive shell") and the connection then idles until it closes, times out,
//! or the server winds down.
//!
//! ## What is bounded
//!
//! - **Concurrent connections** are capped by the acceptor's semaphore
//!   ([`LoginServerConfig::with_max_connections`]).
//! - **Per-frame allocation** is capped per state by [`ConnectionLimits`], and
//!   the decompressed size of any compressed frame by [`CompressionState`].
//! - **Per-connection time** is capped by an I/O timeout on every socket read and
//!   write, so a stalled or slow-loris peer cannot pin a task forever.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs};
use tokio::sync::watch;
use tokio::time::timeout;

use ferrumc_proto::generated::configuration::{
    ClientboundConfigurationPacket, ClientboundKnownPacks, FinishConfiguration, KnownPack,
    ServerboundConfigurationPacket,
};
use ferrumc_proto::generated::handshake::ServerboundHandshakePacket;
use ferrumc_proto::generated::login::{
    ClientboundLoginPacket, LoginSuccess, ServerboundLoginPacket, SetCompression,
};
use ferrumc_proto::generated::play::ClientboundKeepAlive;

use crate::compression::CompressionState;
use crate::error::{FrameDecodeError, FrameEncodeError};
use crate::inbound::{InboundDecoder, InboundPacket};
use crate::limits::ConnectionLimits;
use crate::offline::offline_uuid;
use crate::outbound::{OutboundEncoder, OutboundPacket};
use crate::server::{DEFAULT_IO_TIMEOUT, DEFAULT_MAX_CONNECTIONS};
use crate::state::ConnectionState;

/// The `next_state` value in a handshake that selects the login branch.
const NEXT_STATE_LOGIN: i32 = 2;

/// Number of bytes read from the socket per `read` call before decoding.
///
/// The decoder's own accumulation buffer (bounded by [`ConnectionLimits`]) is the
/// real allocation cap; this is just the transient stack staging buffer.
const READ_CHUNK: usize = 4096;

/// Default id carried by the keepalive sent on entering play.
///
/// A placeholder for the future play loop, which will issue rolling keepalive
/// ids; this milestone sends one fixed-id `KeepAlive` purely to mark the play
/// transition.
pub const DEFAULT_KEEP_ALIVE_ID: i64 = 1;

/// A failure in the login state machine: a correctly-framed, individually-valid
/// packet that arrived out of order for the connection's current phase.
///
/// This is distinct from a framing/decode failure ([`crate::DecodeError`]): the
/// packet decoded fine, it simply does not belong in the phase the flow is in
/// (for example a Login Acknowledged before Login Start). It always classifies as
/// a protocol violation.
///
/// The enum is `#[non_exhaustive]`: new failure modes may be added without a
/// breaking change, so downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LoginFlowError {
    /// A valid packet arrived that the current login phase (carried as its
    /// [`ConnectionState`]) does not expect.
    #[error("unexpected packet for the {state:?} state during login")]
    UnexpectedPacket {
        /// The connection state the unexpected packet was received in.
        state: ConnectionState,
    },
}

impl LoginFlowError {
    /// Classifies this error into the [`DisconnectClass`](crate::DisconnectClass)
    /// the connection layer should act on. Always a protocol violation.
    pub fn disconnect_class(&self) -> crate::DisconnectClass {
        crate::DisconnectClass::ProtocolViolation
    }
}

/// The fine-grained phase the login flow is in, one step finer than
/// [`ConnectionState`] so out-of-order-but-valid packets can be rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginPhase {
    /// Expecting the initial Handshake.
    AwaitingHandshake,
    /// Login state, expecting Login Start.
    AwaitingLoginStart,
    /// Login state, expecting Login Acknowledged (Login Success already sent).
    AwaitingLoginAck,
    /// Configuration state, expecting Ack Finish Configuration.
    AwaitingFinishAck,
    /// Play state reached; the keepalive shell is active.
    Play,
}

/// One instruction the [`LoginFlow`] hands back to the I/O driver.
#[derive(Debug, Clone, PartialEq)]
enum LoginDirective {
    /// Write this clientbound packet as a frame, using the current compression
    /// state at the time it is executed.
    Send(OutboundPacket),
    /// Enable compression at `threshold` bytes for every subsequent frame in
    /// both directions. Emitted immediately after the Set Compression packet so
    /// that Set Compression itself is sent uncompressed.
    EnableCompression(usize),
}

/// What the driver should do after the flow processes one packet.
#[derive(Debug, Clone, PartialEq)]
enum FlowControl {
    /// Keep the connection open and continue.
    Continue,
    /// Close the connection cleanly (not an error; e.g. a non-login handshake).
    Close,
    /// Reject the connection: a protocol violation to be classified and logged.
    Reject(LoginFlowError),
}

/// The directives plus control decision produced by one [`LoginFlow::handle`].
#[derive(Debug, Clone, PartialEq)]
struct FlowStep {
    directives: Vec<LoginDirective>,
    control: FlowControl,
}

impl FlowStep {
    /// A step that emits `directives` and keeps the connection open.
    fn cont(directives: Vec<LoginDirective>) -> Self {
        Self {
            directives,
            control: FlowControl::Continue,
        }
    }

    /// A step that emits nothing and closes the connection cleanly.
    fn close() -> Self {
        Self {
            directives: Vec::new(),
            control: FlowControl::Close,
        }
    }

    /// A step that rejects the connection for an out-of-order packet in `state`.
    fn reject(state: ConnectionState) -> Self {
        Self {
            directives: Vec::new(),
            control: FlowControl::Reject(LoginFlowError::UnexpectedPacket { state }),
        }
    }
}

/// The sync, I/O-free login/configuration/play state machine for one connection.
///
/// It consumes decoded serverbound [`InboundPacket`]s via [`handle`](Self::handle)
/// and returns the clientbound frames to send and the next control decision. It
/// holds no sockets and performs no I/O, so the whole login protocol is unit
/// testable without the network.
struct LoginFlow {
    phase: LoginPhase,
    compression_threshold: Option<i32>,
    known_packs: Vec<KnownPack>,
    keep_alive_packet: OutboundPacket,
}

impl LoginFlow {
    /// Builds a flow seeded from the shared connection context.
    fn new(ctx: &LoginConnContext) -> Self {
        Self {
            phase: LoginPhase::AwaitingHandshake,
            compression_threshold: ctx.compression_threshold,
            known_packs: ctx.known_packs.clone(),
            keep_alive_packet: ctx.keep_alive_packet.clone(),
        }
    }

    /// The [`ConnectionState`] the decoder and encoder should use for the next
    /// frame, derived from the current phase.
    fn connection_state(&self) -> ConnectionState {
        match self.phase {
            LoginPhase::AwaitingHandshake => ConnectionState::Handshaking,
            LoginPhase::AwaitingLoginStart | LoginPhase::AwaitingLoginAck => ConnectionState::Login,
            LoginPhase::AwaitingFinishAck => ConnectionState::Configuration,
            LoginPhase::Play => ConnectionState::Play,
        }
    }

    /// The active compression threshold (`>= 0`), or `None` when compression is
    /// disabled (absent or a negative threshold).
    fn enabled_threshold(&self) -> Option<i32> {
        self.compression_threshold
            .filter(|threshold| *threshold >= 0)
    }

    /// Advances the flow with one decoded serverbound packet.
    fn handle(&mut self, packet: &InboundPacket) -> FlowStep {
        match (self.phase, packet) {
            (
                LoginPhase::AwaitingHandshake,
                InboundPacket::Handshake(ServerboundHandshakePacket::Handshake(handshake)),
            ) => {
                if handshake.next_state() == NEXT_STATE_LOGIN {
                    self.phase = LoginPhase::AwaitingLoginStart;
                    FlowStep::cont(Vec::new())
                } else {
                    // Status (1), transfer (3), or anything else: this server
                    // serves only login, so close cleanly rather than error.
                    FlowStep::close()
                }
            }
            (
                LoginPhase::AwaitingLoginStart,
                InboundPacket::Login(ServerboundLoginPacket::LoginStart(login_start)),
            ) => {
                let name = login_start.name().clone();
                // Offline mode: the client's claimed UUID is ignored; the server
                // derives a deterministic one from the name.
                let uuid = offline_uuid(name.as_str());

                let mut directives = Vec::new();
                if let Some(threshold) = self.enabled_threshold() {
                    directives.push(LoginDirective::Send(OutboundPacket::Login(
                        ClientboundLoginPacket::SetCompression(SetCompression::new(threshold)),
                    )));
                    // `threshold >= 0` is guaranteed by `enabled_threshold`.
                    directives.push(LoginDirective::EnableCompression(threshold as usize));
                }
                directives.push(LoginDirective::Send(OutboundPacket::Login(
                    ClientboundLoginPacket::LoginSuccess(LoginSuccess::new(uuid, name, Vec::new())),
                )));

                self.phase = LoginPhase::AwaitingLoginAck;
                FlowStep::cont(directives)
            }
            (
                LoginPhase::AwaitingLoginAck,
                InboundPacket::Login(ServerboundLoginPacket::LoginAcknowledged(_)),
            ) => {
                self.phase = LoginPhase::AwaitingFinishAck;
                // Minimal configuration: advertise our (possibly empty) known
                // packs and immediately finish. No registry data is sent — there
                // is no world this milestone.
                let directives = vec![
                    LoginDirective::Send(OutboundPacket::Configuration(
                        ClientboundConfigurationPacket::ClientboundKnownPacks(
                            ClientboundKnownPacks::new(self.known_packs.clone()),
                        ),
                    )),
                    LoginDirective::Send(OutboundPacket::Configuration(
                        ClientboundConfigurationPacket::FinishConfiguration(FinishConfiguration),
                    )),
                ];
                FlowStep::cont(directives)
            }
            (LoginPhase::AwaitingFinishAck, InboundPacket::Configuration(config)) => {
                self.handle_configuration(config)
            }
            // In play, frames are surfaced raw and ignored: this is the keepalive
            // shell, not a play loop.
            (LoginPhase::Play, InboundPacket::Play(_)) => FlowStep::cont(Vec::new()),
            // A correctly-framed, valid packet that does not belong in this phase.
            _ => FlowStep::reject(self.connection_state()),
        }
    }

    /// Handles a serverbound configuration packet while awaiting the finish ack.
    fn handle_configuration(&mut self, packet: &ServerboundConfigurationPacket) -> FlowStep {
        match packet {
            // Accepted but unused: with no registries to negotiate, client
            // settings and the client's known-pack list need no action.
            ServerboundConfigurationPacket::ClientInformation(_)
            | ServerboundConfigurationPacket::ServerboundKnownPacks(_) => {
                FlowStep::cont(Vec::new())
            }
            ServerboundConfigurationPacket::AckFinishConfiguration(_) => {
                self.phase = LoginPhase::Play;
                // Keepalive shell: one clientbound KeepAlive marks entry to play.
                FlowStep::cont(vec![LoginDirective::Send(self.keep_alive_packet.clone())])
            }
        }
    }
}

/// Transport and policy configuration for a [`LoginServer`].
///
/// Bundles the per-state frame caps, the per-I/O timeout, the concurrent
/// connection ceiling, the optional compression threshold, the advertised known
/// packs, and the keepalive id. Start from [`LoginServerConfig::default`] and
/// override individual fields with the `with_*` builders.
#[derive(Debug, Clone)]
pub struct LoginServerConfig {
    limits: ConnectionLimits,
    io_timeout: Duration,
    max_connections: usize,
    compression_threshold: Option<i32>,
    known_packs: Vec<KnownPack>,
    keep_alive_id: i64,
}

impl LoginServerConfig {
    /// Overrides the per-state frame size caps.
    #[must_use]
    pub fn with_limits(mut self, limits: ConnectionLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Overrides the deadline applied to every socket read and write.
    #[must_use]
    pub fn with_io_timeout(mut self, io_timeout: Duration) -> Self {
        self.io_timeout = io_timeout;
        self
    }

    /// Overrides the ceiling on concurrent connections.
    ///
    /// See [`crate::accept`] for the backpressure contract: at the ceiling the
    /// acceptor stops accepting and further peers wait in the kernel backlog.
    #[must_use]
    pub fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections;
        self
    }

    /// Sets the compression threshold, in bytes, negotiated during login.
    ///
    /// `Some(threshold)` (with `threshold >= 0`) makes the server send a
    /// Set Compression packet after Login Start and switch both directions to the
    /// `zlib` framing; packets at or above the threshold are compressed. `None`
    /// (the default) — or a negative value — leaves compression disabled and the
    /// Set Compression packet unsent.
    #[must_use]
    pub fn with_compression_threshold(mut self, compression_threshold: Option<i32>) -> Self {
        self.compression_threshold = compression_threshold;
        self
    }

    /// Overrides the known packs advertised in the configuration phase.
    ///
    /// Defaults to empty (no packs), which is sufficient for the minimal
    /// configuration handshake this milestone drives.
    #[must_use]
    pub fn with_known_packs(mut self, known_packs: Vec<KnownPack>) -> Self {
        self.known_packs = known_packs;
        self
    }

    /// Overrides the id of the keepalive sent on entering play.
    #[must_use]
    pub fn with_keep_alive_id(mut self, keep_alive_id: i64) -> Self {
        self.keep_alive_id = keep_alive_id;
        self
    }
}

impl Default for LoginServerConfig {
    /// Default caps ([`ConnectionLimits::default`]), [`DEFAULT_IO_TIMEOUT`],
    /// [`DEFAULT_MAX_CONNECTIONS`], compression disabled, no known packs, and
    /// [`DEFAULT_KEEP_ALIVE_ID`].
    fn default() -> Self {
        Self {
            limits: ConnectionLimits::default(),
            io_timeout: DEFAULT_IO_TIMEOUT,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            compression_threshold: None,
            known_packs: Vec::new(),
            keep_alive_id: DEFAULT_KEEP_ALIVE_ID,
        }
    }
}

/// Immutable, shared per-connection context built once per [`LoginServer::run`].
#[derive(Debug)]
struct LoginConnContext {
    limits: ConnectionLimits,
    io_timeout: Duration,
    compression_threshold: Option<i32>,
    known_packs: Vec<KnownPack>,
    /// The keepalive frame to send on entering play, pre-encoded once so the
    /// fallible proto encode happens at startup rather than per connection.
    keep_alive_packet: OutboundPacket,
}

impl LoginConnContext {
    /// Builds the shared context, pre-encoding the play keepalive packet.
    fn build(config: &LoginServerConfig) -> io::Result<Self> {
        // Encode the keepalive id+body once; play packets carry no typed
        // outbound variant, so it travels as a raw play frame body.
        let mut body = BytesMut::new();
        ClientboundKeepAlive::new(config.keep_alive_id)
            .encode(&mut body)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        let keep_alive_packet = OutboundPacket::Play(body.freeze());

        Ok(Self {
            limits: config.limits,
            io_timeout: config.io_timeout,
            compression_threshold: config.compression_threshold,
            known_packs: config.known_packs.clone(),
            keep_alive_packet,
        })
    }
}

/// A bound login server, ready to [`run`](Self::run).
///
/// Construct with [`LoginServer::bind`], read the actual listening address with
/// [`local_addr`](Self::local_addr) (useful when binding to port `0`), then hand
/// the server to [`run`](Self::run) along with a shutdown future.
#[derive(Debug)]
pub struct LoginServer {
    listener: TcpListener,
    config: LoginServerConfig,
    local_addr: SocketAddr,
}

impl LoginServer {
    /// Binds a TCP listener at `addr` and returns a server ready to run.
    ///
    /// Binding to a port of `0` lets the OS choose a free port; recover it with
    /// [`local_addr`](Self::local_addr). The keepalive packet is pre-encoded
    /// here, so a configuration that cannot encode it fails fast at bind time.
    pub async fn bind<A>(addr: A, config: LoginServerConfig) -> io::Result<Self>
    where
        A: ToSocketAddrs,
    {
        // Validate the config up front by building (and discarding) the context.
        let _ = LoginConnContext::build(&config)?;
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            listener,
            config,
            local_addr,
        })
    }

    /// The address the listener is actually bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Runs the accept loop until `shutdown` resolves, then drains in-flight
    /// connections.
    ///
    /// One task is spawned per accepted socket, each bounded by the configured
    /// connection ceiling and I/O timeout. Per-connection failures (timeouts,
    /// malformed frames, protocol violations) close that one socket and never
    /// propagate out of `run`; only a fatal listener-level or configuration error
    /// is returned.
    pub async fn run<S>(self, shutdown: S) -> io::Result<()>
    where
        S: Future<Output = ()> + Send,
    {
        let Self {
            listener,
            config,
            local_addr: _,
        } = self;

        let ctx = Arc::new(LoginConnContext::build(&config)?);
        let max_connections = config.max_connections;

        crate::accept::run(
            listener,
            max_connections,
            shutdown,
            move |stream, winddown| {
                let ctx = Arc::clone(&ctx);
                async move {
                    let _ = handle_login_connection(stream, &ctx, winddown).await;
                }
            },
        )
        .await
    }
}

/// Every way a single login connection's lifetime can end abnormally.
///
/// These are per-connection and never escape [`LoginServer::run`]: the connection
/// task closes the socket and discards the error. The taxonomy exists so the
/// close reason is classifiable (and testable) rather than an opaque boolean.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LoginConnError {
    /// A socket read or write failed at the OS level.
    #[error("socket I/O failed: {0}")]
    Io(#[from] io::Error),

    /// An inbound frame failed to decode (framing, `zlib`, or typed decode).
    #[error(transparent)]
    Frame(#[from] FrameDecodeError),

    /// A clientbound packet failed to encode (a server-side fault).
    #[error(transparent)]
    Encode(#[from] FrameEncodeError),

    /// A read or write did not complete within the configured I/O timeout.
    #[error("connection timed out waiting for I/O")]
    Timeout,

    /// The peer closed the connection with a partially-buffered frame pending.
    #[error("peer closed the connection mid-frame")]
    UnexpectedEof,

    /// The login state machine rejected an out-of-order packet.
    #[error(transparent)]
    Flow(#[from] LoginFlowError),
}

/// Drives one accepted socket through the login -> configuration -> play flow
/// until it closes.
async fn handle_login_connection(
    mut stream: TcpStream,
    ctx: &LoginConnContext,
    mut winddown: watch::Receiver<bool>,
) -> Result<(), LoginConnError> {
    let mut flow = LoginFlow::new(ctx);
    let mut decoder = InboundDecoder::new(ctx.limits);
    let encoder = OutboundEncoder::new(ctx.limits);
    // Compression starts off and is flipped on by an `EnableCompression`
    // directive once Set Compression has been sent. The same state is used for
    // both reading and writing.
    let mut compression = CompressionState::disabled();
    let mut read_buf = [0u8; READ_CHUNK];

    loop {
        let state = flow.connection_state();
        // Drain everything already buffered before blocking on another read so
        // pipelined frames progress without an extra round trip.
        if let Some(packet) = decoder.next_packet_compressed(state, &compression)? {
            match drive_packet(
                &mut stream,
                &mut flow,
                &packet,
                &encoder,
                &mut compression,
                ctx,
            )
            .await?
            {
                ControlFlow::Continue(()) => continue,
                ControlFlow::Break(()) => return Ok(()),
            }
        }

        let read = tokio::select! {
            // Prefer an in-progress shutdown over reading more bytes.
            biased;
            _ = winddown.changed() => return Ok(()),
            result = timeout(ctx.io_timeout, stream.read(&mut read_buf)) => result,
        };
        let read = read.map_err(|_| LoginConnError::Timeout)?;
        let n = read?;
        if n == 0 {
            // A clean half-close with no partial frame is normal; a peer that
            // vanishes mid-frame is reported so the close reason is unambiguous.
            return if decoder.buffered_len() > 0 {
                Err(LoginConnError::UnexpectedEof)
            } else {
                Ok(())
            };
        }
        decoder
            .push(&read_buf[..n])
            .map_err(FrameDecodeError::Decode)?;
    }
}

/// Feeds one decoded packet to the flow and executes the resulting directives,
/// returning whether the connection should continue or close.
async fn drive_packet(
    stream: &mut TcpStream,
    flow: &mut LoginFlow,
    packet: &InboundPacket,
    encoder: &OutboundEncoder,
    compression: &mut CompressionState,
    ctx: &LoginConnContext,
) -> Result<ControlFlow<()>, LoginConnError> {
    let step = flow.handle(packet);
    for directive in step.directives {
        match directive {
            LoginDirective::Send(outbound) => {
                write_frame(stream, encoder, &outbound, compression, ctx.io_timeout).await?;
            }
            // Flip compression after the (uncompressed) Set Compression frame so
            // every subsequent frame uses the negotiated `zlib` framing.
            LoginDirective::EnableCompression(threshold) => {
                *compression = CompressionState::enabled(threshold);
            }
        }
    }
    match step.control {
        FlowControl::Continue => Ok(ControlFlow::Continue(())),
        FlowControl::Close => Ok(ControlFlow::Break(())),
        FlowControl::Reject(err) => Err(LoginConnError::Flow(err)),
    }
}

/// Encodes `packet` into a (possibly compressed) length-delimited frame and
/// writes it, bounded by the I/O timeout.
async fn write_frame(
    stream: &mut TcpStream,
    encoder: &OutboundEncoder,
    packet: &OutboundPacket,
    compression: &CompressionState,
    io_timeout: Duration,
) -> Result<(), LoginConnError> {
    let mut buf = BytesMut::new();
    encoder.encode_compressed(packet, &mut buf, compression)?;
    timeout(io_timeout, stream.write_all(&buf))
        .await
        .map_err(|_| LoginConnError::Timeout)??;
    timeout(io_timeout, stream.flush())
        .await
        .map_err(|_| LoginConnError::Timeout)??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use ferrumc_codec::BoundedString;
    use ferrumc_proto::generated::configuration::AckFinishConfiguration;
    use ferrumc_proto::generated::handshake::Handshake;
    use ferrumc_proto::generated::login::{LoginAcknowledged, LoginStart};

    use super::*;

    /// Builds a flow with the given compression threshold for direct testing.
    fn flow_with(compression_threshold: Option<i32>) -> LoginFlow {
        let ctx = LoginConnContext::build(
            &LoginServerConfig::default().with_compression_threshold(compression_threshold),
        )
        .expect("context builds");
        LoginFlow::new(&ctx)
    }

    fn handshake_packet(next_state: i32) -> InboundPacket {
        InboundPacket::Handshake(ServerboundHandshakePacket::Handshake(Handshake::new(
            772,
            BoundedString::<255>::new("localhost".to_string()).expect("address fits"),
            25565,
            next_state,
        )))
    }

    fn login_start_packet(name: &str) -> InboundPacket {
        InboundPacket::Login(ServerboundLoginPacket::LoginStart(LoginStart::new(
            BoundedString::<16>::new(name.to_string()).expect("name fits"),
            uuid::Uuid::nil(),
        )))
    }

    fn login_ack_packet() -> InboundPacket {
        InboundPacket::Login(ServerboundLoginPacket::LoginAcknowledged(LoginAcknowledged))
    }

    fn ack_finish_packet() -> InboundPacket {
        InboundPacket::Configuration(ServerboundConfigurationPacket::AckFinishConfiguration(
            AckFinishConfiguration,
        ))
    }

    /// Asserts a directive is a Send of a login packet matching `pred`.
    fn assert_send_login(
        directive: &LoginDirective,
        pred: impl Fn(&ClientboundLoginPacket) -> bool,
    ) {
        match directive {
            LoginDirective::Send(OutboundPacket::Login(packet)) => {
                assert!(pred(packet), "unexpected login packet: {packet:?}");
            }
            other => panic!("expected a login Send, got {other:?}"),
        }
    }

    #[test]
    fn full_flow_reaches_play_without_compression() {
        let mut flow = flow_with(None);
        assert_eq!(flow.connection_state(), ConnectionState::Handshaking);

        // Handshake -> Login.
        let step = flow.handle(&handshake_packet(NEXT_STATE_LOGIN));
        assert_eq!(step.control, FlowControl::Continue);
        assert!(step.directives.is_empty());
        assert_eq!(flow.connection_state(), ConnectionState::Login);

        // Login Start -> Login Success (no Set Compression when disabled).
        let step = flow.handle(&login_start_packet("Saad"));
        assert_eq!(step.control, FlowControl::Continue);
        assert_eq!(
            step.directives.len(),
            1,
            "only Login Success when uncompressed"
        );
        assert_send_login(&step.directives[0], |p| {
            matches!(p, ClientboundLoginPacket::LoginSuccess(_))
        });

        // Login Acknowledged -> Configuration (Known Packs + Finish).
        let step = flow.handle(&login_ack_packet());
        assert_eq!(step.control, FlowControl::Continue);
        assert_eq!(flow.connection_state(), ConnectionState::Configuration);
        assert_eq!(step.directives.len(), 2);

        // Ack Finish Configuration -> Play (keepalive shell).
        let step = flow.handle(&ack_finish_packet());
        assert_eq!(step.control, FlowControl::Continue);
        assert_eq!(flow.connection_state(), ConnectionState::Play);
        assert_eq!(step.directives.len(), 1);
        assert!(matches!(
            &step.directives[0],
            LoginDirective::Send(OutboundPacket::Play(_))
        ));
    }

    #[test]
    fn login_start_emits_set_compression_when_enabled() {
        let mut flow = flow_with(Some(256));
        flow.handle(&handshake_packet(NEXT_STATE_LOGIN));

        let step = flow.handle(&login_start_packet("Saad"));
        assert_eq!(
            step.directives.len(),
            3,
            "SetCompression + Enable + Success"
        );
        assert_send_login(&step.directives[0], |p| {
            matches!(p, ClientboundLoginPacket::SetCompression(_))
        });
        assert_eq!(step.directives[1], LoginDirective::EnableCompression(256));
        assert_send_login(&step.directives[2], |p| {
            matches!(p, ClientboundLoginPacket::LoginSuccess(_))
        });
    }

    #[test]
    fn login_success_echoes_name_and_offline_uuid() {
        let mut flow = flow_with(None);
        flow.handle(&handshake_packet(NEXT_STATE_LOGIN));
        let step = flow.handle(&login_start_packet("Saad"));
        let LoginDirective::Send(OutboundPacket::Login(ClientboundLoginPacket::LoginSuccess(
            success,
        ))) = &step.directives[0]
        else {
            panic!("expected Login Success");
        };
        assert_eq!(success.name().as_str(), "Saad");
        assert_eq!(success.uuid(), offline_uuid("Saad"));
    }

    #[test]
    fn negative_compression_threshold_disables_compression() {
        let mut flow = flow_with(Some(-1));
        flow.handle(&handshake_packet(NEXT_STATE_LOGIN));
        let step = flow.handle(&login_start_packet("Saad"));
        assert_eq!(
            step.directives.len(),
            1,
            "no Set Compression for a negative threshold"
        );
    }

    #[test]
    fn non_login_handshake_closes_cleanly() {
        let mut flow = flow_with(None);
        // next_state 1 selects status, which this server does not serve.
        let step = flow.handle(&handshake_packet(1));
        assert_eq!(step.control, FlowControl::Close);
        assert!(step.directives.is_empty());
    }

    #[test]
    fn out_of_order_login_ack_is_rejected() {
        let mut flow = flow_with(None);
        flow.handle(&handshake_packet(NEXT_STATE_LOGIN));
        // Login Acknowledged before Login Start is valid but out of order.
        let step = flow.handle(&login_ack_packet());
        match step.control {
            FlowControl::Reject(err) => {
                assert_eq!(
                    err,
                    LoginFlowError::UnexpectedPacket {
                        state: ConnectionState::Login,
                    }
                );
                assert_eq!(
                    err.disconnect_class(),
                    crate::DisconnectClass::ProtocolViolation
                );
            }
            other => panic!("expected a reject, got {other:?}"),
        }
    }

    #[test]
    fn client_information_in_configuration_is_accepted_and_ignored() {
        use ferrumc_proto::generated::configuration::ClientInformation;

        let mut flow = flow_with(None);
        flow.handle(&handshake_packet(NEXT_STATE_LOGIN));
        flow.handle(&login_start_packet("Saad"));
        flow.handle(&login_ack_packet());

        let info = ClientInformation::new(
            BoundedString::<16>::new("en_us".to_string()).expect("locale fits"),
            10,
            0,
            true,
            0x7f,
            1,
            false,
            true,
            0,
        );
        let step = flow.handle(&InboundPacket::Configuration(
            ServerboundConfigurationPacket::ClientInformation(info),
        ));
        assert_eq!(step.control, FlowControl::Continue);
        assert!(step.directives.is_empty(), "client settings need no reply");
        assert_eq!(flow.connection_state(), ConnectionState::Configuration);
    }
}
