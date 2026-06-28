//! The self-describing report: run metadata, per-benchmark results, and a
//! Markdown renderer.
//!
//! Every field here is plain serializable data so a run can be emitted as JSON
//! (machine-readable, the canonical artifact) and rendered to Markdown (for a
//! pull request or a glance). The module is pure: it never reads a clock, runs a
//! subprocess, or panics. The runner binary gathers [`RunMetadata`] from the
//! outside world and hands it in.

use serde::{Deserialize, Serialize};

/// The schema version stamped on every emitted [`BenchReport`].
///
/// Bumped when the JSON shape changes so downstream tooling can detect drift.
pub const SCHEMA_VERSION: u32 = 1;

/// Reproducibility metadata describing the environment a run was captured in.
///
/// The library never fills this in itself (that would mean touching a clock or a
/// subprocess); the runner binary gathers it and passes it through.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunMetadata {
    /// Full commit SHA the binary was built from, or `"unknown"`.
    pub commit_sha: String,
    /// Short commit SHA, for display.
    pub commit_short: String,
    /// `rustc --version` output, or `"unknown"`.
    pub rustc_version: String,
    /// Target operating system (`std::env::consts::OS`).
    pub os: String,
    /// Target architecture (`std::env::consts::ARCH`).
    pub arch: String,
    /// Available parallelism (logical CPUs), or `0` if undetectable.
    pub cpu_count: usize,
    /// Host name, or `"unknown"`.
    pub hostname: String,
    /// Build profile: `"release"` or `"debug"`.
    pub profile: String,
    /// Wall-clock capture time as a Unix timestamp (seconds), if available.
    pub timestamp_unix: Option<u64>,
    /// A caller-supplied timestamp label (for example an ISO-8601 string), if any.
    pub timestamp_label: Option<String>,
}

/// A throughput figure: a rate of work units per second.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Throughput {
    /// The work unit (for example `"bytes"`).
    pub unit: String,
    /// The rate, in units per second.
    pub value: f64,
}

/// A named auxiliary metric attached to a [`BenchResult`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Metric {
    /// Metric name (for example `"edits_per_tick"`).
    pub name: String,
    /// Metric unit (for example `"edits"` or `"ms"`).
    pub unit: String,
    /// Metric value.
    pub value: f64,
}

/// The reduced statistics for one benchmark.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchResult {
    /// Benchmark name.
    pub name: String,
    /// Benchmark group.
    pub group: String,
    /// Number of timed iterations recorded.
    pub iterations: u64,
    /// Number of untimed warmup iterations run first.
    pub warmup_iterations: u64,
    /// Sum of all iteration durations, in nanoseconds.
    pub total_ns: u64,
    /// Mean iteration duration, in nanoseconds.
    pub mean_ns: f64,
    /// Fastest iteration, in nanoseconds.
    pub min_ns: u64,
    /// Slowest iteration, in nanoseconds.
    pub max_ns: u64,
    /// Median (`p50`) iteration duration, in nanoseconds.
    pub p50_ns: u64,
    /// 95th-percentile iteration duration, in nanoseconds.
    pub p95_ns: u64,
    /// 99th-percentile iteration duration, in nanoseconds.
    pub p99_ns: u64,
    /// Operations (iterations) per second.
    pub ops_per_sec: f64,
    /// Work-unit throughput, if the benchmark declared a unit.
    pub throughput: Option<Throughput>,
    /// Any extra metrics the benchmark builder attached.
    pub extra_metrics: Vec<Metric>,
}

impl BenchResult {
    /// Attaches an auxiliary [`Metric`] to this result.
    pub fn add_metric(&mut self, name: &str, unit: &str, value: f64) {
        self.extra_metrics.push(Metric {
            name: name.to_owned(),
            unit: unit.to_owned(),
            value,
        });
    }
}

/// A complete benchmark run: schema version, environment metadata, and results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchReport {
    /// The JSON schema version (see [`SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Environment/reproducibility metadata.
    pub metadata: RunMetadata,
    /// One entry per benchmark, in run order.
    pub benchmarks: Vec<BenchResult>,
}

