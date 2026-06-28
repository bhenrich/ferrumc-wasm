#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! # Overview
//!
//! This crate is the bridge between networking and simulation. It owns the
//! player<->shard mapping and moves messages — never raw state — across the
//! boundary in both directions:
//!
//! ```text
//!   NetEvent  --net_event_to_input-->  GameInput  --bounded mpsc-->  shard inbox
//!                                                                          |
//!   client  <--bounded mpsc--  OutboundMessage  <--output_to_clientbound--
//! ```
//!
//! The per-player outbound channel carries an [`OutboundMessage`] — a
//! [`ClientboundPlayPacket`](ferrumc_proto::generated::play::ClientboundPlayPacket)
//! tagged with the [`Criticality`](ferrumc_net::Criticality) and
//! [`OutboundPriority`](ferrumc_net::OutboundPriority) the router assigns it at the
//! send site — so the connection writer never re-infers either from packet type.
//!
//! The [`SessionRouter`] is the only component that knows where each player
//! lives. It holds no [`SimShard`](ferrumc_sim::SimShard), chunk, socket, or
//! database handle: it talks to shards and connections exclusively over bounded
//! [`tokio::sync::mpsc`] channels, using non-blocking sends so it never stalls
//! the tick loop. A full channel is surfaced as a classified [`SessionError`],
//! never silently dropped.
//!
//! - [`SessionRouter`] / [`PlayerSessionHandle`] — the mapping and the
//!   per-connection handle.
//! - [`NetEvent`] — the network-side input vocabulary.
//! - [`net_event_to_input`] / [`output_to_clientbound`] / [`shard_for_position`]
//!   — the pure translation and routing-policy functions.
//! - [`SessionError`] — the classifying error type.
//!
//! # Scope (this milestone)
//!
//! The translation is deliberately minimal: serverbound movement maps to a
//! [`GameInput::PlayerMove`](ferrumc_sim::GameInput) and the clientbound shells
//! carry placeholders for state the router does not yet own (entity ids, teleport
//! ids). A player stays bound to the shard they joined; cross-shard handoff,
//! richer events, and fully populated packets arrive in later milestones.

mod error;
mod event;
mod outbound;
mod router;
mod text;
mod translate;

pub use error::SessionError;
pub use event::NetEvent;
pub use outbound::OutboundMessage;
pub use router::{
    PlayerSessionHandle, SessionRouter, DEFAULT_OUTBOUND_CAPACITY, DEFAULT_SHARD_INPUT_CAPACITY,
    DEFAULT_VIEW_DISTANCE,
};
pub use text::system_chat;
pub use translate::{
    net_event_to_input, output_to_clientbound, player_info_add, shard_for_position,
    use_item_on_face, use_item_on_target, PLAYER_INFO_ADD,
};
