//! End-to-end tests for the dump path: a [`SessionDebug`] and a
//! [`CounterRegistry`] must each emit a structured tracing event carrying a valid
//! JSON snapshot when dumped.
//!
//! These drive the *real* `dump()` methods through a capturing tracing
//! subscriber (rather than only the snapshot serialization the unit tests cover),
//! so they prove the acceptance behaviour: on a disconnect or decode error the
//! retained traces actually reach the log as parseable JSON.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrumc_core::{PluginId, Tick};
use ferrumc_observability::{
    CounterRegistry, Direction, MutationKind, MutationResult, PacketState, PacketTrace,
    PluginInvocationObservation, PluginMetricRecordOutcome, SessionDebug,
};
use tracing_subscriber::fmt::MakeWriter;

/// A `MakeWriter` that appends every formatted event into a shared buffer so a
/// test can read back what the subscriber rendered.
#[derive(Clone)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.0.lock().expect("capture buffer poisoned");
        guard.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CaptureWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Runs `body` with a capturing subscriber installed and returns everything it
/// logged as a single string.
fn capture_logs(body: impl FnOnce()) -> String {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(CaptureWriter(Arc::clone(&buf)))
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    let bytes = buf.lock().expect("capture buffer poisoned").clone();
    String::from_utf8(bytes).expect("log output is valid UTF-8")
}

/// Pulls the JSON object embedded in a captured `json=` tracing field back out so
/// the test can assert on its parsed structure.
fn extract_json(logs: &str) -> serde_json::Value {
    let start = logs.find('{').expect("a JSON object in the log line");
    let end = logs.rfind('}').expect("a closing brace in the log line");
    serde_json::from_str(&logs[start..=end]).expect("the dumped json field is valid JSON")
}

fn trace(id: i32) -> PacketTrace {
    PacketTrace {
        direction: Direction::Inbound,
        state: PacketState::Play,
        packet_id: id,
        packet_name: "set_player_position",
        size: 32,
        compressed: true,
        tick: Tick::new(id as u64),
    }
}

#[test]
fn session_dump_emits_a_tracing_event_with_valid_json() {
    let logs = capture_logs(|| {
        let mut debug = SessionDebug::new("127.0.0.1:5000");
        debug.set_session("Notch");
        // Push past the inbound capacity to prove only the newest survive.
        for id in 0..300 {
            debug.record_inbound(trace(id));
        }
        debug.record_outbound(trace(999));
        debug.observe_outbound_queue_len(11);
        debug.dump("disconnect");
    });

    assert!(
        logs.contains("session packet dump"),
        "missing dump message in: {logs}"
    );
    assert!(logs.contains("disconnect"), "missing reason in: {logs}");

    let json = extract_json(&logs);
    assert_eq!(json["session"], "Notch");
    assert_eq!(json["reason"], "disconnect");
    assert_eq!(json["outbound_queue_len"], 11);
    // Inbound ring caps at 256: the 300 pushed traces are evicted down to 256.
    let inbound = json["inbound"].as_array().expect("inbound array");
    assert_eq!(inbound.len(), 256);
    assert_eq!(inbound[0]["packet_id"], 300 - 256);
    assert_eq!(inbound.last().unwrap()["packet_id"], 299);
    assert_eq!(json["outbound"].as_array().unwrap().len(), 1);
}

#[test]
fn metrics_dump_emits_a_tracing_event_with_exact_metric_keys() {
    let logs = capture_logs(|| {
        let reg = CounterRegistry::new();
        reg.incr_chunk_sent(4);
        reg.incr_chunk_unloaded(1);
        reg.record_block_mutation(MutationKind::Break, MutationResult::Accepted);
        reg.record_packet_decode_error(PacketState::Play, "malformed_play");
        let plugin = PluginInvocationObservation::new(
            PluginId::new("fixture-dynamic"),
            Duration::from_micros(12),
        )
        .expect("bounded fixture plugin");
        assert_eq!(
            reg.record_plugin_invocation(plugin),
            PluginMetricRecordOutcome::Recorded
        );
        reg.dump();
    });

    assert!(
        logs.contains("metrics snapshot"),
        "missing metrics message in: {logs}"
    );

    let json = extract_json(&logs);
    assert_eq!(json["ferrumc_chunk_sent_total"], 4);
    assert_eq!(json["ferrumc_chunk_unloaded_total"], 1);
    assert_eq!(json["ferrumc_block_mutation_total"]["break"]["accepted"], 1);
    let decode = &json["ferrumc_packet_decode_error_total"]["entries"];
    assert_eq!(decode[0]["packet"], "malformed_play");
    assert_eq!(decode[0]["count"], 1);
    let plugin = &json["ferrumc_plugin_metrics"]["entries"][0];
    assert_eq!(plugin["plugin_id"], "fixture-dynamic");
    assert_eq!(plugin["invocation_count"], 1);
    assert_eq!(plugin["invocation_time_us_total"], 12);
}
