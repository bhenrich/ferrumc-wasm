//! The live Tokio TCP status-ping server (M09).
//!
//! [`StatusServer`] binds a [`tokio::net::TcpListener`] and runs a connection
//! task per accepted socket. Each task drives the sync M08 framing types
//! ([`InboundDecoder`]/[`OutboundEncoder`]) over the wire: it reads
//! length-delimited frames, walks the `Handshaking -> Status` portion of the
//! protocol, answers a `StatusRequest` with a server-list JSON
//! [`StatusResponse`], echoes a `PingRequest` payload back in a `PongResponse`,
//! and then closes.
//!
//! Nothing here touches login, play, world, or simulation: a `next_state` other
//! than status is closed cleanly rather than handled.
//!
//! ## What is bounded
//!
//! - **Concurrent connections** are capped by a [`tokio::sync::Semaphore`]; see
//!   [`StatusServerConfig::with_max_connections`] for the backpressure contract.
//! - **Per-frame allocation** is capped per state by [`ConnectionLimits`], which
//!   the inbound decoder and outbound encoder both enforce.
//! - **Per-connection time** is capped by an I/O timeout applied to every socket
//!   read and write, so a stalled or slow-loris peer cannot pin a task forever.

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

use ferrumc_codec::BoundedString;
use ferrumc_proto::generated::handshake::ServerboundHandshakePacket;
use ferrumc_proto::generated::status::{
    ClientboundStatusPacket, PongResponse, ServerboundStatusPacket, StatusResponse,
};

use crate::inbound::{InboundDecoder, InboundPacket};
use crate::limits::ConnectionLimits;
use crate::outbound::{OutboundEncoder, OutboundPacket};
use crate::state::ConnectionState;

/// Default ceiling on concurrent connections handled at once.
///
/// Chosen as a conservative bound for a status endpoint: high enough to absorb a
/// normal server-list refresh storm, low enough that the per-connection task and
/// buffer cost stays bounded under load.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;

/// Default deadline applied to every individual socket read and write.
///
/// Mirrors the vanilla client's read-timeout ballpark. A peer that neither
/// completes a frame nor drains the server's response within this window is
/// disconnected, freeing its connection slot.
pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Number of bytes read from the socket per `read` call before decoding.
///
/// The decoder's own accumulation buffer (bounded by [`ConnectionLimits`]) is
/// the real allocation cap; this is just the transient stack staging buffer.
const READ_CHUNK: usize = 4096;

/// The `next_state` value in a handshake that selects the status branch.
const NEXT_STATE_STATUS: i32 = 1;

/// Maximum size cap, in code units, for the status-response JSON string.
///
/// Matches the `StatusResponse` field width in `ferrumc-proto`.
const STATUS_JSON_MAX_CHARS: usize = 32_767;

/// Server-list metadata rendered into the status-response JSON.
///
/// This is the data a client shows on its multiplayer server list: the
/// reported version, the player counts, and the MOTD. It is serialized to the
/// minimal `{version, players, description}` JSON the status protocol expects;
/// the JSON is opaque to `ferrumc-proto`, which only sees the resulting string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusInfo {
    version_name: String,
    protocol_version: i32,
    max_players: u32,
    online_players: u32,
    description: String,
}

impl StatusInfo {
    /// Builds status metadata from its component fields.
    ///
    /// `protocol_version` is the wire protocol number a matching client reports
    /// in its handshake (772 for Minecraft 1.21.8); `version_name` is the
    /// human-readable label shown beside it. `description` is the MOTD text.
    pub fn new(
        version_name: impl Into<String>,
        protocol_version: i32,
        max_players: u32,
        online_players: u32,
        description: impl Into<String>,
    ) -> Self {
        Self {
            version_name: version_name.into(),
            protocol_version,
            max_players,
            online_players,
            description: description.into(),
        }
    }

