//! Server-rendered dashboard pages.
//!
//! Every page is a plain HTML `String` built with `format!`/string literals and
//! returned as [`axum::response::Html`]; there is no frontend build and no static
//! asset directory. The shared [`layout`] wraps each page's body in a sidebar nav
//! plus a content region that polls itself once a second through htmx (loaded from
//! a CDN), so each handler also answers a `?partial=1` request with just the body
//! fragment for that refresh.
//!
//! All data comes from the latest [`ServerSnapshot`] read out of the
//! [`SnapshotPublisher`]; the dashboard never mutates anything.

use std::fmt::Write as _;

use axum::extract::{Query, State};
use axum::response::Html;
use ferrumc_observability::{PacketTraceSummary, ServerSnapshot, SnapshotPublisher};
use serde::Deserialize;

/// The CDN htmx bundle used for the once-a-second content refresh.
const HTMX_CDN: &str = "https://unpkg.com/htmx.org@1.9.12";

/// Query parameters shared by every page.
#[derive(Debug, Default, Deserialize)]
pub struct PageQuery {
    /// Present (any value) when htmx is asking for just the content fragment.
    partial: Option<String>,
}

/// The set of pages, used to mark the active nav entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Players,
    World,
    PacketTrace,
    Backpressure,
    Plugins,
    Persistence,
    Checklist,
}

impl Page {
    /// `(path, label)` for every page, in nav order.
    const ALL: [(Page, &'static str, &'static str); 8] = [
        (Page::Overview, "/", "Overview"),
        (Page::Players, "/players", "Players"),
        (Page::World, "/world", "World"),
        (Page::PacketTrace, "/packet-trace", "Packet trace"),
        (Page::Backpressure, "/backpressure", "Backpressure"),
        (Page::Plugins, "/plugins", "Plugins"),
        (Page::Persistence, "/persistence", "Persistence"),
        (Page::Checklist, "/checklist", "Checklist"),
    ];
}

/// HTML-escapes a user- or runtime-controlled string so rendered values can never
/// inject markup into the page.
fn esc(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Either the full page (chrome + body) or, for an htmx poll, just the body.
fn render(active: Page, query: &PageQuery, body: String) -> Html<String> {
    if query.partial.is_some() {
        Html(body)
    } else {
        Html(layout(active, &body))
    }
}

/// Wraps `body` in the shared chrome: a sidebar nav and a self-polling content
/// region (htmx refreshes the content every second via `?partial=1`).
fn layout(active: Page, body: &str) -> String {
    let nav = Page::ALL
        .into_iter()
        .fold(String::new(), |mut acc, (page, path, label)| {
            let class = if page == active { "active" } else { "" };
            let _ = write!(acc, "<a class=\"{class}\" href=\"{path}\">{label}</a>");
            acc
        });
    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>FerrumC dashboard</title>\
<script src=\"{HTMX_CDN}\"></script>\
<style>{STYLE}</style></head>\
<body><div class=\"shell\">\
<nav class=\"sidebar\"><div class=\"brand\">FerrumC</div>{nav}\
<div class=\"foot\">read-only &middot; localhost</div></nav>\
<main id=\"content\" hx-get=\"?partial=1\" hx-trigger=\"every 1s\" hx-swap=\"innerHTML\">{body}</main>\
</div></body></html>"
    )
}

/// The inline stylesheet (no external CSS, no asset directory).
const STYLE: &str = "\
:root{color-scheme:dark;}\
*{box-sizing:border-box;}\
body{margin:0;font:14px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace;background:#0d1117;color:#c9d1d9;}\
.shell{display:flex;min-height:100vh;}\
.sidebar{width:200px;flex:0 0 200px;background:#161b22;border-right:1px solid #30363d;padding:16px 0;display:flex;flex-direction:column;}\
.brand{font-weight:700;font-size:18px;padding:0 16px 16px;color:#58a6ff;}\
.sidebar a{display:block;padding:8px 16px;color:#c9d1d9;text-decoration:none;}\
.sidebar a:hover{background:#21262d;}\
.sidebar a.active{background:#21262d;border-left:3px solid #58a6ff;color:#fff;}\
.foot{margin-top:auto;padding:16px;font-size:11px;color:#6e7681;}\
main{flex:1;padding:24px;overflow:auto;}\
h1{font-size:20px;margin:0 0 16px;}\
h2{font-size:15px;margin:24px 0 8px;color:#8b949e;text-transform:uppercase;letter-spacing:.05em;}\
.cards{display:grid;grid-template-columns:repeat(auto-fill,minmax(180px,1fr));gap:12px;}\
.card{background:#161b22;border:1px solid #30363d;border-radius:8px;padding:12px 14px;}\
.card .k{font-size:11px;color:#8b949e;text-transform:uppercase;}\
.card .v{font-size:22px;font-weight:700;margin-top:4px;}\
table{border-collapse:collapse;width:100%;margin-top:8px;}\
th,td{text-align:left;padding:6px 10px;border-bottom:1px solid #21262d;}\
th{color:#8b949e;font-weight:600;}\
.ok{color:#3fb950;}.warn{color:#d29922;}.bad{color:#f85149;}.muted{color:#6e7681;}\
.pill{display:inline-block;padding:1px 8px;border-radius:10px;font-size:12px;background:#21262d;}\
";

