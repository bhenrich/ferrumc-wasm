use std::path::Path;

use ferrumc_plugin_abi::{FcPluginEntryV1Fn, ENTRYPOINT_V1};
use libloading::Library;

use crate::error::LoadError;
use crate::raw::{copy_metadata, validate_descriptor, validate_function_table};
use crate::values::{OwnedPluginMetadata, PluginSemanticVersion, ValidatedCallbacks};

/// A validated native plugin with host-owned metadata.
///
/// The underlying platform library is opened once and remains resident until
/// process exit. This type exposes neither the library handle nor callback
/// pointers.
pub struct LoadedAbiPlugin {
    _resident_library: &'static Library,
    metadata: OwnedPluginMetadata,
    callbacks: ValidatedCallbacks,
}

impl LoadedAbiPlugin {
    /// Returns the validated host-owned metadata.
    pub const fn metadata(&self) -> &OwnedPluginMetadata {
        &self.metadata
    }

    pub(crate) fn into_parts(self) -> (OwnedPluginMetadata, ValidatedCallbacks) {
        (self.metadata, self.callbacks)
    }
}

/// Opens and validates one native plugin library.
///
/// Once the platform loader opens the file successfully, its handle is leaked
/// immediately. It therefore remains resident even when symbol resolution or
/// later validation rejects the plugin.
pub fn load(path: &Path) -> Result<LoadedAbiPlugin, LoadError> {
    // SAFETY: loading executes operator-trusted native initializers. The caller
    // selects the file, and this boundary immediately makes every successfully
    // opened handle process-resident before inspecting any exported data.
    let library =
        unsafe { Library::new(path) }.map_err(|source| LoadError::open_library(path, source))?;
    let library: &'static Library = Box::leak(Box::new(library));

    let entry = {
        // SAFETY: the lookup uses the ABI's exact NUL-terminated bootstrap name
        // and copies the symbol only with its specified C function signature.
        // The just-leaked library keeps the resolved address resident forever.
        let symbol = unsafe { library.get::<FcPluginEntryV1Fn>(ENTRYPOINT_V1.to_bytes_with_nul()) }
            .map_err(|source| LoadError::missing_entrypoint(path, source))?;
        *symbol
    };

    // SAFETY: `entry` came from the exact bootstrap symbol in a permanently
    // resident operator-trusted library, whose contract promises this signature,
    // a live descriptor result, and no unwind.
    let descriptor_pointer = unsafe { entry() };
    let (descriptor, abi_version) = validate_descriptor(descriptor_pointer)
        .map_err(|source| LoadError::validation(path, source))?;

    let functions_getter = descriptor.functions();
    // SAFETY: the descriptor's raw getter slot was checked for null before its
    // typed value was read, the library is permanently resident, and the
    // operator-trusted getter promises a live result and no unwind.
    let functions_pointer = unsafe { functions_getter() };
    let functions = validate_function_table(functions_pointer, abi_version)
        .map_err(|source| LoadError::validation(path, source))?;

    // Each getter result is bounded and copied before the next getter runs.
    let id = copy_metadata(descriptor.id(), "id")
        .map_err(|source| LoadError::validation(path, source))?;
    let name = copy_metadata(descriptor.name(), "name")
        .map_err(|source| LoadError::validation(path, source))?;
    let target = copy_metadata(descriptor.target(), "target")
        .map_err(|source| LoadError::validation(path, source))?;

    let version = descriptor.version();
    let metadata = OwnedPluginMetadata::new(
        abi_version,
        PluginSemanticVersion::new(version.major(), version.minor(), version.patch()),
        descriptor.requested_capabilities(),
        id,
        name,
        target,
    );
    let callbacks =
        ValidatedCallbacks::new(functions.init(), functions.on_event(), functions.shutdown());

    Ok(LoadedAbiPlugin {
        _resident_library: library,
        metadata,
        callbacks,
    })
}