    /// Renders the metadata into the status-response JSON string.
    ///
    /// String fields are JSON-escaped, so an MOTD or version label containing
    /// quotes, backslashes, or control characters cannot break the document.
    pub fn to_json(&self) -> String {
        let mut json = String::new();
        json.push_str("{\"version\":{\"name\":\"");
        push_json_escaped(&mut json, &self.version_name);
        json.push_str("\",\"protocol\":");
        json.push_str(&self.protocol_version.to_string());
        json.push_str("},\"players\":{\"max\":");
        json.push_str(&self.max_players.to_string());
        json.push_str(",\"online\":");
        json.push_str(&self.online_players.to_string());
        json.push_str(",\"sample\":[]},\"description\":{\"text\":\"");
        push_json_escaped(&mut json, &self.description);
        json.push_str("\"}}");
        json
    }
}

impl Default for StatusInfo {
    /// A neutral default advertising protocol 772 (Minecraft 1.21.8), 20 player
    /// slots, none online, and a generic MOTD.
    fn default() -> Self {
        Self::new("1.21.8", 772, 20, 0, "A FerrumC server")
    }
}

/// Appends `value` to `out`, escaping it as a JSON string body (no surrounding
/// quotes).
///
/// Handles the characters JSON forbids in a string literal: the quote and
/// backslash, the named control escapes, and any remaining C0 control character
/// as a `\u00XX` escape.
fn push_json_escaped(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => {
                // A C0 control character: the two high hex digits are always
                // `00`, so emit `\u00` followed by the low byte's hex digits.
                const HEX: &[u8; 16] = b"0123456789abcdef";
                let byte = ch as usize;
                out.push_str("\\u00");
                out.push(char::from(HEX[(byte >> 4) & 0xf]));
                out.push(char::from(HEX[byte & 0xf]));
            }
            ch => out.push(ch),
        }
    }
}

/// Transport and policy configuration for a [`StatusServer`].
///
/// Bundles the per-state frame caps, the per-I/O timeout, the concurrent
/// connection ceiling, and the advertised [`StatusInfo`]. Start from
/// [`StatusServerConfig::default`] and override individual fields with the
/// `with_*` builders.
#[derive(Debug, Clone)]
pub struct StatusServerConfig {
    limits: ConnectionLimits,
    io_timeout: Duration,
    max_connections: usize,
    status: StatusInfo,
}

impl StatusServerConfig {
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
    /// ## Backpressure
    ///
    /// When this many connections are in flight the acceptor stops calling
    /// `accept` and waits for a slot to free. Further connections queue in the
    /// kernel's accept backlog and are refused by the OS once that fills; the
    /// server never spawns an unbounded number of tasks.
    #[must_use]
    pub fn with_max_connections(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections;
        self
    }

    /// Overrides the advertised server-list [`StatusInfo`].
    #[must_use]
    pub fn with_status(mut self, status: StatusInfo) -> Self {
        self.status = status;
        self
    }
}

impl Default for StatusServerConfig {
    /// Default caps ([`ConnectionLimits::default`]), [`DEFAULT_IO_TIMEOUT`],
    /// [`DEFAULT_MAX_CONNECTIONS`], and the default [`StatusInfo`].
    fn default() -> Self {
        Self {
            limits: ConnectionLimits::default(),
            io_timeout: DEFAULT_IO_TIMEOUT,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            status: StatusInfo::default(),
        }
    }
}

/// A bound status-ping server, ready to [`run`](Self::run).
///
/// Construct with [`StatusServer::bind`], read the actual listening address with
/// [`local_addr`](Self::local_addr) (useful when binding to port `0`), then hand
/// the server to [`run`](Self::run) along with a shutdown future.
#[derive(Debug)]
pub struct StatusServer {
    listener: TcpListener,
    config: StatusServerConfig,
    local_addr: SocketAddr,
}

