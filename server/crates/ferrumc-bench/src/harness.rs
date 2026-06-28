//! The measurement harness: warmup, timed iterations, and percentile stats.
//!
//! A benchmark is expressed as a single closure that, once per iteration, does
//! its own untimed preparation and then times only the region under test with
//! [`timed`], returning a [`Sample`] (the measured nanoseconds plus the work
//! units that region produced, for example encoded bytes). [`run_benchmark`]
//! runs the warmup, collects the per-iteration samples, and reduces them to a
//! [`BenchResult`] (mean/min/max, `p50`/`p95`/`p99` by nearest-rank, ops/sec,
//! and a work-unit throughput).
//!
//! The harness uses [`std::time::Instant`] and [`std::hint::black_box`] only; it
//! is deterministic, allocation-light in the timed region, and never panics.

use std::hint::black_box;
use std::time::Instant;

use crate::report::{BenchResult, Throughput};

/// The static description of one benchmark: where it lives, how hard to run it,
/// and what unit (if any) its throughput is measured in.
#[derive(Debug, Clone)]
pub struct BenchSpec {
    /// Human-readable benchmark name, unique within its group.
    pub name: String,
    /// The group this benchmark belongs to (`world`, `placement`, ...).
    pub group: String,
    /// Untimed warmup iterations run before measurement begins.
    pub warmup: u32,
    /// Timed iterations whose durations are recorded.
    pub iterations: u32,
    /// The unit the per-iteration work count is expressed in (for example
    /// `"bytes"` or `"edits"`), or `None` when only the operation rate matters.
    pub throughput_unit: Option<String>,
}

/// One measured iteration: how long the timed region took and how much work it
/// did.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Wall-clock nanoseconds spent in the timed region.
    pub nanos: u64,
    /// Work units produced (for example encoded byte count, or block edits).
    pub units: u64,
}

/// Times `f`, returning the elapsed nanoseconds and `f`'s output.
///
/// The output is passed through [`black_box`] so the optimizer cannot elide the
/// work under measurement. Callers typically derive the iteration's work units
/// from the returned value (for example the length of an encoded buffer).
#[must_use]
pub fn timed<R>(f: impl FnOnce() -> R) -> (u64, R) {
    let start = Instant::now();
    let out = black_box(f());
    let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    (nanos, out)
}

/// Runs `bench` for `spec.warmup` untimed then `spec.iterations` timed
/// iterations and reduces the samples into a [`BenchResult`].
///
/// Percentiles are nearest-rank over the sorted per-iteration nanoseconds.
/// Throughput (`spec.throughput_unit` per second) is the total work units across
/// all iterations divided by the total measured time; the operation rate
/// (iterations per second) is reported separately. All divisions guard against a
/// zero denominator, so a degenerate run yields zeroes rather than panicking.
#[must_use]
pub fn run_benchmark(spec: &BenchSpec, mut bench: impl FnMut() -> Sample) -> BenchResult {
    for _ in 0..spec.warmup {
        let _ = black_box(bench());
    }

    let iterations = spec.iterations.max(1) as usize;
    let mut samples: Vec<u64> = Vec::with_capacity(iterations);
    let mut total_units: u64 = 0;
    for _ in 0..iterations {
        let sample = bench();
        total_units = total_units.saturating_add(sample.units);
        samples.push(sample.nanos);
    }
    samples.sort_unstable();

    let count = samples.len() as u64;
    let total_ns: u128 = samples.iter().map(|&n| u128::from(n)).sum();
    let mean_ns = if count == 0 {
        0.0
    } else {
        total_ns as f64 / count as f64
    };
    let secs = total_ns as f64 / 1.0e9;
    let ops_per_sec = if secs > 0.0 { count as f64 / secs } else { 0.0 };
    let throughput = spec.throughput_unit.as_ref().map(|unit| Throughput {
        unit: unit.clone(),
        value: if secs > 0.0 {
            total_units as f64 / secs
        } else {
            0.0
        },
    });

    BenchResult {
        name: spec.name.clone(),
        group: spec.group.clone(),
        iterations: count,
        warmup_iterations: u64::from(spec.warmup),
        total_ns: u64::try_from(total_ns).unwrap_or(u64::MAX),
        mean_ns,
        min_ns: samples.first().copied().unwrap_or(0),
        max_ns: samples.last().copied().unwrap_or(0),
        p50_ns: percentile(&samples, 50),
        p95_ns: percentile(&samples, 95),
        p99_ns: percentile(&samples, 99),
        ops_per_sec,
        throughput,
        extra_metrics: Vec::new(),
    }
}

/// Returns the nearest-rank percentile `q` (in `0..=100`) of a sorted slice.
///
/// Uses rank `ceil(q * n / 100)`, clamped into bounds; an empty slice yields `0`.
fn percentile(sorted: &[u64], q: u32) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    // ceil(q * n / 100) without floating point.
    let rank = (q as usize * n).div_ceil(100);
    let index = rank.saturating_sub(1).min(n - 1);
    sorted[index]
}