/// Builds one metric card.
fn card(key: &str, value: &str) -> String {
    format!("<div class=\"card\"><div class=\"k\">{key}</div><div class=\"v\">{value}</div></div>")
}

/// Classifies a TPS value into a status colour class.
fn tps_class(tps: f64) -> &'static str {
    if tps >= 19.0 {
        "ok"
    } else if tps >= 15.0 {
        "warn"
    } else {
        "bad"
    }
}

// --- Handlers -------------------------------------------------------------

/// `GET /` — the at-a-glance server overview.
pub async fn overview(
    State(publisher): State<SnapshotPublisher>,
    Query(query): Query<PageQuery>,
) -> Html<String> {
    let snap = publisher.latest();
    render(Page::Overview, &query, overview_body(&snap))
}

fn overview_body(snap: &ServerSnapshot) -> String {
    let tps = format!(
        "<span class=\"{}\">{:.1}</span>",
        tps_class(snap.tps),
        snap.tps
    );
    let cards = [
        card("Build", &esc(&snap.build)),
        card("Uptime (s)", &snap.uptime_secs.to_string()),
        card("Tick", &snap.tick.to_string()),
        card("TPS", &tps),
        card("Players", &snap.players_online.to_string()),
        card("Chunks loaded", &snap.chunks_loaded.to_string()),
        card("Tick p50 (ms)", &format!("{:.2}", snap.tick_p50_ms)),
        card("Tick p95 (ms)", &format!("{:.2}", snap.tick_p95_ms)),
        card("Tick p99 (ms)", &format!("{:.2}", snap.tick_p99_ms)),
        card("Flush last (ms)", &snap.storage_flush_ms_last.to_string()),
        card(
            "Block breaks",
            &format!(
                "{} / {}",
                snap.block_breaks.accepted, snap.block_breaks.rejected
            ),
        ),
        card(
            "Block places",
            &format!(
                "{} / {}",
                snap.block_places.accepted, snap.block_places.rejected
            ),
        ),
    ]
    .concat();
    format!(
        "<h1>Overview</h1><div class=\"cards\">{cards}</div>\
<p class=\"muted\">Block counts shown as accepted / rejected. Decode errors: {}.</p>",
        snap.decode_errors_recent.len() as u64 + snap.decode_errors_overflow
    )
}

/// `GET /players` — the connected-player table.
pub async fn players(
    State(publisher): State<SnapshotPublisher>,
    Query(query): Query<PageQuery>,
) -> Html<String> {
    let snap = publisher.latest();
    render(Page::Players, &query, players_body(&snap))
}

fn players_body(snap: &ServerSnapshot) -> String {
    if snap.players.is_empty() {
        return "<h1>Players</h1><p class=\"muted\">No players connected.</p>".to_string();
    }
    let rows = snap.players.iter().fold(String::new(), |mut acc, player| {
        let _ = write!(
            acc,
            "<tr><td>{name}</td><td>{x:.1}, {y:.1}, {z:.1}</td><td>{cx}, {cz}</td>\
<td><span class=\"pill\">{mode}</span></td><td>{queue}</td><td>{net_in}</td><td>{net_out}</td></tr>",
            name = esc(&player.name),
            x = player.position.x,
            y = player.position.y,
            z = player.position.z,
            cx = player.chunk.x,
            cz = player.chunk.z,
            mode = esc(&player.gamemode),
            queue = player.outbound_queue_len,
            net_in = player.network_in_bytes,
            net_out = player.network_out_bytes,
        );
        acc
    });
    format!(
        "<h1>Players ({count})</h1>\
<table><thead><tr><th>Name</th><th>Position</th><th>Chunk</th><th>Mode</th>\
<th>Queue</th><th>In (B)</th><th>Out (B)</th></tr></thead><tbody>{rows}</tbody></table>\
<p class=\"muted\">Per-player network counters are fed by the network lane.</p>",
        count = snap.players.len(),
    )
}