/// Renders a report as Markdown: a metadata block followed by one table per
/// group.
#[must_use]
pub fn to_markdown(report: &BenchReport) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    // Each write targets an in-memory String, so the formatting cannot fail;
    // the Result is intentionally ignored.
    let _ = writeln!(out, "# `FerrumC` bench results\n");
    let _ = writeln!(
        out,
        "> Server-internal microbenchmarks (encode / placement / codec / tick \
         throughput on synthetic data). NOT capacity or player-count claims.\n"
    );

    let meta = &report.metadata;
    let _ = writeln!(out, "- schema version: {}", report.schema_version);
    let _ = writeln!(
        out,
        "- commit: `{}` (`{}`)",
        meta.commit_short, meta.commit_sha
    );
    let _ = writeln!(out, "- rustc: `{}`", meta.rustc_version);
    let _ = writeln!(out, "- os / arch: {} / {}", meta.os, meta.arch);
    let _ = writeln!(out, "- logical CPUs: {}", meta.cpu_count);
    let _ = writeln!(out, "- hostname: `{}`", meta.hostname);
    let _ = writeln!(out, "- profile: {}", meta.profile);
    if let Some(unix) = meta.timestamp_unix {
        let _ = writeln!(out, "- timestamp (unix): {unix}");
    }
    if let Some(label) = &meta.timestamp_label {
        let _ = writeln!(out, "- timestamp (label): {label}");
    }
    out.push('\n');

    for group in groups_in_order(report) {
        let _ = writeln!(out, "## {group}\n");
        let _ = writeln!(
            out,
            "| Name | Iters | Mean | p50 | p95 | p99 | Ops/s | Throughput | Extra |"
        );
        let _ = writeln!(
            out,
            "|------|-------|------|-----|-----|-----|-------|------------|-------|"
        );
        for result in report.benchmarks.iter().filter(|r| r.group == group) {
            let throughput = result
                .throughput
                .as_ref()
                .map_or_else(|| "-".to_owned(), |t| fmt_rate(t.value, &t.unit));
            let extra = if result.extra_metrics.is_empty() {
                "-".to_owned()
            } else {
                result
                    .extra_metrics
                    .iter()
                    .map(|m| format!("{}={:.3}{}", m.name, m.value, m.unit))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                result.name,
                result.iterations,
                fmt_ns(result.mean_ns),
                fmt_ns(result.p50_ns as f64),
                fmt_ns(result.p95_ns as f64),
                fmt_ns(result.p99_ns as f64),
                fmt_rate(result.ops_per_sec, "ops"),
                throughput,
                extra,
            );
        }
        out.push('\n');
    }

    out
}

/// Collects the distinct group names in first-seen order.
fn groups_in_order(report: &BenchReport) -> Vec<String> {
    let mut groups: Vec<String> = Vec::new();
    for result in &report.benchmarks {
        if !groups.iter().any(|g| g == &result.group) {
            groups.push(result.group.clone());
        }
    }
    groups
}

/// Formats a nanosecond duration with an adaptive unit.
fn fmt_ns(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{ns:.1} ns")
    } else if ns < 1_000_000.0 {
        format!("{:.3} us", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.3} ms", ns / 1_000_000.0)
    } else {
        format!("{:.3} s", ns / 1_000_000_000.0)
    }
}

/// Formats a per-second rate with an adaptive SI-ish prefix.
fn fmt_rate(value: f64, unit: &str) -> String {
    if value >= 1.0e9 {
        format!("{:.2} G{unit}/s", value / 1.0e9)
    } else if value >= 1.0e6 {
        format!("{:.2} M{unit}/s", value / 1.0e6)
    } else if value >= 1.0e3 {
        format!("{:.2} k{unit}/s", value / 1.0e3)
    } else {
        format!("{value:.2} {unit}/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> BenchReport {
        BenchReport {
            schema_version: SCHEMA_VERSION,
            metadata: RunMetadata {
                commit_sha: "abc123".to_owned(),
                commit_short: "abc123".to_owned(),
                rustc_version: "rustc 1.80.0".to_owned(),
                os: "macos".to_owned(),
                arch: "aarch64".to_owned(),
                cpu_count: 8,
                hostname: "test-host".to_owned(),
                profile: "release".to_owned(),
                timestamp_unix: Some(1_700_000_000),
                timestamp_label: Some("2026-06-28T00:00:00Z".to_owned()),
            },
            benchmarks: vec![BenchResult {
                name: "demo".to_owned(),
                group: "world".to_owned(),
                iterations: 10,
                warmup_iterations: 2,
                total_ns: 10_000,
                mean_ns: 1_000.0,
                min_ns: 900,
                max_ns: 1_200,
                p50_ns: 1_000,
                p95_ns: 1_150,
                p99_ns: 1_200,
                ops_per_sec: 1_000_000.0,
                throughput: Some(Throughput {
                    unit: "bytes".to_owned(),
                    value: 16_000_000.0,
                }),
                extra_metrics: vec![Metric {
                    name: "encoded_bytes".to_owned(),
                    unit: "bytes".to_owned(),
                    value: 16.0,
                }],
            }],
        }
    }

    #[test]
    fn markdown_contains_metadata_and_table() {
        let md = to_markdown(&sample_report());
        assert!(md.contains("## world"));
        assert!(md.contains("demo"));
        assert!(md.contains("abc123"));
        assert!(md.contains("Throughput"));
    }

    #[test]
    fn report_round_trips_through_json() {
        let report = sample_report();
        let json = serde_json::to_string(&report).expect("serialize");
        let back: BenchReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(report, back);
    }

    #[test]
    fn rate_formatting_scales() {
        assert_eq!(fmt_rate(500.0, "ops"), "500.00 ops/s");
        assert_eq!(fmt_rate(2_000.0, "ops"), "2.00 kops/s");
        assert_eq!(fmt_rate(3_000_000.0, "bytes"), "3.00 Mbytes/s");
    }
}
