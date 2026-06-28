#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! Server configuration parsing and validation. TOML-based.
//!
//! Currently this crate owns the access-control configuration ([`AccessConfig`]
//! and its resolved runtime form [`ResolvedAccess`]): the per-IP connection
//! limit, the ban list, and the optional whitelist that gate a public-facing
//! server. The larger [`AppConfig`](../ferrumc_app/struct.AppConfig.html) still
//! lives in the application crate and embeds [`AccessConfig`] as its `[access]`
//! section.

mod access;

pub use access::{
    AccessConfig, AccessConfigError, DenyReason, LoginDecision, ResolvedAccess,
    DEFAULT_PER_IP_CONNECTION_LIMIT, DEFAULT_WHITELIST_ENABLED,
};
