//! Per-connection network telemetry, aggregated into the live [`ServerSnapshot`].
//!
//! The networking lane owns per-connection counters (frames, bytes, drops) and
//! per-connection packet-trace rings, but those live inside each connection task
//! and are otherwise only surfaced on a disconnect dump. This module bridges that
//! gap: a connection publishes a cheap, bounded [`ConnNetTelemetry`] snapshot into
//! the shared [`NetTelemetryHub`] at an existing off-hot-path seam (its outbound
//! queue-depth sample), and the driver folds every session's snapshot into the
//! per-tick [`ServerSnapshot`](crate::ServerSnapshot) — per-player counters plus a
//! server-wide top-N packet-trace summary.
//!
//! Everything here is bounded:
//!
//! - a [`PacketTally`] never holds more than [`TALLY_CAPACITY`] distinct
//!   `(state, packet)` keys (further keys fold into an overflow count);
//! - the hub never holds more than [`HUB_CAPACITY`] sessions (further sessions are
//!   refused, not queued), and is pruned each tick against the connected roster;
//! - aggregation produces at most `top_n` packet rows per direction.
//!
//! The hub is explicit shared state (an [`Arc<NetTelemetryHub>`](NetTelemetryHub)
//! threaded through the connection context and the driver), not a global static,
//! and is poison-safe like the rest of the crate.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, PoisonError};

use crate::snapshot::{
    packet_state_label, NetworkMetricsSnapshot, PacketFrequency, PacketTraceSummary,
};
use crate::trace::PacketState;

/// Maximum number of distinct `(state, packet)` keys one [`PacketTally`] retains
/// before further keys fold into its overflow count.
///
/// The modelled protocol slice has well under this many distinct packet names per
/// direction (a few dozen clientbound play packets, ~14 serverbound), so in
/// practice nothing overflows; the cap is a hard ceiling against a hostile or
/// buggy peer inventing labels, keeping the tally a small fixed-size structure.
pub const TALLY_CAPACITY: usize = 64;

/// Maximum number of sessions the [`NetTelemetryHub`] retains.
///
/// Sized far above any realistic concurrent-connection count for this milestone;
/// a [`publish`](NetTelemetryHub::publish) for a new session beyond the cap is
/// refused rather than growing the map, so a connection storm cannot make the hub
/// unbounded. Disconnected sessions are pruned each tick, so the live size tracks
/// the connected roster.
pub const HUB_CAPACITY: usize = 1024;

/// Default number of packet rows each trace summary direction reports.
pub const DEFAULT_TOP_N: usize = 16;

/// A bounded `(state, packet) -> count` frequency table.
///
/// Embedded in a connection's trace recorder (so every recorded packet is tallied
/// with no extra allocation) and published into the [`NetTelemetryHub`] for the
/// driver to merge into the server-wide top-N summary. Records in O(rows) over a
/// table capped at [`TALLY_CAPACITY`]; a key beyond the cap increments
/// [`overflow`](Self::overflow) instead of growing the table.
#[derive(Debug, Clone, Default)]
pub struct PacketTally {
    /// One row per distinct `(state, packet)` key seen, capped at
    /// [`TALLY_CAPACITY`].
    rows: Vec<PacketTallyRow>,
    /// Count of increments for keys that did not fit the capped table.
    overflow: u64,
}

/// One `(state, packet, count)` row of a [`PacketTally`].
#[derive(Debug, Clone, Copy)]
struct PacketTallyRow {
    state: PacketState,
    packet: &'static str,
    count: u64,
}

