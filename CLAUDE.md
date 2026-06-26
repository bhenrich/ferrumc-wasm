# CLAUDE.md — FerrumC v2

> You are working on **FerrumC**, a high-performance Minecraft Java Edition server
> implementation in Rust. This is a clean rewrite. Owner: Saad (GitHub: Sweattypalms).
> License: MIT. Target protocol: Minecraft Java 1.21.8 (protocol 772).

---

## Architecture — Know This Cold

Five hard-separated layers. Every crate lives in exactly one lane. You do NOT cross lanes.

```
ferrumc-app          → wiring, config, startup, shutdown, metrics
  ↓
ferrumc-net          → Tokio TCP, framing, encryption, compression, packet decode
  ↓
ferrumc-session      → player↔shard mapping, packet budgets, state routing
  ↓
ferrumc-sim          → deterministic shard workers, chunks, entities, events, plugins
  ↓
ferrumc-storage      → redb/lmdb behind traits, async request/response
```

### The Rules That Matter

1. **Networking never mutates the world.** It decodes, validates, budgets, and sends typed messages.
2. **Simulation never owns sockets or database handles.** It owns chunks/entities and talks to net/storage through bounded channels.
3. **Plugins never see raw internals.** Stable host capabilities only. No raw `SimShard`, no raw `Chunk &mut`, no raw DB handle.
4. **No broad global locks.** World state is owned by simulation shards. Cross-shard writes are messages applied at tick boundaries.
5. **Generated protocol code is checked in and verified.** You do NOT hand-edit files in `crates/ferrumc-proto/src/generated/`.
6. **Every queue is bounded.** If you create a channel, it has a capacity. No `unbounded()`. Ever.

---

## Crate Map — What Goes Where

Before writing a single line, confirm you're in the right crate. If you're not sure, STOP and ask.

| Crate | Purpose | Depends on | NEVER depends on |
|-------|---------|-----------|-----------------|
| `ferrumc-core` | Shared types: `PlayerId`, `EntityId`, `Tick`, `ServerError`, `Result<T>` | nothing | Tokio, storage, networking |
| `ferrumc-math` | `BlockPos`, `ChunkPos`, `ShardPos`, `Aabb`, `Vec3` | `core` | anything else |
| `ferrumc-codec` | `VarInt`, `VarLong`, `BoundedReader`, `BoundedString` — all hostile-input protection | `core` | world, sim, net |
| `ferrumc-nbt` | NBT parse/write with depth/size/list limits | `codec` | world, sim, net |
| `ferrumc-registry` | Generated block/item/entity/biome/packet registry data | `core` | sim, net, storage |
| `ferrumc-proto` | Generated packet enums for 1.21.8. Typed packets. | `codec`, `nbt`, `registry` | sim, storage |
| `ferrumc-proto-gen` | Code generator. NOT used at runtime. | N/A | everything at runtime |
| `ferrumc-net` | Tokio TCP, framing, compression, encryption, connection lifecycle | `proto`, `codec` | world, sim, storage |
| `ferrumc-session` | Bridge net↔sim. `NetEvent→GameInput`, `GameOutput→Packet` | `net`, `proto` | raw world, raw storage |
| `ferrumc-world` | Pure world model: `Chunk`, `ChunkSection`, `Palette`, `Heightmap` | `math`, `nbt`, `registry` | threads, DB, packets |
| `ferrumc-storage` | `WorldStore`/`PlayerStore`/`PluginStore` traits + redb impl | `world`, `core` | sim internals, net |
| `ferrumc-sim` | Tick coordinator, shard workers, entity systems | `world`, `storage` traits | raw net, raw DB handles |
| `ferrumc-command` | Command tree, parsing, suggestions, execution | `core` | raw sim internals |
| `ferrumc-permission` | Permission nodes, subjects, grants | `core` | everything else |
| `ferrumc-plugin-api` | Stable plugin-facing API | `core`, `math`, `command`, `permission` | raw sim, raw world, raw DB |
| `ferrumc-plugin-host` | Plugin registry, lifecycle, event dispatch, panic isolation | `plugin-api`, `sim` bridge | raw net |
| `ferrumc-anvil` | Vanilla Anvil import/export (separate from native storage) | `world`, `nbt` | sim, net |
| `ferrumc-testkit` | Fake clients, fake storage, deterministic tick harness | anything needed | N/A (test only) |
| `ferrumc-app` | Wires everything. The ONLY crate that depends on (almost) all others. | everything | N/A |

