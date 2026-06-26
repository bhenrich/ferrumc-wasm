# Invariants: ferrumc-storage

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- All public items have rustdoc.
- Error types classify the failure mode.

## Crate-Specific

- Reads of a missing key return `Ok(None)` (or an empty collection), never an
  `Err`. An `Err` means the operation itself failed.
- Every record carries a `SchemaVersion`, preserved verbatim across save/load.
  Storage never reinterprets or rewrites it.
- `PluginStore` is namespaced per `PluginId`. A plugin can never read,
  enumerate, or delete another plugin's keys.
- Untrusted input (plugin keys/values, record payloads, save batches) is bounded
  before allocation and rejected with a classifying `StorageError`.
- `InMemoryStore` is an owned struct, not a global. State is shared via `Arc`,
  not static mutable storage.
- Coordinates in keys are typed (`ChunkPos`), never raw tuples.
- Trait methods are `async` via `async_trait`, keeping the traits `Send + Sync`
  and dyn-compatible.
