//! Dynamic plugin loading: scan a directory of `cdylib` plugins, load each
//! across the narrow C ABI, and register the survivors with the host.
//!
//! This module builds on the in-process host: a loaded library is wrapped in an
//! [`adapter::LoadedPlugin`] that implements
//! [`Plugin`](ferrumc_plugin_api::Plugin), so once registered it goes through
//! exactly the same panic-catching and budget-timing machinery as a compiled-in
//! plugin (see [`PluginHost`](crate::PluginHost)).
//!
//! The C ABI itself is defined in
//! [`ferrumc_plugin_api::abi`](ferrumc_plugin_api::abi). Only one submodule
//! here, [`ffi`], performs `unsafe` work; it is annotated with a scoped
//! `#[allow(unsafe_code)]` and every block is documented. See
//! `docs/safety/ferrumc-plugin-host.md`.

mod adapter;
mod error;
mod loader;

// The sole `unsafe` module. The crate is otherwise `deny(unsafe_code)`; this is
// the single, audited place FFI happens.
#[allow(unsafe_code)]
mod ffi;

pub use error::{DirLoadReport, LoadError};
pub use loader::PluginLoader;
