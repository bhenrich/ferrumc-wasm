# FerrumC v2 — Status Ledger

> Single source of truth for "where are we / what's pending." Living doc — update at each milestone.
> Last updated: 2026-06-29.

## Snapshot
- Branch: `rework/v2-skeleton` · HEAD `f9f61c9c` · 120 commits ahead of the v1 base (`75d6f73e`) — 41 commits past the prior `f4d7b1a5` snapshot, after a large autonomous merge batch (see "Shipped (this autonomous batch)").
- 1347 tests, all green (0 failed; grown from the prior ~1118 across the new lanes). Every commit passes: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask generate --check`, `cargo test --workspace`.
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

## Shipped (this autonomous batch)
Merged across 41 commits since the prior snapshot (newest first per the orchestration merge log). Grouped:

Alpha-hardening / security:
- `810b3ee7` serverbound packet budget wired into the play read loop (admit_frame charge → disconnect, 300/600 cfg) + audit DoS fix (post-join-drain flood was skipping leave-save + ReleaseChunks → permanent chunk-ticket leak).
- `a2a0cacd` hostile-input proptest fuzz for codec/nbt/net + real fix (net compression empty-packet at threshold 0).
- `89601a04` ops: per-IP connection limiter + whitelist + ban + access-control config (config `access.rs`, net `ip_limit.rs`, tests).
- `7ab0a632` `SUPPORTED_VERSION.md` + README v2 positioning + gitignore `config.toml`.
- `f9f61c9c` real-client black-box smoke extended — 31/31 PASS vs live + restarted server (status → login → chunks → set-creative-slot → stateful place/break with state-id asserts → cross-chunk → reconnect → restart-persistence).

Persistence:
- `69c1a77e` player-state (pos/yaw/pitch/gamemode/hotbar/46-slot inventory) persists across rejoin AND restart; JoinSet shutdown drain; Codex-found teleport-mirror bug fixed + regression test.

Gameplay / creative:
- `c59930ce` placement edge cases (trapdoor / fence-gate / button / lever / anvil / end-rod / stair-corner) + sim neighbour-state exposure.
- `e90aa1a2` block-entities SIGNS — place → OpenSignEditor → UpdateSign → store → BlockEntityData broadcast → viewers + joiners render; world block-entity model (4096/chunk cap), sim apply, session packets, 2-client integ test. (v1: sign text in-memory only.)
- `f84e34a2` + `dba5e41b` worldedit-lite (`/fill` `/replace` `/undo`) via sim region funnel, applied across ticks under a per-tick budget (8192/tick default; bounded pending/undo caps).
- `0f801bb4` presentation builders (titles / subtitle / actionbar / sound / particle) + `/title` `/subtitle` `/actionbar` `/playsound` `/particle` commands.
- `6bb1eb5f` scoreboard / team / bossbar session builders + `/scoreboard` `/team` `/bossbar` commands.

Observability / dashboard:
- `a9d65005` rebuilt dashboard — Svelte 5 SPA + axum SSE `/events` + `/api/snapshot` + `ServeDir(dist)` (retired the htmx `pages.rs`).
- `3a9e6dd6` live telemetry wiring — bounded net_telemetry hub + plugin decision tally → live snapshot aggregation (per-player net, packet-trace summaries, plugin_decisions).
- `b8a4df1f` Prometheus `GET /metrics` exporter in the dashboard (zero-dep, reads live snapshot).

Tooling / data:
- `f0147795` 14 CB + 1 SB reserved protocol packets (titles / sounds / particles / scoreboard / team / bossbar / block-entity); ProtoVerify 15/15 ids match.
- `dffa2d11` registry named block/item id constants + drift-guard + de-magic'd block-rules plugin.
- `2107c68a` real Anvil `.mca` region reader + chunk import + 16KB fixture + malformed-input tests (greenfield crate). (Startup map-load still un-wired.)

## Deferred / NOT-yet-built (GPT's "later" bucket — out of CreativeCore-v0 scope, in rough priority)
1. **Published benchmark numbers** — harness exists; no numbers run on real hardware yet (commit SHA + hardware).
2. **Fresh-clone build verification** — confirm a clean clone builds + runs.
3. **Armor/offhand equipment** + remaining **placement edge cases** (waterlogging, rails, doors/beds multi-block). (Single-block edge cases — trapdoor/fence-gate/button/lever/anvil/end-rod/stair-corner — shipped this batch.)
4. **Block entities: chests/containers** — prerequisite for survival storage. (Signs shipped as block-entity #1.)
5. **Online-mode** auth + encryption; then **proxy/Velocity** forwarding. (Per-IP limit + whitelist + ban shipped this batch.)
6. **Real terrain gen** (noise/biomes/caves/structures) + **real light engine** (currently full-bright).
7. **Survival** — health/hunger/XP/death/respawn, crafting/smelting, containers/GUIs, tools/mining-time/drops.
8. **Entities/mobs** — generic entity system, AI/pathfinding, item entities, projectiles, combat.
9. **Fluids + block physics + redstone + scheduled/random ticks.**
10. **Social (remaining)** — signed chat, resource packs. (Scoreboards/teams/bossbars/titles/sounds/particles shipped this batch.)
11. **Ops (remaining)** — RCON/admin, anti-cheat, config hot-reload. (Ban/whitelist, per-IP rate-limit, and the dashboard /metrics exporter shipped this batch.)
12. **Scale** — multi-shard runtime + cross-shard transfer + determinism replay harness.
13. **Plugins** — WASM/Component-Model runtime; the C-ABI cdylib loader is a stub (in-process plugins work today).

## Known minor follow-ups (not blocking)
- JoinGame `dimension=undefined` seen by the node client (it still joins + gets chunks; likely a log/field-name detail, verify the JoinGame dimension field).
- Window-click container handling is minimal (state-id check → resync).
- Full-bright lighting placeholder.

New tracked follow-ups (this batch; non-blocking):
- Sign text persistence across restart/unload (v1 stores sign text in-memory only).
- Crash-safety flush: only SIGINT (ctrl_c) flushes today; add a SIGTERM/SIGKILL graceful-shutdown path (smoke found this).
- Player inventory data-components (schema v2) + conditional `PlayerAbilities`-on-join.
- Anvil startup map-load wiring (reader crate done; not yet wired into startup).
- Per-sign edit-session ownership.
- Budget: `burst < 1.0` config validation + durable Prometheus kick metric.
- Presentation/scoreboard hand-encoded wire payloads: confirm via a real client (golden tests guard regression only).

## Public-alpha gate
Mostly green (see `docs/public-alpha.md`). The real-client smoke now covers place/break/reconnect/restart-persistence (31/31). Open: published benchmarks, fresh-clone build check.

## How to run / test
```
cd core/server && cargo run                      # join 127.0.0.1:25565; dashboard 127.0.0.1:9090
cargo run -p ferrumc-bench --release             # reproducible microbenchmarks
cd ../tests/blackbox && pnpm install && node smoke.mjs 127.0.0.1 25565   # independent client
```

## Dev method (for future agents — READ THIS)
- **Identity:** commit as `Sweattypalms <stranger8722@gmail.com>` (repo-local git config is set). NEVER add a Co-Authored-By trailer or mention Claude/Codex/AI in commits or PRs (the business identity `git@saadm.com` must never appear).
- **Pushing is gated:** commit locally freely, but NEVER `git push` / publish without Saad's EXPLICIT confirmation each time — the repo is public and he tests locally before anything goes public.
- **Workflow:** per-commit pipeline scout → implement → adversarial-review → fix-to-green; subagents on 1M-context Opus; **Codex (gpt-5.5) independently audits every protocol-touching commit** (it has caught ~a dozen real bugs the Claude review missed). Keep workflow structured-output schemas LEAN (a complex schema once hit the retry cap).
- **Parallelism:** one code-editing workflow per checkout; for true parallel lanes use a git **worktree per workstream** with its own `CARGO_TARGET_DIR`, then merge serially (placement ‖ dashboard proved this). Reserve new crates in one tiny serial commit first to avoid root-Cargo.toml/lock collisions.
- **Protocol ground truth:** `fixtures/protocol/1_21_8/protocol.json` (+ `blocks.json`). Human-readable: `../wiki/protocol/`. v1 reference impl: `../ref/`. NEVER hand-edit `crates/ferrumc-proto/src/generated/`; edit `docs/protocol/1_21_8/packets.toml` + `cargo xtask generate`. Complex tagged-union packets are modeled as opaque bytes + hand-encoded.
- **Gates every commit:** fmt + clippy -D + xtask generate --check + full test. Verify the gate yourself before committing (don't trust a workflow's self-report).
