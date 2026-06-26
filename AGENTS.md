# AGENTS.md — Universal Agent Instructions

> This file is for ALL AI coding agents (Claude Code, Codex, Cursor, Copilot, etc.).
> Agent-specific instructions are in their respective files (CLAUDE.md for Claude Code).
> If something here conflicts with an agent-specific file, the agent-specific file wins.

---

## Project Identity

- **Name:** FerrumC (capital F, capital C — always)
- **What:** High-performance Minecraft Java Edition server implementation in Rust
- **Owner:** Saad (GitHub: Sweattypalms)
- **License:** MIT
- **Protocol:** Minecraft Java 1.21.8 (protocol version 772)
- **Repo:** github.com/ferrumc-rs/ferrumc
- **Branch:** rework/ferrumc-v2

---

## Before You Write Code

1. **Read CLAUDE.md** (or this file) completely.
2. **Identify which crate you're working in.** Check the crate map.
3. **Read that crate's INVARIANTS.md.**
4. **Read the relevant docs/architecture/*.md and docs/adr/*.md.**
5. **Confirm scope.** Your change should touch 1-2 crates max.

If you skip these steps, your output will be wrong.

---

## Hard Rules (Violations = Rejected Output)

### Architecture Rules

1. **One crate per task.** Maybe two if one is `ferrumc-testkit`. Never three+.
2. **Respect the dependency graph.** `ferrumc-net` does NOT depend on `ferrumc-world`. Check CLAUDE.md crate map.
3. **No lane crossing.** Network code doesn't mutate world state. Simulation doesn't touch sockets. Storage runs on its own thread.
4. **No hand-editing generated files.** Anything in `crates/ferrumc-proto/src/generated/` is machine-generated. Fix the generator instead.

### Code Rules

5. **No `unwrap()` or `expect()` outside tests.** Use `?` operator or explicit error handling.
6. **No unbounded channels.** Every `mpsc::channel()` has an explicit capacity. Document why you chose that number.
7. **No blocking in async.** Use `spawn_blocking` or dedicated threads for CPU/IO work.
8. **No global mutable state.** No `lazy_static!` mutexes, no `static mut`, no mutable `OnceLock`.
9. **No `pub` fields on cross-crate types.** Use methods.
10. **No raw `(i32, i32)` coordinates.** Use `BlockPos`, `ChunkPos`, `ShardPos`.
11. **Error types must be specific.** `BadVarInt`, `FrameTooLarge` — not `anyhow!("error")` (except in ferrumc-app).
12. **`#![forbid(unsafe_code)]`** unless documented in `docs/safety/<crate>.md`.
13. **Rustdoc on every public item.** No exceptions.

### Test Rules

14. **Every parser needs malformed-input tests.** Not just happy path.
15. **No wall-clock sleeps.** Use `tokio::time::pause()` or deterministic harness.
16. **No real network in unit tests.** Use `ferrumc-testkit::FakeClient`.
17. **Tests must be deterministic.** Same input = same output, always.

### Style Rules

18. **`cargo fmt`** — no custom formatting rules.
19. **`cargo clippy -- -D warnings`** — all warnings are errors.
20. **Commit messages:** `type(crate): description` — e.g. `feat(codec): add VarLong decoder`
21. **Comments explain WHY, not WHAT.**

---

## Crate Purposes (Quick Reference)

| Crate | Lane | Purpose |
|-------|------|---------|
| `ferrumc-core` | Shared | PlayerId, EntityId, Tick, Result, errors |
| `ferrumc-math` | Shared | BlockPos, ChunkPos, ShardPos, Vec3, Aabb |
| `ferrumc-codec` | Network | VarInt, VarLong, bounded readers/writers |
| `ferrumc-nbt` | Network | NBT parsing with safety limits |
| `ferrumc-proto` | Network | Generated packet types for 1.21.8 |
| `ferrumc-net` | Network | TCP, framing, compression, encryption |
| `ferrumc-session` | Bridge | Player↔shard routing, packet budgets |
| `ferrumc-world` | Simulation | Chunk, Palette, Heightmap (pure data, no IO) |
| `ferrumc-sim` | Simulation | Tick coordinator, shards, entity systems |
| `ferrumc-storage` | Storage | Traits + redb impl, dedicated worker thread |
| `ferrumc-plugin-api` | Plugin | Stable API surface for plugins |
| `ferrumc-plugin-host` | Plugin | Registry, lifecycle, dispatch, isolation |
| `ferrumc-app` | Wiring | Connects everything, startup, shutdown |

---

## Common Mistakes

1. **Adding wrong dependency.** Check the crate map before adding `use ferrumc_*`.
2. **Using DashMap.** Data should be owned by a shard, not shared behind a lock.
3. **Making things too public.** Default to `pub(crate)`. Only `pub` if it crosses crate boundaries.
4. **`async fn` for CPU work.** Parsing, encoding, palette ops are sync.
5. **Validation in wrong layer.** Wire format limits → codec/net. Game logic limits → sim.
6. **Creating "utils" modules.** Shared stuff goes in `ferrumc-core` or `ferrumc-math`.
7. **Forgetting backpressure.** Every queue: what happens when full? Document it.
8. **Testing only happy path.** Include: empty, truncated, oversized, maximum-boundary, malicious.

---

## Task Acceptance Checklist

Before submitting output, verify:

- [ ] Touches ≤2 crates
- [ ] All new public items have rustdoc
- [ ] All new parsers have malformed-input tests  
- [ ] No `unwrap()` outside tests
- [ ] No unbounded channels
- [ ] No hand-edited generated files
- [ ] Commit message follows convention
- [ ] Relevant INVARIANTS.md updated if new rules apply
