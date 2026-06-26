//! A dynamic-plugin fixture for `ferrumc-plugin-host`'s loader tests.
//!
//! It is compiled as a `cdylib` and exports several entrypoints with the
//! [`PluginEntryFn`](ferrumc_plugin_api::abi::PluginEntryFn) signature so the
//! host's integration tests can exercise every branch of the loader against a
//! *real* dynamic library:
//!
//! - [`ferrumc_plugin_entry`] — a well-formed plugin that loads and initializes
//!   successfully.
//! - [`ferrumc_plugin_entry_bad_abi`] — reports an incompatible ABI version so
//!   the loader rejects it.
//! - [`ferrumc_plugin_entry_failinit`] — loads fine but fails initialization, so
//!   the host can prove the failure is isolated.
//!
//! The only thing here that counts as "unsafe code" is exporting the
//! `no_mangle` C entrypoints (a single scoped `#[allow(unsafe_code)]`);
//! building the `'static` [`PluginVTable`] itself needs no `unsafe`. All the
//! real unsafe machinery lives on the host side of the boundary.

#![deny(unsafe_code)]

use core::ffi::c_char;
use std::panic::catch_unwind;

use ferrumc_plugin_api::abi::{PluginVTable, ABI_VERSION, STATUS_OK, STATUS_PANIC};
use ferrumc_plugin_api::{Capability, CapabilityManifest};

/// The capabilities this fixture declares it needs.
const FIXTURE_CAPS: u32 = CapabilityManifest::empty()
    .with(Capability::ReceiveEvents)
    .with(Capability::Storage)
    .bits();

/// Returns the fixture's stable id.
extern "C" fn fixture_id() -> *const c_char {
    c"ferrumc-fixture".as_ptr()
}

/// Returns the fixture's display name.
extern "C" fn fixture_name() -> *const c_char {
    c"Fixture Plugin".as_ptr()
}

/// Id used by the bad-ABI vtable (distinct so it can coexist in one registry).
extern "C" fn bad_abi_id() -> *const c_char {
    c"ferrumc-fixture-bad-abi".as_ptr()
}

/// Id used by the failing-init vtable.
extern "C" fn fail_init_id() -> *const c_char {
    c"ferrumc-fixture-fail-init".as_ptr()
}

/// A successful init shim.
///
/// The body is wrapped in [`catch_unwind`] to honor the ABI rule that a plugin
/// must never unwind across the C boundary: a panic is converted into a status
/// code instead of being allowed to abort the host.
extern "C" fn init_ok(_abi_version: u32, _granted_capabilities: u32) -> i32 {
    catch_unwind(|| {
        // A real plugin would build its state here; the fixture has none.
        STATUS_OK
    })
    .unwrap_or(STATUS_PANIC)
}

/// An init shim that always reports failure, isolated behind the same
/// panic-catching wrapper.
extern "C" fn init_fail(_abi_version: u32, _granted_capabilities: u32) -> i32 {
    catch_unwind(|| {
        // A nonzero status the host classifies as an initialization failure.
        1
    })
    .unwrap_or(STATUS_PANIC)
}

/// A no-op shutdown shim.
extern "C" fn shutdown_noop() {}

/// The well-formed plugin vtable.
static GOOD_VTABLE: PluginVTable = PluginVTable {
    abi_version: ABI_VERSION,
    version_major: 1,
    version_minor: 2,
    version_patch: 3,
    capability_bits: FIXTURE_CAPS,
    id: fixture_id,
    name: fixture_name,
    init: init_ok,
    shutdown: shutdown_noop,
};

/// A vtable advertising an ABI version the host does not understand.
static BAD_ABI_VTABLE: PluginVTable = PluginVTable {
    abi_version: ABI_VERSION.wrapping_add(1),
    version_major: 0,
    version_minor: 1,
    version_patch: 0,
    capability_bits: 0,
    id: bad_abi_id,
    name: fixture_name,
    init: init_ok,
    shutdown: shutdown_noop,
};

/// A well-formed vtable whose init always fails.
static FAIL_INIT_VTABLE: PluginVTable = PluginVTable {
    abi_version: ABI_VERSION,
    version_major: 0,
    version_minor: 1,
    version_patch: 0,
    capability_bits: 0,
    id: fail_init_id,
    name: fixture_name,
    init: init_fail,
    shutdown: shutdown_noop,
};

/// The canonical entrypoint: a plugin that loads and initializes cleanly.
///
/// # Panics
///
/// Never; it only hands back a pointer to a `'static` vtable.
#[allow(unsafe_code)] // exporting a C symbol is the only "unsafe" act here
#[no_mangle]
pub extern "C" fn ferrumc_plugin_entry() -> *const PluginVTable {
    core::ptr::addr_of!(GOOD_VTABLE)
}

/// An alternate entrypoint whose vtable reports an incompatible ABI version.
#[allow(unsafe_code)] // exporting a C symbol is the only "unsafe" act here
#[no_mangle]
pub extern "C" fn ferrumc_plugin_entry_bad_abi() -> *const PluginVTable {
    core::ptr::addr_of!(BAD_ABI_VTABLE)
}

/// An alternate entrypoint whose plugin fails during initialization.
#[allow(unsafe_code)] // exporting a C symbol is the only "unsafe" act here
#[no_mangle]
pub extern "C" fn ferrumc_plugin_entry_failinit() -> *const PluginVTable {
    core::ptr::addr_of!(FAIL_INIT_VTABLE)
}

/// An entrypoint that returns a null vtable, to exercise the host's null check.
#[allow(unsafe_code)] // exporting a C symbol is the only "unsafe" act here
#[no_mangle]
pub extern "C" fn ferrumc_plugin_entry_null() -> *const PluginVTable {
    core::ptr::null()
}