impl PacketTally {
    /// Creates an empty tally.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one packet observed in `state` named `packet`.
    ///
    /// Increments the matching row if present; otherwise inserts a new row, unless
    /// the table is already at [`TALLY_CAPACITY`], in which case the increment
    /// folds into [`overflow`](Self::overflow). Counts saturate rather than wrap.
    pub fn record(&mut self, state: PacketState, packet: &'static str) {
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.state == state && row.packet == packet)
        {
            row.count = row.count.saturating_add(1);
            return;
        }
        if self.rows.len() < TALLY_CAPACITY {
            self.rows.push(PacketTallyRow {
                state,
                packet,
                count: 1,
            });
        } else {
            self.overflow = self.overflow.saturating_add(1);
        }
    }

    /// The number of increments dropped because the table was full.
    #[must_use]
    pub fn overflow(&self) -> u64 {
        self.overflow
    }

    /// Iterates the recorded `(state, packet, count)` rows.
    pub fn entries(&self) -> impl Iterator<Item = (PacketState, &'static str, u64)> + '_ {
        self.rows
            .iter()
            .map(|row| (row.state, row.packet, row.count))
    }
}

/// A single connection's most recent network telemetry, published into the hub.
///
/// An inert transfer struct (public fields, the same precedent as the snapshot
/// DTOs): a connection fills one cheaply at its outbound queue-depth sample and
/// hands it to [`NetTelemetryHub::publish`]. The counters are cumulative for the
/// connection's lifetime; the hub keeps only the latest per session.
#[derive(Debug, Clone, Default)]
pub struct ConnNetTelemetry {
    /// The session label (the player name once login completes).
    pub session: String,
    /// Serverbound frames decoded for this connection.
    pub frames_in: u64,
    /// Serverbound body bytes decoded for this connection.
    pub bytes_in: u64,
    /// Clientbound frames encoded for this connection.
    pub frames_out: u64,
    /// Clientbound on-wire bytes produced for this connection.
    pub bytes_out: u64,
    /// Serverbound frames classified over the packet budget.
    pub over_budget: u64,
    /// Clientbound packets dropped per outbound priority (index = priority rank).
    pub dropped: [u64; 4],
    /// The connection's current outbound queue depth.
    pub queue_depth: u64,
    /// Inbound packet-name frequencies for the server-wide summary.
    pub inbound: PacketTally,
    /// Outbound packet-name frequencies for the server-wide summary.
    pub outbound: PacketTally,
}

impl ConnNetTelemetry {
    /// Total clientbound packets dropped across all priorities.
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.dropped
            .iter()
            .fold(0u64, |acc, n| acc.saturating_add(*n))
    }
}

/// The per-player counters folded into a [`PlayerSnapshot`](crate::PlayerSnapshot).
///
/// A compact projection of [`ConnNetTelemetry`] the driver applies to each
/// player's snapshot row (the richer per-priority view lives in
/// [`NetworkMetricsSnapshot`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct PlayerNetCounters {
    /// Bytes received from the player.
    pub network_in_bytes: u64,
    /// Bytes sent to the player.
    pub network_out_bytes: u64,
    /// Frames decoded from the player.
    pub frames_decoded: u64,
    /// Frames encoded to the player.
    pub frames_encoded: u64,
    /// Total packets dropped for the player.
    pub packets_dropped_total: u64,
    /// The player's outbound queue depth.
    pub outbound_queue_len: usize,
}

/// The aggregated network telemetry the driver folds into a tick's snapshot.
///
/// Produced by [`NetTelemetryHub::aggregate`]: a per-player metrics list, a
/// by-session counter lookup (so the driver can fill each player row it already
/// builds), and the two server-wide top-N packet-trace summaries.
#[derive(Debug, Clone, Default)]
pub struct NetTelemetryParts {
    /// Per-player network metrics, one row per live session.
    pub per_player: Vec<NetworkMetricsSnapshot>,
    /// Per-player counters keyed by session label, for folding into player rows.
    pub by_session: BTreeMap<String, PlayerNetCounters>,
    /// The top inbound packets across all sessions.
    pub inbound: PacketTraceSummary,
    /// The top outbound packets across all sessions.
    pub outbound: PacketTraceSummary,
}

