#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod lifecycle;
mod loader;
mod manifest;
mod sha256;

pub use error::{LoaderConfigError, PluginLoadError};
pub use ferrumc_plugin_abi::FcStatus;
pub use ferrumc_plugin_abi_sys::{
    CallbackError, HostCallOutcome, HostServices, InvocationLimits, OwnedCommand, OwnedEvent,
    OwnedHostRequest, OwnedPluginMetadata,
};
pub use lifecycle::{ActivePlugin, LoadedPlugin, LoadedPlugins};
pub use loader::{
    LoaderConfig, PluginLoader, HOST_TARGET, MAX_DIRECTORY_ENTRIES, MAX_MANIFEST_BYTES,
    MAX_PLUGINS, SERVER_API_VERSION,
};
pub use manifest::{ManifestError, PluginCapabilities, PluginCapability, PluginManifest};
