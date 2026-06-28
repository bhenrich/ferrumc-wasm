#![forbid(unsafe_code)]

//! Read-only localhost observability dashboard (axum + htmx).
//!
//! The dashboard renders the latest [`ServerSnapshot`] published by the
//! application driver through a [`SnapshotPublisher`]. It is read-only by
//! construction: every route is a `GET`, a method-guard layer rejects anything
//! else with `405`, and the server is bound to a caller-supplied address (the app
//! defaults that to `127.0.0.1`). Pages are server-rendered HTML built inline with
//! `format!`/string literals; htmx is loaded from a CDN to refresh each page's
//! content region once a second. There is no frontend build and no static asset
//! directory.
//!
//! [`ServerSnapshot`]: ferrumc_observability::ServerSnapshot
//! [`SnapshotPublisher`]: ferrumc_observability::SnapshotPublisher

mod pages;
mod server;

use std::net::SocketAddr;

use ferrumc_observability::SnapshotPublisher;

pub use server::router;

/// Binds the dashboard on `bind` and serves it until the listener errors or the
/// hosting task is dropped.
///
/// The caller (the app) decides the bind address and defaults it to a loopback
/// address, so the dashboard is not exposed off-host by default. `snapshots` is a
/// read handle onto the driver's published [`ServerSnapshot`]; the server only
/// ever reads from it.
///
/// # Errors
///
/// Returns an error if the address cannot be bound or the accept loop fails.
pub async fn run(bind: SocketAddr, snapshots: SnapshotPublisher) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, router(snapshots)).await?;
    Ok(())
}
