# Invariants: ferrumc-plugin-sdk-dynamic

- The crate forbids unsafe code. Raw pointers, exported-symbol attributes, and
  foreign callback invocation remain inside `ferrumc-plugin-abi-sys`.
- `export_plugin!` is the only author-facing packaging operation. It exports
  the ABI v1 bootstrap through the audited system-boundary macro.
- Every ABI callback constructs exactly one call-scoped services backend over
  the supplied `PluginCall`; opaque host resource handles remain private.
- All event, command, request, and response payloads use the checked ABI v1
  binary grammar. JSON and allocator ownership never cross the boundary.
- Event payloads reject truncation, invalid tags or reserved bytes, excessive
  counts or lengths, and trailing bytes before plugin code runs.
- World handles are obtained lazily for the current callback and are never
  exposed to plugin authors or retained in plugin state.
- A `SubmitIntents`-only `SET_BLOCK` reports unavailable when the current host
  denies its `ReadWorld`-gated dimension lookup. The adapter never treats a
  shard handle as a dimension or silently broadens the capability grant.
- An unwinding plugin panic is caught inside the bridge and reported as
  `FC_PLUGIN_PANIC`. The bridge installs no process-global panic hook.
- Panic diagnostics are bounded and copied during the callback. Panic payload
  destruction is separately caught so a hostile destructor cannot unwind
  across the C boundary.
- Cooperative callback errors return `FC_ERROR`; malformed event input returns
  `FC_INVALID_ARGUMENT`; both cause the host to discard that callback's command
  buffer.
- The descriptor target is Cargo's exact build target, including the reviewed
  64-bit GNU/Linux AArch64 target.
