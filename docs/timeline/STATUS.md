# FerrumC v2 — Status Ledger

> Single source of truth for "where are we / what's pending." Living doc — update at each milestone.
> Last updated: 2026-06-29.

## Snapshot
- Branch: `rework/v2-skeleton` · HEAD `274f53ba` · **85 commits ahead of `origin/rework/v2-skeleton`** — all LOCAL, unpushed (origin = github.com/ferrumc-rs/ferrumc). (165 commits ahead of `master`.)
- **1490 tests, all green (0 failed)** across the workspace. All four gates verified green at HEAD: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask generate --check`, `cargo test --workspace`.
- North star: **a deterministic, Rust-native, observable creative/minigame server core for vanilla 1.21.8 clients** — NOT full vanilla.
- Validated by a **live independent client** (PrismarineJS `minecraft-protocol`) AND a **real vanilla 1.21.8 client** (Saad's): status (proto 772) → offline login → play → chunks → cross-boundary streaming → place/break → restart-persistence, all pass. Dashboard proven demo-ready against the real client.

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

## Shipped — autonomous batch (32 lanes, this run)
32 feature lanes merged serially since the prior snapshot. Each lane was built in an isolated worktree, gated green, and integrated one at a time. Grouped by theme (commit hashes from the merge log):

**Alpha-hardening / security:**
- `810b3ee7` serverbound packet budget wired into the play read loop (admit_frame charge → disconnect, 300/600 cfg) **+ audit DoS fix** (post-join-drain flood was skipping leave-save + ReleaseChunks → permanent chunk-ticket leak).
- `a2a0cacd` hostile-input proptest fuzz for codec/nbt/net **+ real fix** (net compression empty-packet at threshold 0).
- `89601a04` ops — per-IP connection limiter + whitelist + ban + access-control config (config `access.rs`, net `ip_limit.rs`).
- `622def09` SIGTERM/SIGHUP now trigger the same graceful shutdown + flush as ctrl_c (was SIGINT-only — smoke found it).
- `f9f61c9c` real-client black-box smoke extended — **31/31 PASS** vs live + restarted server (status → login → chunks → set-creative-slot → stateful place/break with state-id asserts → cross-chunk → reconnect → restart-persistence).
- `7ab0a632` `SUPPORTED_VERSION.md` + README v2 positioning + gitignore `config.toml`.

**Persistence (now COMPLETE for the flat-world milestone):**
- `69c1a77e` player-state (pos / yaw / pitch / gamemode / hotbar / 46-slot inventory) persists across rejoin AND restart; JoinSet shutdown drain; **Codex-found teleport-mirror desync fixed** + regression test.
- chunk-edit overlays + journal (redb) — player-changed blocks survive unload/reload + restart.
- `8d42c04f` block-entity persistence — sign text + chest contents serialized into the chunk overlay (schema v3, bounded codec, item-conserving, malformed-safe, v2 backward-compat).
- `274f53ba` **multi-section overlay data-loss FIX** (cumulative ever-persist-edited section mask) — edits to different sections of one chunk across different ticks/flushes all survive restart. Was a CRITICAL data-loss bug; found by audit, fixed + audited + regression test. **This commit is HEAD.**

**Gameplay / creative:**
- `c59930ce` + `7ee6626b` placement (2 rounds, ~25 block families) — trapdoor / fence-gate / button / lever / anvil / end-rod / stair-corners, then waterlogging / double-slab / candle merges / walls / glass-panes / iron-bars / ladders / chains / lanterns / amethyst / dripstone.
- `a80d366d` multi-block doors/beds (atomic, obstruction-rejected) + sim honors `place_at` (double-slab/candle merges) + water-replaceable cell (waterlogging fires in-game).
- `e90aa1a2` SIGN block entities — place → OpenSignEditor → UpdateSign → store → BlockEntityData broadcast → viewers + joiners render.
- `df84f65f` functional CHESTS (block-entity #2) — place → OpenScreen → put/take → persist; server-authoritative, dupe/loss-safe (audited hard), CloseContainer.
- `f84e34a2` + `dba5e41b` worldedit-lite `/fill` `/replace` `/undo` via sim region funnel, applied across ticks under a per-tick budget (8192/tick default; bounded pending/undo caps).
- `0f801bb4` presentation builders (titles / subtitle / actionbar / sound / particle) + `/title` `/subtitle` `/actionbar` `/playsound` `/particle`.
- `6bb1eb5f` scoreboard / team / bossbar session builders + `/scoreboard` `/team` `/bossbar`.
- `10f58ff3` `/tp` (coords/player) + `/weather` (GameEvent) + targeted `/gamemode <player>`.
- `ceeeea1b` day-night cycle — deterministic WorldTime clock + Update Time broadcast + `/time set|add|query` (sun/moon animate on clients).
- `52f26043` armor (slots 5-8) + off-hand (45) broadcast via SetEquipment (was mainhand-only) → players visibly armored.

**Plugins:**
- `f14de4f4` plugin events Chat(deny) / Interact(deny) / PlayerMove(throttled) + greeter + chat-filter sample plugin + `WorldView.player_position`; **+ audit fix** (interact-Deny ghost block heal).
- `50f585cb` decision-tally fix — chat/interact plugin decisions now counted in `fold_event_decision` (was block-only) → surface on dashboard + /metrics.

**Observability / dashboard:**
- `a9d65005` rebuilt dashboard — Svelte 5 SPA over axum SSE `/events` + `/api/snapshot` + `ServeDir(dist)` (retired the htmx `pages.rs`).
- `3a9e6dd6` live telemetry wiring — bounded net_telemetry hub + plugin decision tally → live snapshot aggregation (per-player net, packet-trace summaries, plugin_decisions).
- `b8a4df1f` Prometheus `GET /metrics` exporter (zero-dep, reads live snapshot; per-player feed).
- **VERIFIED demo-ready** — all dashboard endpoints proven with live data against a real vanilla 1.21.8 client.

**Tooling / data:**
- `f0147795` 14 CB + 1 SB reserved protocol packets (titles / sounds / particles / scoreboard / team / bossbar / block-entity); ProtoVerify 15/15 ids match. (OpenScreen / CloseContainer / UpdateTime added by consuming lanes; the dead EntitySoundEffect entry was later dropped.)
- `dffa2d11` registry named block/item id constants + drift-guard + de-magic'd block-rules plugin.
- `2107c68a` + `827d033a` real Anvil `.mca` region reader + chunk import + malformed-input tests, **wired into startup** via `[world].anvil_import_dir`.
- `783c3843` published characterization microbenchmarks (`docs/benchmarks/2026-06-29-10f58ff3.{md,json}`) — M5 Pro: chunk-gen ~146µs, chunk-encode ~832ns, placement-mix ~1.11µs, sim-tick@256 ~4.52µs.

### Quality / process (how this batch stayed honest)
- Every lane ran all four gates green in its own worktree before integration, and the full gate was re-run on trunk after each serial merge — no lane moved trunk while another rebase was in flight.
- Hot / risky / protocol-touching lanes got an **independent Codex audit** before merge. That audit loop **caught + fixed 4 real bugs** the first-pass review missed:
  1. player-state teleport-mirror desync (`/gamemode <player>` mirrored to the wrong target),
  2. budget post-join-drain chunk-ticket-leak DoS (flood skipped leave-save + ReleaseChunks → permanent leak),
  3. plugin interact-Deny ghost block (denied interaction left a client-side ghost),
  4. multi-section overlay data-loss (cross-tick edits to different sections of one chunk lost on reload).
- A final cross-cutting integration review (`dae6520e` review-fixes) confirmed the trunk coherent + demo-ready: 1430 tests / 0 fail at review time, no CRITICAL findings, no conflict cruft, SIGTERM durable, exhaustive dispatch. The 1 HIGH (`/gamemode <player>` target-mirror) + 3 MED it found were all fixed.

## Deferred / NOT-yet-built (out of the flat-world / creative-core milestone)
Big buckets, still entirely deferred:
1. **Online-mode** auth + encryption; then **proxy/Velocity** forwarding. (Offline login + per-IP limit + whitelist + ban exist; the encryption/auth handshake does not.)
2. **Real terrain gen** (noise/biomes/caves/structures) + **real light engine** (currently full-bright; flat world only).
3. **Survival** — health/hunger/XP/death/respawn, crafting/smelting, tools/mining-time/drops, container GUIs beyond chests.
4. **Entities/mobs** — generic entity system, AI/pathfinding, item-drop entities, projectiles, combat.
5. **Fluids + block physics + redstone + scheduled/random ticks.**
6. **Scale** — multi-shard runtime + cross-shard transfer + determinism replay harness.
7. **Plugins** — WASM/Component-Model runtime; the C-ABI cdylib loader is a stub (in-process Rust plugins work today).

## Tracked follow-ups (small, non-blocking)
- **Anvil-imported-world edit-revert-on-reload:** the overlay reload path regenerates the FLAT baseline, so an Anvil-imported world reverts imported terrain on edit + reload. Pre-existing, NOT reachable on the flat-world milestone (runtime save is overlay-only). Fix when imported worlds become editable + persistent.
- **Inventory data-components (schema v2)** + **conditional `PlayerAbilities`-on-join.**
- **Per-sign edit-session ownership** (no ownership check on re-opening an edit session today).
- **Budget `burst < 1.0` config validation** + a durable Prometheus kick counter.
- **SIGKILL durability:** SIGTERM/SIGHUP now flush (`622def09`), but SIGKILL is uncatchable — bound the loss window with a periodic flush.
- **Window-click container handling** is minimal (state-id check → resync; full click-modes / sneak-place deferred).
- **JoinGame `dimension=undefined`** seen by the node client (still joins + gets chunks; verify the JoinGame dimension field).
- Presentation/scoreboard hand-encoded wire payloads: confirm via a real client (golden tests guard regression only).

## Public-alpha gate
Mostly green (see `docs/public-alpha.md`). The real-client smoke covers place/break/reconnect/restart-persistence (31/31); characterization benchmarks are published (`docs/benchmarks/`); the dashboard is verified demo-ready against a real client. Open: fresh-clone build verification on a clean checkout.

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
