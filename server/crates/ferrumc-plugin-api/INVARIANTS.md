# Invariants: ferrumc-plugin-api

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- All public items have rustdoc.
- Error types classify the failure mode.

## Crate-Specific

- This crate exposes **no raw internals**: no `SimShard`, `Chunk`, `EntityStore`,
  socket, DB handle, or `tokio::Runtime`. Everything a plugin can touch is one of
  the capability-gated facade traits (`WorldView`, `CommandSink`, `PermissionApi`,
  `PluginStorageApi`).
- `WorldView`, `CommandSink`, `PermissionApi`, and `PluginStorageApi` are **trait
  shells**. The simulation and storage layers inject the concrete
  implementations; this crate must never depend on `ferrumc-sim`,
  `ferrumc-world`, or `ferrumc-storage`.
- Every facade is reached only through a context (`SetupContext`,
  `EventContext`, `TeardownContext`) that checks the relevant `Capability` first.
  No accessor may hand out a facade the plugin was not granted.
- Plugin-facing world access is read-only; world *changes* are expressed as
  `WorldIntent`s submitted to a sink, never applied directly.
- Storage keys and values are bounded (`MAX_KEY_LEN`, `MAX_VALUE_LEN`); the
  plugin-facing storage handle is namespaced by the host, so a plugin can never
  name another plugin's namespace.
