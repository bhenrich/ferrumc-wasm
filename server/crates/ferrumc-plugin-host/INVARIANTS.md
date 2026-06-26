# Invariants: ferrumc-plugin-host

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- All public items have rustdoc.
- Error types classify the failure mode.

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
