//! Play-state reader/writer infrastructure (M15).
//!
//! This module adds the sync, socket-free plumbing the play phase needs on top
//! of the M08 framing and M09/M11 transport, **without** any gameplay mutation:
//!
//! - [`PlayReader`] decodes serverbound [`ServerboundPlayPacket`]s from the
//!   inbound byte stream and charges each against a per-connection
//!   [`PacketBudget`] (a token bucket, ~300 frames/sec sustained) that classifies
//!   over-budget frames.
//! - [`PlayWriter`] queues clientbound [`ClientboundPlayPacket`]s into bounded
//!   per-[`OutboundPriority`] queues (`Critical > State > World > Cosmetic`) and
//!   drains them, highest priority first, into encoded frame batches bounded by a
//!   [`BatchLimits`] threshold. A full queue tail-drops with a documented policy.
//! - [`MovementCoalescer`] is the placeholder per-tick movement hook: it keeps
//!   only the latest position offered within a tick.
//! - [`DisconnectReason`]/[`DisconnectPolicy`] classify why and how a play
//!   connection is torn down.
//! - [`PlayMetrics`] are placeholder per-connection counters (no metrics
//!   backend).
//!
//! Everything here is synchronous and unit-testable without a socket; the live
//! transport (a later milestone) drives these types with the same patterns the
//! [`StatusServer`](crate::StatusServer) and [`LoginServer`](crate::LoginServer)
//! already use.
//!
//! [`ServerboundPlayPacket`]: ferrumc_proto::generated::play::ServerboundPlayPacket
//! [`ClientboundPlayPacket`]: ferrumc_proto::generated::play::ClientboundPlayPacket

mod budget;
mod coalesce;
mod disconnect;
mod metrics;
mod priority;
mod reader;
mod writer;

pub use budget::{BudgetStatus, PacketBudget, DEFAULT_PLAY_FRAME_BURST, DEFAULT_PLAY_FRAME_RATE};
pub use coalesce::{is_movement, MovementCoalescer, OfferOutcome};
pub use disconnect::{DisconnectPolicy, DisconnectReason};
pub use metrics::PlayMetrics;
pub use priority::{
    Criticality, OutboundPriority, DEFAULT_COSMETIC_CAPACITY, DEFAULT_CRITICAL_CAPACITY,
    DEFAULT_STATE_CAPACITY, DEFAULT_WORLD_CAPACITY, PRIORITY_COUNT,
};
pub use reader::{InboundPlayPacket, PlayReader};
pub use writer::{
    BatchLimits, EnqueueOutcome, PlayBatch, PlayWriter, DEFAULT_BATCH_MAX_BYTES,
    DEFAULT_BATCH_MAX_FRAMES,
};
