# Invariants: ferrumc-plugin-fixture-dynamic

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- The crate forbids unsafe code. Its C bootstrap is emitted only through the
  audited `ferrumc-plugin-abi-sys` export macro.
- No `unwrap()` or `expect()` appears outside tests.
- Every public Rust item has rustdoc.
- The fixture must compile only with unwinding panic behavior.

## Crate-specific

- This is a test-only `cdylib`, never a production gameplay plugin.
- The descriptor target is the exact Cargo target captured by `build.rs`.
- The descriptor requests only `receive-events` and `submit-intents`.
- Initialization subscribes deterministically to `BLOCK_BREAK`,
  `AFTER_BLOCK_BREAK`, and `PLAYER_JOIN`.
- A normal block-break callback derives its message target from the event
  payload; malformed payloads produce `FC_INVALID_ARGUMENT`.
- The undeclared-access callback stages an allowed command before requesting
  world-read access. The host must return `FC_CAPABILITY_DENIED` and discard
  that callback's complete staged buffer.
- The panic-status callback stages a command before returning
  `FC_PLUGIN_PANIC`; the host must discard the buffer before disabling it.
- Installed bundles contain the exact copied platform library and a generated
  `plugin.toml` whose lowercase SHA-256 covers those exact bytes.
- The fixture performs no network, clock, random, background-thread, or global
  mutable-state work.
