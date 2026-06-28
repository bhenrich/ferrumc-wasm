# `ferrumc-bench`

Reproducible, self-describing **server-internal microbenchmarks** for `FerrumC`
v2.

These measure the throughput of individual server subsystems on synthetic data:
chunk generation and network encoding, paletted-section encoding, block-placement
resolution, item-slot encode/decode, and synthetic simulation ticks. They do
**not** require a real client or a live socket, and they are **not** capacity or
player-count claims. Real-client join-storm benchmarks come later.

## Running

```text
cargo run -p ferrumc-bench --release -- \
    --out target/bench-results.json \
    --timestamp "$(date -u +%FT%TZ)"
```

Always build with `--release`; a debug build prints a warning and produces
meaningless numbers. Flags:

- `--out <path>` — JSON output path (default `target/bench-results.json`).
- `--timestamp <label>` — a free-form run label recorded in the metadata.
- `--quick` — continuous-integration-fast iteration counts.
- `--filter <substr>` — run only benchmarks whose group or name contains
  `<substr>`.

## What it measures

- `world` — flat chunk generation, whole-chunk network encode (bytes/sec), and
  paletted-section encode for the single-valued and indirect representations.
- `placement` — `compute_placement` throughput for each block family
  (simple cube, axis log, slab, stairs, floor/wall torch, fence, horizontal
  facing) plus an all-families mix.
- `items` — trusted slot encode (empty, present, present-with-component), the
  container-content payload, and untrusted slot decode plus validation.
- `sim` — `SimShard::run_tick` cost as the per-tick block-edit count grows,
  reported as nanoseconds per tick (mean mspt) and edits/sec.

## Output

Each run emits both a JSON artifact and a Markdown summary on stdout. Both carry
the same reproducibility metadata so a result is interpretable later:

- the git commit (full and short SHA, read via `FERRUMC_COMMIT` or
  `git rev-parse`),
- the `rustc` version,
- operating system, architecture, and logical CPU count,
- the host name and build profile,
- a Unix timestamp plus an optional caller-supplied label.

Per benchmark it reports the iteration count, total/mean/min/max nanoseconds,
the `p50`/`p95`/`p99` percentiles, the operation rate, and (where meaningful) a
work-unit throughput such as bytes/sec or edits/sec.

## Design

The library half (`harness`, `report`, `benches`) is pure: it never reads a
clock, shells out, or panics on a measured path. The runner binary is the only
place that gathers environment metadata. A small `std`-only `block_on` drives the
single asynchronous call the synthetic tick setup needs, so the crate adds no
async-runtime dependency and keeps `#![forbid(unsafe_code)]`.
