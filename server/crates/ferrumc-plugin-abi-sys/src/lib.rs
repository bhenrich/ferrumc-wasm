#![doc = include_str!("../README.md")]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

mod error;
mod export;
mod invoke;
mod loader;
mod raw;
mod values;

pub use error::{AbiRecord, LoadError, ValidationError};
pub use export::{
    plugin_descriptor_v1, plugin_functions_v1, ExportedPluginDescriptorV1, PluginBridge,
    PluginCall, PluginCallError, PluginEvent,
};
pub use invoke::{
    CallbackError, HostCallOutcome, HostServices, InvocationLimits, PluginInstance,
    DEFAULT_OUTPUT_CAPACITY, DEFAULT_PAYLOAD_LIMIT, MAX_OUTPUT_CAPACITY, MAX_PAYLOAD_LIMIT,
};
pub use loader::{load, LoadedAbiPlugin};
pub use values::{
    OwnedCommand, OwnedEvent, OwnedHostRequest, OwnedPluginMetadata, PluginSemanticVersion,
};
