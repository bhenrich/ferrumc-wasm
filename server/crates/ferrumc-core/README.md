# ferrumc-core

Shared leaf types that every other crate in the server builds on. Depends on no
other project crate, and never on Tokio, storage, or networking.

Provides:

- **Identifiers** — `PlayerId` (UUID-backed, with offline-mode and random
  constructors), `ConnectionId`, `EntityId`, `WorldId`, `DimensionId`, and
  `PluginId`.
- **`Tick`** — the server tick counter, with overflow-aware advance helpers.
- **`ServerError`** and **`Result<T>`** — the shared, classifying error root.
- **`TextComponent`** / **`TextColor`** — a small structured text model for
  chat, disconnect reasons, and command output.
- **`GameMode`** — game modes with protocol id mapping.

Serde support for these value types is available behind the default-on `serde`
feature.

## Invariants

See `INVARIANTS.md` in this directory.
