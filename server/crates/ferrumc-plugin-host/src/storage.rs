//! The multi-tenant storage backend and the per-plugin namespaced view.
//!
//! The host owns a [`PluginStorageBackend`] keyed by [`PluginId`]. For each
//! plugin call it wraps that backend in a [`NamespacedStorage`] bound to the
//! calling plugin's id, which it hands to the plugin as a
//! [`PluginStorageApi`](ferrumc_plugin_api::PluginStorageApi). Because the
//! plugin id comes from the host and not the plugin, a plugin can only ever
//! reach its own namespace.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use ferrumc_core::PluginId;
use ferrumc_plugin_api::{PluginStorageApi, StorageError, MAX_KEY_LEN, MAX_VALUE_LEN};

/// The per-plugin map of key-value entries, partitioned by [`PluginId`].
type NamespaceMap = HashMap<PluginId, HashMap<String, Vec<u8>>>;

/// A backend that stores key-value data partitioned by [`PluginId`].
///
/// Every operation takes the owning plugin's id explicitly, so the namespace is
/// chosen by the caller (the host), never by plugin code. Implementations must
/// be `Send + Sync` so the host can move between worker threads.
pub trait PluginStorageBackend: Send + Sync {
    /// Returns the value for `key` in `plugin`'s namespace, or `Ok(None)`.
    fn get(&self, plugin: &PluginId, key: &str) -> Result<Option<Vec<u8>>, StorageError>;

    /// Stores `value` under `key` in `plugin`'s namespace.
    fn put(&self, plugin: &PluginId, key: &str, value: &[u8]) -> Result<(), StorageError>;

    /// Removes `key` from `plugin`'s namespace, returning whether it existed.
    fn delete(&self, plugin: &PluginId, key: &str) -> Result<bool, StorageError>;

    /// Returns every key set in `plugin`'s namespace.
    fn keys(&self, plugin: &PluginId) -> Result<Vec<String>, StorageError>;
}

/// Validates a key, returning the classifying [`StorageError`] if it is invalid.
fn check_key(key: &str) -> Result<(), StorageError> {
    if key.is_empty() {
        return Err(StorageError::EmptyKey);
    }
    if key.len() > MAX_KEY_LEN {
        return Err(StorageError::KeyTooLong {
            len: key.len(),
            max: MAX_KEY_LEN,
        });
    }
    Ok(())
}

/// Validates a value length, returning the classifying [`StorageError`] if it is
/// too large.
fn check_value(value: &[u8]) -> Result<(), StorageError> {
    if value.len() > MAX_VALUE_LEN {
        return Err(StorageError::ValueTooLong {
            len: value.len(),
            max: MAX_VALUE_LEN,
        });
    }
    Ok(())
}

/// An in-memory [`PluginStorageBackend`] suitable for tests and a default host.
///
/// Data lives in a single [`Mutex`]-guarded map; the mutex is held only for the
/// duration of a synchronous map operation, never across an `.await`. Cloning
/// shares the same underlying storage, which lets a test observe what plugins
/// wrote.
#[derive(Debug, Clone, Default)]
pub struct InMemoryPluginStorage {
    data: Arc<Mutex<NamespaceMap>>,
}

impl InMemoryPluginStorage {
    /// Creates an empty in-memory storage backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the inner map, converting mutex poisoning into a classifying error
    /// rather than panicking.
    fn lock(&self) -> Result<MutexGuard<'_, NamespaceMap>, StorageError> {
        self.data
            .lock()
            .map_err(|_| StorageError::backend("plugin storage mutex poisoned"))
    }
}

impl PluginStorageBackend for InMemoryPluginStorage {
    fn get(&self, plugin: &PluginId, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        check_key(key)?;
        let map = self.lock()?;
        Ok(map.get(plugin).and_then(|ns| ns.get(key).cloned()))
    }

    fn put(&self, plugin: &PluginId, key: &str, value: &[u8]) -> Result<(), StorageError> {
        check_key(key)?;
        check_value(value)?;
        let mut map = self.lock()?;
        map.entry(plugin.clone())
            .or_default()
            .insert(key.to_owned(), value.to_vec());
        Ok(())
    }

