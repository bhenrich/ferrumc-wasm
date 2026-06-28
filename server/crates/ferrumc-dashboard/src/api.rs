//! The read-only data endpoints the SPA consumes.
//!
//! Two `GET` routes, both reading the latest [`ServerSnapshot`] out of the shared
//! [`SnapshotPublisher`] and never mutating anything:
//!
//! - [`snapshot`] (`GET /api/snapshot`) returns the current snapshot once, as
//!   JSON. The SPA calls it on load to seed itself and as the polling fallback
//!   when `EventSource` is unavailable.
//! - [`events`] (`GET /events`) is a Server-Sent Events stream that re-emits the
//!   latest snapshot at a fixed cadence so the dashboard's gauges and charts stay
//!   live. Native `EventSource` reconnects on its own, so there is no server-side
//!   reconnect bookkeeping.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use ferrumc_observability::{ServerSnapshot, SnapshotPublisher};
use futures_util::stream::{self, Stream};

/// How often the SSE stream re-publishes the latest snapshot (~10 Hz). The sim
/// driver republishes once per 20 TPS tick (50 ms), so 100 ms is a touch slower
/// than the source and never starves a frame while keeping the localhost socket
/// quiet.
const SSE_INTERVAL: Duration = Duration::from_millis(100);

/// `GET /api/snapshot` — the current [`ServerSnapshot`] as a one-shot JSON body.
///
/// Clones only the `Arc` (not the snapshot) out of the publisher, then serializes
/// it. Read-only: it never writes through the handle.
pub async fn snapshot(State(publisher): State<SnapshotPublisher>) -> Json<ServerSnapshot> {
    Json((*publisher.latest()).clone())
}

/// `GET /events` — a Server-Sent Events stream of the latest [`ServerSnapshot`].
///
/// Emits a serialized snapshot every [`SSE_INTERVAL`]; the first tick fires
/// immediately, so a freshly connected client paints without waiting a frame. A
/// serialization failure degrades to an SSE comment rather than tearing the
/// stream down. The stream itself is read-only — it owns a clone of the publisher
/// and only ever reads from it.
pub async fn events(
    State(publisher): State<SnapshotPublisher>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let ticker = tokio::time::interval(SSE_INTERVAL);
    // Carry the interval in the unfold state so cadence is preserved across
    // iterations instead of being reset each step.
    let stream = stream::unfold((publisher, ticker), |(publisher, mut ticker)| async move {
        ticker.tick().await;
        let snap = publisher.latest();
        let event = match serde_json::to_string(&*snap) {
            Ok(json) => Event::default().data(json),
            // Never kill the live stream over one bad frame; tell the client and
            // try again on the next tick.
            Err(_) => Event::default().comment("snapshot serialization failed"),
        };
        Some((Ok(event), (publisher, ticker)))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::extract::State;
    use ferrumc_observability::{ServerSnapshot, SnapshotPublisher};

    fn publisher_with_tick(tick: u64) -> SnapshotPublisher {
        let publisher = SnapshotPublisher::default();
        publisher.publish(ServerSnapshot {
            tick,
            build: "ferrumc test".to_string(),
            ..ServerSnapshot::default()
        });
        publisher
    }

    #[tokio::test]
    async fn snapshot_serializes_the_latest_tick() {
        let publisher = publisher_with_tick(42);
        let Json(body) = snapshot(State(publisher)).await;
        assert_eq!(body.tick, 42);
        assert_eq!(body.build, "ferrumc test");
    }

    #[tokio::test]
    async fn snapshot_is_a_clone_not_a_shared_handle() {
        // The returned JSON is a value snapshot: republishing after the read must
        // not retroactively change what was returned.
        let publisher = publisher_with_tick(1);
        let Json(body) = snapshot(State(publisher.clone())).await;
        publisher.publish(ServerSnapshot {
            tick: 999,
            ..ServerSnapshot::default()
        });
        assert_eq!(body.tick, 1);
    }
}
