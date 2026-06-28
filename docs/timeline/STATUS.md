# FerrumC v2 — Status Ledger

> Single source of truth for "where are we / what's pending." Living doc — update at each milestone.
> Last updated: 2026-06-29.

## Snapshot
- Branch: `rework/v2-skeleton` · HEAD `f4d7b1a5` · 79 commits ahead of the v1 base (`75d6f73e`).
- ~1118 tests, all green. Every commit passes: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask generate --check`, `cargo test --workspace`.
- North star: **a deterministic, Rust-native, observable creative/minigame server core for vanilla 1.21.8 clients** — NOT full vanilla.
- Validated by a **live independent client** (PrismarineJS `minecraft-protocol`): status (proto 772) → offline login → play → chunks → cross-boundary streaming, all pass.

## Shipped (this rework)
Networking / protocol:
- `4003971d` observability (packet-trace ring buffers + tick/queue/chunk metrics)
- `40ad0d93` shard-owned mutation path + block-change sequence ack
- `58163c6e` system_chat carrier — in-game chat + command feedback + /gamemode visible
- `a31bbd58` real multiplayer — fixed malformed PlayerInfoUpdate crash; player entity 149; movement; despawn
- `87d45c68` command-tree sync + tab-complete (client autocomplete)
- `e8fce17a` + `1700c996` explicit outbound backpressure + mandatory-packet delivery + (criticality,priority) channel envelope
- `b69d6965` equipment visibility (SetEquipment) + remote rotation/head-yaw
- `8b03f7cd` strict raw-wire oracle + golden-byte fixtures (the durable "fake clients lied" guard)
World / gameplay:
- `23d7bc90` + `cedc0d80` redb persistence of player-changed chunks (overlays + journal); blocks survive rejoin AND restart
- `4422d0eb` ferrumc-items: item model + 1.21.8 registry data + trusted/untrusted slot wire types
- `4fd02c07` creative inventory + place-from-hotbar
- `8d95bcdb` block-state placement engine (logs/slabs/stairs/torches/fences — rotation/facing/half) via ferrumc-placement + registry catalog
- `b4cfeaa3` plugin block before/after events (Allow/Deny/Replace/EmitIntents) at the intent boundary; sample plugins
Ops / tooling:
- `7267ff5f` connection.rs split into a module tree (parallelism unlock)
- `4a89e75a`/`b694cc52` ServerSnapshot + read-only **loopback-only** axum dashboard (ferrumc-dashboard)
- `cc1249e3` real CLI (clap: --config/--port/--bind/--version) + first-run config template + startup banner
- `23bd1fba` ferrumc-bench: reproducible server-internal microbenchmark harness (JSON + markdown + commit/host metadata)
- `e23c19d4`/`f4d7b1a5` independent node `minecraft-protocol` black-box smoke (passing for join/world/streaming)
- docs: FEATURES.md · ROADMAP.md · public-alpha.md · parity/v1-v2.md · dev/parallel-workstreams.md

## Deferred / NOT-yet-built (GPT's "later" bucket — out of CreativeCore-v0 scope, in rough priority)
1. **Player-state persistence** — wire the existing redb `PlayerStore` into join/leave/shutdown (position, game mode, inventory). Currently only chunk edits persist.
2. **Real-client smoke: place/break/reconnect steps** — smoke.mjs join+stream pass; place/break/creative-slot/persistence-verify are still TODO stubs.
3. **Published benchmark numbers** — harness exists; no numbers run on real hardware yet (commit SHA + hardware).
4. **Fresh-clone build verification** — confirm a clean clone builds + runs.
5. **Serverbound packet budget** — the 300 fps token bucket (`PlayReader`/`PacketBudget`) exists but is never constructed; play loop unbudgeted.
6. **Armor/offhand equipment** + **placement edge cases** (waterlogging, doors, beds, rails, signs, stair corners).
7. **Block entities** (chests/signs) — prerequisite for survival storage.
8. **Online-mode** auth + encryption + whitelist; then **proxy/Velocity** forwarding.
9. **Anvil import/export** (skeleton crate only; not wired).
10. **Real terrain gen** (noise/biomes/caves/structures) + **real light engine** (currently full-bright).
11. **Survival** — health/hunger/XP/death/respawn, crafting/smelting, containers/GUIs, tools/mining-time/drops.
12. **Entities/mobs** — generic entity system, AI/pathfinding, item entities, projectiles, combat.
13. **Fluids + block physics + redstone + scheduled/random ticks.**
14. **Social** — scoreboards/teams/bossbars/titles, sounds/particles, signed chat, resource packs.
15. **Ops** — RCON/admin, ban/whitelist, rate-limit/anti-cheat, config hot-reload, dashboard /metrics exporter.
16. **Scale** — multi-shard runtime + cross-shard transfer + determinism replay harness.
17. **Plugins** — WASM/Component-Model runtime; the C-ABI cdylib loader is a stub (in-process plugins work today).

## Known minor follow-ups (not blocking)
- JoinGame `dimension=undefined` seen by the node client (it still joins + gets chunks; likely a log/field-name detail, verify the JoinGame dimension field).
- Window-click container handling is minimal (state-id check → resync).
- Full-bright lighting placeholder.

## Public-alpha gate
Mostly green (see `docs/public-alpha.md`). Open: published benchmarks, place/break in the real-client smoke, fresh-clone build check.

## How to run / test
```
cd core/server && cargo run                      # join 127.0.0.1:25565; dashboard 127.0.0.1:9090
cargo run -p ferrumc-bench --release             # reproducible microbenchmarks
cd ../tests/blackbox && pnpm install && node smoke.mjs 127.0.0.1 25565   # independent client
```

## Dev method (for future agents — READ THIS)
- **Identity:** commit as `Sweattypalms <stranger8722@gmail.com>` (repo-local git config is set). NEVER add a Co-Authored-By trailer or mention Claude/Codex/AI in commits or PRs (the business identity `git@saadm.com` must never appear). See memory `no-ai-attribution`.
- **Workflow:** per-commit pipeline scout → implement → adversarial-review → fix-to-green; subagents on 1M-context Opus; **Codex (gpt-5.5) independently audits every protocol-touching commit** (it has caught ~a dozen real bugs the Claude review missed). Keep workflow structured-output schemas LEAN (a complex schema once hit the retry cap).
- **Parallelism:** one code-editing workflow per checkout; for true parallel lanes use a git **worktree per workstream** with its own `CARGO_TARGET_DIR`, then merge serially (placement ‖ dashboard proved this). Reserve new crates in one tiny serial commit first to avoid root-Cargo.toml/lock collisions.
- **Protocol ground truth:** `fixtures/protocol/1_21_8/protocol.json` (+ `blocks.json`). Human-readable: `../wiki/protocol/`. v1 reference impl: `../ref/`. NEVER hand-edit `crates/ferrumc-proto/src/generated/`; edit `docs/protocol/1_21_8/packets.toml` + `cargo xtask generate`. Complex tagged-union packets are modeled as opaque bytes + hand-encoded.
- **Gates every commit:** fmt + clippy -D + xtask generate --check + full test. Verify the gate yourself before committing (don't trust a workflow's self-report).
