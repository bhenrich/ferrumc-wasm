# ADR-0006: C ABI for Dynamic Plugin Loading

**Status:** Accepted
**Date:** 2026-02-16

## Context

Rust has no stable ABI. Dynamic Rust plugins (.so/.dll) compiled with different Rust versions will crash. Need a stable interface.

## Decision

Use C ABI (`extern "C"`) with opaque handles for the plugin ↔ host boundary.

## How It Works

1. Plugin compiles as `cdylib` (produces .so/.dll)
2. Plugin exports a C-compatible registration function:
   ```c
   extern "C" fn ferrumc_plugin_init(host: *mut HostApi) -> i32;
   ```
3. Host loads plugin with `libloading`, calls init
4. Host passes opaque handles through C-compatible vtables
5. Plugin uses type-safe Rust wrappers around the C handles (provided by ferrumc-plugin-api)

## User Workflow

```
1. cargo build --release (compiles plugin as .so/.dll)
2. Copy target/release/libmy_plugin.so to server/plugins/
3. Configure capabilities in server.toml
4. Start server
```

## Trade-offs

**Pro:** Works across Rust versions, stable ABI, proven pattern (most plugin systems work this way)
**Con:** FFI boundary is ugly to write, must be careful with ownership across the boundary, no generics at the boundary

## Consequences

- ferrumc-plugin-api provides safe Rust wrappers over the C ABI
- Plugin authors write normal Rust, the API crate handles FFI
- Host must validate all data crossing the boundary
- Version compatibility checked via API version number in plugin metadata
