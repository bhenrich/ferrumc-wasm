# ferrumc-plugin-host

Plugin registry, capability checks, lifecycle, event dispatch, panic isolation,
and dynamic loading of `cdylib` plugins across a narrow C ABI.

`PluginLoader` scans a directory of plugin libraries, validates each one's ABI
version, reads its metadata across the C ABI defined in
`ferrumc_plugin_api::abi`, and registers it through the host so it inherits the
same panic and time-budget isolation as a compiled-in plugin. See ADR-0006 for
the ABI rationale.

## Safety

This crate is `deny(unsafe_code)` (not `forbid`): dynamic loading needs FFI. All
`unsafe` is confined to one audited module, `dynamic::ffi`. See
`docs/safety/ferrumc-plugin-host.md`.

## Invariants

See `INVARIANTS.md` in this directory.
