//! The `GET /metrics` Prometheus endpoint.
//!
//! Renders the latest [`ServerSnapshot`] as Prometheus text exposition format
//! (version 0.0.4) so the live server telemetry is scrapeable by a Prometheus
//! server, Grafana Agent, or `curl`. Like the rest of the dashboard this is a
//! pure read: it clones the current snapshot out of the [`SnapshotPublisher`] and
//! never mutates anything.
//!
//! ## Reachability
//!
//! `/metrics` rides the same router as the SPA, so it inherits both guards: the
//! `GET`/`HEAD`-only method layer and the bind-time loopback enforcement in
//! [`crate::run`]. That means the endpoint is **loopback-only** today — fine for a
//! local alpha where you scrape `127.0.0.1`. A real off-host Prometheus would need
//! a non-loopback bind (and, with the player names/positions this exposes, its own
//! authn/TLS in front); that is deliberately deferred future work and must not be
//! bolted on by relaxing the loopback guard.
//!
//! ## Cardinality
//!
//! Every series here is bounded by something small and operator-controlled: the
//! per-player families by the online player count, the plugin family by the number
//! of loaded plugins, the packet-trace family by the snapshot's top-N trace
//! window, and the decode-error family by the fixed-size decode-error table. There
//! are no free-form/unbounded label values, so a scrape can never blow up the
//! time-series database.
//!
//! [`ServerSnapshot`]: ferrumc_observability::ServerSnapshot
//! [`SnapshotPublisher`]: ferrumc_observability::SnapshotPublisher

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use ferrumc_observability::{ServerSnapshot, SnapshotPublisher};

/// The Prometheus text exposition format content type (format version 0.0.4).
///
/// Prometheus uses this `Content-Type` to pick the legacy text parser; emitting it
/// verbatim is what makes the body a recognised scrape target rather than opaque
/// `text/plain`.
const EXPOSITION_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// `GET /metrics` — the latest [`ServerSnapshot`] in Prometheus text exposition
/// format.
///
/// Clones the current `Arc` out of the publisher (cheap, not the struct), renders
/// it, and tags the body with the exposition [`Content-Type`](EXPOSITION_CONTENT_TYPE)
/// so Prometheus parses it. Read-only: it never writes through the handle.
pub async fn metrics(State(publisher): State<SnapshotPublisher>) -> impl IntoResponse {
    let snapshot = publisher.latest();
    let body = render(&snapshot);
    ([(header::CONTENT_TYPE, EXPOSITION_CONTENT_TYPE)], body)
}

/// The two Prometheus metric kinds this exporter emits.
///
/// We only need counters (monotonic `_total` series) and gauges (point-in-time
/// values); histograms/summaries are intentionally out of scope for the alpha.
#[derive(Clone, Copy)]
enum MetricKind {
    /// A monotonically increasing total; named with a `_total` suffix by
    /// convention.
    Counter,
    /// A point-in-time value that can go up or down.
    Gauge,
}

impl MetricKind {
    /// The `# TYPE` token for this kind.
    fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
        }
    }
}

/// A tiny, dependency-free writer for the Prometheus text exposition format.
///
/// It enforces the one structural rule scrapers care about: each metric family is
/// introduced by exactly one `# HELP`/`# TYPE` pair, followed by all of its
/// samples contiguously. Callers get that for free by calling [`Self::family`]
/// once and then [`Self::sample`] for each series in the family.
struct Exposition {
    out: String,
}

impl Exposition {
    /// Creates an empty exposition buffer.
    fn new() -> Self {
        Self { out: String::new() }
    }

    /// Writes the `# HELP`/`# TYPE` header for a metric family.
    ///
    /// Call this exactly once per metric name, before any of its samples.
    fn family(&mut self, name: &str, help: &str, kind: MetricKind) {
        self.out.push_str("# HELP ");
        self.out.push_str(name);
        self.out.push(' ');
        push_escaped_help(&mut self.out, help);
        self.out.push('\n');
        self.out.push_str("# TYPE ");
        self.out.push_str(name);
        self.out.push(' ');
        self.out.push_str(kind.as_str());
        self.out.push('\n');
    }

