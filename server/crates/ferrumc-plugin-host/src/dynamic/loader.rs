//! The directory-scanning, libloading-based plugin loader.
//!
//! [`PluginLoader`] turns dynamic libraries on disk into registered plugins in a
//! [`PluginHost`]. It validates the ABI version, reads metadata across the C
//! ABI, and registers each plugin through the compatibility adapter. Load and
//! registration errors that return are classified as [`LoadError`]. Native
//! library initialization, aborts, and invalid memory behavior remain
//! process-wide risks.

use std::ffi::CStr;
use std::fs;
use std::path::Path;

use ferrumc_core::PluginId;
use ferrumc_plugin_api::abi::ENTRYPOINT;

use super::error::{DirLoadReport, LoadError};
use super::ffi;
use crate::host::PluginHost;

/// Loads plugins from dynamic libraries and registers them with a host.
///
/// The loader is configured with the entrypoint symbol it resolves by default
/// ([`ENTRYPOINT`]); construct it with [`PluginLoader::new`], or
/// [`PluginLoader::with_entry_symbol`] to resolve a custom symbol (useful for a
/// host that supports versioned entrypoints, and for tests).
#[derive(Debug, Clone, Copy)]
pub struct PluginLoader {
    entry_symbol: &'static CStr,
}

impl PluginLoader {
    /// Creates a loader that resolves the canonical [`ENTRYPOINT`] symbol.
    pub const fn new() -> Self {
        Self {
            entry_symbol: ENTRYPOINT,
        }
    }

    /// Creates a loader that resolves `entry_symbol` instead of the canonical
    /// one.
    pub const fn with_entry_symbol(entry_symbol: &'static CStr) -> Self {
        Self { entry_symbol }
    }

    /// Returns the entrypoint symbol this loader resolves by default.
    pub const fn entry_symbol(&self) -> &CStr {
        self.entry_symbol
    }

    /// Loads the single plugin library at `path` and registers it with `host`,
    /// resolving this loader's configured entrypoint symbol.
    ///
    /// Returns the registered plugin's [`PluginId`], or a classified
    /// [`LoadError`]. The plugin is registered but **not** enabled; the caller
    /// drives the lifecycle through `host` (so an `init` status failure that
    /// returns surfaces as a host enable error).
    pub fn load_file(&self, path: &Path, host: &mut PluginHost) -> Result<PluginId, LoadError> {
        self.load_file_with_entry(path, self.entry_symbol, host)
    }

    /// Loads the plugin library at `path` resolving an explicit `entry_symbol`,
    /// then registers it with `host`.
    ///
    /// This is the building block behind [`load_file`](Self::load_file) and
    /// exists for hosts (and tests) that need to select a non-default
    /// entrypoint.
    pub fn load_file_with_entry(
        &self,
        path: &Path,
        entry_symbol: &CStr,
        host: &mut PluginHost,
    ) -> Result<PluginId, LoadError> {
        let plugin = ffi::load(path, entry_symbol)?;
        host.register(Box::new(plugin))
            .map_err(|source| LoadError::Registration {
                path: path.to_path_buf(),
                source,
            })
    }

    /// Scans `dir` for dynamic libraries, loading and registering each one.
    ///
    /// Only files whose extension matches the platform's dynamic-library
    /// extension (`.so`, `.dylib`, `.dll`) are attempted; everything else is
    /// ignored. Each load or registration failure that returns is recorded in
    /// the [`DirLoadReport`], and later directory entries are still attempted.
    ///
    /// The outer [`Result`] fails only if the directory itself cannot be read.
    pub fn load_dir(&self, dir: &Path, host: &mut PluginHost) -> Result<DirLoadReport, LoadError> {
        let mut report = DirLoadReport::default();

        let entries = fs::read_dir(dir).map_err(|source| LoadError::Scan {
            path: dir.to_path_buf(),
            source,
        })?;

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(source) => {
                    // A single unreadable entry is recorded, not fatal.
                    report.record_failure(
                        dir.to_path_buf(),
                        LoadError::Scan {
                            path: dir.to_path_buf(),
                            source,
                        },
                    );
                    continue;
                }
            };

            let path = entry.path();
            if !is_dynamic_library(&path) {
                continue;
            }

            match self.load_file(&path, host) {
                Ok(id) => report.record_loaded(id),
                Err(error) => report.record_failure(path, error),
            }
        }

        Ok(report)
    }
}

impl Default for PluginLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns whether `path` has the platform's dynamic-library extension.
fn is_dynamic_library(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case(std::env::consts::DLL_EXTENSION))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_platform_dynamic_libraries() {
        let good = Path::new("plugin").with_extension(std::env::consts::DLL_EXTENSION);
        assert!(is_dynamic_library(&good));
        assert!(!is_dynamic_library(Path::new("plugin.txt")));
        assert!(!is_dynamic_library(Path::new("plugin")));
    }

    #[test]
    fn load_dir_on_missing_directory_is_a_scan_error() {
        let mut host = PluginHost::in_memory();
        let loader = PluginLoader::new();
        let err = loader
            .load_dir(Path::new("/definitely/not/a/real/dir/here"), &mut host)
            .expect_err("missing directory must error");
        assert!(matches!(err, LoadError::Scan { .. }));
    }

    #[test]
    fn empty_directory_yields_empty_report() {
        let dir =
            std::env::temp_dir().join(format!("ferrumc-plugin-host-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let mut host = PluginHost::in_memory();
        let report = PluginLoader::new()
            .load_dir(&dir, &mut host)
            .expect("scan succeeds");
        assert!(report.is_empty());
        assert_eq!(report.loaded_count(), 0);
        let _ = std::fs::remove_dir(&dir);
    }
}
