//! Per-packet traces and the per-connection rolling trace holder.
//!
//! The acceptance target is to be able to dump the last 256 inbound and 512
//! outbound packet traces on a disconnect or a decode error, each carrying the
//! packet name, size, connection state, compression flag, and the server tick it
//! was seen on. [`SessionDebug`] keeps those two fixed-capacity rings and
//! [`SessionDebug::dump`] renders them as one structured tracing event plus JSON.

use ferrumc_core::Tick;
use serde::Serialize;

use crate::net_telemetry::PacketTally;
use crate::ring::RingBuffer;

/// Capacity of the inbound trace ring (the acceptance number).
const INBOUND_CAPACITY: usize = 256;

/// Capacity of the outbound trace ring (the acceptance number).
const OUTBOUND_CAPACITY: usize = 512;

/// The travel direction of a traced packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// A serverbound packet (client -> server).
    Inbound,
    /// A clientbound packet (server -> client).
    Outbound,
}

/// The connection state a packet was observed in.
///
/// This is an observability-local copy of the networking state machine so this
/// crate never has to depend on `ferrumc-net` or `ferrumc-proto`. Callers map
/// their own state type onto it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PacketState {
    /// Initial handshake.
    Handshaking,
    /// Server-list status ping.
    Status,
    /// Login negotiation.
    Login,
    /// The 1.20.2+ configuration phase.
    Configuration,
    /// In-game play.
    Play,
}

/// One traced packet: exactly the fields the acceptance dump must carry.
///
/// Fields are public because this is an inert serialization DTO whose entire
/// purpose is to be read back out of a dump (as a tracing field or JSON); it
/// carries no invariants to protect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PacketTrace {
    /// Whether the packet was inbound or outbound.
    pub direction: Direction,
    /// The connection state it was seen in.
    pub state: PacketState,
    /// The wire packet id.
    pub packet_id: i32,
    /// A human-readable packet name (a `&'static str` supplied by the caller, so
    /// the trace stays allocation-free on the hot path).
    pub packet_name: &'static str,
    /// The packet size in bytes (see the crate-level docs for which size: an
    /// exact body length where available, `0` where the decoder does not surface
    /// it).
    pub size: usize,
    /// Whether the connection had compression negotiated when the packet was
    /// recorded (connection-level, not a per-frame "this frame was deflated" bit).
    pub compressed: bool,
    /// The last server tick the connection observed when the packet was recorded.
    pub tick: Tick,
}

impl PacketTrace {
    /// Builds an inbound (serverbound) trace.
    #[must_use]
    pub fn inbound(
        state: PacketState,
        packet_id: i32,
        packet_name: &'static str,
        size: usize,
        compressed: bool,
        tick: Tick,
    ) -> Self {
        Self {
            direction: Direction::Inbound,
            state,
            packet_id,
            packet_name,
            size,
            compressed,
            tick,
        }
    }

    /// Builds an outbound (clientbound) trace.
    #[must_use]
    pub fn outbound(
        state: PacketState,
        packet_id: i32,
        packet_name: &'static str,
        size: usize,
        compressed: bool,
        tick: Tick,
    ) -> Self {
        Self {
            direction: Direction::Outbound,
            state,
            packet_id,
            packet_name,
            size,
            compressed,
            tick,
        }
    }
}

/// A per-connection rolling holder of the most recent packet traces.
///
/// Holds a fixed 256-deep inbound ring and a fixed 512-deep outbound ring (the
/// acceptance capacities). Both are const-generic [`RingBuffer`]s, so the holder
/// never grows: once a ring is full the oldest trace is evicted. The struct is
/// moderately large (it embeds both backing arrays inline); callers that keep one
/// alive for a whole connection should box it so it does not bloat an async task
/// frame.
#[derive(Debug)]
pub struct SessionDebug {
    /// The session label: a peer address at accept time, upgraded to the player
    /// name once login completes.
    session: String,
    /// The most recent inbound traces, oldest to newest.
    inbound: RingBuffer<PacketTrace, INBOUND_CAPACITY>,
    /// The most recent outbound traces, oldest to newest.
    outbound: RingBuffer<PacketTrace, OUTBOUND_CAPACITY>,
    /// The last sampled outbound queue depth, surfaced in the dump.
    outbound_queue_len: usize,
    /// Cumulative count of inbound traces recorded (the connection's
    /// frames-decoded total, unbounded by the ring's eviction window).
    inbound_frames: u64,
    /// Cumulative sum of inbound trace sizes in bytes (the connection's
    /// bytes-in total; `0`-sized login traces contribute nothing).
    inbound_bytes: u64,
    /// Bounded `(state, packet)` frequency tally of every inbound trace, for the
    /// server-wide live summary (distinct from the eviction-bounded ring above).
    inbound_tally: PacketTally,
    /// Bounded `(state, packet)` frequency tally of every outbound trace.
    outbound_tally: PacketTally,
}

