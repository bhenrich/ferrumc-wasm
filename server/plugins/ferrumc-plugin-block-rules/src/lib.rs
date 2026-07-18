#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! The block-rules sample plugin.
//!
//! One pure placement policy powers two adapters:
//!
//! - the default `builtin` feature preserves the established in-process
//!   [`BlockRulesPlugin`] API used by `ferrumc-app`;
//! - the `dynamic` feature packages the same rules through the shared plugin
//!   SDK and exports the modern trusted-native ABI-v1 entrypoint.
//!
//! Build only the dynamic artifact with
//! `--no-default-features --features dynamic`. The audited dynamic SDK owns
//! the exported symbol, so this plugin crate contains no authored unsafe code.

mod policy;

#[cfg(feature = "dynamic")]
mod dynamic;
#[cfg(feature = "builtin")]
mod plugin;

#[cfg(feature = "builtin")]
pub use plugin::BlockRulesPlugin;
pub use policy::{
    DENIED_BLOCK_STATE_ID, GLASS_BLOCK_STATE_ID, PLUGIN_ID, PLUGIN_NAME,
    TINTED_GLASS_BLOCK_STATE_ID,
};

#[cfg(feature = "dynamic")]
ferrumc_plugin_sdk_dynamic::export_plugin!(crate::dynamic::SdkBlockRulesPlugin);
