//! The axum router, the read-only method guard, and the static SPA service.
//!
//! The router carries the [`SnapshotPublisher`] as shared state and exposes the
//! read-only data endpoints ([`api::snapshot`], [`api::events`], and the Prometheus
//! [`prometheus::metrics`] exporter) plus a [`ServeDir`] static service for the
//! built single-page app. A [`from_fn`](axum::middleware::from_fn) layer rejects
//! any method other than `GET`/`HEAD` with `405 Method Not Allowed`, so the
//! dashboard stays read-only by construction: there is no route that mutates server
//! state, and non-read methods never reach a handler or the file service.

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use ferrumc_observability::SnapshotPublisher;
use tower_http::services::{ServeDir, ServeFile};

use crate::{api, prometheus};

/// Absolute path to the committed SPA build output (`dist/`), resolved at compile
/// time from the crate root so the binary serves the right directory regardless of
/// the working directory it is launched from.
const DIST_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/dist");

/// Builds the dashboard router over `snapshots`.
///
/// Routes:
/// - `GET /api/snapshot` — one-shot JSON snapshot.
/// - `GET /events` — Server-Sent Events live snapshot stream.
/// - `GET /metrics` — the latest snapshot in Prometheus text exposition format.
/// - everything else — the static SPA from `dist/`, with `index.html` as the
///   not-found fallback so client-side navigation survives a hard refresh.
///
/// Every route is a `GET`; the method-guard layer turns any other verb into a
/// `405` before a handler or the file service runs.
pub fn router(snapshots: SnapshotPublisher) -> Router {
    // Serve built assets; fall back to the SPA shell so a refresh on any client
    // route still loads the app rather than 404ing.
    let spa = ServeDir::new(DIST_DIR)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(format!("{DIST_DIR}/index.html")));

    Router::new()
        .route("/api/snapshot", get(api::snapshot))
        .route("/events", get(api::events))
        .route("/metrics", get(prometheus::metrics))
        .fallback_service(spa)
        .layer(middleware::from_fn(reject_non_get))
        .with_state(snapshots)
}

/// Rejects any method other than `GET`/`HEAD` with `405`, enforcing the
/// dashboard's read-only contract across both the data endpoints and the static
/// file service.
async fn reject_non_get(request: Request, next: Next) -> Response {
    if *request.method() == Method::GET || *request.method() == Method::HEAD {
        next.run(request).await
    } else {
        (
            StatusCode::METHOD_NOT_ALLOWED,
            "read-only dashboard: GET only",
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{to_bytes, Body};
    use axum::http::header::CONTENT_TYPE;
    use axum::http::Request as HttpRequest;
    use ferrumc_observability::{
        CounterRegistry, PlayerSnapshot, ServerSnapshotParts, SnapshotPublisher,
    };
    use tower::ServiceExt;

    fn publisher_with_player() -> SnapshotPublisher {
        let registry = CounterRegistry::new();
        let parts = ServerSnapshotParts {
            build: "ferrumc test".to_string(),
            players_online: 1,
            players: vec![PlayerSnapshot {
                name: "Notch".to_string(),
                gamemode: "creative".to_string(),
                ..PlayerSnapshot::default()
            }],
            ..ServerSnapshotParts::default()
        };
        let publisher = SnapshotPublisher::default();
        publisher.publish(registry.server_snapshot(parts));
        publisher
    }

    async fn body_string(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        String::from_utf8(bytes.to_vec()).expect("utf8 body")
    }

    #[tokio::test]
    async fn snapshot_endpoint_returns_json() {
        let app = router(publisher_with_player());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/api/snapshot")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(content_type.starts_with("application/json"));
        let body = body_string(response).await;
        assert!(body.contains("\"build\":\"ferrumc test\""));
        assert!(body.contains("Notch"));
    }

    #[tokio::test]
    async fn events_endpoint_is_an_sse_stream() {
        let app = router(publisher_with_player());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/events")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(content_type.starts_with("text/event-stream"));
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_prometheus_text() {
        let app = router(publisher_with_player());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        // The exposition `Content-Type` (with the `version=0.0.4` parameter) is what
        // makes Prometheus pick the text parser.
        assert!(content_type.starts_with("text/plain"));
        assert!(content_type.contains("version=0.0.4"));
        let body = body_string(response).await;
        assert!(body.contains("# HELP ferrumc_tps"));
        assert!(body.contains("# TYPE ferrumc_players_online gauge"));
        assert!(body.contains("ferrumc_players_online 1\n"));
    }

    #[tokio::test]
    async fn metrics_endpoint_rejects_post_with_405() {
        // `/metrics` rides the same read-only method guard as every other route.
        let app = router(publisher_with_player());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn post_is_rejected_with_405() {
        let app = router(publisher_with_player());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/api/snapshot")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn unknown_path_falls_back_to_the_spa_shell() {
        // A deep client-route path is not a file on disk; ServeDir's fallback must
        // serve the committed index.html so a hard refresh still boots the app.
        let app = router(publisher_with_player());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/some/client/route")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_string(response).await;
        assert!(body.contains("<!doctype html>") || body.contains("<!DOCTYPE html>"));
    }
}
