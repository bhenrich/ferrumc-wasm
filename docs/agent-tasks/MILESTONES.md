# FerrumC v2 — Milestone Roadmap (M00–M29)

> Execution plan for the vertical slice MVP: status ping → offline login → flat world → storage → one shard → one plugin.
> Each milestone is one delegatable agent task. One milestone per branch/worktree. See [TEMPLATE.md](TEMPLATE.md) for the per-task format.

## How to read this

- **Touch** = the only crates an agent may modify for that milestone.
- **Don't touch** = explicitly out of scope; if you think you need it, STOP and report the smallest blocker.
- **Acceptance** = commands that must pass (run from the Cargo workspace root `server/`).
- **Lane** = which parallel track it belongs to (see Lanes below).
- Paths: git root is `core/`, Cargo workspace is `core/server/`, docs are `core/docs/`.

## Status legend

✅ done & committed · 🔄 in progress · ⏳ blocked/queued · ⬜ not started

| Milestone | Status |
|---|---|
| PR2 codec | ✅ committed `0e5cb19e` |
| PR3 nbt | ✅ committed `71097320` |
| M01 nbt consumed-bytes | 🔄 |
| M02 core types | 🔄 |
| M03 math types | 🔄 |
| M00, M04–M29 | ⬜ |

## Common prompt prefix (paste into every milestone agent)

```
Read CLAUDE.md, AGENTS.md, docs/architecture/overview.md, and the README.md/INVARIANTS.md
for every crate you touch. Follow the milestone scope exactly. Do not edit unrelated crates.
Do not hand-edit generated files. Add tests. Run the listed acceptance commands. If you hit a
real blocker, stop and report the smallest blocker instead of broadening scope. Do not commit;
the orchestrator commits.
```

## Standard acceptance gates

