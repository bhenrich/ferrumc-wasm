# Invariants: ferrumc-plugin-host

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- All public items have rustdoc.
- Error types classify the failure mode.
- This crate is `deny(unsafe_code)` (not `forbid`): dynamic plugin loading needs
  FFI. **Every** `unsafe` operation lives in the single
  `#[allow(unsafe_code)]` module `dynamic::ffi`; nothing else may use `unsafe`.
  See `docs/safety/ferrumc-plugin-host.md`.

## Crate-Specific

- **Panic isolation is absolute.** Every call into plugin code
  (`metadata`, `on_enable`, `on_event`, `on_disable`) is wrapped in
  `std::panic::catch_unwind`. A panicking plugin is disabled and never called
  again; it must never crash the host. This is the one place the crate reasons
  about unwinding: closures capture `&mut` plugin state and use
  `AssertUnwindSafe`, which is sound because a panicked plugin's state is never
  observed again. No `unsafe` is used.
- **Capability gating is enforced by the host, not trusted from plugins.** The
  host grants each plugin a `CapabilityManifest` (from metadata or an explicit
  override) and builds every context from it.
- **Storage namespaces are isolated by construction.** The per-plugin
  `NamespacedStorage` captures the plugin id from the host; plugin code never
  supplies it, so it can only reach its own namespace.
- **Every plugin call is timed against a `CallBudget`.** Overruns are recorded
  and, when configured, disable the plugin. The budget comparison is a pure
  function so it stays deterministically testable.
- The registry is **bounded** (`HostConfig::max_plugins`); registration past the
  limit is rejected.
- No async, no channels, and no blocking primitives held across `.await`: all
  dispatch is synchronous; the only lock (in the in-memory store) is held for a
  single map operation.

### Dynamic Loading (C ABI)

- **The ABI version is checked before any other vtable field is trusted.** A
  mismatch is rejected as `LoadError::AbiMismatch`; a binary built against a
  different host can never reach the rest of the load path.
- **Nothing of Rust's choosing crosses the boundary.** Only `#[repr(C)]`
  structs, `extern "C"` function pointers, scalars, and nul-terminated C strings
  (defined in `ferrumc_plugin_api::abi`). No `String`/`Vec`/`Result`/`Option`/
  slice/reference/trait object/Rust-layout `enum` ever crosses.
- **Ownership is one-directional.** The host copies plugin metadata out at load
  time and otherwise holds only `Copy` function pointers plus the `Library`
  handle; it never frees plugin memory and the plugin never frees host memory.
- **The library outlives every call into it.** It is owned by the adapter, whose
  `Drop` runs the plugin's `shutdown` (if initialized) before the library
  unloads.
- **A loaded plugin is isolated exactly like a compiled-in one.** It is wrapped
  in a `Plugin` adapter and registered through the normal host path, so the
  existing `catch_unwind` + budget machinery applies. Plugins must not unwind
  across the `extern "C"` boundary (they catch panics and return a status).
- **One bad plugin never blocks the rest.** Directory scanning records each
  failure as a classified `LoadError` in the `DirLoadReport` and continues.
