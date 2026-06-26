//! The namespaced key-value storage facade exposed to plugins.

use crate::error::StorageError;

/// Maximum accepted storage key length, in bytes.
///
/// Keys arrive from plugin code and are bounded so a backend never indexes on
/// an unbounded string.
pub const MAX_KEY_LEN: usize = 256;

/// Maximum accepted storage value length, in bytes.
///
/// Values are bounded so a single entry cannot grow without limit.
pub const MAX_VALUE_LEN: usize = 64 * 1024;

/// A plugin's private, namespaced key-value store.
///
/// Every handle is bound by the host to a single plugin's namespace, so a
/// plugin can only read and write its own data and never names another plugin's
/// namespace — the plugin id is supplied by the host, not by the plugin. This
/// trait is a shell; the host provides the concrete, isolating implementation.
///
/// Implementations reject an empty key with [`StorageError::EmptyKey`], a key
/// longer than [`MAX_KEY_LEN`] with [`StorageError::KeyTooLong`], and a value
/// longer than [`MAX_VALUE_LEN`] with [`StorageError::ValueTooLong`]. Reading an
/// unset key returns `Ok(None)`, never an error.
pub trait PluginStorageApi {
    /// Returns the value stored under `key`, or `Ok(None)` if it is unset.
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError>;

    /// Stores `value` under `key`, overwriting any existing value.
    fn put(&self, key: &str, value: &[u8]) -> Result<(), StorageError>;

    /// Removes `key`, returning `Ok(true)` if a value was present.
    fn delete(&self, key: &str) -> Result<bool, StorageError>;

    /// Returns every key currently set in this namespace, in unspecified order.
    fn keys(&self) -> Result<Vec<String>, StorageError>;
}