```bash
# from core/server
cargo fmt -p <crate> -- --check
cargo clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
# workspace gate before merge:
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

## Lanes (max 3 active implementation agents at once)

```
Lane A  protocol/net      M01 → M04 → M05 → M06 → M08 → M09 → M10 → M11 → M14 → M15
Lane B  world/storage/sim M02 → M03 → M12 → M13 → M16 → M17 → M18 → M19 → M20
Lane C  commands/plugins  M02 → M26 → M27 → M28 → M29
Lane D  testkit/tooling   M00 → M07, supports all lanes
```

Never run two milestones in parallel that both touch a chokepoint crate: `ferrumc-proto`, `ferrumc-net`, `ferrumc-sim`, `ferrumc-plugin-api`.

## Worktree protocol (one agent = one worktree)

```bash
# from core/
git worktree add ../ferrumc-m03 -b agent/m03-math rework/v2-skeleton
cd ../ferrumc-m03/server && claude   # run /ferrumc:milestone M03 (or paste the task)
# after acceptance + review pass, from core/:
git checkout rework/v2-skeleton
git merge --ff-only agent/m03-math
git worktree remove ../ferrumc-m03 && git branch -d agent/m03-math
```

---

# Milestones

## M00 — Freeze baseline + CI bootstrap
- **Goal:** PR2/PR3 committed cleanly; minimal CI + git hooks in place.
- **Touch:** `.github/workflows/`, `.githooks/`, `scripts/`, `server/.cargo/config.toml`.
- **Don't touch:** codec/nbt impl except to fix failures.
- **Acceptance:** workspace fmt/clippy/test green in CI on Ubuntu.
- **Lane:** D. Do first. (PR2/PR3 commit portion: ✅ already done.)

## M01 — NBT embedded `bytes_consumed` API  🔄
- **Goal:** decode NBT embedded in a larger buffer, returning bytes consumed.
- **Touch:** `ferrumc-nbt`.
- **Don't touch:** `ferrumc-proto`, `ferrumc-net`, Anvil.
- **Deliver:** `read_network_root_with_consumed`, `read_named_root_with_consumed`; `max_bytes` bounds *consumed* bytes, not slice length; trailing bytes left unread; existing readers unchanged.
- **Acceptance:** `cargo test -p ferrumc-nbt`.
- **Lane:** A.

## M02 — Core IDs, errors, ticks, text components  🔄
- **Goal:** stable shared leaf types.
- **Touch:** `ferrumc-core`.
- **Don't touch:** networking, world, storage; no other ferrumc crate dep; no tokio.
- **Deliver:** `PlayerId` (UUID), `ConnectionId`, `EntityId`, `WorldId`, `DimensionId`, `PluginId`, `Tick`, `ServerError`/`Result<T>`, `TextComponent`, `GameMode`.
- **Acceptance:** `cargo test -p ferrumc-core`.
- **Lane:** B/C root.

## M03 — Math and coordinate types  🔄
- **Goal:** all positions strongly typed; floor-division correct for negatives.
- **Touch:** `ferrumc-math` (dep: `ferrumc-core`).
- **Don't touch:** world storage, packet code.
- **Deliver:** `BlockPos`, `ChunkPos`, `SectionPos`, `RegionPos`, `ShardPos` (8×8 chunks), `LocalBlockPos`, `Vec3`, `Aabb`, `Direction`.
- **Acceptance:** `cargo test -p ferrumc-math`.
- **Lane:** B.

## M04 — Registry snapshot + minimal runtime registry
- **Goal:** deterministic 1.21.8 data foundation.
- **Touch:** `ferrumc-registry`, `fixtures/protocol/1_21_8`, `docs/protocol`.
- **Don't touch:** generated packet Rust output.
- **Deliver:** pinned data manifest (source/version/checksum), minimal block-state/biome/dimension IDs for flat world.
- **Acceptance:** `cargo test -p ferrumc-registry`.
- **Lane:** A. Blocks M05, M12.
- **Decision now:** vendor a pinned `minecraft-data` snapshot for Java 1.21.8 (protocol 772); record upstream version/commit/checksums. **Verify 772 packet data exists before relying on it.**

## M05 — Protocol generator scaffold
- **Goal:** deterministic generation before meaningful packet code.
- **Touch:** `ferrumc-proto-gen`, `xtask`, `ferrumc-proto/src/generated`, `docs/protocol`.
- **Don't touch:** net, sim.
- **Deliver:** `cargo xtask generate` + `--check`; generated files carry `// @generated by ferrumc-proto-gen. Do not edit by hand.`
- **Acceptance:** `cargo xtask generate --check`; `cargo test -p ferrumc-proto-gen`.
- **Lane:** A. After M04.

## M06 — Generated protocol subset: handshake/status/login/config
- **Goal:** typed packets for the first connection states.
- **Touch:** `ferrumc-proto`, `ferrumc-proto-gen`, `fixtures/protocol/1_21_8`.
- **Deliver:** Handshake, Status req/resp, Ping/Pong, LoginStart/Success, SetCompression, LoginAcknowledged, minimal config transition packets.
- **Acceptance:** `cargo xtask generate --check`; `cargo test -p ferrumc-proto`.
- **Lane:** A. After M05. Blocks M08/M09.

## M07 — Testkit fake client + protocol scripts
- **Goal:** the permanent test harness (fake clients, replay).
- **Touch:** `ferrumc-testkit`, `fixtures/protocol/1_21_8`.
- **Deliver:** `ScriptedClient`, `PacketScript`, `HexFixture`, `assert_packet_roundtrip`, transcript record/replay.
- **Acceptance:** `cargo test -p ferrumc-testkit`.
- **Lane:** D. Fixture utils can start early; client after M06.

## M08 — Net connection state machine + frame integration
- **Goal:** integrate codec/proto into net (no gameplay).
- **Touch:** `ferrumc-net`.
- **Deliver:** `ConnectionState`, `ConnectionLimits`, frame decode integration, `DecodeError -> DisconnectClass`, reader/writer split types.
- **Acceptance:** `cargo test -p ferrumc-net`.
- **Lane:** A. After M06.

