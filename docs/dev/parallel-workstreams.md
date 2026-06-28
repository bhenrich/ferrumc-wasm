# Parallel Workstreams — Execution Policy

How many workstreams (WS) run at once without stepping on each other on the road to
[Public Alpha](../public-alpha.md). Paths below are relative to the `server/` workspace
root unless noted (e.g. `docs/protocol/...` lives at the repo root, one level above `server/`).

---

## 1. Core policy

- **One git worktree per workstream.** Each WS gets its own checkout so branches never share a working tree.
- **Each worktree sets its own `CARGO_TARGET_DIR`.** No shared `target/`; parallel builds never thrash or corrupt each other's artifacts.
- **Crate-local checks during implementation.** While building, run `cargo check -p <crate>` / `cargo test -p <crate>` only. Fast loop, no workspace-wide cost.
- **Only the integration branch runs the full gate.** The complete gate runs once, at merge, on the integration branch:
  - `cargo fmt --all --check`
  - `cargo clippy --workspace -- -D warnings`
  - `cargo xtask generate --check`
  - `cargo test --workspace`
- **Generated proto files are exclusive.** `ferrumc-proto/src/generated/` is hand-off-limits and owned by exactly one WS at a time (see PROTO lock). Regenerate via `cargo xtask generate`, never hand-edit.
- **Root `Cargo.toml` / `Cargo.lock` edits are serialized.** Adding crates, deps, or bumping the lockfile goes through the ROOT-WORKSPACE lock — one WS at a time, rebase others after.
- **Read any crate, write only owned paths.** A WS may read the whole tree for context, but may only commit changes inside the paths it owns. Touching another WS's owned path = grab the lock or hand it to that WS.

---

## 2. Chokepoint EXCLUSIVE LOCKS

A WS that needs one of these holds it exclusively for the duration; others wait or rebase. These are the few files that two workstreams genuinely cannot edit at the same time.

| Lock | Covers |
|------|--------|
| **APP-CONNECTION** | `app/src/connection*` |
| **PROTO** | `docs/protocol/1_21_8/packets.toml` + `ferrumc-proto/src` (incl. generated) |
| **SIM-MUTATION** | `ferrumc-sim/src` mutation / chunk / entity / plugin paths |
| **SESSION-TRANSLATE** | `ferrumc-session/src/{translate.rs,router.rs}` |
| **ROOT-WORKSPACE** | `Cargo.toml` / `Cargo.lock` |

---

## 3. Workstream table

Merge order is the integration sequence, not a hard schedule — independent rows run in parallel and merge whenever their deps have landed. `(done)` rows are already on the integration branch.

| ID | Workstream | Delivers | Exclusive ownership | Dependencies | Merge order |
|----|-----------|----------|---------------------|--------------|-------------|
| **WS0** | Build lanes / policy | Worktree + per-WS `CARGO_TARGET_DIR` convention, CI integration gate (fmt + clippy + `xtask generate --check` + workspace test), this doc | `.github/workflows`, `.cargo/config.toml`, `xtask/`, this doc | — | 0 |
| **WS1** | Connection split | Break monolithic `connection.rs` into focused modules; stable seam for everything that emits packets | **APP-CONNECTION** | WS0 | 1 |
| **WS2** | Persistence integrity *(done)* | Spawn-chunk JoinKit rebuilt per join (rejoin shows edits); pinned-chunk + journal/restart persistence tests | `ferrumc-storage/src`, `ferrumc-sim/src` chunk-persistence paths (SIM-MUTATION) | WS0 | done |
| **WS3** | Config + plugin Replace | No-config safe startup (no panic on missing config); fix sample-plugin Replace block-state ids so Replace fires | `ferrumc-config/src`, `plugins/` sample (block-rules); coordinates on **APP-CONNECTION** | WS0, WS1 | 2 |
| **WS4** | Observability snapshot | Metrics-snapshot trigger + counters on the dirty-state/journal path; `dump_metrics` reachable on demand | `ferrumc-observability/src` | WS0 | 2 |
| **WS5** | Dashboard | Local dashboard (HTTP/UI) that opens against the observability snapshot | new dashboard crate / `app` module; **ROOT-WORKSPACE** to register crate | WS4 | 4 |
| **WS6** | Block-state catalog | Block-state property model in `ferrumc-world` (axis/half/facing/waterlog) for logs, slabs, stairs, torches, fences | `ferrumc-world/src` block-state module | WS0 | 2 |
| **WS7** | Placement crate | Pure placement logic: derive real block state from UseItemOn cursor + clicked face | new placement crate/module; **ROOT-WORKSPACE** to register crate | WS6 | 3 |
| **WS8** | Placement integration | Wire placement into the shard mutation funnel and the connection place path | **SIM-MUTATION**, **APP-CONNECTION** | WS1, WS7 | 4 |
| **WS9** | Equipment proto | Add SetEquipment to `packets.toml`; regenerate typed packet | **PROTO** | WS0 | 1 |
| **WS10** | Equipment emit | Thread held/main-hand (and rotation/head-yaw) through sim → session → connection; send SetEquipment | **SIM-MUTATION**, **SESSION-TRANSLATE**, **APP-CONNECTION** | WS1, WS9 | 4 |
| **WS11** | Strict trace oracle | Raw-wire capture + golden compare for critical packets; documented real-client smoke checklist | `ferrumc-testkit/src`, `ferrumc-observability` trace; `docs/` | WS9 | 5 |
| **WS12** | Benchmark harness | Startup / join-storm / movement / active-builder / plugin-overhead benches; results template with commit SHA + hardware | `benches/`, `xtask` bench, `docs/experiments/`; **ROOT-WORKSPACE** | WS0 (run after feature WS land) | 6 |
| **WS13** | v1 parity audit | Audit current build vs vanilla 1.21.8; reconcile [FEATURES.md](../FEATURES.md) + [ROADMAP.md](../ROADMAP.md) | `docs/FEATURES.md`, `docs/ROADMAP.md` | WS1–WS11 | 6 |
| **WS14** | Public-alpha docs | README honesty block (flat-only / offline-only / no survival / no parity / roadmap) + tick [public-alpha.md](../public-alpha.md) | `README.md`, `docs/public-alpha.md` | all | 7 |

### Contention notes

- **APP-CONNECTION** is the hottest lock: WS1, WS3, WS8, WS10 all touch it. WS1 lands first to create a stable seam; the rest serialize behind it.
- **SIM-MUTATION** is shared by WS2 (done), WS8, WS10 — serialize on the mutation/entity paths.
- **PROTO** is single-owner: WS9 holds it; WS10 and WS11 consume the regenerated output afterward.
- **ROOT-WORKSPACE** is touched by WS5, WS7, WS9 (lockfile), WS12 — batch crate/dep additions and rebase.
