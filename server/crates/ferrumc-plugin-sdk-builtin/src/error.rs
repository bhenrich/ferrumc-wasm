//! Classified failures from a built-in plugin callback.

use core::fmt;

use ferrumc_plugin_sdk::{Capability, PluginError};

/// A built-in plugin could not complete one lifecycle or event callback.
///
/// The caller must discard the callback's staged mutating host effects for
/// every variant; reads need no commit and diagnostics may remain as
/// observability. [`Cooperative`](Self::Cooperative) leaves an existing instance
/// active, while [`Panicked`](Self::Panicked) poisons it. An error from a
/// block-place, block-break, chat, or interaction decision callback also
/// requires the caller to deny the attempted action without feedback.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BuiltinCallbackError {
    /// The plugin returned a classified cooperative callback error.
    Cooperative(PluginError),
    /// An unwinding panic was caught inside the packaging adapter.
    Panicked,
    /// A prior callback panic made the plugin instance unusable.
    Poisoned,
    /// The callback or its backend lacks a required effective grant.
    CapabilityDenied(Capability),
    /// A future shared-SDK event has no route in this adapter version.
    UnsupportedEvent,
}

impl fmt::Display for BuiltinCallbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cooperative(error) => write!(formatter, "plugin callback failed: {error}"),
            Self::Panicked => formatter.write_str("plugin callback panicked"),
            Self::Poisoned => formatter.write_str("plugin instance is poisoned"),
            Self::CapabilityDenied(capability) => {
                write!(
                    formatter,
                    "required capability {capability} is unavailable for plugin callback"
                )
            }
            Self::UnsupportedEvent => {
                formatter.write_str("event has no built-in adapter callback route")
            }
        }
    }
}

impl std::error::Error for BuiltinCallbackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cooperative(error) => Some(error),
            Self::Panicked
            | Self::Poisoned
            | Self::CapabilityDenied(_)
            | Self::UnsupportedEvent => None,
        }
    }
}

impl From<PluginError> for BuiltinCallbackError {
    fn from(error: PluginError) -> Self {
        Self::Cooperative(error)
    }
}