### One-Crate Rule

Your PR touches ONE crate. Maybe two if one is `ferrumc-testkit`. If you're about to touch three+ crates, you're doing it wrong. Break the task into smaller PRs.

---

## Coding Standards — Hard Rules

These are not suggestions. Violating these means your PR is rejected.

### Must Do

1. **Bounded channels only.** `tokio::sync::mpsc::channel(N)`, never `unbounded_channel()`. Document why you chose N.
2. **No `unwrap()`/`expect()` outside tests.** Use `?` or explicit error handling. The only exceptions: startup config validation and test code.
3. **No blocking in async.** If you need to do CPU work or synchronous I/O inside a Tokio task, use `tokio::task::spawn_blocking` or a dedicated thread. Never `std::thread::sleep` in async.
4. **Error types classify the problem.** `BadVarInt`, `FrameTooLarge`, `ChunkNotLoaded` — not `anyhow!("something went wrong")`. Use thiserror for library crates, anyhow only in ferrumc-app.
5. **Every parser needs malformed-input tests.** If you write a decoder, write tests for every way it can fail: too long, too short, negative, overflow, trailing bytes, zero-length, maximum-length.
6. **Every queue needs documented backpressure behavior.** What happens when it's full? Who blocks? Who drops? Document it.
7. **No `pub` fields on types that cross crate boundaries.** Use methods. Internal representation is not your API.
8. **Rustdoc on every public item.** No exceptions. Brief is fine. `/// Decodes a VarInt from the reader, rejecting inputs longer than 5 bytes.`
9. **No hand-editing generated files.** If `crates/ferrumc-proto/src/generated/` needs changes, fix the generator.
10. **No new dependencies without justification.** Add a comment in Cargo.toml explaining why. Prefer pure-Rust crates.
11. **`#![forbid(unsafe_code)]`** in every crate by default. If you genuinely need unsafe, document it in `docs/safety/<crate>.md` and minimize the surface.
12. **No global mutable state.** No `lazy_static!` mutexes, no `static mut`, no `OnceCell` holding mutable shared state. Pass state through function arguments or channels.
13. **Coordinates are typed.** `BlockPos`, `ChunkPos`, `ShardPos` — never raw `(i32, i32)` in any public API.

### Style

- `cargo fmt` — no exceptions, no custom rules
- `cargo clippy -- -D warnings` — treat all warnings as errors
- Imports: group by std → external crates → internal crates, separated by blank lines
- Error handling: `thiserror` for library error types, `?` propagation, structured variants
- Naming: Rust conventions. `snake_case` functions, `PascalCase` types, `SCREAMING_SNAKE` constants
- Comments: explain WHY, not WHAT. The code shows what. `// Reject frames > 2 MiB to prevent OOM from malicious clients` is good. `// Check if frame is too large` is useless.

### Test Conventions

- Unit tests: `#[cfg(test)] mod tests` in the same file
- Integration tests: `tests/` directory in the crate
- Fuzz targets: `fuzz/` directory, run with `cargo fuzz`
- Fixture data: `fixtures/` at workspace root, committed to git
- Test helpers: `ferrumc-testkit` crate
- No wall-clock sleeps in tests. Use `tokio::time::pause()` or deterministic tick harness.
- No real network in unit tests. Use `ferrumc-testkit::FakeClient`.

---

## Simulation Model — How the Game Works

### Actor-Sharded, Not Global ECS

Each simulation shard is 8×8 chunks. A shard exclusively owns its chunks, entities, and spatial index. No locks required — nothing else can touch them.

```rust
struct SimShard {
    shard_pos: ShardPos,
    chunks: ChunkMap,
    entities: EntityStore,        // SlotMap + ComponentVecs
    spatial: EntitySpatialIndex,  // chunk-bucket broadphase
    players: PlayerSet,
    inbox: Vec<GameInput>,
    cross_shard_outbox: Vec<CrossShardMessage>,
    dirty: DirtyTracker,
}
```