    /// Writes one sample line: `name{label="value",...} value`.
    ///
    /// Label values are escaped per the exposition spec; an empty `labels` slice
    /// omits the brace group entirely.
    fn sample(&mut self, name: &str, labels: &[(&str, &str)], value: &str) {
        self.out.push_str(name);
        if let Some(((first_key, first_val), rest)) = labels.split_first() {
            self.out.push('{');
            self.push_label(first_key, first_val);
            for (key, val) in rest {
                self.out.push(',');
                self.push_label(key, val);
            }
            self.out.push('}');
        }
        self.out.push(' ');
        self.out.push_str(value);
        self.out.push('\n');
    }

    /// Writes a single `key="escaped value"` label pair.
    fn push_label(&mut self, key: &str, value: &str) {
        self.out.push_str(key);
        self.out.push_str("=\"");
        push_escaped_label_value(&mut self.out, value);
        self.out.push('"');
    }

    /// Consumes the writer, returning the rendered body.
    fn into_string(self) -> String {
        self.out
    }
}

/// Appends `value` to `out`, escaping it for use inside a Prometheus label value.
///
/// The exposition format requires `\` → `\\`, `"` → `\"`, and a literal newline →
/// `\n`; everything else is passed through. This keeps hostile or whitespace-laden
/// player names from breaking the line grammar.
fn push_escaped_label_value(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
}

/// Appends `help` to `out`, escaping it for use in a `# HELP` line.
///
/// Only `\` → `\\` and newline → `\n` are escaped (quotes are literal in HELP
/// text). Our help strings are static and clean, but escaping keeps the writer
/// correct by construction.
fn push_escaped_help(out: &mut String, help: &str) {
    for ch in help.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
}

/// Formats a `u64` metric value.
fn fmt_u64(value: u64) -> String {
    value.to_string()
}

/// Formats an `f64` metric value, mapping the non-finite cases onto the tokens
/// Prometheus expects (`NaN`, `+Inf`, `-Inf`) rather than Rust's `inf`/`NaN`
/// Display output.
fn fmt_f64(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value.is_infinite() {
        if value.is_sign_positive() {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        }
    } else {
        value.to_string()
    }
}

/// Renders `snapshot` as a full Prometheus text exposition document.
///
/// Each metric family is emitted once, with its `# HELP`/`# TYPE` header followed
/// by every sample in that family, so the output parses cleanly and carries no
/// duplicate or interleaved series.
fn render(snapshot: &ServerSnapshot) -> String {
    let mut e = Exposition::new();
    render_runtime(&mut e, snapshot);
    render_world(&mut e, snapshot);
    render_network_storage(&mut e, snapshot);
    render_block_mutations(&mut e, snapshot);
    render_decode_errors(&mut e, snapshot);
    render_plugins(&mut e, snapshot);
    render_players(&mut e, snapshot);
    render_packet_trace(&mut e, snapshot);
    e.into_string()
}

/// Server identity, uptime, and tick-performance families.
fn render_runtime(e: &mut Exposition, snapshot: &ServerSnapshot) {
    e.family(
        "ferrumc_build_info",
        "Server build string, exposed as a constant `1` gauge labelled by build.",
        MetricKind::Gauge,
    );
    e.sample("ferrumc_build_info", &[("build", &snapshot.build)], "1");

    e.family(
        "ferrumc_uptime_seconds",
        "Server uptime in seconds.",
        MetricKind::Gauge,
    );
    e.sample(
        "ferrumc_uptime_seconds",
        &[],
        &fmt_u64(snapshot.uptime_secs),
    );

    e.family(
        "ferrumc_tick",
        "Current simulation tick number.",
        MetricKind::Gauge,
    );
    e.sample("ferrumc_tick", &[], &fmt_u64(snapshot.tick));

    e.family(
        "ferrumc_tps",
        "Effective ticks per second over the last wall-clock second.",
        MetricKind::Gauge,
    );
    e.sample("ferrumc_tps", &[], &fmt_f64(snapshot.tps));

    e.family(
        "ferrumc_tick_ms",
        "Tick duration in milliseconds at the given quantile over the sliding window.",
        MetricKind::Gauge,
    );
    for (quantile, value) in [
        ("0.5", snapshot.tick_p50_ms),
        ("0.95", snapshot.tick_p95_ms),
        ("0.99", snapshot.tick_p99_ms),
    ] {
        e.sample(
            "ferrumc_tick_ms",
            &[("quantile", quantile)],
            &fmt_f64(value),
        );
    }
}

