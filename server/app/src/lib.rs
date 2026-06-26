#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Application entry point. Wires config, the simulation, networking, and the
//! shutdown path into the first runnable vertical slice: a client connects, logs
//! in offline, enters play, and receives a `JoinGame`, a position sync, and the
//! flat-world spawn chunks.
//!
//! - [`AppConfig`] is the minimal, validated server configuration.
//! - [`run`] builds the world, starts the simulation, binds the listener, and
//!   begins accepting connections, returning a [`RunningServer`].

mod config;
mod connection;
mod driver;
mod server;
mod world;

pub use config::AppConfig;
pub use server::{run, RunningServer};