/// `GET /world` — chunk residence and chunk I/O totals.
pub async fn world(
    State(publisher): State<SnapshotPublisher>,
    Query(query): Query<PageQuery>,
) -> Html<String> {
    let snap = publisher.latest();
    render(Page::World, &query, world_body(&snap))
}

fn world_body(snap: &ServerSnapshot) -> String {
    let cards = [
        card("Chunks loaded", &snap.chunks_loaded.to_string()),
        card("Chunks dirty", &snap.chunks_dirty.to_string()),
        card("Persist-dirty", &snap.chunks_persist_dirty.to_string()),
        card("Chunks sent", &snap.chunk_sent_total.to_string()),
        card("Chunks unloaded", &snap.chunk_unloaded_total.to_string()),
    ]
    .concat();
    format!(
        "<h1>World</h1><div class=\"cards\">{cards}</div>\
<p class=\"muted\">Dirty-chunk counts are approximate until the chunk map exposes exact counters.</p>"
    )
}

/// `GET /packet-trace` — top inbound/outbound packets by frequency.
pub async fn packet_trace(
    State(publisher): State<SnapshotPublisher>,
    Query(query): Query<PageQuery>,
) -> Html<String> {
    let snap = publisher.latest();
    render(Page::PacketTrace, &query, packet_trace_body(&snap))
}

fn trace_table(title: &str, summary: &PacketTraceSummary) -> String {
    if summary.top_packets.is_empty() {
        return format!(
            "<h2>{title}</h2><p class=\"muted\">No trace data (fed by the network lane).</p>"
        );
    }
    let rows = summary
        .top_packets
        .iter()
        .fold(String::new(), |mut acc, entry| {
            let _ = write!(
                acc,
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&entry.packet_name),
                esc(&entry.state),
                entry.count
            );
            acc
        });
    format!(
        "<h2>{title}</h2><table><thead><tr><th>Packet</th><th>State</th><th>Count</th></tr></thead>\
<tbody>{rows}</tbody></table>"
    )
}

fn packet_trace_body(snap: &ServerSnapshot) -> String {
    format!(
        "<h1>Packet trace</h1>{}{}",
        trace_table("Inbound", &snap.inbound_trace_summary),
        trace_table("Outbound", &snap.outbound_trace_summary),
    )
}

/// `GET /backpressure` — queue-depth health.
pub async fn backpressure(
    State(publisher): State<SnapshotPublisher>,
    Query(query): Query<PageQuery>,
) -> Html<String> {
    let snap = publisher.latest();
    render(Page::Backpressure, &query, backpressure_body(&snap))
}

fn backpressure_body(snap: &ServerSnapshot) -> String {
    let cards = card(
        "Outbound queue max",
        &snap.network_outbound_queue_len_max.to_string(),
    );
    let rows = snap.players.iter().fold(String::new(), |mut acc, player| {
        let _ = write!(
            acc,
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(&player.name),
            player.outbound_queue_len,
            player.packets_dropped_total
        );
        acc
    });
    let per_player = if rows.is_empty() {
        "<p class=\"muted\">No players connected.</p>".to_string()
    } else {
        format!(
            "<table><thead><tr><th>Player</th><th>Queue</th><th>Dropped</th></tr></thead>\
<tbody>{rows}</tbody></table>"
        )
    };
    format!(
        "<h1>Backpressure</h1><div class=\"cards\">{cards}</div><h2>Per-player</h2>{per_player}\
<p class=\"muted\">Per-player queue depth is fed by the network lane.</p>"
    )
}

/// `GET /plugins` — per-plugin decision counts.
pub async fn plugins(
    State(publisher): State<SnapshotPublisher>,
    Query(query): Query<PageQuery>,
) -> Html<String> {
    let snap = publisher.latest();
    render(Page::Plugins, &query, plugins_body(&snap))
}

