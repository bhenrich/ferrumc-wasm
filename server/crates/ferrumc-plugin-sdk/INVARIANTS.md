# Invariants: ferrumc-plugin-sdk

- The crate forbids unsafe code and exposes no raw simulation, world, network,
  storage, executor, or channel internals.
- The author-facing contract is packaging-independent. Built-in and
  trusted native plugin adapters implement the same hidden host-services seam.
- Every callback context contains one mutable, call-scoped host-services
  backend. Facades are short reborrows of that backend and cannot outlive the
  callback through safe code.
- Every facade accessor checks its capability before returning a facade.
  Diagnostics and deterministic tick timers are packaging services and do not
  invent capability bits absent from ABI v1.
- World observation is read-only. World changes are submitted as bounded,
  typed operations and are never applied directly by this crate.
- Plugin storage is host-namespaced. Authors cannot supply a plugin namespace.
  Keys, values, and key-list responses are bounded to fit the current ABI
  output contract.
- Command registrations are bounded pure data with stable, nonzero handler
  identifiers. They contain no Rust closures or function pointers.
- Timers use tick delays only; there is no wall-clock scheduling API.
- All public items have rustdoc. No generated files are edited by this crate.
