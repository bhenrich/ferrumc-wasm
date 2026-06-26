// This crate cannot inherit the workspace `forbid(unsafe_code)`: exporting the
// `#[no_mangle]` C entrypoint counts as unsafe code under modern rustc. It is
// `deny`ed instead, with a single scoped `#[allow(unsafe_code)]` on the export
// (mirroring `ferrumc-plugin-fixture`). All real FFI machinery lives on the host
// side of the boundary; here the "unsafe" act is only exposing a C symbol.
#![deny(unsafe_code)]
#![warn(missing_docs)]

//! The spawn-protection plugin.
//!
//! This is the milestone's MVP plugin, shipped two ways from one crate:
//!
//! - As a **`cdylib`** exporting the M28 C ABI ([`ferrumc_plugin_api::abi`]) so
//!   the host's dynamic loader can load it from a `/plugins` directory across the
//!   stable boundary. This proves the dynamic-loading path end to end.
//! - As an **`rlib`** exposing an in-process [`SpawnProtectPlugin`] and the pure
//!   [`SpawnProtect`] policy, so the application can drive the same logic
//!   directly.
//!
//! # Why the veto runs in-process
//!
//! The plugin "vetoes" unauthorized block edits inside a configurable spawn
//! radius. A *true* cross-ABI veto would need the host to route each block event
//! into the plugin and read a decision back, but the M28 [`PluginVTable`] carries
//! only lifecycle hooks (`init` / `shutdown`) — there is no event entry point,
//! and extending the ABI is out of this milestone's scope. So the dynamic library
//! proves the loader, while the authoritative veto decision is the pure
//! [`SpawnProtect`] policy the application consults before an edit reaches the
//! simulation. The in-process [`SpawnProtectPlugin`] additionally exercises the
//! full SDK the deliverable calls for: namespaced storage for its configuration,
//! the permission API for the bypass check, mutation intents for messaging, and
//! the join/break event subscriptions.
//!
//! [`PluginVTable`]: ferrumc_plugin_api::abi::PluginVTable

mod guard;
mod plugin;

pub use guard::{SpawnProtect, DEFAULT_RADIUS, ENCODED_LEN};
pub use plugin::{
    bypass_node, SpawnProtectPlugin, BYPASS_PERMISSION, CONFIG_KEY, PLUGIN_ID, PLUGIN_NAME,
};

use core::ffi::c_char;
use std::panic::catch_unwind;

use ferrumc_plugin_api::abi::{PluginVTable, ABI_VERSION, STATUS_OK, STATUS_PANIC};

/// The capability bitset the `cdylib` declares across the C ABI.
///
/// Matches [`SpawnProtectPlugin::capabilities`] so a host reads back exactly the
/// capabilities the in-process plugin requests.
const CAPABILITY_BITS: u32 = SpawnProtectPlugin::capabilities().bits();

/// Returns the plugin's stable id as a nul-terminated C string.
extern "C" fn vtable_id() -> *const c_char {
    c"spawn-protect".as_ptr()
}

/// Returns the plugin's display name as a nul-terminated C string.
extern "C" fn vtable_name() -> *const c_char {
    c"Spawn Protection".as_ptr()
}

/// The C-ABI init shim.
///
/// Wrapped in [`catch_unwind`] to honor the ABI rule that a plugin must never
/// unwind across the boundary: any panic becomes a status code. The dynamic
/// instance holds no state of its own (the in-process plugin owns the policy),
/// so initialization is a no-op success.
extern "C" fn vtable_init(_abi_version: u32, _granted_capabilities: u32) -> i32 {
    catch_unwind(|| STATUS_OK).unwrap_or(STATUS_PANIC)
}

/// The C-ABI shutdown shim. Nothing to tear down on the dynamic side.
extern "C" fn vtable_shutdown() {}

/// The plugin's `'static` C-ABI vtable, handed to the host by
/// [`ferrumc_plugin_entry`].
static VTABLE: PluginVTable = PluginVTable {
    abi_version: ABI_VERSION,
    version_major: 0,
    version_minor: 1,
    version_patch: 0,
    capability_bits: CAPABILITY_BITS,
    id: vtable_id,
    name: vtable_name,
    init: vtable_init,
    shutdown: vtable_shutdown,
};

/// The C-ABI entrypoint the dynamic loader resolves.
///
/// Returns a pointer to the plugin's `'static` [`PluginVTable`]; the host copies
/// the metadata out and never frees it. Exporting this symbol is the single
/// "unsafe" act in the crate.
///
/// # Panics
///
/// Never; it only returns a pointer to a `'static` value.
#[allow(unsafe_code)] // exporting a C symbol is the only "unsafe" act here
#[no_mangle]
pub extern "C" fn ferrumc_plugin_entry() -> *const PluginVTable {
    core::ptr::addr_of!(VTABLE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_plugin_api::CapabilityManifest;

    #[test]
    fn entrypoint_exposes_a_valid_vtable() {
        let ptr = ferrumc_plugin_entry();
        assert!(!ptr.is_null());
        // SAFETY-free: we only read fields of our own `'static` vtable.
        assert_eq!(VTABLE.abi_version, ABI_VERSION);
        assert_eq!(
            CapabilityManifest::from_bits_truncate(VTABLE.capability_bits),
            SpawnProtectPlugin::capabilities()
        );
    }

    #[test]
    fn init_and_shutdown_shims_are_well_behaved() {
        assert_eq!(vtable_init(ABI_VERSION, CAPABILITY_BITS), STATUS_OK);
        vtable_shutdown();
    }
}
