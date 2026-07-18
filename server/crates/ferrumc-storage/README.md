# ferrumc-storage

Persistent storage traits and the M16 in-memory backend.

This crate defines *what* the rest of the server can persist, behind async
traits, plus an owned in-memory implementation used by tests and the test
harness. The durable redb/LMDB worker-thread backend lands in a later milestone;
nothing here touches a database or spawns a thread yet.

## Traits

- [`WorldStore`] — chunk columns ([`ChunkRecord`]) and entities
  ([`EntityRecord`]), addressed by typed [`ChunkKey`] / [`EntityKey`].
- [`PlayerStore`] — per-player records ([`PlayerRecord`]) keyed by `PlayerId`.
- [`PluginStore`] — per-plugin private key-value storage. Every call is scoped
  to a `PluginId`, so a plugin can never read, enumerate, or delete another
  plugin's keys.

Reads of a missing key return `Ok(None)` (or an empty collection), never an
error.

## Records and versioning

Every record carries a [`SchemaVersion`], preserved verbatim across save/load,
so its owning layer can detect and refuse data written by an incompatible
build. No automatic pre-alpha migration is attempted. A `ChunkRecord` holds a
structured `Chunk`; `EntityRecord` and `PlayerRecord` carry a length-bounded
opaque payload owned by the simulation layer (a
`PlayerRecord` also keeps the typed `GameMode`).

## Async strategy

The traits use [`async_trait`](https://docs.rs/async-trait). This boxes returned
futures as `Box<dyn Future + Send>`, which keeps the traits dyn-compatible (the
simulation holds `Arc<dyn WorldStore>`) and guarantees `Send` futures. Native
async-fn-in-trait was rejected: it cannot express the `Send` bound and trips the
`async_fn_in_trait` lint under `-D warnings`.

## Bounded shapes

Untrusted input is bounded before allocation: plugin keys
([`MAX_PLUGIN_KEY_LEN`]) and values ([`MAX_PLUGIN_VALUE_LEN`]), record payloads
([`MAX_ENTITY_DATA_LEN`], [`MAX_PLAYER_DATA_LEN`]), and batched saves
([`MAX_SAVE_BATCH`]). Over-limit requests are rejected with a classifying
[`StorageError`] that converts into `ServerError::Capacity`.

## Not a global

[`InMemoryStore`] is a plain owned struct. Callers construct one and share it
(typically `Arc<InMemoryStore>`); there is no global mutable state.

## Invariants

See `INVARIANTS.md` in this directory.
