//! Classifying error types returned across the plugin-facing API surface.
//!
//! Each error type names *why* an operation failed so the host (and the plugin)
//! can react programmatically instead of parsing message strings. None of these
//! types expose raw internals.

use crate::capability::Capability;

/// A plugin attempted an operation it was not granted the [`Capability`] for.
///
/// Capability checks are enforced by the contexts the host hands to a plugin
/// (see [`crate::SetupContext`] and [`crate::EventContext`]): requesting a
/// facade the plugin lacks the capability for fails with this error instead of
/// returning the facade.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("plugin lacks the required capability: {capability}")]
pub struct CapabilityError {
    capability: Capability,
}

impl CapabilityError {
    /// Builds an error reporting that `capability` was required but not granted.
    pub const fn missing(capability: Capability) -> Self {
        Self { capability }
    }

    /// Returns the capability that was required but not granted.
    pub const fn capability(&self) -> Capability {
        self.capability
    }
}

/// A failure interacting with a plugin's namespaced key-value storage.
///
/// Length limits classify rejected input up front (see [`crate::MAX_KEY_LEN`]
/// and [`crate::MAX_VALUE_LEN`]) so a backend never works on unbounded keys or
/// values; [`StorageError::Backend`] carries any lower-level failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum StorageError {
    /// A key was the empty string, which is never a valid storage key.
    #[error("storage key must not be empty")]
    EmptyKey,

    /// A key exceeded the maximum accepted length.
    #[error("storage key length {len} exceeds the maximum of {max} bytes")]
    KeyTooLong {
        /// The rejected key's length, in bytes.
        len: usize,
        /// The maximum accepted key length, in bytes.
        max: usize,
    },

    /// A value exceeded the maximum accepted length.
    #[error("storage value length {len} exceeds the maximum of {max} bytes")]
    ValueTooLong {
        /// The rejected value's length, in bytes.
        len: usize,
        /// The maximum accepted value length, in bytes.
        max: usize,
    },

    /// The underlying storage backend failed for an implementation-specific
    /// reason described by the message.
    #[error("storage backend failure: {0}")]
    Backend(String),
}

impl StorageError {
    /// Builds a [`StorageError::Backend`] from a human-readable message.
    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend(message.into())
    }
}

/// A failure submitting a mutation intent through a [`crate::CommandSink`].
///
/// Intents are queued for the simulation to apply at a tick boundary; they are
/// never applied directly. Submission can fail if the queue is full or the
/// intent is rejected by policy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IntentError {
    /// The intent queue is full; the caller should retry on a later tick.
    #[error("intent queue is full")]
    QueueFull,

    /// The intent was rejected for the given reason.
    #[error("intent rejected: {0}")]
    Rejected(String),
}

impl IntentError {
    /// Builds an [`IntentError::Rejected`] from a human-readable reason.
    pub fn rejected(reason: impl Into<String>) -> Self {
        Self::Rejected(reason.into())
    }
}

/// An error a plugin reports from a lifecycle hook (for example
/// [`crate::Plugin::on_enable`]).
///
/// The host treats a returned `PluginError` as an enable failure and leaves the
/// plugin disabled; it never crashes the host.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PluginError {
    /// The plugin could not complete its setup for the given reason.
    #[error("plugin setup failed: {reason}")]
    Setup {
        /// Human-readable description of what went wrong during setup.
        reason: String,
    },

    /// Setup required a capability the plugin was not granted.
    #[error(transparent)]
    Capability(#[from] CapabilityError),

    /// A storage operation during setup failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl PluginError {
    /// Builds a [`PluginError::Setup`] from a human-readable reason.
    pub fn setup(reason: impl Into<String>) -> Self {
        Self::Setup {
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_error_reports_capability() {
        let err = CapabilityError::missing(Capability::Storage);
        assert_eq!(err.capability(), Capability::Storage);
        assert!(err.to_string().contains("storage"));
    }

    #[test]
    fn plugin_error_converts_from_sources() {
        let from_cap: PluginError = CapabilityError::missing(Capability::ReadWorld).into();
        assert!(matches!(from_cap, PluginError::Capability(_)));

        let from_storage: PluginError = StorageError::EmptyKey.into();
        assert!(matches!(from_storage, PluginError::Storage(_)));

        let setup = PluginError::setup("boom");
        assert_eq!(setup.to_string(), "plugin setup failed: boom");
    }

    #[test]
    fn storage_error_display_is_classified() {
        assert_eq!(
            StorageError::KeyTooLong { len: 9, max: 4 }.to_string(),
            "storage key length 9 exceeds the maximum of 4 bytes"
        );
        assert_eq!(
            StorageError::backend("disk gone").to_string(),
            "storage backend failure: disk gone"
        );
    }
}