/// Player count and world/chunk residence families.
fn render_world(e: &mut Exposition, snapshot: &ServerSnapshot) {
    e.family(
        "ferrumc_players_online",
        "Number of players currently connected.",
        MetricKind::Gauge,
    );
    e.sample(
        "ferrumc_players_online",
        &[],
        &fmt_u64(snapshot.players_online as u64),
    );

    e.family(
        "ferrumc_chunks_loaded",
        "Resident (loaded) chunk count.",
        MetricKind::Gauge,
    );
    e.sample(
        "ferrumc_chunks_loaded",
        &[],
        &fmt_u64(snapshot.chunks_loaded as u64),
    );

    e.family(
        "ferrumc_chunks_dirty",
        "Chunks marked network-dirty (pending client sync).",
        MetricKind::Gauge,
    );
    e.sample(
        "ferrumc_chunks_dirty",
        &[],
        &fmt_u64(snapshot.chunks_dirty as u64),
    );

    e.family(
        "ferrumc_chunks_persist_dirty",
        "Chunks marked persist-dirty (pending storage flush).",
        MetricKind::Gauge,
    );
    e.sample(
        "ferrumc_chunks_persist_dirty",
        &[],
        &fmt_u64(snapshot.chunks_persist_dirty as u64),
    );

    e.family(
        "ferrumc_chunks_sent_total",
        "Total chunk columns sent to clients.",
        MetricKind::Counter,
    );
    e.sample(
        "ferrumc_chunks_sent_total",
        &[],
        &fmt_u64(snapshot.chunk_sent_total),
    );

    e.family(
        "ferrumc_chunks_unloaded_total",
        "Total chunk columns unloaded from residence.",
        MetricKind::Counter,
    );
    e.sample(
        "ferrumc_chunks_unloaded_total",
        &[],
        &fmt_u64(snapshot.chunk_unloaded_total),
    );
}

/// Outbound-queue and storage-flush families.
fn render_network_storage(e: &mut Exposition, snapshot: &ServerSnapshot) {
    e.family(
        "ferrumc_network_outbound_queue_len_max",
        "Largest outbound queue depth observed across all sessions.",
        MetricKind::Gauge,
    );
    e.sample(
        "ferrumc_network_outbound_queue_len_max",
        &[],
        &fmt_u64(snapshot.network_outbound_queue_len_max),
    );

    e.family(
        "ferrumc_storage_flush_ms_last",
        "Most recent storage flush latency in milliseconds.",
        MetricKind::Gauge,
    );
    e.sample(
        "ferrumc_storage_flush_ms_last",
        &[],
        &fmt_u64(snapshot.storage_flush_ms_last),
    );

    e.family(
        "ferrumc_storage_flush_ms_avg",
        "Mean storage flush latency in milliseconds.",
        MetricKind::Gauge,
    );
    e.sample(
        "ferrumc_storage_flush_ms_avg",
        &[],
        &fmt_f64(snapshot.storage_flush_ms_avg),
    );
}

/// The block-mutation counter family.
fn render_block_mutations(e: &mut Exposition, snapshot: &ServerSnapshot) {
    e.family(
        "ferrumc_block_mutations_total",
        "Block mutations by operation (break/place) and result (accepted/rejected).",
        MetricKind::Counter,
    );
    for (op, counts) in [
        ("break", &snapshot.block_breaks),
        ("place", &snapshot.block_places),
    ] {
        e.sample(
            "ferrumc_block_mutations_total",
            &[("op", op), ("result", "accepted")],
            &fmt_u64(counts.accepted),
        );
        e.sample(
            "ferrumc_block_mutations_total",
            &[("op", op), ("result", "rejected")],
            &fmt_u64(counts.rejected),
        );
    }
}

/// The decode-error counter families (bounded by the fixed-size decode-error table).
fn render_decode_errors(e: &mut Exposition, snapshot: &ServerSnapshot) {
    e.family(
        "ferrumc_decode_errors_total",
        "Packet decode errors by connection state and packet.",
        MetricKind::Counter,
    );
    for entry in &snapshot.decode_errors_recent {
        e.sample(
            "ferrumc_decode_errors_total",
            &[("state", &entry.state), ("packet", &entry.packet)],
            &fmt_u64(entry.count),
        );
    }

    e.family(
        "ferrumc_decode_errors_overflow_total",
        "Distinct decode-error keys that did not fit the fixed-size table.",
        MetricKind::Counter,
    );
    e.sample(
        "ferrumc_decode_errors_overflow_total",
        &[],
        &fmt_u64(snapshot.decode_errors_overflow),
    );
}

