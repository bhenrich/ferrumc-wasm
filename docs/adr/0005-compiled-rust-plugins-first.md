# ADR-0005: Compiled Rust Plugins First

**Status:** Accepted
**Date:** 2026-02-16

## Context

Plugin system needed. Options: compiled Rust, dynamic libraries, WASM, scripting (Lua/JS).

## Decision

Phase A (dev): compiled-in Rust plugins for API iteration.
Phase A (ship): dynamic libraries (.so/.dll) loaded from /plugins/ folder via C ABI.
Phase B (later): WASM for language-agnostic sandboxed plugins.

## Rationale

- Compiled-in lets us iterate on the API fast without solving dynamic loading simultaneously
- C ABI + opaque handles solves Rust's unstable ABI for dynamic loading
- WASM adds complexity (runtime, memory model, API marshalling) — not justified until the API is stable
- Users expect drop-in: download .so → put in plugins/ → start server

## Plugin API Design

Capability-based. Plugins declare needs, server config grants/denies.

**Plugins get:** WorldView (read-only), PlayerApi, CommandSink (intents), PermissionApi, PluginStorageApi
**Plugins never get:** raw sim internals, raw chunks, sockets, DB handles, Tokio runtime

WorldView is !Send — cannot be held across await points (enforced at compile time).

## Consequences

- Phase A (dev) requires recompilation — acceptable for API development
- Phase A (ship) with C ABI is the user-facing deliverable
- Plugin developers must compile against the plugin API crate
- API must be narrow and stable — changing it breaks all plugins