/// A shared, bounded hub of the latest per-connection network telemetry.
///
/// Connection tasks [`publish`](Self::publish) their latest snapshot (off the
/// per-packet hot path — at the outbound queue-depth sample), the driver
/// [`prunes`](Self::retain_sessions) disconnected sessions and
/// [`aggregates`](Self::aggregate) the rest once per tick. The single `Mutex` is
/// taken only for those bounded operations (a handful of sessions), never per
/// packet, and recovers from poisoning like the rest of the crate.
#[derive(Debug, Default)]
pub struct NetTelemetryHub {
    /// Latest telemetry keyed by session label; capped at [`HUB_CAPACITY`].
    sessions: Mutex<BTreeMap<String, ConnNetTelemetry>>,
}

impl NetTelemetryHub {
    /// Creates an empty hub.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes `telemetry` as the latest snapshot for its session.
    ///
    /// Overwrites any prior snapshot for the same session label. A snapshot for a
    /// *new* session is refused once the hub holds [`HUB_CAPACITY`] sessions, so
    /// the map can never grow without bound. Poison-safe.
    pub fn publish(&self, telemetry: ConnNetTelemetry) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if !sessions.contains_key(&telemetry.session) && sessions.len() >= HUB_CAPACITY {
            return;
        }
        sessions.insert(telemetry.session.clone(), telemetry);
    }

    /// Drops every session whose label is not in `keep`.
    ///
    /// Called each tick with the connected roster so disconnected sessions never
    /// linger. Poison-safe.
    pub fn retain_sessions(&self, keep: &BTreeSet<String>) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        sessions.retain(|session, _| keep.contains(session));
    }

    /// The number of sessions currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether the hub holds no sessions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    /// Folds every live session into [`NetTelemetryParts`], reporting at most
    /// `top_n` packet rows per direction.
    ///
    /// Cheap and bounded: it walks the (small) session map once, merges the
    /// per-session tallies into a per-direction map keyed by `(state, packet)`
    /// (bounded by the modelled packet space), then sorts and truncates to
    /// `top_n`. Ordering is deterministic — by descending count, then by
    /// `(state, packet)` — so the summary never jitters between equal-count ticks.
    /// Poison-safe.
    #[must_use]
    pub fn aggregate(&self, top_n: usize) -> NetTelemetryParts {
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);

        let mut per_player = Vec::with_capacity(sessions.len());
        let mut by_session = BTreeMap::new();
        let mut inbound_merge: BTreeMap<(u8, &'static str), u64> = BTreeMap::new();
        let mut outbound_merge: BTreeMap<(u8, &'static str), u64> = BTreeMap::new();

        for telemetry in sessions.values() {
            per_player.push(NetworkMetricsSnapshot {
                player_name: telemetry.session.clone(),
                frames_in: telemetry.frames_in,
                bytes_in: telemetry.bytes_in,
                frames_out: telemetry.frames_out,
                bytes_out: telemetry.bytes_out,
                over_budget: telemetry.over_budget,
                dropped: telemetry.dropped,
            });
            by_session.insert(
                telemetry.session.clone(),
                PlayerNetCounters {
                    network_in_bytes: telemetry.bytes_in,
                    network_out_bytes: telemetry.bytes_out,
                    frames_decoded: telemetry.frames_in,
                    frames_encoded: telemetry.frames_out,
                    packets_dropped_total: telemetry.dropped_total(),
                    outbound_queue_len: usize::try_from(telemetry.queue_depth)
                        .unwrap_or(usize::MAX),
                },
            );

            merge_tally(&mut inbound_merge, &telemetry.inbound);
            merge_tally(&mut outbound_merge, &telemetry.outbound);
        }

        NetTelemetryParts {
            per_player,
            by_session,
            inbound: top_packets(inbound_merge, top_n),
            outbound: top_packets(outbound_merge, top_n),
        }
    }
}

