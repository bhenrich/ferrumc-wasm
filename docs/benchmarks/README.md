# FerrumC benchmarks

This directory documents how FerrumC's benchmarks are run and what they do — and,
just as importantly, what they do **not** claim.

> **Honesty note.** Everything here today is a **server-internal
> microbenchmark**: it measures the throughput of individual server subsystems
> (chunk encode, placement resolution, item codec, simulation tick) on synthetic
> data. These are **not** capacity numbers and **not** "X concurrent players"
> claims. A real-client join-storm harness — the thing that would justify a
> player-count claim — comes later. Do not quote these numbers as player counts.

## v0: `ferrumc-bench`

The `ferrumc-bench` crate (`server/crates/ferrumc-bench`) is a deterministic,
`std::time::Instant`-based runner. It needs no real client and no live socket.

### Running

From `server/`:

```bash
cargo run -p ferrumc-bench --release -- \
    --out target/bench-results.json \
    --timestamp "$(date -u +%FT%TZ)"
```

Build with `--release`. A debug build prints a warning and produces meaningless
numbers (the runner records the profile in the output either way).

Flags:

| Flag | Meaning |
|------|---------|
| `--out <path>` | JSON output path (default `target/bench-results.json`). |
| `--timestamp <label>` | Free-form run label recorded in the metadata. |
| `--quick` | CI-fast iteration counts (1 warmup, 3 iterations, tiny edit sweep). |
| `--filter <substr>` | Run only benchmarks whose group or name contains `<substr>`. |

The commit can be pinned explicitly via the `FERRUMC_COMMIT` environment variable;
otherwise the runner reads it from `git rev-parse`.

### What it measures

| Group | Benchmarks |
|-------|-----------|
| `world` | flat chunk generation; whole-chunk network encode (bytes/sec); paletted-section encode, single-valued (all-air) and indirect (flat-surface mix). |
| `placement` | `compute_placement` per family — simple cube, axis log, slab, stairs, floor torch, wall torch, fence, horizontal facing — plus an all-families mix. |
| `items` | trusted slot encode (empty, present, present-with-component); container-content payload; untrusted slot decode + validation (empty, present, with component). |
| `sim` | `SimShard::run_tick` cost as the per-tick block-edit count `K` sweeps `{0, 1, 16, 64, 256}`, reported as mean mspt and edits/sec. |

### Output and reproducibility fields

Each run emits **both** a JSON artifact (the canonical, machine-readable result)
and a Markdown summary on stdout. Both share one metadata block so any result is
interpretable months later:

- `commit_sha` / `commit_short` — the exact build, from `FERRUMC_COMMIT` or
  `git rev-parse`.
- `rustc_version` — from `rustc --version`.
- `os`, `arch`, `cpu_count` — target OS/architecture and logical CPU count.
- `hostname`, `profile` — the machine and whether it was a release build.
- `timestamp_unix`, `timestamp_label` — when the run happened (the runner reads
  the wall clock only in the binary; the library never does).

Per benchmark the report carries the iteration count, total/mean/min/max
nanoseconds, the `p50`/`p95`/`p99` percentiles (nearest-rank), the operation rate
(ops/sec), an optional work-unit throughput (e.g. bytes/sec, edits/sec), and any
extra metrics (e.g. encoded byte size, mean mspt).

The JSON shape is versioned by a top-level `schema_version` so downstream tooling
can detect drift.

### Smoke test

`server/crates/ferrumc-bench/tests/smoke.rs` runs the whole harness with
`BenchConfig::quick()` and asserts every group produced finite, ordered stats.
That keeps the harness exercised in CI cheaply without making any performance
assertion.

## Later (not done yet)

- Real-client join-storm and steady-state capacity benchmarks (these would be the
  only basis for any player-count claim).
- Storage backend comparison (see `docs/experiments/`).
- Compression throughput comparison (see `docs/experiments/`).
