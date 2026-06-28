//! Cheap smoke test: every benchmark group runs a few iterations without
//! panicking and produces finite, self-consistent statistics. This exercises
//! every measured path in continuous integration without the cost of a full run.

use ferrumc_bench::{run_all, BenchConfig};

#[test]
fn quick_run_produces_consistent_results_for_every_group() {
    let results = run_all(&BenchConfig::quick());
    assert!(
        !results.is_empty(),
        "expected at least one benchmark result"
    );

    // Every group must be represented.
    for group in ["world", "placement", "items", "sim"] {
        assert!(
            results.iter().any(|r| r.group == group),
            "group {group} produced no results"
        );
    }

    for result in &results {
        assert!(
            result.iterations >= 1,
            "{} ran zero iterations",
            result.name
        );
        assert!(
            result.mean_ns.is_finite() && result.mean_ns >= 0.0,
            "{} has a non-finite mean",
            result.name
        );
        assert!(
            result.ops_per_sec.is_finite() && result.ops_per_sec >= 0.0,
            "{} has a non-finite ops/sec",
            result.name
        );
        // Percentiles are monotonic by construction.
        assert!(
            result.p50_ns <= result.p95_ns && result.p95_ns <= result.p99_ns,
            "{} percentiles are not ordered (p50={}, p95={}, p99={})",
            result.name,
            result.p50_ns,
            result.p95_ns,
            result.p99_ns,
        );
        assert!(
            result.min_ns <= result.max_ns,
            "{} min exceeds max",
            result.name
        );
        if let Some(throughput) = &result.throughput {
            assert!(
                throughput.value.is_finite() && throughput.value >= 0.0,
                "{} has a non-finite throughput",
                result.name
            );
        }
    }
}

#[test]
fn filter_restricts_to_matching_group() {
    let mut config = BenchConfig::quick();
    config.filter = Some("placement".to_owned());
    let results = run_all(&config);
    assert!(!results.is_empty(), "filter excluded everything");
    assert!(
        results.iter().all(|r| r.group == "placement"),
        "filter leaked non-placement benchmarks"
    );
}