/// Maps a [`PacketState`] onto a stable, ordered index so a merge map can key on
/// `(state, packet)` deterministically without requiring `PacketState: Ord`.
fn state_index(state: PacketState) -> u8 {
    match state {
        PacketState::Handshaking => 0,
        PacketState::Status => 1,
        PacketState::Login => 2,
        PacketState::Configuration => 3,
        PacketState::Play => 4,
    }
}

/// Reconstructs the [`PacketState`] from its [`state_index`].
fn state_from_index(index: u8) -> PacketState {
    match index {
        0 => PacketState::Handshaking,
        1 => PacketState::Status,
        2 => PacketState::Login,
        3 => PacketState::Configuration,
        // Index 4 (and any unreachable other) maps to Play, the only remaining
        // state — the index always comes from `state_index`, so this never lies.
        _ => PacketState::Play,
    }
}

/// Folds a session's `tally` into the per-direction merge map (saturating).
fn merge_tally(merge: &mut BTreeMap<(u8, &'static str), u64>, tally: &PacketTally) {
    for (state, packet, count) in tally.entries() {
        let slot = merge.entry((state_index(state), packet)).or_insert(0);
        *slot = slot.saturating_add(count);
    }
}

/// Sorts a merged `(state, packet) -> count` map into a bounded top-`top_n`
/// [`PacketTraceSummary`], deterministically (count desc, then state, then name).
fn top_packets(merge: BTreeMap<(u8, &'static str), u64>, top_n: usize) -> PacketTraceSummary {
    let mut rows: Vec<((u8, &'static str), u64)> = merge.into_iter().collect();
    rows.sort_by(|a, b| {
        b.1.cmp(&a.1) // count, descending
            .then_with(|| a.0 .0.cmp(&b.0 .0)) // then state index, ascending
            .then_with(|| a.0 .1.cmp(b.0 .1)) // then packet name, ascending
    });
    rows.truncate(top_n);
    let top_packets = rows
        .into_iter()
        .map(|((state_idx, packet), count)| PacketFrequency {
            packet_name: packet.to_string(),
            state: packet_state_label(state_from_index(state_idx)),
            count: usize::try_from(count).unwrap_or(usize::MAX),
        })
        .collect();
    PacketTraceSummary { top_packets }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn telemetry(session: &str) -> ConnNetTelemetry {
        ConnNetTelemetry {
            session: session.to_string(),
            ..ConnNetTelemetry::default()
        }
    }

    #[test]
    fn tally_increments_and_caps_overflow() {
        let mut tally = PacketTally::new();
        tally.record(PacketState::Play, "set_player_position");
        tally.record(PacketState::Play, "set_player_position");
        tally.record(PacketState::Play, "player_action");
        let counts: BTreeMap<&'static str, u64> = tally
            .entries()
            .map(|(_, name, count)| (name, count))
            .collect();
        assert_eq!(counts["set_player_position"], 2);
        assert_eq!(counts["player_action"], 1);
        assert_eq!(tally.overflow(), 0);
    }

    #[test]
    fn tally_folds_distinct_keys_beyond_capacity_into_overflow() {
        let mut tally = PacketTally::new();
        // Leak distinct &'static labels (test-only) past the cap.
        for i in 0..(TALLY_CAPACITY + 5) {
            let label: &'static str = Box::leak(format!("packet_{i}").into_boxed_str());
            tally.record(PacketState::Play, label);
        }
        assert_eq!(tally.entries().count(), TALLY_CAPACITY);
        assert_eq!(tally.overflow(), 5);
    }

    #[test]
    fn hub_publish_overwrites_same_session_and_prunes() {
        let hub = NetTelemetryHub::new();
        let mut first = telemetry("Steve");
        first.bytes_in = 10;
        hub.publish(first);
        let mut second = telemetry("Steve");
        second.bytes_in = 99;
        hub.publish(second);
        assert_eq!(hub.len(), 1, "same session overwrites, never duplicates");

        hub.publish(telemetry("Alex"));
        assert_eq!(hub.len(), 2);

        let keep: BTreeSet<String> = ["Steve".to_string()].into_iter().collect();
        hub.retain_sessions(&keep);
        assert_eq!(hub.len(), 1, "disconnected sessions are pruned");

        let parts = hub.aggregate(DEFAULT_TOP_N);
        assert_eq!(parts.by_session["Steve"].network_in_bytes, 99);
    }

    #[test]
    fn hub_refuses_new_sessions_past_capacity() {
        let hub = NetTelemetryHub::new();
        for i in 0..HUB_CAPACITY {
            hub.publish(telemetry(&format!("p{i}")));
        }
        assert_eq!(hub.len(), HUB_CAPACITY);
        hub.publish(telemetry("one_too_many"));
        assert_eq!(
            hub.len(),
            HUB_CAPACITY,
            "new session past the cap is refused"
        );
        // An existing session can still update.
        let mut update = telemetry("p0");
        update.bytes_in = 5;
        hub.publish(update);
        assert_eq!(hub.len(), HUB_CAPACITY);
    }

    #[test]
    fn aggregate_merges_tallies_into_deterministic_top_n() {
        let hub = NetTelemetryHub::new();

        let mut a = telemetry("A");
        a.frames_in = 3;
        a.bytes_in = 30;
        a.frames_out = 7;
        a.bytes_out = 70;
        a.dropped = [1, 0, 2, 0];
        a.queue_depth = 4;
        a.inbound.record(PacketState::Play, "set_player_position");
        a.inbound.record(PacketState::Play, "set_player_position");
        a.inbound.record(PacketState::Play, "player_action");
        a.outbound.record(PacketState::Play, "chunk_data_and_light");
        hub.publish(a);

        let mut b = telemetry("B");
        b.inbound.record(PacketState::Play, "set_player_position");
        b.inbound.record(PacketState::Login, "login_start");
        hub.publish(b);

        let parts = hub.aggregate(2);

        // Per-player metrics survive the fold.
        assert_eq!(parts.per_player.len(), 2);
        let counters = &parts.by_session["A"];
        assert_eq!(counters.network_in_bytes, 30);
        assert_eq!(counters.network_out_bytes, 70);
        assert_eq!(counters.frames_decoded, 3);
        assert_eq!(counters.frames_encoded, 7);
        assert_eq!(counters.packets_dropped_total, 3);
        assert_eq!(counters.outbound_queue_len, 4);

        // Inbound top-N is merged across sessions and ordered by count desc.
        assert_eq!(parts.inbound.top_packets.len(), 2, "truncated to top_n");
        assert_eq!(
            parts.inbound.top_packets[0].packet_name,
            "set_player_position"
        );
        assert_eq!(parts.inbound.top_packets[0].count, 3); // 2 (A) + 1 (B)
        assert_eq!(parts.inbound.top_packets[0].state, "play");
        // The two count-1 keys are tie-broken deterministically: Login (index 2)
        // sorts before Play (index 4), so `login_start` takes the second slot.
        assert_eq!(parts.inbound.top_packets[1].packet_name, "login_start");
        assert_eq!(parts.inbound.top_packets[1].count, 1);

        assert_eq!(parts.outbound.top_packets.len(), 1);
        assert_eq!(
            parts.outbound.top_packets[0].packet_name,
            "chunk_data_and_light"
        );
    }

    #[test]
    fn aggregate_of_empty_hub_is_empty() {
        let hub = NetTelemetryHub::new();
        let parts = hub.aggregate(DEFAULT_TOP_N);
        assert!(parts.per_player.is_empty());
        assert!(parts.by_session.is_empty());
        assert!(parts.inbound.top_packets.is_empty());
        assert!(parts.outbound.top_packets.is_empty());
    }
}
