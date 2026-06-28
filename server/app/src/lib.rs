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
//! - [`build_command_tree`] exposes the `/spawn` + `/gamemode` command set, and
//!   [`load_plugins`] scans a directory for dynamic plugins — both public so the
//!   MVP test can assert server-side behaviour that has no fake-client carrier.

mod cli;
mod command;
mod config;
mod connection;
mod driver;
mod inventory;
mod observe;
mod plugins;
mod registries;
mod server;
mod storage_worker;
mod world;

pub use cli::{load_or_init_config, Cli};
pub use command::build_command_tree;
pub use config::AppConfig;
pub use plugins::load_plugins;
pub use server::{run, RunningServer};
