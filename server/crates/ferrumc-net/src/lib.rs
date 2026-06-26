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
//!
//! Everything here is sync and performs no I/O: the Tokio reader/writer tasks
//! that drive these types arrive in M09.

mod error;
mod inbound;
mod limits;
mod outbound;
mod state;

pub use error::{DecodeError, DisconnectClass, EncodeError};
pub use inbound::{decode_inbound_frame, DecodeOutcome, InboundDecoder, InboundPacket};
pub use limits::{
    ConnectionLimits, DEFAULT_CONFIGURATION_MAX_FRAME, DEFAULT_HANDSHAKE_MAX_FRAME,
    DEFAULT_LOGIN_MAX_FRAME, DEFAULT_PLAY_MAX_FRAME, DEFAULT_STATUS_MAX_FRAME,
    MAX_LENGTH_PREFIX_BYTES,
};
pub use outbound::{OutboundEncoder, OutboundPacket};
pub use state::ConnectionState;
