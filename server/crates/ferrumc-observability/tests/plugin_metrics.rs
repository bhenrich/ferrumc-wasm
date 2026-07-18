use std::time::Duration;

use ferrumc_core::PluginId;
use ferrumc_observability::{
    CounterRegistry, PluginInvocationObservation, PluginMetricEntry, PluginMetricRecordOutcome,
    ServerSnapshot, ServerSnapshotParts,
};

fn completed(plugin_id: &str, elapsed_us: u64) -> PluginInvocationObservation {
    PluginInvocationObservation::new(PluginId::new(plugin_id), Duration::from_micros(elapsed_us))
        .expect("fixture metric identifier is bounded")
}

fn plugin<'a>(entries: &'a [PluginMetricEntry], plugin_id: &str) -> &'a PluginMetricEntry {
    entries
        .iter()
        .find(|entry| entry.plugin_id() == &PluginId::new(plugin_id))
        .expect("plugin metric row")
}

#[test]
fn fixture_invocations_are_monotonic_and_do_not_bleed_across_plugins() {
    let registry = CounterRegistry::new();

    assert_eq!(
        registry.record_plugin_invocation(completed("fixture-dynamic", 10)),
        PluginMetricRecordOutcome::Recorded
    );
    assert_eq!(
        registry.record_plugin_invocation(completed("unrelated", 7)),
        PluginMetricRecordOutcome::Recorded
    );
    assert_eq!(
        registry.record_plugin_invocation(completed("fixture-dynamic", 20).with_over_budget()),
        PluginMetricRecordOutcome::Recorded
    );
    assert_eq!(
        registry.record_plugin_invocation(completed("fixture-dynamic", 30).with_panic_status()),
        PluginMetricRecordOutcome::Recorded
    );
    assert_eq!(
        registry
            .record_plugin_invocation(completed("fixture-dynamic", 40).with_host_call_errors(1)),
        PluginMetricRecordOutcome::Recorded
    );
    let before = registry.snapshot();
    let fixture = plugin(before.plugin_metrics.entries(), "fixture-dynamic");
    assert_eq!(fixture.invocation_count(), 4);
    assert_eq!(fixture.invocation_time_us_total(), 100);
    assert_eq!(fixture.invocation_time_us_last(), 40);
    assert_eq!(fixture.invocation_time_us_max(), 40);
    assert!((fixture.invocation_time_us_avg() - 25.0).abs() < f64::EPSILON);
    assert_eq!(fixture.over_budget_count(), 1);
    assert_eq!(fixture.panic_count(), 1);
    assert_eq!(fixture.host_call_error_count(), 1);
    assert!(!fixture.is_hung());

    let unrelated = plugin(before.plugin_metrics.entries(), "unrelated");
    assert_eq!(unrelated.invocation_count(), 1);
    assert_eq!(unrelated.invocation_time_us_total(), 7);
    assert_eq!(unrelated.over_budget_count(), 0);
    assert_eq!(unrelated.panic_count(), 0);
    assert_eq!(unrelated.host_call_error_count(), 0);

    assert_eq!(
        registry.record_plugin_invocation(
            completed("fixture-dynamic", 5)
                .with_over_budget()
                .with_panic_status()
                .with_host_call_errors(2)
        ),
        PluginMetricRecordOutcome::Recorded
    );
    let after = registry.snapshot();
    let fixture_after = plugin(after.plugin_metrics.entries(), "fixture-dynamic");
    assert!(fixture_after.invocation_count() >= fixture.invocation_count());
    assert!(fixture_after.invocation_time_us_total() >= fixture.invocation_time_us_total());
    assert!(fixture_after.over_budget_count() >= fixture.over_budget_count());
    assert!(fixture_after.panic_count() >= fixture.panic_count());
    assert!(fixture_after.host_call_error_count() >= fixture.host_call_error_count());
    assert_eq!(fixture_after.invocation_count(), 5);
    assert_eq!(fixture_after.invocation_time_us_total(), 105);
    assert_eq!(fixture_after.over_budget_count(), 2);
    assert_eq!(fixture_after.panic_count(), 2);
    assert_eq!(fixture_after.host_call_error_count(), 3);

    let unrelated_after = plugin(after.plugin_metrics.entries(), "unrelated");
    assert_eq!(
        unrelated_after.invocation_count(),
        unrelated.invocation_count()
    );
    assert_eq!(
        unrelated_after.invocation_time_us_total(),
        unrelated.invocation_time_us_total()
    );
    assert_eq!(
        unrelated_after.host_call_error_count(),
        unrelated.host_call_error_count()
    );
    assert_eq!(
        unrelated_after.over_budget_count(),
        unrelated.over_budget_count()
    );
    assert_eq!(unrelated_after.panic_count(), unrelated.panic_count());
    assert_eq!(unrelated_after.is_hung(), unrelated.is_hung());
}

#[test]
fn plugin_metrics_are_exposed_by_metric_dump_and_server_snapshot_surfaces() {
    let registry = CounterRegistry::new();
    assert_eq!(
        registry.record_plugin_invocation(completed("fixture-dynamic", 12)),
        PluginMetricRecordOutcome::Recorded
    );

    let metric_json = serde_json::to_value(registry.snapshot()).expect("metric JSON");
    let metric_rows = metric_json["ferrumc_plugin_metrics"]["entries"]
        .as_array()
        .expect("plugin rows");
    assert_eq!(metric_rows.len(), 1);
    assert_eq!(metric_rows[0]["plugin_id"], "fixture-dynamic");
    assert_eq!(metric_rows[0]["invocation_count"], 1);
    assert_eq!(metric_rows[0]["invocation_time_us_total"], 12);
    assert_eq!(metric_rows[0]["over_budget_count"], 0);
    assert_eq!(metric_rows[0]["panic_count"], 0);
    assert_eq!(metric_rows[0]["hung"], false);
    assert_eq!(metric_rows[0]["host_call_error_count"], 0);
    assert_eq!(
        metric_json["ferrumc_plugin_metrics"]["untracked_hung_plugins"],
        0
    );

    let server_snapshot = registry.server_snapshot(ServerSnapshotParts::default());
    assert_eq!(server_snapshot.plugin_metrics.entries().len(), 1);
    let server_json = serde_json::to_value(server_snapshot).expect("server JSON");
    assert_eq!(
        server_json["plugin_metrics"]["entries"][0]["plugin_id"],
        "fixture-dynamic"
    );
    let mut legacy_json = server_json.clone();
    legacy_json
        .as_object_mut()
        .expect("server snapshot object")
        .remove("plugin_metrics");
    let legacy: ServerSnapshot =
        serde_json::from_value(legacy_json).expect("legacy server snapshot");
    assert!(legacy.plugin_metrics.entries().is_empty());

    let round_trip: ServerSnapshot =
        serde_json::from_value(server_json).expect("server snapshot round trip");
    assert_eq!(
        round_trip.plugin_metrics.entries()[0].plugin_id(),
        &PluginId::new("fixture-dynamic")
    );
}
