#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! Connection state machine and frame-decoding integration for protocol 772
//! (Minecraft 1.21.8).
//!
//! This crate sits between the raw socket (M09) and `ferrumc-proto`: it tracks
//! the per-connection [`ConnectionState`], enforces per-state hostile-input
//! caps via [`ConnectionLimits`], and turns length-delimited byte frames into
//! typed packets (and back).
//!
//! - [`InboundDecoder`] accumulates serverbound bytes and yields typed
//!   [`InboundPacket`]s one frame at a time. An incomplete frame is reported as
//!   [`DecodeOutcome::NeedMore`], never an error; genuine corruption is a
//!   [`DecodeError`] that classifies into a [`DisconnectClass`] for the caller
//!   to act on.
//! - [`OutboundEncoder`] serializes clientbound [`OutboundPacket`]s into
//!   length-delimited frames, bounded by the same caps.
//! - [`CompressionState`] applies the post-`SetCompression` `data_length`
//!   framing: it `zlib`-compresses packets at or above a threshold, passes
//!   smaller ones through verbatim, and decompresses inbound frames behind a
//!   decompressed-output cap that defends against zip bombs.
//!
//! The framing types above are sync and perform no I/O. Two live Tokio paths
//! drive them over a real socket, both via the shared bounded acceptor:
//!
//! - [`StatusServer`] (M09): the connection-per-task server-list status-ping
//!   exchange.
//! - [`LoginServer`] (M11): offline-mode login through the configuration phase
//!   into the play state. It assigns each player the canonical Java-compatible
//!   identity with [`ferrumc_core::PlayerId::offline`] and reuses the
//!   [`CompressionState`] framing when a compression threshold is configured.
//!
//! The [`play`] module (M15) adds the play-phase reader/writer infrastructure on
//! top of the framing: [`PlayReader`] (budgeted serverbound decode), [`PlayWriter`]
//! (bounded per-[`OutboundPriority`] queues drained into batches), the
//! [`MovementCoalescer`] hook, the [`DisconnectReason`] policy, and placeholder
//! [`PlayMetrics`]. It is pure logic — no gameplay mutation and no socket.

mod accept;
mod compression;
mod error;
mod inbound;
mod ip_limit;
mod limits;
mod login;
mod offline;
mod outbound;
mod play;
mod server;
mod state;

pub use compression::{CompressionState, DEFAULT_MAX_DECOMPRESSED};
pub use error::{
    CompressionError, DecodeError, DisconnectClass, EncodeError, FrameDecodeError, FrameEncodeError,
};
pub use inbound::{decode_inbound_frame, DecodeOutcome, InboundDecoder, InboundPacket};
pub use ip_limit::{IpConnectionGuard, PerIpConnections};
pub use limits::{
    ConnectionLimits, DEFAULT_CONFIGURATION_MAX_FRAME, DEFAULT_HANDSHAKE_MAX_FRAME,
    DEFAULT_LOGIN_MAX_FRAME, DEFAULT_PLAY_MAX_FRAME, DEFAULT_STATUS_MAX_FRAME,
    MAX_LENGTH_PREFIX_BYTES,
};
pub use login::{LoginFlowError, LoginServer, LoginServerConfig, DEFAULT_KEEP_ALIVE_ID};
pub use offline::offline_uuid;
pub use outbound::{OutboundEncoder, OutboundPacket};
pub use play::{
    is_movement, BatchLimits, BudgetStatus, Criticality, DisconnectPolicy, DisconnectReason,
    EnqueueOutcome, InboundPlayPacket, MovementCoalescer, OfferOutcome, OutboundPriority,
    PacketBudget, PlayBatch, PlayMetrics, PlayReader, PlayWriter, DEFAULT_BATCH_MAX_BYTES,
    DEFAULT_BATCH_MAX_FRAMES, DEFAULT_COSMETIC_CAPACITY, DEFAULT_CRITICAL_CAPACITY,
    DEFAULT_PLAY_FRAME_BURST, DEFAULT_PLAY_FRAME_RATE, DEFAULT_STATE_CAPACITY,
    DEFAULT_WORLD_CAPACITY, PRIORITY_COUNT,
};
pub use server::{
    StatusInfo, StatusServer, StatusServerConfig, DEFAULT_IO_TIMEOUT, DEFAULT_MAX_CONNECTIONS,
};
pub use state::ConnectionState;
