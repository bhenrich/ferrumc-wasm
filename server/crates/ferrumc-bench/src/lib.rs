#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

//! ## Module layout
//!
//! - [`harness`] — the warmup/timed-iteration runner and percentile statistics.
//! - [`report`] — the serializable run report and its Markdown renderer.
//! - [`benches`] — one builder per group (`world`, `placement`, `items`, `sim`).
//!
//! The library is pure: it never reads a clock ([`std::time::SystemTime`]), shells
//! out, or panics on a measured path. The runner binary (`src/main.rs`) is the
//! only place that touches the outside world to gather [`report::RunMetadata`].

pub mod benches;
mod block_on;
pub mod harness;
pub mod report;

use harness::BenchSpec;
use report::BenchResult;

pub use report::{BenchReport, Metric, RunMetadata, Throughput};

/// Configuration for a benchmark run: how many iterations each benchmark runs,
/// which synthetic-tick edit counts to sweep, and an optional name/group filter.
#[derive(Debug, Clone)]
pub struct BenchConfig {
    /// Untimed warmup iterations per benchmark.
    pub warmup: u32,
    /// Timed iterations per benchmark.
    pub iterations: u32,
    /// The per-tick edit counts (`K`) the `sim` group sweeps.
    pub sim_edit_counts: Vec<usize>,
    /// If set, only benchmarks whose group or name contains this substring run.
    pub filter: Option<String>,
}

impl Default for BenchConfig {
    /// A full run: enough iterations for stable percentiles, sweeping the
    /// standard edit counts.
    fn default() -> Self {
        Self {
            warmup: 50,
            iterations: 500,
            sim_edit_counts: vec![0, 1, 16, 64, 256],
            filter: None,
        }
    }
}

impl BenchConfig {
    /// A fast, cheap configuration for smoke tests and continuous integration:
    /// one warmup, three timed iterations, and a tiny edit sweep.
    #[must_use]
    pub fn quick() -> Self {
        Self {
            warmup: 1,
            iterations: 3,
            sim_edit_counts: vec![0, 4],
            filter: None,
        }
    }

    /// Builds a [`BenchSpec`] for `group`/`name`, or `None` if the configured
    /// filter excludes it (so the caller skips running it entirely).
    #[must_use]
    pub fn spec_if_included(
        &self,
        group: &str,
        name: &str,
        throughput_unit: Option<&str>,
    ) -> Option<BenchSpec> {
        if let Some(filter) = &self.filter {
            if !group.contains(filter.as_str()) && !name.contains(filter.as_str()) {
                return None;
            }
        }
        Some(BenchSpec {
            name: name.to_owned(),
            group: group.to_owned(),
            warmup: self.warmup,
            iterations: self.iterations,
            throughput_unit: throughput_unit.map(str::to_owned),
        })
    }
}

/// Runs every benchmark group with `config` and returns the combined results.
///
/// Groups run in a fixed order (`world`, `placement`, `items`, `sim`) so output
/// is stable across runs.
#[must_use]
pub fn run_all(config: &BenchConfig) -> Vec<BenchResult> {
    let mut results = Vec::new();
    results.extend(benches::world::benchmarks(config));
    results.extend(benches::placement::benchmarks(config));
    results.extend(benches::items::benchmarks(config));
    results.extend(benches::sim::benchmarks(config));
    results
}