impl StatusServer {
    /// Binds a TCP listener at `addr` and returns a server ready to run.
    ///
    /// Binding to a port of `0` lets the OS choose a free port; recover it with
    /// [`local_addr`](Self::local_addr).
    pub async fn bind<A>(addr: A, config: StatusServerConfig) -> io::Result<Self>
    where
        A: ToSocketAddrs,
    {
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
    /// connection semaphore and I/O timeout. When `shutdown` resolves the loop
    /// stops accepting, signals every live connection to wind down, and waits
    /// for the connection tasks to finish before returning.
    ///
    /// Per-connection failures (timeouts, malformed frames, hostile sizes) are
    /// handled by closing that one socket and never propagate out of `run`; only
    /// a fatal listener-level or configuration error is returned.
    pub async fn run<S>(self, shutdown: S) -> io::Result<()>
    where
        S: Future<Output = ()> + Send,
    {
        let Self {
            listener,
            config,
            local_addr: _,
        } = self;

        let handler = Arc::new(ConnHandler::build(&config)?);
        let max_connections = config.max_connections;

        crate::accept::run(
            listener,
            max_connections,
            shutdown,
            move |stream, winddown| {
                let handler = Arc::clone(&handler);
                async move {
                    let _ = handle_connection(stream, &handler, winddown).await;
                }
            },
        )
        .await
    }
}

/// Immutable, shared per-connection context built once per [`StatusServer::run`].
#[derive(Debug)]
struct ConnHandler {
    limits: ConnectionLimits,
    io_timeout: Duration,
    /// The prebuilt status response. Built once so each `StatusRequest` only
    /// clones it rather than re-rendering and re-validating the JSON.
    status_response: OutboundPacket,
}

impl ConnHandler {
    /// Builds the shared context, rendering and validating the status JSON once.
    fn build(config: &StatusServerConfig) -> io::Result<Self> {
        let json = config.status.to_json();
        let bounded = BoundedString::<STATUS_JSON_MAX_CHARS>::new(json)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        let status_response = OutboundPacket::Status(ClientboundStatusPacket::StatusResponse(
            StatusResponse::new(bounded),
        ));
        Ok(Self {
            limits: config.limits,
            io_timeout: config.io_timeout,
            status_response,
        })
    }
}

/// Every way a single connection's lifetime can end abnormally.
///
/// These are per-connection and never escape [`StatusServer::run`]: the
/// connection task closes the socket and discards the error. The taxonomy exists
/// so the close reason is classifiable (and testable) rather than an opaque
/// boolean.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConnectionError {
    /// A socket read or write failed at the OS level.
    #[error("socket I/O failed: {0}")]
    Io(#[from] io::Error),

    /// An inbound frame failed to decode (malformed, oversized, or a protocol
    /// violation). Carries the classifying M08 error.
    #[error(transparent)]
    Decode(#[from] crate::error::DecodeError),

    /// A clientbound packet failed to encode (a server-side fault).
    #[error(transparent)]
    Encode(#[from] crate::error::EncodeError),

    /// A read or write did not complete within the configured I/O timeout.
    #[error("connection timed out waiting for I/O")]
    Timeout,

    /// The peer closed the connection with a partially-buffered frame pending.
    #[error("peer closed the connection mid-frame")]
    UnexpectedEof,

    /// A correctly-framed packet arrived that the status path does not expect in
    /// the current state.
    #[error("unexpected packet for state {state:?}")]
    UnexpectedPacket {
        /// The state the unexpected packet was decoded in.
        state: ConnectionState,
    },
}

/// Drives one accepted socket through the handshake/status exchange until it
/// closes.
async fn handle_connection(
    mut stream: TcpStream,
    handler: &ConnHandler,
    mut winddown: watch::Receiver<bool>,
) -> Result<(), ConnectionError> {
    let mut state = ConnectionState::Handshaking;
    let mut decoder = InboundDecoder::new(handler.limits);
    let encoder = OutboundEncoder::new(handler.limits);
    let mut read_buf = [0u8; READ_CHUNK];

    loop {
        // Drain everything already buffered before blocking on another read, so
        // pipelined frames (handshake + request + ping in one packet) progress
        // without an extra round trip.
        if let Some(packet) = decoder.next_packet(state)? {
            match handle_packet(&mut stream, &mut state, packet, handler, &encoder).await? {
                ControlFlow::Continue(()) => continue,
                ControlFlow::Break(()) => return Ok(()),
            }
        }

        let read = tokio::select! {
            // Prefer an in-progress shutdown over reading more bytes.
            biased;
            _ = winddown.changed() => return Ok(()),
            result = timeout(handler.io_timeout, stream.read(&mut read_buf)) => result,
        };
        let read = read.map_err(|_| ConnectionError::Timeout)?;
        let n = read?;
        if n == 0 {
            // A clean half-close with no partial frame is normal; a peer that
            // vanishes mid-frame is reported so the close reason is unambiguous.
            return if decoder.buffered_len() > 0 {
                Err(ConnectionError::UnexpectedEof)
            } else {
                Ok(())
            };
        }
        decoder.push(&read_buf[..n])?;
    }
}

/// Acts on one decoded serverbound packet, returning whether the connection
/// should continue or close.
async fn handle_packet(
    stream: &mut TcpStream,
    state: &mut ConnectionState,
    packet: InboundPacket,
    handler: &ConnHandler,
    encoder: &OutboundEncoder,
) -> Result<ControlFlow<()>, ConnectionError> {
    match packet {
        InboundPacket::Handshake(ServerboundHandshakePacket::Handshake(hs)) => {
            if hs.next_state() == NEXT_STATE_STATUS {
                *state = ConnectionState::Status;
                Ok(ControlFlow::Continue(()))
            } else {
                // Login (next_state 2) and anything else are out of this
                // milestone's scope: close cleanly rather than proceed.
                Ok(ControlFlow::Break(()))
            }
        }
        InboundPacket::Status(ServerboundStatusPacket::StatusRequest(_)) => {
            write_frame(
                stream,
                encoder,
                &handler.status_response,
                handler.io_timeout,
            )
            .await?;
            Ok(ControlFlow::Continue(()))
        }
        InboundPacket::Status(ServerboundStatusPacket::PingRequest(request)) => {
            let reply = OutboundPacket::Status(ClientboundStatusPacket::PongResponse(
                PongResponse::new(request.payload()),
            ));
            write_frame(stream, encoder, &reply, handler.io_timeout).await?;
            // The status exchange ends after the pong echo.
            Ok(ControlFlow::Break(()))
        }
        other => Err(ConnectionError::UnexpectedPacket {
            state: other.state(),
        }),
    }
}

/// Encodes `packet` into a length-delimited frame and writes it, bounded by the
/// I/O timeout.
async fn write_frame(
    stream: &mut TcpStream,
    encoder: &OutboundEncoder,
    packet: &OutboundPacket,
    io_timeout: Duration,
) -> Result<(), ConnectionError> {
    let mut buf = BytesMut::new();
    encoder.encode(packet, &mut buf)?;
    timeout(io_timeout, stream.write_all(&buf))
        .await
        .map_err(|_| ConnectionError::Timeout)??;
    timeout(io_timeout, stream.flush())
        .await
        .map_err(|_| ConnectionError::Timeout)??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_status_json_has_the_expected_shape() {
        let json = StatusInfo::default().to_json();
        assert!(json.contains("\"version\""));
        assert!(json.contains("\"name\":\"1.21.8\""));
        assert!(json.contains("\"protocol\":772"));
        assert!(json.contains("\"players\""));
        assert!(json.contains("\"max\":20"));
        assert!(json.contains("\"online\":0"));
        assert!(json.contains("\"description\""));
        assert!(json.contains("A FerrumC server"));
    }

    #[test]
    fn status_json_escapes_string_fields() {
        let info = StatusInfo::new("v\"1\"", 772, 1, 0, "line1\nline2\\end\ttab");
        let json = info.to_json();
        assert!(json.contains("\\\"1\\\""));
        assert!(json.contains("line1\\nline2\\\\end\\ttab"));
        // The raw control/quote characters must not survive into the output.
        assert!(!json.contains('\n'));
        assert!(!json.contains('\t'));
    }

    #[test]
    fn status_json_escapes_other_control_chars_as_unicode() {
        let info = StatusInfo::new("a\u{01}b", 0, 0, 0, "");
        let json = info.to_json();
        assert!(json.contains("a\\u0001b"));
    }

    #[test]
    fn default_status_json_builds_a_valid_bounded_string() {
        // The handler build path must not reject the default JSON.
        let handler = ConnHandler::build(&StatusServerConfig::default());
        assert!(handler.is_ok());
    }

    #[test]
    fn config_builders_override_fields() {
        let config = StatusServerConfig::default()
            .with_io_timeout(Duration::from_millis(5))
            .with_max_connections(7)
            .with_status(StatusInfo::new("x", 1, 2, 3, "y"));
        assert_eq!(config.io_timeout, Duration::from_millis(5));
        assert_eq!(config.max_connections, 7);
        assert_eq!(config.status.protocol_version, 1);
    }
}