Systems are plain functions: `fn movement_system(ctx: &mut ShardTickCtx)`. Not Bevy systems. Not trait objects. Plain functions called in explicit order.

### Tick Flow (20 TPS target)

```
1. Session router drains network inputs → shard inboxes
2. Storage completions delivered
3. Plugin scheduled tasks queued
4. Active shards run IN PARALLEL on sim worker pool
5. Cross-shard messages collected (applied NEXT tick)
6. Outputs routed to sessions/network
7. Dirty chunks/entities queued for storage
8. Metrics emitted
```

Cross-shard entity transfer happens at tick boundaries ONLY. No mid-tick cross-shard mutation.

### Overload Handling (in order)

1. Coalesce movement, defer chunk sends
2. Reduce plugin schedule budget
3. Defer chunk generation and entity AI
4. Tick slower and report lag
5. **NEVER run catch-up ticks** — that turns lag into server collapse

---

## Storage Model

Storage is behind traits. The simulation never holds a DB handle.

```rust
#[async_trait]
pub trait WorldStore: Send + Sync {
    async fn load_chunk(&self, key: ChunkKey) -> Result<Option<ChunkRecord>>;
    async fn save_chunks(&self, chunks: Vec<ChunkSaveRecord>) -> Result<()>;
    // ...
}
```

Implementation runs on a dedicated storage worker thread. Requests go through a bounded queue:

```
sim/net → StorageRequest → bounded channel → storage worker → DB transaction → response oneshot
```

**No DB transaction on a sim worker or Tokio worker. Ever.**

Current backend: redb (may switch to LMDB after benchmarking — see `docs/experiments/`).

---

## Plugin Model

### Phase A (current): Compiled Rust plugins for API development
### Phase A (shipping): Dynamic libraries (.so/.dll) loaded from /plugins/ folder
### Phase B (later): WASM for language-agnostic plugins

Plugins get:
- `WorldView<'tick>` — read-only snapshot (NOT `&mut World`)
- `PlayerApi<'tick>` — send messages, teleport, query
- `CommandSink<'tick>` — mutation INTENTS, not direct mutation
- `PermissionApi<'tick>` — query permission nodes
- `PluginStorageApi` — namespaced private key-value storage

Plugins NEVER get:
- Raw `SimShard`
- Raw `Chunk` mutable reference
- Raw `EntityStore`
- Raw TCP socket or connection state
- Raw DB handle or transaction
- `tokio::Runtime` handle
- Unbounded sender to anything

`WorldView<'tick>` is `!Send` — plugins literally cannot hold it across `.await` points.

---

## Networking Model

Single Tokio multi-thread runtime. Connection-per-task with reader/writer split.

### Inbound Pipeline

```
TCP bytes → decrypt → VarInt frame length → frame body → decompress → decode packet → validate → budget check → bounded queue → session router → shard inbox
```

### Outbound Pipeline (priority queues)

```
Shard output → session router → encode packet → compress → batch (≤64KiB or 128 frames or 1ms) → encrypt → TCP write
```

Priority: Critical > State > World > Cosmetic

### Hostile Client Protection

- VarInt max 5 bytes, VarLong max 10 bytes
- Frame size limits per connection state (handshake: 4 KiB, play: 512 KiB default)
- Decompressed output caps (2 MiB default)
- Per-connection packet budget (300 frames/sec sustained)
- Per-IP rate limiting
- Trailing byte rejection
- Immediate disconnect on protocol violation

---

## Project Conventions

### Commit Messages

```
feat(codec): add bounded VarLong decoder with overflow protection
fix(net): reject compressed frames below threshold
test(nbt): add fuzz target for deeply nested compounds
docs(adr): document redb selection rationale
refactor(world): extract palette logic to separate module
chore(ci): add cargo-deny advisory check
```

Prefix with crate name in parentheses. One logical change per commit.

### Branch Naming

```
feat/codec-varlong
fix/net-compression-threshold
test/nbt-fuzz
docs/adr-storage
```

### PR Requirements