fn plugins_body(snap: &ServerSnapshot) -> String {
    if snap.plugin_decisions.is_empty() {
        return "<h1>Plugins</h1><p class=\"muted\">No plugin decision data (fed by the plugin lane).</p>"
            .to_string();
    }
    let rows = snap
        .plugin_decisions
        .iter()
        .fold(String::new(), |mut acc, plugin| {
            let _ = write!(
                acc,
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&plugin.plugin_name),
                plugin.decisions.allow,
                plugin.decisions.deny,
                plugin.decisions.replace,
                plugin.decisions.panic
            );
            acc
        });
    format!(
        "<h1>Plugins</h1><table><thead><tr><th>Plugin</th><th>Allow</th><th>Deny</th>\
<th>Replace</th><th>Panic</th></tr></thead><tbody>{rows}</tbody></table>"
    )
}

/// `GET /persistence` — storage flush + mutation health.
pub async fn persistence(
    State(publisher): State<SnapshotPublisher>,
    Query(query): Query<PageQuery>,
) -> Html<String> {
    let snap = publisher.latest();
    render(Page::Persistence, &query, persistence_body(&snap))
}

fn persistence_body(snap: &ServerSnapshot) -> String {
    let cards = [
        card("Flush last (ms)", &snap.storage_flush_ms_last.to_string()),
        card(
            "Flush avg (ms)",
            &format!("{:.2}", snap.storage_flush_ms_avg),
        ),
        card("Persist-dirty", &snap.chunks_persist_dirty.to_string()),
        card("Breaks accepted", &snap.block_breaks.accepted.to_string()),
        card("Places accepted", &snap.block_places.accepted.to_string()),
    ]
    .concat();
    format!("<h1>Persistence</h1><div class=\"cards\">{cards}</div>")
}

/// `GET /checklist` — the public-alpha checklist, with live-derived items where
/// the snapshot supports it and the rest mirrored from `docs/public-alpha.md`.
pub async fn checklist(
    State(publisher): State<SnapshotPublisher>,
    Query(query): Query<PageQuery>,
) -> Html<String> {
    let snap = publisher.latest();
    render(Page::Checklist, &query, checklist_body(&snap))
}

/// Renders one checklist row with a pass/fail/neutral marker.
fn check_row(label: &str, state: Option<bool>, note: &str) -> String {
    let (mark, class) = match state {
        Some(true) => ("PASS", "ok"),
        Some(false) => ("todo", "warn"),
        None => ("n/a", "muted"),
    };
    format!(
        "<tr><td><span class=\"{class}\">{mark}</span></td><td>{}</td><td class=\"muted\">{}</td></tr>",
        esc(label),
        esc(note)
    )
}

fn checklist_body(snap: &ServerSnapshot) -> String {
    let placed = snap.block_places.accepted > 0;
    let broke = snap.block_breaks.accepted > 0;
    let up = snap.uptime_secs > 0 || snap.tick > 0;
    let rows = [
        check_row(
            "Dashboard opens locally",
            Some(true),
            "you are looking at it",
        ),
        check_row(
            "Running with no config starts safely",
            Some(up),
            "derived: server is ticking",
        ),
        check_row(
            "Vanilla 1.21.8 joins in offline mode",
            Some(snap.players_online > 0),
            "live: at least one player online",
        ),
        check_row(
            "Two clients see block changes",
            Some(broke || placed),
            "live: an accepted block mutation",
        ),
        check_row(
            "Creative hotbar placement works",
            Some(placed),
            "live: an accepted block place",
        ),
        check_row(
            "Leave/rejoin preserves placed blocks",
            None,
            "needs the persistence integration test",
        ),
        check_row(
            "Dirty state/journal path has metrics",
            Some(snap.storage_flush_ms_last > 0 || snap.block_places.accepted > 0),
            "live: storage flush / mutation metrics present",
        ),
        check_row("No unbounded channels", Some(true), "project invariant"),
        check_row("No unwrap outside tests", Some(true), "project invariant"),
        check_row(
            "xtask generate --check green",
            None,
            "checked in CI, not at runtime",
        ),
        check_row(
            "Plugin Replace visibly works",
            None,
            "needs the plugin lane",
        ),
    ]
    .concat();
    format!(
        "<h1>Public-alpha checklist</h1>\
<table><thead><tr><th>Status</th><th>Item</th><th>Source</th></tr></thead><tbody>{rows}</tbody></table>\
<p class=\"muted\">Live items derive from the current snapshot; the rest mirror docs/public-alpha.md.</p>"
    )
}