impl SessionDebug {
    /// Creates an empty holder labelled `session` (typically the peer address).
    #[must_use]
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            inbound: RingBuffer::new(),
            outbound: RingBuffer::new(),
            outbound_queue_len: 0,
            inbound_frames: 0,
            inbound_bytes: 0,
            inbound_tally: PacketTally::new(),
            outbound_tally: PacketTally::new(),
        }
    }

    /// Replaces the session label (for example upgrading a peer address to the
    /// player name once login completes).
    pub fn set_session(&mut self, label: impl Into<String>) {
        self.session = label.into();
    }

    /// Records one inbound trace, evicting the oldest if the ring is full.
    ///
    /// Also folds the trace into the cumulative inbound frame/byte counters and
    /// the inbound frequency tally (both unbounded by the ring's eviction
    /// window), so a connection's live network telemetry reflects its whole
    /// lifetime, not just the last [`INBOUND_CAPACITY`] packets.
    pub fn record_inbound(&mut self, trace: PacketTrace) {
        self.inbound_frames = self.inbound_frames.saturating_add(1);
        self.inbound_bytes = self.inbound_bytes.saturating_add(trace.size as u64);
        self.inbound_tally.record(trace.state, trace.packet_name);
        self.inbound.push(trace);
    }

    /// Records one outbound trace, evicting the oldest if the ring is full, and
    /// folds it into the outbound frequency tally for the live summary.
    pub fn record_outbound(&mut self, trace: PacketTrace) {
        self.outbound_tally.record(trace.state, trace.packet_name);
        self.outbound.push(trace);
    }

    /// The session label (peer address, or player name once login completes).
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Cumulative inbound frames recorded (the connection's frames-decoded total).
    #[must_use]
    pub fn inbound_frames(&self) -> u64 {
        self.inbound_frames
    }

    /// Cumulative inbound bytes recorded (the connection's bytes-in total).
    #[must_use]
    pub fn inbound_bytes(&self) -> u64 {
        self.inbound_bytes
    }

    /// The inbound `(state, packet)` frequency tally for the live summary.
    #[must_use]
    pub fn inbound_tally(&self) -> &PacketTally {
        &self.inbound_tally
    }

    /// The outbound `(state, packet)` frequency tally for the live summary.
    #[must_use]
    pub fn outbound_tally(&self) -> &PacketTally {
        &self.outbound_tally
    }

    /// Samples the current outbound queue depth for the next dump.
    pub fn observe_outbound_queue_len(&mut self, depth: usize) {
        self.outbound_queue_len = depth;
    }

    /// Builds an owned, serializable snapshot tagged with `reason`.
    #[must_use]
    pub fn snapshot(&self, reason: impl Into<String>) -> SessionDebugSnapshot {
        SessionDebugSnapshot {
            session: self.session.clone(),
            reason: reason.into(),
            inbound: self.inbound.to_vec(),
            outbound: self.outbound.to_vec(),
            outbound_queue_len: self.outbound_queue_len,
        }
    }

    /// Dumps the retained traces: one structured `warn` event carrying the JSON
    /// snapshot, targeted at `ferrumc::observability::session`.
    ///
    /// Emitted at `warn` so it survives a default `info` log filter on the
    /// disconnect / decode-error paths it is meant to capture. Never panics:
    /// a serialization failure falls back to a short error JSON string.
    pub fn dump(&self, reason: &str) {
        let snapshot = self.snapshot(reason);
        let json = serde_json::to_string(&snapshot).unwrap_or_else(|err| {
            format!("{{\"error\":\"failed to serialize session dump: {err}\"}}")
        });
        tracing::warn!(
            target: "ferrumc::observability::session",
            session = %snapshot.session,
            reason = %snapshot.reason,
            inbound_len = snapshot.inbound.len(),
            outbound_len = snapshot.outbound.len(),
            outbound_queue_len = snapshot.outbound_queue_len,
            json = %json,
            "session packet dump"
        );
    }
}

