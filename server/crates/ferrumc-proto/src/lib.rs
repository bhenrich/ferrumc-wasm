#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! Generated packet enums and codecs for protocol 772 (Minecraft 1.21.8).
//!
//! The wire types are emitted by `ferrumc-proto-gen` into [`generated`] and
//! verified by `cargo xtask generate --check`. The hand-written [`wire`] module
//! supplies the codec primitives the generated code calls, and [`ProtoError`]
//! classifies every decode/encode failure.

// `ferrumc-registry` is a declared dependency reserved for registry-bearing
// packets; bind it anonymously so the link is intentional until a packet
// references it directly.
use ferrumc_registry as _;

mod error;
mod wire;

pub mod generated;
pub mod types;

pub use error::ProtoError;
pub use types::BlockPosition;

/// A Minecraft protocol connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// The initial handshaking state.
    Handshake,
    /// Server list ping (status request/response).
    Status,
    /// Login negotiation.
    Login,
    /// The 1.20.2+ configuration state, between login and play.
    Configuration,
    /// The play state: the in-game world session.
    Play,
}

/// The direction a packet travels relative to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Client to server.
    Serverbound,
    /// Server to client.
    Clientbound,
}
