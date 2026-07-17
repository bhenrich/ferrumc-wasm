#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! # Overview
//!
//! This milestone delivers the deterministic *skeleton* of the simulation
//! layer: the messages crossing the sim boundary, the tick counter that drives
//! it, and one shard that applies inputs at tick boundaries. There is no
//! networking, no storage handle, and no plugin dispatch here yet.
//!
//! The pieces fit together as a one-way pipeline driven entirely by explicit
//! `tick` calls (never a wall clock):
//!
//! ```text
//!   GameInput  --enqueue-->  SimShard inbox  --run_tick-->  GameOutput
//!                                  ^                 ^
//!                                  |                 |
//!                          (bounded, reject     (TickCoordinator
//!                           backpressure)        advances the Tick)
//! ```
//!
//! - [`GameInput`] / [`GameOutput`] — the minimal typed messages.
//! - [`TickCoordinator`] / [`TickRate`] — the authoritative [`Tick`] counter,
//!   advanced one tick at a time with **no catch-up**.
//! - [`SimShard`] — owns a bounded inbox, the player set, and the resident
//!   chunks, applying inputs only at tick boundaries.
//! - [`ShardPartitioner`] / [`ShardOwnershipMap`] — deterministic typed 8x8
//!   region partitioning and overlap-free runtime ownership claims.
//! - [`ShardLifecycle`] — the explicit lifecycle of a future scheduled shard,
//!   without introducing a worker pool.
//! - [`SimHarness`] / [`TickOutcome`] — a deterministic, wall-clock-free driver
//!   tying a coordinator to one shard, used by tests and replay.
//!
//! # Chunk residency
//!
//! A shard owns its chunks in a [`LoadedChunkMap`], governed by
//! [`ChunkTicket`]s: a chunk is resident exactly while it holds a ticket.
//! [`LoadedChunkMap::acquire`] runs the load-or-generate flow
//! ([`load_or_generate`]: try the [`WorldStore`](ferrumc_storage::WorldStore),
//! else generate with [`FlatWorldGenerator`](ferrumc_world::FlatWorldGenerator)),
//! [`LoadedChunkMap::release`] drops tickets and unloads, and
//! [`LoadedChunkMap::take_dirty`] hands changed chunks off as save records. The
//! [`SpawnChunkTickets`] set keeps the world spawn resident. The map collects
//! dirty chunks but never persists them — flush *policy* is the caller's.
//!
//! [`Tick`]: ferrumc_core::Tick

mod coordinator;
mod error;
mod harness;
mod loaded;
mod message;
mod mutation;
mod ownership;
mod region;
mod shard;
mod spawn;
mod ticket;
mod time;

pub use coordinator::{TickCoordinator, TickRate};
pub use error::{SimError, SimResult};
pub use harness::{SimHarness, TickOutcome};
pub use loaded::{
    load_or_generate, ChunkAcquired, ChunkProvenance, LoadedChunkMap, TicketRelease,
    CHUNK_SCHEMA_VERSION, OVERLAY_SCHEMA_VERSION,
};
pub use message::{GameInput, GameOutput};
// Only the cause is public: the structured `MutationResult` stays internal to
// the shard so the app never confuses it with `ferrumc_observability::MutationResult`.
pub use mutation::{MutationCause, PendingMutation};
pub use ownership::{
    ShardId, ShardLifecycle, ShardLifecycleState, ShardOwnership, ShardOwnershipMap,
    ShardPartitioner, ShardRegion, MAX_SHARD_COORD, MIN_SHARD_COORD, SHARD_WIDTH_CHUNKS,
};
pub use region::{RegionLimits, RegionOp};
pub use shard::SimShard;
pub use time::{WorldTime, DAY_LENGTH_TICKS, TIME_DAY, TIME_MIDNIGHT, TIME_NIGHT, TIME_NOON};

/// Re-exported from [`ferrumc_world`] so callers can name the block-state ids
/// carried by [`GameOutput::BlockChanged`] / produced by block-edit inputs
/// without taking a direct dependency on the world crate.
pub use ferrumc_world::BlockStateId;
/// Re-exported from [`ferrumc_world`] so the session layer can name the sign
/// block-entity carried by [`GameOutput::SignUpdated`] (to encode its network
/// NBT) without depending on the world crate directly.
pub use ferrumc_world::{Sign, SignFace, SignKind, SIGN_LINES};
pub use spawn::SpawnChunkTickets;
pub use ticket::{ChunkTicket, TicketLevel, TicketReason};
