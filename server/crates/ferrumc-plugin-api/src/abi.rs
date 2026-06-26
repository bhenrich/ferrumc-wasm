//! The narrow C ABI shared between the plugin host and dynamically-loaded
//! plugins.
//!
//! Rust has no stable ABI, so a plugin compiled as a `cdylib` can only talk to
//! the host across a C-compatible boundary (see ADR-0006). This module is that
//! boundary, and *only* that boundary: it deliberately contains nothing but
//! `#[repr(C)]` types, `extern "C"` function-pointer aliases, and integer
//! constants. No `String`, `Vec`, `Result`, `Option`, slice, reference, trait
//! object, or Rust-layout `enum` ever crosses it.
//!
//! # Contract
//!
//! 1. A plugin compiles as a `cdylib` and exports one symbol named
//!    [`ENTRYPOINT`] with the signature [`PluginEntryFn`].
//! 2. The host resolves that symbol, calls it, and receives a pointer to a
//!    `'static` [`PluginVTable`] owned by the plugin.
//! 3. The host checks [`PluginVTable::abi_version`] against [`ABI_VERSION`] and
//!    refuses to load on mismatch — this is what makes "compiled with a
//!    different Rust version" fail loudly instead of corrupting memory.
//! 4. The host reads the plugin's metadata through the vtable's string
//!    function pointers and registers the plugin.
//! 5. Lifecycle calls ([`PluginInitFn`], [`PluginShutdownFn`]) return / take
//!    only scalars and **must not unwind**: a plugin that might panic has to
//!    catch the panic itself and report it as a status code.
//!
//! # Ownership
//!
//! Everything reachable through the vtable is owned by the *plugin* and lives in
//! the plugin's loaded library image: the host borrows it and must keep the
//! library loaded for as long as it holds any of these pointers. The host never
//! frees plugin-owned memory, and the plugin never frees host-owned memory.
//!
//! # A note on `pub` fields
//!
//! The project style normally forbids `pub` fields on types that cross a crate
//! boundary. [`PluginVTable`] is the documented exception: its layout *is* the
//! ABI contract, the plugin must build it field by field, and the host must read
//! it field by field. Hiding the fields behind methods would not encapsulate
//! anything — the representation is the API.

use core::ffi::{c_char, CStr};

/// The current plugin ABI version.
///
/// Bump this on *any* change to the layout or semantics of [`PluginVTable`] or
/// the function-pointer signatures. The host refuses to load a plugin whose
/// [`PluginVTable::abi_version`] differs, which is the entire point: it turns an
/// incompatible binary into a clean rejection instead of undefined behavior.
pub const ABI_VERSION: u32 = 1;

/// The name of the symbol every plugin library must export.
///
/// A [`CStr`] so it can be handed straight to the dynamic loader. The exported
/// function must have the [`PluginEntryFn`] signature.
pub const ENTRYPOINT: &CStr = c"ferrumc_plugin_entry";

/// Status returned by [`PluginInitFn`] to report a successful initialization.
///
/// Any value other than this is treated by the host as an initialization
/// failure; the plugin is left disabled and the host keeps running.
pub const STATUS_OK: i32 = 0;

/// Status a plugin's init shim should return when it caught a panic.
///
/// This is a *convention*, not enforced by the ABI: the host treats every
/// nonzero status as a failure. Using a distinct code lets logs distinguish a
/// caught panic from an ordinary refusal.
pub const STATUS_PANIC: i32 = -1;

/// Returns a pointer to a nul-terminated, UTF-8, `'static` string owned by the
/// plugin (for example its id or display name).
///
/// The returned pointer must stay valid for as long as the plugin library is
/// loaded. The host copies the string out immediately; it never frees it.
pub type PluginStrFn = extern "C" fn() -> *const c_char;

/// Initializes a plugin.
///
/// Receives the [`ABI_VERSION`] the host negotiated and the bitset of
/// capabilities the host granted (see
/// [`CapabilityManifest::bits`](crate::CapabilityManifest::bits)). Returns
/// [`STATUS_OK`] on success or any nonzero value to signal failure.
///
/// # Must not unwind
///
/// Unwinding across this `extern "C"` boundary aborts the process. A plugin
/// whose initialization can panic must wrap its body in
/// [`std::panic::catch_unwind`] and translate a caught panic into a nonzero
/// status (by convention [`STATUS_PANIC`]).
pub type PluginInitFn = extern "C" fn(abi_version: u32, granted_capabilities: u32) -> i32;

/// Shuts a plugin down. Called at most once, and only if init succeeded.
///
/// # Must not unwind
///
/// Like [`PluginInitFn`], this must not unwind across the boundary.
pub type PluginShutdownFn = extern "C" fn();

/// The plugin entrypoint: returns a pointer to the plugin's `'static`
/// [`PluginVTable`].
///
/// Returning a null pointer is a valid way to signal "I cannot produce a
/// vtable"; the host rejects it. The pointer must remain valid for as long as
/// the library is loaded.
pub type PluginEntryFn = extern "C" fn() -> *const PluginVTable;

/// The C-ABI description of a dynamically-loaded plugin.
///
/// A plugin builds one of these as a `'static` value and hands the host a
/// pointer to it from its [`PluginEntryFn`]. Every field is a scalar or an
/// `extern "C"` function pointer, so the struct is `Send + Sync` and has a
/// stable, C-compatible layout.
///
/// See the [module documentation](self) for the full contract and the rationale
/// for the `pub` fields.
#[repr(C)]
pub struct PluginVTable {
    /// The ABI version this plugin was built against. The host compares it to
    /// [`ABI_VERSION`] and refuses to load on mismatch.
    pub abi_version: u32,
    /// Major component of the plugin's semantic version.
    pub version_major: u32,
    /// Minor component of the plugin's semantic version.
    pub version_minor: u32,
    /// Patch component of the plugin's semantic version.
    pub version_patch: u32,
    /// The capabilities the plugin requests, as a
    /// [`CapabilityManifest`](crate::CapabilityManifest) bitset.
    pub capability_bits: u32,
    /// Returns the plugin's stable id (a nul-terminated UTF-8 string).
    pub id: PluginStrFn,
    /// Returns the plugin's human-readable name (a nul-terminated UTF-8 string).
    pub name: PluginStrFn,
    /// Initializes the plugin. See [`PluginInitFn`].
    pub init: PluginInitFn,
    /// Shuts the plugin down. See [`PluginShutdownFn`].
    pub shutdown: PluginShutdownFn,
}