    fn delete(&self, plugin: &PluginId, key: &str) -> Result<bool, StorageError> {
        check_key(key)?;
        let mut map = self.lock()?;
        Ok(map
            .get_mut(plugin)
            .is_some_and(|ns| ns.remove(key).is_some()))
    }

    fn keys(&self, plugin: &PluginId) -> Result<Vec<String>, StorageError> {
        let map = self.lock()?;
        Ok(map
            .get(plugin)
            .map(|ns| ns.keys().cloned().collect())
            .unwrap_or_default())
    }
}

/// A per-plugin view over a [`PluginStorageBackend`] that implements the
/// plugin-facing [`PluginStorageApi`].
///
/// It captures the owning plugin's id and forwards every call to the backend
/// with that id, so the plugin cannot reach any other namespace.
pub(crate) struct NamespacedStorage<'a> {
    backend: &'a dyn PluginStorageBackend,
    plugin: PluginId,
}

impl<'a> NamespacedStorage<'a> {
    /// Binds `backend` to `plugin`'s namespace.
    pub(crate) fn new(backend: &'a dyn PluginStorageBackend, plugin: PluginId) -> Self {
        Self { backend, plugin }
    }
}

impl PluginStorageApi for NamespacedStorage<'_> {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        self.backend.get(&self.plugin, key)
    }

    fn put(&self, key: &str, value: &[u8]) -> Result<(), StorageError> {
        self.backend.put(&self.plugin, key, value)
    }

    fn delete(&self, key: &str) -> Result<bool, StorageError> {
        self.backend.delete(&self.plugin, key)
    }

    fn keys(&self) -> Result<Vec<String>, StorageError> {
        self.backend.keys(&self.plugin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(s: &str) -> PluginId {
        PluginId::new(s)
    }

    #[test]
    fn namespaces_are_isolated() {
        let store = InMemoryPluginStorage::new();
        let a = pid("a");
        let b = pid("b");

        store.put(&a, "k", b"from-a").expect("put a");
        store.put(&b, "k", b"from-b").expect("put b");

        assert_eq!(
            store.get(&a, "k").expect("get a").as_deref(),
            Some(&b"from-a"[..])
        );
        assert_eq!(
            store.get(&b, "k").expect("get b").as_deref(),
            Some(&b"from-b"[..])
        );
        // A namespace never sees a key it did not set.
        assert_eq!(store.get(&a, "other").expect("get a other"), None);
    }

    #[test]
    fn namespaced_view_forwards_to_its_namespace_only() {
        let store = InMemoryPluginStorage::new();
        let a = pid("a");
        let b = pid("b");

        {
            let view_a = NamespacedStorage::new(&store, a.clone());
            view_a.put("secret", b"alpha").expect("put via view");
            assert_eq!(view_a.keys().expect("keys").len(), 1);
        }

        // The data landed in a's namespace, not b's.
        assert_eq!(
            store.get(&a, "secret").expect("get a").as_deref(),
            Some(&b"alpha"[..])
        );
        assert_eq!(store.get(&b, "secret").expect("get b"), None);
    }

    #[test]
    fn rejects_invalid_keys_and_values() {
        let store = InMemoryPluginStorage::new();
        let a = pid("a");

        assert_eq!(store.get(&a, "").unwrap_err(), StorageError::EmptyKey);
        let long_key = "k".repeat(MAX_KEY_LEN + 1);
        assert!(matches!(
            store.put(&a, &long_key, b"v").unwrap_err(),
            StorageError::KeyTooLong { .. }
        ));
        let big = vec![0u8; MAX_VALUE_LEN + 1];
        assert!(matches!(
            store.put(&a, "k", &big).unwrap_err(),
            StorageError::ValueTooLong { .. }
        ));
    }

    #[test]
    fn delete_reports_presence() {
        let store = InMemoryPluginStorage::new();
        let a = pid("a");
        store.put(&a, "k", b"v").expect("put");
        assert!(store.delete(&a, "k").expect("delete present"));
        assert!(!store.delete(&a, "k").expect("delete absent"));
    }
}