## M09 — Status ping server
- **Goal:** real TCP status endpoint.
- **Touch:** `ferrumc-net`, `ferrumc-app` (minimal wiring).
- **Deliver:** TCP acceptor, handshake→status→JSON, ping→pong, timeouts, connection limits, fake-client integration test.
- **Acceptance:** `cargo test -p ferrumc-net status`; `cargo test -p ferrumc-app status`.
- **Lane:** A. After M08.

## M10 — Compression pipeline
- **Goal:** compression framing before login/play.
- **Touch:** `ferrumc-net` (+ `ferrumc-proto` tests).
- **Deliver:** `CompressionState`, threshold handling, compressed/uncompressed body rules, inbound output cap, compressed-below-threshold rejection.
- **Acceptance:** `cargo test -p ferrumc-net compression`.
- **Lane:** A. After M08, before M11.

## M11 — Offline login + play transition
- **Goal:** fake client reaches Play state.
- **Touch:** `ferrumc-net`, `ferrumc-proto`, `ferrumc-testkit`.
- **Deliver:** offline UUID, LoginSuccess, SetCompression, LoginAcknowledged/config→Play, keepalive shell.
- **Acceptance:** `cargo test -p ferrumc-net offline_login`.
- **Lane:** A. After M10.

## M12 — World block model + palettes
- **Goal:** chunk internals.
- **Touch:** `ferrumc-world`, `ferrumc-registry`.
- **Deliver:** `BlockStateId`, `PalettedContainer`, `PackedArray`, chunk-section palette with promotion rules.
- **Acceptance:** `cargo test -p ferrumc-world`.
- **Lane:** B. After M03/M04.

## M13 — Chunk model + flat world generator
- **Goal:** chunks the sim can own and net can encode.
- **Touch:** `ferrumc-world`.
- **Deliver:** `Chunk`, `ChunkSection`, `Heightmaps`, light/block-entity placeholders, dirty flags, `FlatWorldGenerator`.
- **Acceptance:** `cargo test -p ferrumc-world`.
- **Lane:** B. After M12.

## M14 — Play protocol subset (chunks/movement/blocks)
- **Goal:** minimum Play packets.
- **Touch:** `ferrumc-proto`, `ferrumc-proto-gen`, `fixtures/protocol/1_21_8`.
- **Deliver:** JoinGame, PlayerPosition, KeepAlive, ChunkDataAndLight, BlockUpdate, PlayerInfoUpdate, AddEntity/spawn, MovePlayer, PlayerAction, UseItemOn, SetCreativeModeSlot (if needed), ChatCommand.
- **Acceptance:** `cargo xtask generate --check`; `cargo test -p ferrumc-proto play_subset`.
- **Lane:** A. After M06 + M13. Blocks M15/M22.

## M15 — Net Play reader/writer, queues, budgets
- **Goal:** safe Play I/O without game logic.
- **Touch:** `ferrumc-net`.
- **Deliver:** `PlayReader`/`PlayWriter`, `OutboundPriority`, bounded outbound queues, packet budget token buckets, movement coalescing placeholder, disconnect policy, metrics counter placeholders.
- **Acceptance:** `cargo test -p ferrumc-net play_io`.
- **Lane:** A. After M14 + M11.

## M16 — Storage traits + memory backend
- **Goal:** sim can be written against storage before DB choice.
- **Touch:** `ferrumc-storage` (+ `ferrumc-world`).
- **Don't touch:** redb/LMDB impl.
- **Deliver:** `WorldStore`/`PlayerStore`/`PluginStore` traits, versioned record types, `InMemoryStore`.
- **Acceptance:** `cargo test -p ferrumc-storage`.
- **Lane:** B. After M13.

## M17 — Storage backend benchmark (redb vs heed/LMDB)
- **Goal:** decide storage with data.
- **Touch:** `ferrumc-storage`, `docs/experiments`, `benches/`.
- **Deliver:** benchmark harness (flat/mixed chunk save/load, random reads, batched flush, entity index churn, player + plugin KV, reopen smoke); `docs/experiments/storage-backend-results.md`; `docs/adr/0007-storage-backend.md`.
- **Decision rule:** choose redb unless heed/LMDB is materially better (~1.75× p95 in relevant ops, or clearly better size/crash behavior).
- **Acceptance:** `cargo test -p ferrumc-storage`; `cargo bench -p ferrumc-storage`.
- **Lane:** B. After M16.

