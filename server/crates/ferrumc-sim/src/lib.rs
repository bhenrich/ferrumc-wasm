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
//! - [`SimShard`] — owns a bounded inbox and the player set, applying inputs
//!   only at tick boundaries.
//! - [`SimHarness`] / [`TickOutcome`] — a deterministic, wall-clock-free driver
//!   tying a coordinator to one shard, used by tests and replay.
//!
//! [`Tick`]: ferrumc_core::Tick

// Mandated crate-map dependency: the simulation layer owns the world model
// (chunks/entities). This skeleton milestone does not touch chunk data yet, so
// the crate is bound anonymously to keep the dependency intentional rather than
// dead weight (mirrors how ferrumc-math binds ferrumc-core).
use ferrumc_world as _;

mod coordinator;
mod error;
mod harness;
mod message;
mod shard;

pub use coordinator::{TickCoordinator, TickRate};
pub use error::{SimError, SimResult};
pub use harness::{SimHarness, TickOutcome};
pub use message::{GameInput, GameOutput};
pub use shard::SimShard;
