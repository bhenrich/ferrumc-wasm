#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! Compiled-in packaging for the shared `FerrumC` plugin SDK.

mod adapter;
mod error;
mod services;

pub use adapter::{BuiltinPluginFactory, BuiltinPluginInstance, CallbackOutcome};
pub use error::BuiltinCallbackError;
