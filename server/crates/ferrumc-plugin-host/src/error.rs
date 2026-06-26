//! Errors the plugin host returns from registry and lifecycle operations.

use ferrumc_core::PluginId;
use ferrumc_plugin_api::PluginError;

/// A failure performing a host registry or lifecycle operation.
///
/// Variants classify *why* an operation failed so callers can react without
/// parsing strings. A plugin that panics never reaches the caller as a panic:
/// it is reported as [`HostError::Panicked`] (for explicit operations) or
/// recorded in the dispatch report (for event dispatch).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HostError {
    /// A plugin with this id is already registered.
    #[error("a plugin with id '{0}' is already registered")]
    DuplicateId(PluginId),

    /// No plugin is registered with this id.
    #[error("no plugin is registered with id '{0}'")]
    UnknownPlugin(PluginId),

    /// The plugin is registered but not currently enabled.
    #[error("plugin '{0}' is not enabled")]
    NotEnabled(PluginId),

    /// The plugin is already enabled.
    #[error("plugin '{0}' is already enabled")]
    AlreadyEnabled(PluginId),

    /// The registry is full and cannot accept another plugin.
    #[error("plugin registry is full (capacity {max})")]
    CapacityExceeded {
        /// The configured maximum number of registered plugins.
        max: usize,
    },

    /// The plugin reported a failure from a lifecycle hook.
    #[error("plugin '{id}' failed during enable")]
    PluginFailed {
        /// The plugin that failed.
        id: PluginId,
        /// The error the plugin returned.
        #[source]
        source: PluginError,
    },

    /// The plugin panicked; it has been disabled and the host kept running.
    #[error("plugin '{id}' panicked and was disabled")]
    Panicked {
        /// The plugin that panicked.
        id: PluginId,
    },
}
