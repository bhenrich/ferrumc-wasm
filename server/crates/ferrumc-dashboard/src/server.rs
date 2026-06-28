//! The axum router and the read-only method guard.
//!
//! The router carries the [`SnapshotPublisher`] as shared state and exposes one
//! `GET` route per page. A [`from_fn`](axum::middleware::from_fn) layer rejects
//! any method other than `GET`/`HEAD` with `405 Method Not Allowed`, so the
//! dashboard is read-only by construction: there is no route that mutates server
//! state, and non-read methods never reach a handler.

use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use ferrumc_observability::SnapshotPublisher;

use crate::pages;

/// Builds the dashboard router over `snapshots`.
///
/// Every route is a `GET`; the method-guard layer turns any other verb into a
/// `405` before a handler runs.
pub fn router(snapshots: SnapshotPublisher) -> Router {
    Router::new()
        .route("/", get(pages::overview))
        .route("/players", get(pages::players))
        .route("/world", get(pages::world))
        .route("/packet-trace", get(pages::packet_trace))
        .route("/backpressure", get(pages::backpressure))
        .route("/plugins", get(pages::plugins))
        .route("/persistence", get(pages::persistence))
        .route("/checklist", get(pages::checklist))
        .layer(middleware::from_fn(reject_non_get))
        .with_state(snapshots)
}

/// Rejects any method other than `GET`/`HEAD` with `405`, enforcing the
/// dashboard's read-only contract.
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
    async fn get_overview_returns_200() {
        let app = router(publisher_with_player());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_string(response).await;
        assert!(body.contains("Overview"));
        assert!(body.contains("FerrumC"));
    }

    #[tokio::test]
    async fn get_players_lists_the_player() {
        let app = router(publisher_with_player());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/players")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_string(response).await;
        assert!(body.contains("Notch"));
    }

    #[tokio::test]
    async fn partial_request_omits_the_chrome() {
        let app = router(publisher_with_player());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::GET)
                    .uri("/?partial=1")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_string(response).await;
        // The fragment carries the page body but not the surrounding document.
        assert!(body.contains("Overview"));
        assert!(!body.contains("<!doctype html>"));
    }

    #[tokio::test]
    async fn post_is_rejected_with_405() {
        let app = router(publisher_with_player());
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
