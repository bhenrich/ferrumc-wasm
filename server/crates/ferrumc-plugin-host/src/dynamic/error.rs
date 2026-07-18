//! Classified failures from loading plugins out of dynamic libraries.

use std::io;
use std::path::PathBuf;

use ferrumc_core::PluginId;

use crate::error::HostError;

/// Why loading a single plugin library failed.
///
/// Each variant names *where* in the load pipeline the failure happened — the
/// library could not be opened, the entrypoint was missing, the ABI version
/// disagreed, the metadata was malformed, or the host refused to register it —
/// so callers can react (log, skip, abort) without parsing message strings.
///
/// This type is intentionally **not** `Clone`/`PartialEq`: it carries the
/// underlying [`libloading::Error`] and [`std::io::Error`], which are neither.
/// Match on the variant in tests with `matches!`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoadError {
    /// The dynamic library could not be opened (missing file, not a library,
    /// wrong architecture, unresolved dependency, ...).
    #[error("failed to open plugin library '{}': {source}", path.display())]
    Open {
        /// The library path that failed to open.
        path: PathBuf,
        /// The underlying loader error.
        #[source]
        source: libloading::Error,
    },

    /// The library opened but does not export the expected entrypoint symbol.
    #[error("plugin library '{}' is missing entrypoint symbol '{symbol}'", path.display())]
    MissingEntrypoint {
        /// The library path.
        path: PathBuf,
        /// The symbol that could not be resolved.
        symbol: String,
        /// The underlying loader error.
        #[source]
        source: libloading::Error,
    },

    /// The entrypoint returned a null pointer instead of a vtable.
    #[error("plugin library '{}' returned a null vtable", path.display())]
    NullVTable {
        /// The library path.
        path: PathBuf,
    },

    /// The plugin's ABI version does not match the host's.
    ///
    /// This compatibility check runs after the operator-trusted entrypoint has
    /// returned a raw pointer, the loader has rejected null, and the loader has
    /// constructed the reference promised by the ABI. It prevents use of the
    /// remaining fields when the reported version differs; it does not validate
    /// an arbitrary symbol signature or pointer.
    #[error(
        "plugin '{}' was built against ABI version {found}, but this host requires {expected}",
        path.display()
    )]
    AbiMismatch {
        /// The library path.
        path: PathBuf,
        /// The ABI version the plugin reported.
        found: u32,
        /// The ABI version this host requires.
        expected: u32,
    },

    /// A metadata field was not a valid nul-terminated UTF-8 string.
    #[error(
        "plugin '{}' has invalid metadata field '{field}' (expected nul-terminated UTF-8)",
        path.display()
    )]
    InvalidMetadata {
        /// The library path.
        path: PathBuf,
        /// The offending field's name (for example `"id"` or `"name"`).
        field: &'static str,
    },

    /// The metadata read cleanly, but the host refused to register the plugin
    /// (duplicate id, registry full, ...).
    #[error("host rejected plugin from '{}': {source}", path.display())]
    Registration {
        /// The library path.
        path: PathBuf,
        /// The host error explaining the rejection.
        #[source]
        source: HostError,
    },

    /// The plugin directory could not be scanned.
    #[error("could not scan plugin directory '{}': {source}", path.display())]
    Scan {
        /// The directory path.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
}

impl LoadError {
    /// Returns the filesystem path the failure is associated with.
    pub fn path(&self) -> &std::path::Path {
        match self {
            LoadError::Open { path, .. }
            | LoadError::MissingEntrypoint { path, .. }
            | LoadError::NullVTable { path }
            | LoadError::AbiMismatch { path, .. }
            | LoadError::InvalidMetadata { path, .. }
            | LoadError::Registration { path, .. }
            | LoadError::Scan { path, .. } => path,
        }
    }
}

/// The outcome of scanning and loading a whole plugin directory.
///
/// Every per-entry failure that returns is collected in
/// [`DirLoadReport::failed`], and scanning then attempts the next entry.
/// Successful registrations are collected in [`DirLoadReport::loaded`].
/// Native initializers or entrypoints that abort, hang, or violate the ABI may
/// prevent the scan from returning. Fields are private; read them through the
/// accessors.
#[derive(Debug, Default)]
pub struct DirLoadReport {
    loaded: Vec<PluginId>,
    failed: Vec<(PathBuf, LoadError)>,
}

impl DirLoadReport {
    /// Records a successfully loaded plugin.
    pub(crate) fn record_loaded(&mut self, id: PluginId) {
        self.loaded.push(id);
    }

    /// Records a failed load attempt against its path.
    pub(crate) fn record_failure(&mut self, path: PathBuf, error: LoadError) {
        self.failed.push((path, error));
    }

    /// Returns the ids of the plugins that loaded successfully.
    pub fn loaded(&self) -> &[PluginId] {
        &self.loaded
    }

    /// Returns each path that failed to load, paired with its error.
    pub fn failed(&self) -> &[(PathBuf, LoadError)] {
        &self.failed
    }

    /// Returns how many plugins loaded successfully.
    pub fn loaded_count(&self) -> usize {
        self.loaded.len()
    }

    /// Returns how many load attempts failed.
    pub fn failure_count(&self) -> usize {
        self.failed.len()
    }

    /// Returns whether nothing was loaded and nothing failed (an empty or
    /// plugin-free directory).
    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty() && self.failed.is_empty()
    }
}
