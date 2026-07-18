# Invariants: ferrumc-plugin-host

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- Every public item has rustdoc, and errors classify their failure mode.
- Dispatch is synchronous. No lock or blocking primitive is held across an
  `.await`.
- The crate uses `deny(unsafe_code)`. The earlier lifecycle-only C-ABI loader
  scopes every operation requiring `unsafe` to `dynamic::ffi`; no other module
  in this crate may add one. Its assumptions are documented in
  `docs/safety/ferrumc-plugin-host.md`.

## Registry and facades

- The registry is bounded by `HostConfig::max_plugins`. Compiled-in and trusted
  native registrations share one stable registration order for dispatch and
  decision folding.
- The host selects a plugin's `CapabilityManifest` and constructs every
  lifecycle or event context from it. A plugin cannot obtain an ungranted
  facade by supplying its own capability bits.
- The host binds each storage view to the registered `PluginId`. Plugin code
  never chooses the namespace passed to `PluginStorageBackend`.
- `CallBudget` is elapsed-time observation after a measured call returns.
  Successful compiled enable/event/decision hooks, successful trusted-native
  initialization, and returning trusted-native event/decision calls are
  measured; dispatch overruns can prevent future calls when configured.
  Metadata and shutdown are not timed, and no budget can interrupt a callback
  that is still running.

## Compiled-in plugins

- Every call into a compiled-in `Plugin` value (`metadata`, `on_enable`,
  `on_event`, `before_block_place`, `before_block_break`, `before_chat`,
  `before_interact`, and `on_disable`) is wrapped in
  `std::panic::catch_unwind`.
- A metadata unwind fails registration. An unwind from a registered instance
  records a panic and makes that registration terminally
  `Disabled(Panicked)`. Public enable must reject it before constructing a
  context or re-entering plugin code.
- `AssertUnwindSafe` permits the host to catch the unwind and enforce that
  terminal rule; it does not roll back effects. Storage writes, registered
  command handlers, submitted intents, and shared-state changes completed
  before the unwind can remain observable. The boxed plugin value is dropped
  normally.
- Catching requires an unwinding panic strategy. A process-aborting panic does
  not return to the host.

## Strict trusted-native runtime

- `register_trusted_native` accepts only a `ferrumc_plugin_loader::LoadedPlugin`
  whose bundle, manifest, target, digest, ABI, descriptor, and exported metadata
  have already passed the strict loader. The host independently rejects
  duplicate IDs, capacity overflow, and any capability outside its implemented
  subset: `ReceiveEvents`, `SubmitIntents`, and `VetoBlockEdits`.
- Initialization may record only valid subscriptions. Event callbacks may
  stage at most the bounded intent limit. A block-decision callback may stage
  that full intent limit plus exactly one mandatory decision.
- Native effects remain in a callback-local bounded stage. A non-success
  callback status, boundary error, or capability denial discards the stage
  before the caller's `CommandSink` is touched. Every service error on a
  decision stage, and a decision command routed to another stage, also discards
  it. Invalid event-resource provenance and an unavailable dimension facade are
  stage-poisoning on every route. Other validation or capacity errors on
  notification and initialization stages reject only the offending operation;
  earlier valid effects may remain when the callback returns `FC_OK`. After
  success, intents are submitted to the sink in order and a block decision is
  returned for folding. Sink submission is not atomic: an earlier accepted
  intent can remain if a later intent is rejected.
- Exact simulation contexts receive a fresh callback-scoped resource handle.
  Connection-side contexts carry only the documented tick-zero and invalid
  shard-resource sentinels; the host must not fabricate simulation provenance.
- A normal `FC_PLUGIN_PANIC` return from an event or block-decision callback
  discards the current stage, retires the active instance without another
  plugin callback, records the panic, clears subscriptions, and makes the
  registration terminally `Disabled(Panicked)`. A lifecycle failure follows
  its separate typed enable or shutdown path.
- Returned statuses cannot protect the process from aborts, invalid native
  memory behavior, undefined behavior, foreign exceptions, deadlocks, hangs,
  or hostile actions. A trusted native library has the server process's
  authority.
- Strict-path libraries remain resident until process exit. Logical retirement
  never implies platform-library unload, and the host has no live reload or
  unload operation.

## Earlier lifecycle-only C ABI

- `PluginLoader` is a compatibility API for
  `ferrumc_plugin_api::abi`; it is not the strict bundle loader used by the
  shipping app. It registers but does not enable adapters and exposes no native
  gameplay callback surface.
- The compatibility loader first invokes the assumed-signature entrypoint,
  checks the returned pointer for null, and constructs an operator-trusted
  vtable reference. It then checks the reported ABI version before using the
  remaining fields. Pointer validity, alignment, extent, C-string termination,
  function signatures, and library initializers rely on the library honoring
  that ABI.
- The adapter copies metadata, retains its `Library` for every function-pointer
  call, attempts `shutdown` after successful `init`, and releases its library
  handle only after the adapter is dropped.
- Directory scanning records returned per-entry failures and continues.
  Native code that aborts, hangs, or violates the pointer contract may prevent
  the loader from returning and is not converted into a recoverable
  `LoadError`.
