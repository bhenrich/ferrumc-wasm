# Invariants: ferrumc-plugin-sdk-builtin

- The crate forbids unsafe code and depends only on `ferrumc-plugin-sdk`.
- A built-in plugin is type-erased behind `BuiltinPluginFactory` and
  `BuiltinPluginInstance`; neither type exposes the concrete plugin or any host
  runtime internal.
- Every callback receives the same public, call-scoped SDK facades used by the
  trusted native packaging adapter.
- The effective capability manifest is frozen to the intersection of requested
  and granted capabilities. A private forwarding backend reports that mask even
  when the caller's backend reports broader grants. Every fresh backend must
  cover the frozen mask before plugin code runs; a missing grant is a typed
  denial rather than a silent downscope.
- Notifications, decisions, commands, and timers use the shared twelve-event
  routing and capability matrix. Decision callback errors remain errors so the
  caller can fail closed. A successful decision must occupy capacity in the
  same bounded command stage before the caller commits any mutating effect.
- The caller owns one fresh bounded transactional mutation stage per callback.
  It commits only a successful result and discards staged subscriptions,
  registrations, operations, storage writes, timers, and decisions for every
  error. Reads need no commit; failed-callback diagnostics may be retained.
- Cooperative callback errors retain an active instance. A caught callback
  panic poisons the instance and forgets its potentially inconsistent plugin
  state.
- Shutdown consumes the instance. Plugin destruction panics are caught after
  load failure, during shutdown, and during an implicit drop. Explicit shutdown
  reports the panic; implicit drop has no result channel and cannot report it.
- The adapter has no global registry, inventory, host registration side effect,
  channel, executor, or direct access to simulation, world, storage, or network
  internals.