## M18 — Native storage backend implementation
- **Goal:** implement the ADR-0007 backend behind the traits.
- **Touch:** `ferrumc-storage`.
- **Deliver:** schema metadata, chunk/entity(+index)/player/plugin tables, batched writes; raw DB handle never escapes the crate.
- **Acceptance:** `cargo test -p ferrumc-storage`.
- **Lane:** B. After M17.

## M19 — Sim messages + tick coordinator
- **Goal:** deterministic simulation, no client dependency.
- **Touch:** `ferrumc-sim`.
- **Deliver:** `GameInput`/`GameOutput`, `TickCoordinator`, one `SimShard`, bounded `ShardInbox`, deterministic tick harness.
- **Acceptance:** `cargo test -p ferrumc-sim`.
- **Lane:** B. After M02/M03 (better after M16).

## M20 — Sim chunk tickets + flat spawn loading
- **Goal:** sim owns generated/loaded spawn chunks.
- **Touch:** `ferrumc-sim`, `ferrumc-world`, `ferrumc-storage` (tests).
- **Deliver:** `ChunkTicket`, `LoadedChunkMap`, load-or-generate flow, spawn ticket, dirty tracking handoff.
- **Acceptance:** `cargo test -p ferrumc-sim chunk`.
- **Lane:** B. After M13/M16/M19.

## M21 — Session router + join bridge
- **Goal:** bridge Play connections to sim (no full chunk send yet).
- **Touch:** `ferrumc-session`, `ferrumc-sim`, `ferrumc-net` message interfaces.
- **Deliver:** `SessionRouter`, `PlayerSessionHandle`, `NetEvent→GameInput`, `GameOutput→OutboundPacket` shell, join flow, disconnect cleanup.
- **Acceptance:** `cargo test -p ferrumc-session`; `cargo test -p ferrumc-sim join`.
- **Lane:** A/B join. After M15/M19.

## M22 — Vertical slice: client joins, sees flat world
- **Goal:** first true vertical slice (fake-client gated).
- **Touch:** `ferrumc-app`, `ferrumc-session`, `ferrumc-sim`, `ferrumc-proto`.
- **Deliver:** server start → offline login → Play → join/config packets → spawn chunks → positioned in flat world; integration test; manual vanilla smoke doc.
- **Acceptance:** `cargo test -p ferrumc-app vertical_join`.
- **Lane:** integration. After M14/M15/M20/M21.

## M23 — Movement handling + position sync
- **Touch:** `ferrumc-sim`, `ferrumc-session`, `ferrumc-net`.
- **Deliver:** serverbound movement → tick-bound inputs, finite-coord validation, per-tick coalescing, position update, optional correction output.
- **Acceptance:** `cargo test -p ferrumc-sim movement`; `cargo test -p ferrumc-session movement`.
- **Lane:** integration. After M22.

## M24 — Multiplayer visibility
- **Touch:** `ferrumc-sim`, `ferrumc-session`, `ferrumc-proto`.
- **Deliver:** player-list update, spawn/despawn player entity packets, movement broadcast to nearby viewers, view-distance scoping, two-client integration test.
- **Acceptance:** `cargo test -p ferrumc-session multiplayer`; `cargo test -p ferrumc-app two_clients`.
- **Lane:** integration. After M23.

## M25 — Block break/place loop
- **Touch:** `ferrumc-sim`, `ferrumc-session`, `ferrumc-world`, `ferrumc-proto`.
- **Deliver:** serverbound break/place inputs, basic reach/dimension checks, chunk mutation, dirty flags, block-update broadcast, fake-client test.
- **Acceptance:** `cargo test -p ferrumc-sim block_interaction`; `cargo test -p ferrumc-app block_break_place`.
- **Lane:** integration. After M22 (better after M23).

