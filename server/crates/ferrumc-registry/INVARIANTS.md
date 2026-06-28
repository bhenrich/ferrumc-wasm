# Invariants: ferrumc-registry

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- All public items have rustdoc.
- Error types classify the failure mode.

## Crate-Specific

- The runtime is dependency-free and does no I/O or parsing. Block/biome/dimension
  data is hardcoded as `const`; item data is generated at **build time** by
  `build.rs` from the vendored `data/*.json` snapshots into static arrays emitted
  to `OUT_DIR`. Either way the runtime never parses JSON and pays zero boot cost
  (no `OnceCell`/`lazy_static`). JSON/TOML parsing exists only in `build.rs` and
  under `#[cfg(test)]`.
- `build.rs` MUST assert the item data's structural invariants (contiguous ids
  `0..=N`, every item carries a `u8`-sized `max_stack_size`) and panic loudly on
  drift, so a botched re-vendor fails the build instead of shipping wrong tables.
- Block-state ids are fixed protocol constants and MUST equal the default state
  of the corresponding block in the vendored `blocks.json` at the pinned commit.
  The drift-guard tests enforce this; do not change a constant without re-pinning
  and re-vendoring the fixtures + manifest checksums together.
- Biome and dimension-type ids are server-assigned dynamic-registry indices, not
  protocol constants; expose resource-location strings, never numeric ids.
- `manifest.toml` checksums and byte counts must match the vendored fixtures.
- This crate may depend only on `ferrumc-core`; never on sim, net, or storage.