/// The plugin-decision counter family (bounded by the loaded-plugin count).
fn render_plugins(e: &mut Exposition, snapshot: &ServerSnapshot) {
    e.family(
        "ferrumc_plugin_decisions_total",
        "Plugin event decisions by plugin and decision (allow/deny/replace/panic).",
        MetricKind::Counter,
    );
    for plugin in &snapshot.plugin_decisions {
        let name = plugin.plugin_name.as_str();
        for (decision, count) in [
            ("allow", plugin.decisions.allow),
            ("deny", plugin.decisions.deny),
            ("replace", plugin.decisions.replace),
            ("panic", plugin.decisions.panic),
        ] {
            e.sample(
                "ferrumc_plugin_decisions_total",
                &[("plugin", name), ("decision", decision)],
                &fmt_u64(count),
            );
        }
    }
}

/// The per-player families (bounded by the online player count).
///
/// Sourced from `players[]` (not the parallel `network_per_player` feed) so each
/// player name yields exactly one series per family and never a duplicate.
fn render_players(e: &mut Exposition, snapshot: &ServerSnapshot) {
    e.family(
        "ferrumc_player_network_bytes_total",
        "Bytes transferred per player by direction (in/out).",
        MetricKind::Counter,
    );
    for player in &snapshot.players {
        let name = player.name.as_str();
        e.sample(
            "ferrumc_player_network_bytes_total",
            &[("player", name), ("dir", "in")],
            &fmt_u64(player.network_in_bytes),
        );
        e.sample(
            "ferrumc_player_network_bytes_total",
            &[("player", name), ("dir", "out")],
            &fmt_u64(player.network_out_bytes),
        );
    }

    e.family(
        "ferrumc_player_frames_total",
        "Frames per player by direction (decoded/encoded).",
        MetricKind::Counter,
    );
    for player in &snapshot.players {
        let name = player.name.as_str();
        e.sample(
            "ferrumc_player_frames_total",
            &[("player", name), ("dir", "decoded")],
            &fmt_u64(player.frames_decoded),
        );
        e.sample(
            "ferrumc_player_frames_total",
            &[("player", name), ("dir", "encoded")],
            &fmt_u64(player.frames_encoded),
        );
    }

    e.family(
        "ferrumc_player_packets_dropped_total",
        "Packets dropped per player.",
        MetricKind::Counter,
    );
    for player in &snapshot.players {
        e.sample(
            "ferrumc_player_packets_dropped_total",
            &[("player", &player.name)],
            &fmt_u64(player.packets_dropped_total),
        );
    }

    e.family(
        "ferrumc_player_outbound_queue_len",
        "Outbound queue depth per player.",
        MetricKind::Gauge,
    );
    for player in &snapshot.players {
        e.sample(
            "ferrumc_player_outbound_queue_len",
            &[("player", &player.name)],
            &fmt_u64(player.outbound_queue_len as u64),
        );
    }
}