## M26 — Commands, permissions, `/spawn`, `/gamemode`
- **Touch:** `ferrumc-command`, `ferrumc-permission`, `ferrumc-sim`, `ferrumc-session`.
- **Deliver:** `CommandTree`, `CommandSource`, `CommandResult`, `PermissionNode`/`PermissionSet`, built-in `/spawn` + `/gamemode`. (No Brigadier compat.)
- **Acceptance:** `cargo test -p ferrumc-command`; `cargo test -p ferrumc-permission`; `cargo test -p ferrumc-sim commands`.
- **Lane:** C. Parser/permission core after M02; sim wiring after M21/M23.

## M27 — Plugin API v0 (in-process fixture)
- **Goal:** stabilize the API shape BEFORE dynamic loading.
- **Touch:** `ferrumc-plugin-api`, `ferrumc-plugin-host`, `ferrumc-command`, `ferrumc-permission`.
- **Don't touch:** dynamic loader.
- **Deliver:** `Plugin` trait (in-process), `PluginMetadata`, `CapabilityManifest`, event/command registrars, `WorldView` read facade shell, `CommandSink` intent shell, panic isolation, budget timing.
- **Acceptance:** `cargo test -p ferrumc-plugin-api`; `cargo test -p ferrumc-plugin-host`.
- **Lane:** C. After M26 core; event bridge after M25.

## M28 — Dynamic plugin loader (C ABI)
- **Goal:** `/plugins` dynamic loading without exposing Rust internals.
- **Touch:** `ferrumc-plugin-api`, `ferrumc-plugin-host`, `plugins/`.
- **Deliver:** libloading host, `extern "C"` entrypoint, ABI version check, metadata loading, opaque handles, fixture dylib plugin, failure isolation, platform extension handling.
- **Rule:** NO Rust `String`/`Vec`/`Result`/trait objects/refs/unspecified-layout enums across the ABI. `repr(C)`, opaque handles, host-owned allocations, explicit free/callbacks. Ergonomics go in a Rust SDK wrapper.
- **Acceptance:** `cargo test -p ferrumc-plugin-host dynamic`.
- **Lane:** C. After M27. **Do not parallelize with M29.**

## M29 — Spawn-protect plugin + final MVP gate
- **Touch:** `plugins/ferrumc-plugin-spawn-protect`, `ferrumc-plugin-host`, `ferrumc-sim`, `ferrumc-app`.
- **Deliver:** spawn-protect dynamic plugin (join welcome, configurable spawn radius, permission checks, private plugin storage), MVP fake-client e2e test, manual vanilla smoke checklist.
- **MVP gate:** status ping · offline login · see flat world · movement updates sim · two clients see each other · break/place updates chunk+viewers · `/spawn` · `/gamemode` · spawn-protect blocks unauthorized break/place · clean shutdown.
- **Acceptance:** `cargo test --workspace`; `cargo test -p ferrumc-app mvp`.
- **Lane:** C. After M28 only.

---

# Cross-cutting work (do as you go, not a milestone)

- **Protocol fixtures own the truth:** `fixtures/protocol/1_21_8/{manifest.toml,source,generated,golden,malformed,transcripts}`. No packet milestone is done without adding/updating fixtures.
- **Transcript replay** (in testkit): a failing join produces a replayable transcript file → deterministic failing artifact instead of "vanilla didn't join".
- **`cargo xtask doctor`** (after xtask exists): generated files current, fixture checksums valid, dep graph valid, no forbidden patterns, plugin ABI version consistent.
- **Metrics first, TUI later:** emit TPS / tick p50-p95 / connections / decode errors / disconnect classes / queue bytes / loaded+dirty chunks / flush latency / plugin hook time. Build the ratatui dashboard only after the metrics exist.

# Known limitations / deferred

- **Java Modified UTF-8** in NBT (NUL `0xC0 0x80`, surrogate-pair astral): currently rejected by `ferrumc-nbt`. Must be added before `ferrumc-anvil` world import.
- Deferred entirely (per CLAUDE.md "What You're NOT Building"): WASM plugins, hot reload, online-mode auth, full lighting/terrain, mob AI, redstone, Anvil import/export, multi-region distributed, full ECS.
