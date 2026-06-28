#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! Server configuration parsing and validation. TOML-based.
//!
//! This crate owns the standalone, reusable config shapes:
//!
//! - the access-control configuration ([`AccessConfig`] and its resolved runtime
//!   form [`ResolvedAccess`]): the per-IP connection limit, the ban list, and the
//!   optional whitelist that gate a public-facing server;
//! - the serverbound [`PacketBudgetConfig`]: the token-bucket sustained rate and
//!   burst that throttle a flooding client.
//!
//! The larger [`AppConfig`](../ferrumc_app/struct.AppConfig.html) still lives in
//! the application crate and embeds these as its `[access]` and `[budget]`
//! sections.

mod access;
mod budget;

pub use access::{
    AccessConfig, AccessConfigError, DenyReason, LoginDecision, ResolvedAccess,
    DEFAULT_PER_IP_CONNECTION_LIMIT, DEFAULT_WHITELIST_ENABLED,
};
pub use budget::{
    PacketBudgetConfig, PacketBudgetConfigError, DEFAULT_PLAY_FRAME_BURST, DEFAULT_PLAY_FRAME_RATE,
};