/// The packet-trace counter family (bounded by the snapshot's top-N window).
fn render_packet_trace(e: &mut Exposition, snapshot: &ServerSnapshot) {
    e.family(
        "ferrumc_packet_trace_total",
        "Top traced packets by direction (inbound/outbound), packet, and state.",
        MetricKind::Counter,
    );
    for (dir, summary) in [
        ("inbound", &snapshot.inbound_trace_summary),
        ("outbound", &snapshot.outbound_trace_summary),
    ] {
        for freq in &summary.top_packets {
            e.sample(
                "ferrumc_packet_trace_total",
                &[
                    ("dir", dir),
                    ("packet", &freq.packet_name),
                    ("state", &freq.state),
                ],
                &fmt_u64(freq.count as u64),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{HashMap, HashSet};

    use ferrumc_observability::{
        DecodeErrorSnapshot, MutationCountSnapshot, PacketFrequency, PacketTraceSummary,
        PlayerSnapshot, PluginDecisionSnapshot, PluginDecisions,
    };

    /// A representative snapshot exercising every family, including a hostile
    /// player name (quote + backslash + newline) to prove label escaping.
    fn representative_snapshot() -> ServerSnapshot {
        ServerSnapshot {
            build: "ferrumc 0.2.0-dev".to_string(),
            uptime_secs: 123,
            tick: 4_560,
            tps: 19.97,
            tick_p50_ms: 2.5,
            tick_p95_ms: 6.0,
            tick_p99_ms: 9.5,
            players_online: 2,
            players: vec![
                PlayerSnapshot {
                    name: "Notch".to_string(),
                    network_in_bytes: 1_024,
                    network_out_bytes: 4_096,
                    frames_decoded: 50,
                    frames_encoded: 80,
                    packets_dropped_total: 1,
                    outbound_queue_len: 3,
                    ..PlayerSnapshot::default()
                },
                PlayerSnapshot {
                    // Hostile name: must be escaped, not break the grammar.
                    name: "ev\"il\\\nguy".to_string(),
                    network_in_bytes: 7,
                    ..PlayerSnapshot::default()
                },
            ],
            chunks_loaded: 81,
            chunks_dirty: 0,
            chunks_persist_dirty: 4,
            chunk_sent_total: 200,
            chunk_unloaded_total: 5,
            network_outbound_queue_len_max: 12,
            storage_flush_ms_last: 8,
            storage_flush_ms_avg: 6.5,
            block_breaks: MutationCountSnapshot {
                accepted: 10,
                rejected: 2,
            },
            block_places: MutationCountSnapshot {
                accepted: 20,
                rejected: 1,
            },
            decode_errors_recent: vec![DecodeErrorSnapshot {
                state: "play".to_string(),
                packet: "set_creative_slot".to_string(),
                count: 3,
            }],
            decode_errors_overflow: 1,
            plugin_decisions: vec![PluginDecisionSnapshot {
                plugin_name: "spawn-protect".to_string(),
                decisions: PluginDecisions {
                    allow: 5,
                    deny: 2,
                    replace: 0,
                    panic: 0,
                },
            }],
            inbound_trace_summary: PacketTraceSummary {
                top_packets: vec![PacketFrequency {
                    packet_name: "player_position".to_string(),
                    state: "play".to_string(),
                    count: 42,
                }],
            },
            outbound_trace_summary: PacketTraceSummary {
                top_packets: vec![PacketFrequency {
                    packet_name: "keep_alive".to_string(),
                    state: "play".to_string(),
                    count: 7,
                }],
            },
            ..ServerSnapshot::default()
        }
    }

    /// The metric name of a sample line is everything before the first `{` (if any
    /// labels) or the trailing space.
    fn metric_name_of(series: &str) -> &str {
        match series.find('{') {
            Some(idx) => &series[..idx],
            None => series,
        }
    }

    /// A value token is valid if it parses as an `f64` or is one of the special
    /// Prometheus tokens.
    fn value_is_valid(value: &str) -> bool {
        matches!(value, "NaN" | "+Inf" | "-Inf") || value.parse::<f64>().is_ok()
    }

    /// Walks the rendered exposition and asserts every structural invariant the
    /// format requires, returning the set of metric names seen for spot checks.
    fn assert_well_formed(body: &str) -> HashSet<String> {
        let mut help_count: HashMap<String, u32> = HashMap::new();
        let mut type_count: HashMap<String, u32> = HashMap::new();
        let mut declared: HashSet<String> = HashSet::new();
        let mut series_seen: HashSet<String> = HashSet::new();
        let mut closed: HashSet<String> = HashSet::new();
        let mut prev_sample_metric: Option<String> = None;

        for line in body.lines() {
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("# HELP ") {
                // Entering a header closes the previous family's sample run.
                if let Some(prev) = prev_sample_metric.take() {
                    closed.insert(prev);
                }
                let name = rest
                    .split(' ')
                    .next()
                    .expect("HELP line carries a metric name");
                *help_count.entry(name.to_string()).or_default() += 1;
                declared.insert(name.to_string());
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                if let Some(prev) = prev_sample_metric.take() {
                    closed.insert(prev);
                }
                let mut parts = rest.split(' ');
                let name = parts.next().expect("TYPE line carries a metric name");
                let kind = parts.next().expect("TYPE line carries a kind");
                assert!(
                    matches!(kind, "counter" | "gauge"),
                    "unexpected metric kind {kind:?}"
                );
                *type_count.entry(name.to_string()).or_default() += 1;
            } else if line.starts_with('#') {
                panic!("unexpected comment line: {line:?}");
            } else {
                let (series, value) = line
                    .rsplit_once(' ')
                    .expect("sample line is `series value`");
                let metric = metric_name_of(series).to_string();

                // HELP and TYPE must precede any sample of the metric.
                assert!(
                    declared.contains(&metric),
                    "sample for {metric:?} before its HELP/TYPE"
                );

                // Samples of one family must be contiguous: we must not return to a
                // metric whose run we already closed.
                match &prev_sample_metric {
                    Some(prev) if prev == &metric => {}
                    Some(prev) => {
                        closed.insert(prev.clone());
                        assert!(
                            !closed.contains(&metric),
                            "family {metric:?} is non-contiguous"
                        );
                    }
                    None => assert!(
                        !closed.contains(&metric),
                        "family {metric:?} is non-contiguous"
                    ),
                }
                prev_sample_metric = Some(metric);

                assert!(
                    series_seen.insert(series.to_string()),
                    "duplicate series: {series:?}"
                );
                assert!(value_is_valid(value), "unparseable value in line: {line:?}");
            }
        }

        // Exactly one HELP and one TYPE per declared family.
        for name in &declared {
            assert_eq!(help_count.get(name), Some(&1), "HELP count for {name:?}");
            assert_eq!(type_count.get(name), Some(&1), "TYPE count for {name:?}");
        }

        declared
    }

    #[test]
    fn rendered_body_is_valid_exposition_format() {
        let body = render(&representative_snapshot());
        let metrics = assert_well_formed(&body);

        // A spread of the families we promised are actually present.
        for expected in [
            "ferrumc_build_info",
            "ferrumc_tps",
            "ferrumc_tick_ms",
            "ferrumc_players_online",
            "ferrumc_chunks_loaded",
            "ferrumc_chunks_sent_total",
            "ferrumc_block_mutations_total",
            "ferrumc_decode_errors_total",
            "ferrumc_plugin_decisions_total",
            "ferrumc_player_network_bytes_total",
            "ferrumc_packet_trace_total",
        ] {
            assert!(metrics.contains(expected), "missing family {expected}");
        }
    }

    #[test]
    fn labels_and_values_match_the_snapshot() {
        let body = render(&representative_snapshot());

        // Counter with composite labels.
        assert!(
            body.contains("ferrumc_block_mutations_total{op=\"break\",result=\"accepted\"} 10\n")
        );
        assert!(
            body.contains("ferrumc_block_mutations_total{op=\"place\",result=\"rejected\"} 1\n")
        );
        // Plugin decisions.
        assert!(body.contains(
            "ferrumc_plugin_decisions_total{plugin=\"spawn-protect\",decision=\"allow\"} 5\n"
        ));
        // Quantile gauge.
        assert!(body.contains("ferrumc_tick_ms{quantile=\"0.95\"} 6\n"));
        // Packet trace across both directions.
        assert!(body.contains(
            "ferrumc_packet_trace_total{dir=\"inbound\",packet=\"player_position\",state=\"play\"} 42\n"
        ));
        assert!(body.contains(
            "ferrumc_packet_trace_total{dir=\"outbound\",packet=\"keep_alive\",state=\"play\"} 7\n"
        ));
        // build_info is a constant 1 gauge.
        assert!(body.contains("ferrumc_build_info{build=\"ferrumc 0.2.0-dev\"} 1\n"));
    }

    #[test]
    fn hostile_label_values_are_escaped() {
        let body = render(&representative_snapshot());
        // `ev"il\<newline>guy` must render with backslash/quote/newline escaped and
        // must never inject a literal newline or stray quote into the line.
        assert!(body.contains(
            "ferrumc_player_network_bytes_total{player=\"ev\\\"il\\\\\\nguy\",dir=\"in\"} 7\n"
        ));
        // The raw (unescaped) newline must not appear inside any sample line — the
        // walker below would otherwise see a malformed line.
        assert_well_formed(&body);
    }

    #[test]
    fn empty_snapshot_still_renders_headers_without_samples() {
        // A default snapshot has no players/plugins/traces: the families still emit
        // their HELP/TYPE headers and the document stays well-formed.
        let body = render(&ServerSnapshot::default());
        let metrics = assert_well_formed(&body);
        assert!(metrics.contains("ferrumc_player_network_bytes_total"));
        assert!(metrics.contains("ferrumc_plugin_decisions_total"));
        // Scalar gauges are present even at zero.
        assert!(body.contains("ferrumc_players_online 0\n"));
    }

    #[test]
    fn non_finite_floats_use_prometheus_tokens() {
        assert_eq!(fmt_f64(f64::NAN), "NaN");
        assert_eq!(fmt_f64(f64::INFINITY), "+Inf");
        assert_eq!(fmt_f64(f64::NEG_INFINITY), "-Inf");
        assert_eq!(fmt_f64(20.0), "20");
    }
}
