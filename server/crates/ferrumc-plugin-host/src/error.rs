//! Errors the plugin host returns from registry and lifecycle operations.

use ferrumc_core::PluginId;
use ferrumc_plugin_api::{Capability, PluginError};
use ferrumc_plugin_loader::CallbackError;

/// A lifecycle callback exposed by a trusted native plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeLifecycleHook {
    /// The initialization callback run when the plugin is enabled.
    Initialize,
    /// The shutdown callback run when the plugin is disabled.
    Shutdown,
}

impl core::fmt::Display for NativeLifecycleHook {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Initialize => formatter.write_str("initialize"),
            Self::Shutdown => formatter.write_str("shutdown"),
        }
    }
}

/// A failure performing a host registry or lifecycle operation.
///
/// Variants classify *why* an operation failed so callers can react without
/// parsing strings. An unwinding compiled-in plugin is reported as
/// [`HostError::Panicked`] (for explicit operations) or recorded in the
/// dispatch report; a later enable attempt returns
/// [`HostError::PanicDisabled`]. Trusted native lifecycle failures cross the
/// audited boundary as [`HostError::NativeLifecycle`].
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

    /// A compiled-in plugin previously unwound; its retained instance cannot
    /// be re-enabled for this host registration.
    #[error(
        "compiled-in plugin '{0}' previously panicked and cannot be re-enabled for this host registration"
    )]
    PanicDisabled(PluginId),

    /// A trusted native plugin reported `FC_PLUGIN_PANIC`; its retired instance
    /// cannot be re-enabled for this host registration.
    #[error(
        "trusted native plugin '{0}' reported FC_PLUGIN_PANIC and cannot be re-enabled for this host registration"
    )]
    NativePanicDisabled(PluginId),

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

    /// A compiled-in Rust plugin unwound; it has been disabled.
    #[error("plugin '{id}' panicked and was disabled")]
    Panicked {
        /// The compiled-in plugin that unwound.
        id: PluginId,
    },

    /// A trusted native plugin requested a host facade this host version does
    /// not implement.
    #[error("trusted native plugin '{id}' requests unsupported host capability '{capability}'")]
    UnsupportedNativeCapability {
        /// The plugin whose manifest requested the unsupported facade.
        id: PluginId,
        /// The unsupported capability.
        capability: Capability,
    },

    /// A trusted native lifecycle callback could not be completed.
    #[error("trusted native plugin '{id}' failed during {hook}: {source}")]
    NativeLifecycle {
        /// The plugin whose callback failed.
        id: PluginId,
        /// The lifecycle callback being run.
        hook: NativeLifecycleHook,
        /// The typed callback-boundary failure.
        #[source]
        source: CallbackError,
    },
}
