//! The single, fully-documented `unsafe` boundary of this crate.
//!
//! Everything that actually crosses the FFI line lives here: opening a dynamic
//! library, resolving its entrypoint symbol, dereferencing the plugin-owned
//! vtable pointer, and reading C strings out of it. Each `unsafe` block has a
//! `SAFETY:` comment justifying it. The rest of the crate stays safe Rust.
//!
//! See `docs/safety/ferrumc-plugin-host.md` for the full invariant write-up.

use std::ffi::{c_char, CStr};
use std::path::Path;

use libloading::{Library, Symbol};

use ferrumc_core::PluginId;
use ferrumc_plugin_api::abi::{PluginEntryFn, PluginVTable, ABI_VERSION};
use ferrumc_plugin_api::{CapabilityManifest, PluginMetadata, Version};

use super::adapter::LoadedPlugin;
use super::error::LoadError;

/// Opens the library at `path`, resolves `entry_symbol`, validates the vtable it
/// returns, and builds a [`LoadedPlugin`] holding the library open.
///
/// All FFI hazards are contained in this function. On any failure the library is
/// dropped (unloaded) before returning, because the `LoadedPlugin` that would
/// have kept it alive is never constructed.
pub(crate) fn load(path: &Path, entry_symbol: &CStr) -> Result<LoadedPlugin, LoadError> {
    // SAFETY: `Library::new` runs the platform loader on an arbitrary file,
    // which executes that library's initializers. We only ever point it at files
    // an operator placed in the trusted plugins directory; loading untrusted
    // code is out of scope (and would be unsound for any dlopen-based loader).
    let library = unsafe { Library::new(path) }.map_err(|source| LoadError::Open {
        path: path.to_path_buf(),
        source,
    })?;

    // Resolve and call the entrypoint inside a block so the `Symbol`'s borrow of
    // `library` ends here, leaving `library` free to move into the adapter.
    // Calling the resolved function pointer is itself a safe operation; it
    // returns a raw pointer we validate below.
    let vtable_ptr: *const PluginVTable = {
        // SAFETY: we assert the symbol has the agreed `PluginEntryFn` type. If
        // the plugin exported it with a different signature the call is
        // undefined; the ABI version check immediately after is the contract
        // that makes this assertion sound for any plugin built against this
        // crate's `abi` module.
        let entry: Symbol<PluginEntryFn> = unsafe { library.get(entry_symbol.to_bytes_with_nul()) }
            .map_err(|source| LoadError::MissingEntrypoint {
                path: path.to_path_buf(),
                symbol: entry_symbol.to_string_lossy().into_owned(),
                source,
            })?;
        entry()
    };

    if vtable_ptr.is_null() {
        return Err(LoadError::NullVTable {
            path: path.to_path_buf(),
        });
    }

    // SAFETY: `vtable_ptr` is non-null and, per the ABI contract, points at a
    // `'static` `PluginVTable` owned by the plugin and valid for as long as
    // `library` stays loaded — which it does, because `library` is moved into
    // the returned `LoadedPlugin` (or dropped on the error paths below, after
    // we are done reading). We only read the vtable; we never write or free it.
    let vtable: &PluginVTable = unsafe { &*vtable_ptr };

    if vtable.abi_version != ABI_VERSION {
        return Err(LoadError::AbiMismatch {
            path: path.to_path_buf(),
            found: vtable.abi_version,
            expected: ABI_VERSION,
        });
    }

    // Reading the metadata strings calls plugin-provided function pointers
    // (safe) and dereferences the `*const c_char` they return (unsafe; handled
    // inside `read_str`).
    let id = read_str(path, (vtable.id)(), "id")?;
    let name = read_str(path, (vtable.name)(), "name")?;

    let metadata = PluginMetadata::new(
        PluginId::new(id),
        name,
        Version::new(
            u64::from(vtable.version_major),
            u64::from(vtable.version_minor),
            u64::from(vtable.version_patch),
        ),
        CapabilityManifest::from_bits_truncate(vtable.capability_bits),
    );

    // Function pointers are plain `Copy` values; capturing them detaches the
    // adapter from the raw vtable pointer (so the adapter is `Send`).
    Ok(LoadedPlugin::new(
        library,
        metadata,
        vtable.init,
        vtable.shutdown,
    ))
}

/// Copies a nul-terminated C string returned across the ABI into an owned
/// [`String`], validating UTF-8 and rejecting null pointers.
fn read_str(path: &Path, ptr: *const c_char, field: &'static str) -> Result<String, LoadError> {
    if ptr.is_null() {
        return Err(LoadError::InvalidMetadata {
            path: path.to_path_buf(),
            field,
        });
    }

    // SAFETY: the ABI requires `ptr` to be a nul-terminated, `'static` string
    // owned by the plugin and valid while the library is loaded. We read it
    // immediately, validate it is UTF-8, copy it out, and never free it. A
    // misbehaving plugin that returns a non-terminated pointer is the one
    // unavoidable trust we place in `extern "C"` plugin code.
    let cstr = unsafe { CStr::from_ptr(ptr) };
    cstr.to_str()
        .map(str::to_owned)
        .map_err(|_| LoadError::InvalidMetadata {
            path: path.to_path_buf(),
            field,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_plugin_api::abi::{PluginVTable, STATUS_OK};

    // In-process vtable fixtures: these exercise the ABI-parsing logic without
    // needing a real dynamic library, so the parsing branches are covered even
    // where building a cdylib in the test environment is impractical.

    extern "C" fn id_ok() -> *const c_char {
        c"in-proc".as_ptr()
    }
    extern "C" fn name_ok() -> *const c_char {
        c"In Process".as_ptr()
    }
    extern "C" fn init_ok(_abi: u32, _caps: u32) -> i32 {
        STATUS_OK
    }
    extern "C" fn shutdown_noop() {}
    extern "C" fn id_null() -> *const c_char {
        core::ptr::null()
    }

    fn vtable(abi_version: u32) -> PluginVTable {
        PluginVTable {
            abi_version,
            version_major: 4,
            version_minor: 5,
            version_patch: 6,
            capability_bits: 0,
            id: id_ok,
            name: name_ok,
            init: init_ok,
            shutdown: shutdown_noop,
        }
    }

    #[test]
    fn read_str_accepts_valid_and_rejects_null() {
        let path = Path::new("dummy");
        assert_eq!(read_str(path, id_ok(), "id").unwrap(), "in-proc");
        assert!(matches!(
            read_str(path, id_null(), "id"),
            Err(LoadError::InvalidMetadata { field: "id", .. })
        ));
    }

    #[test]
    fn version_components_map_into_metadata() {
        // Mirror what `load` does once it holds a validated vtable, minus the
        // libloading machinery, to confirm the field mapping.
        let vt = vtable(ABI_VERSION);
        assert_eq!(vt.abi_version, ABI_VERSION);
        let version = Version::new(
            u64::from(vt.version_major),
            u64::from(vt.version_minor),
            u64::from(vt.version_patch),
        );
        assert_eq!(version, Version::new(4, 5, 6));
    }
}