- [ ] Touches ≤2 crates (one primary + testkit if needed)
- [ ] All tests pass: `cargo test -p <crate>`
- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace -- -D warnings`
- [ ] New public items have rustdoc
- [ ] New parsers have malformed-input tests
- [ ] No `unwrap()` outside tests
- [ ] No unbounded channels
- [ ] No hand-edited generated files
- [ ] Fixtures added for new test cases

---

## What You're NOT Building

Don't get clever. Don't scope-creep. If any of these show up in your PR, it's rejected:

- WASM plugin runtime
- Hot reload
- Bukkit/Paper/Fabric compatibility
- Distributed multi-server (the architecture supports it later, not now)
- Custom region-file database
- RocksDB integration
- Full vanilla terrain generation
- Full vanilla lighting engine
- Mob AI
- Redstone
- Per-core Tokio runtimes
- Global ECS world
- Dynamic plugin dependency resolution

These are all "later" items. The current milestone is a vertical slice: status ping → offline login → flat world → storage → one shard → one plugin.

---

## Common Mistakes (Read Before Every PR)

1. **Adding a dependency on the wrong crate.** Check the crate map. `ferrumc-net` does NOT depend on `ferrumc-world`. Ever. If you think you need to, you're putting logic in the wrong crate.

2. **Using `DashMap` instead of ownership.** If you're reaching for a concurrent map, you're probably solving the wrong problem. Data should be owned by a shard, not shared behind a lock.

3. **Making things `pub` that shouldn't be.** Internal types stay `pub(crate)` or `pub(super)`. Only types that cross crate boundaries get `pub`.

4. **Writing `async fn` for CPU-bound work.** Parsing, encoding, palette operations, spatial queries — these are sync. Don't wrap them in async for no reason.

5. **Putting validation in the wrong layer.** Packet validation (frame limits, VarInt bounds, string lengths) → `ferrumc-codec`/`ferrumc-net`. Game validation (can this player break this block?) → `ferrumc-sim`. Don't mix them.

6. **Creating a "utils" or "common" module.** If something is shared, it goes in `ferrumc-core` or `ferrumc-math`. "Utils" is where code goes to become unfindable.

7. **Ignoring backpressure.** "I'll add a capacity later" means you won't. Set it now. Document what happens when it's full.

8. **Testing only the happy path.** Every decoder needs: valid input, empty input, truncated input, oversized input, maximum-boundary input, and malicious input tests.

---

## File Layout Reference

```
ferrumc/
├── CLAUDE.md                          ← you are here
├── Cargo.toml                         ← workspace root
├── Cargo.lock
├── .gitignore
├── LICENSE
├── README.md
├── crates/
│   ├── ferrumc-app/
│   ├── ferrumc-core/
│   ├── ferrumc-config/
│   ├── ferrumc-codec/
│   ├── ferrumc-nbt/
│   ├── ferrumc-math/
│   ├── ferrumc-registry/
│   ├── ferrumc-proto/
│   ├── ferrumc-proto-gen/
│   ├── ferrumc-net/
│   ├── ferrumc-session/
│   ├── ferrumc-world/
│   ├── ferrumc-storage/
│   ├── ferrumc-sim/
│   ├── ferrumc-command/
│   ├── ferrumc-permission/
│   ├── ferrumc-plugin-api/
│   ├── ferrumc-plugin-host/
│   ├── ferrumc-anvil/
│   ├── ferrumc-observability/
│   └── ferrumc-testkit/
├── plugins/
│   └── ferrumc-plugin-spawn-protect/
├── xtask/
├── docs/
│   ├── architecture/
│   │   ├── overview.md
│   │   ├── networking.md
│   │   ├── simulation.md
│   │   ├── storage.md
│   │   └── plugins.md
│   ├── adr/
│   │   ├── 0001-clean-rewrite.md
│   │   ├── 0002-actor-sharded-simulation.md
│   │   ├── 0003-single-tokio-runtime.md
│   │   ├── 0004-redb-storage.md
│   │   ├── 0005-compiled-rust-plugins-first.md
│   │   └── 0006-c-abi-dynamic-plugins.md
│   ├── agent-tasks/
│   │   └── TEMPLATE.md
│   ├── experiments/
│   │   ├── README.md
│   │   └── (benchmark results go here)
│   └── protocol/
│       └── 1_21_8/
├── fixtures/
│   ├── protocol/1_21_8/
│   ├── nbt/
│   ├── anvil/
│   └── worlds/
└── .github/
    └── workflows/
```