/// An owned, serializable snapshot of a [`SessionDebug`].
///
/// Fields are public for the same reason as [`PacketTrace`]: this is an inert
/// snapshot DTO meant to be serialized and read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionDebugSnapshot {
    /// The session label (peer address or player name).
    pub session: String,
    /// Why the dump was taken (for example `"disconnect"`).
    pub reason: String,
    /// The retained inbound traces, oldest to newest (at most 256).
    pub inbound: Vec<PacketTrace>,
    /// The retained outbound traces, oldest to newest (at most 512).
    pub outbound: Vec<PacketTrace>,
    /// The last sampled outbound queue depth.
    pub outbound_queue_len: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(id: i32, dir: Direction) -> PacketTrace {
        PacketTrace {
            direction: dir,
            state: PacketState::Play,
            packet_id: id,
            packet_name: "test_packet",
            size: 7,
            compressed: false,
            tick: Tick::new(id as u64),
        }
    }

    #[test]
    fn enums_serialize_lowercase() {
        assert_eq!(
            serde_json::to_string(&Direction::Inbound).unwrap(),
            "\"inbound\""
        );
        assert_eq!(
            serde_json::to_string(&PacketState::Configuration).unwrap(),
            "\"configuration\""
        );
    }

    #[test]
    fn packet_trace_round_trips_fields() {
        let json = serde_json::to_value(trace(3, Direction::Inbound)).unwrap();
        assert_eq!(json["direction"], "inbound");
        assert_eq!(json["state"], "play");
        assert_eq!(json["packet_id"], 3);
        assert_eq!(json["packet_name"], "test_packet");
        assert_eq!(json["size"], 7);
        assert_eq!(json["compressed"], false);
        assert_eq!(json["tick"], 3);
    }

    #[test]
    fn snapshot_keeps_only_the_newest_within_capacity() {
        let mut debug = SessionDebug::new("127.0.0.1:1234");
        // Push well past both ring capacities.
        for id in 0..1_000 {
            debug.record_inbound(trace(id, Direction::Inbound));
            debug.record_outbound(trace(id, Direction::Outbound));
            debug.record_outbound(trace(id, Direction::Outbound));
        }
        debug.observe_outbound_queue_len(42);
        let snap = debug.snapshot("disconnect");

        assert_eq!(snap.inbound.len(), 256);
        assert_eq!(snap.outbound.len(), 512);
        assert_eq!(snap.outbound_queue_len, 42);
        // Oldest retained -> newest, evicting everything older.
        assert_eq!(snap.inbound.first().unwrap().packet_id, 1_000 - 256);
        assert_eq!(snap.inbound.last().unwrap().packet_id, 999);
        assert_eq!(snap.reason, "disconnect");
    }

    #[test]
    fn set_session_upgrades_the_label() {
        let mut debug = SessionDebug::new("127.0.0.1:1234");
        debug.set_session("Notch");
        assert_eq!(debug.snapshot("x").session, "Notch");
    }

    #[test]
    fn snapshot_serializes_to_valid_parseable_json() {
        let mut debug = SessionDebug::new("peer");
        debug.record_inbound(trace(1, Direction::Inbound));
        debug.record_outbound(trace(2, Direction::Outbound));
        debug.observe_outbound_queue_len(3);

        let json = serde_json::to_string(&debug.snapshot("decode_error")).unwrap();
        // Parse it back: the dump path must always produce valid JSON.
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["session"], "peer");
        assert_eq!(parsed["reason"], "decode_error");
        assert_eq!(parsed["inbound"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["outbound"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["outbound_queue_len"], 3);
    }
}
