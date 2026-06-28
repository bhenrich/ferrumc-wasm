#![forbid(unsafe_code)]

//! Read-only localhost observability dashboard (axum + htmx).
//!
//! The dashboard renders the latest [`ServerSnapshot`] published by the
//! application driver through a [`SnapshotPublisher`]. It is read-only by
//! construction: every route is a `GET`, a method-guard layer rejects anything
//! else with `405`, and the server refuses to bind anything but a loopback
//! address (loopback is enforced at bind time by [`run`], so the ops console —
//! which exposes player names/positions and packet traces — is never reachable
//! off-host). Pages are server-rendered HTML built inline with `format!`/string
//! literals; htmx is loaded from a CDN to refresh each page's content region once
//! a second. There is no frontend build and no static asset directory.
//!
//! [`ServerSnapshot`]: ferrumc_observability::ServerSnapshot
//! [`SnapshotPublisher`]: ferrumc_observability::SnapshotPublisher

mod pages;
mod server;

use std::fmt;
use std::net::SocketAddr;

use ferrumc_observability::SnapshotPublisher;

pub use server::router;

/// An error from starting the read-only dashboard server.
///
/// A small dependency-free enum (no `anyhow`): each variant classifies the
/// failure so the caller can log it precisely.
#[derive(Debug)]
pub enum DashboardError {
    /// The caller requested a non-loopback bind address. The dashboard refuses
    /// it: it serves the ops console (player data, packet traces) and must never
    /// be reachable off-host. Carries the rejected address.
    NonLoopbackBind(SocketAddr),
    /// Binding the listener or running the accept loop failed.
    Bind(std::io::Error),
}

impl fmt::Display for DashboardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonLoopbackBind(addr) => write!(
                f,
                "refusing to bind the read-only dashboard to non-loopback address {addr}; \
                 only 127.0.0.1/::1 are allowed"
            ),
            Self::Bind(err) => write!(f, "dashboard listener failed: {err}"),
        }
    }
}

impl std::error::Error for DashboardError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind(err) => Some(err),
            Self::NonLoopbackBind(_) => None,
        }
    }
}

impl From<std::io::Error> for DashboardError {
    fn from(err: std::io::Error) -> Self {
        Self::Bind(err)
    }
}

/// Returns `bind` unchanged when it is a loopback address, otherwise
/// [`DashboardError::NonLoopbackBind`].
///
/// [`IpAddr::is_loopback`](std::net::IpAddr::is_loopback) covers both the IPv4
/// `127.0.0.0/8` block and the IPv6 `::1` address, which is exactly the
/// "only `127.0.0.1` / `::1`" policy the dashboard requires. Pure and side-effect
/// free so it is unit-testable without opening a socket.
fn ensure_loopback(bind: SocketAddr) -> Result<SocketAddr, DashboardError> {
    if bind.ip().is_loopback() {
        Ok(bind)
    } else {
        Err(DashboardError::NonLoopbackBind(bind))
    }
}

/// Binds the dashboard on `bind` and serves it until the listener errors or the
/// hosting task is dropped.
///
/// Loopback is enforced here, at bind time: a non-loopback `bind` (e.g.
/// `0.0.0.0` or any routable address) is **rejected** before any socket is
/// opened, so the read-only ops console — which exposes player names/positions
/// and packet traces — can never be served off-host. The rejection is logged and
/// returned as [`DashboardError::NonLoopbackBind`]; the operator must use a
/// loopback address (`127.0.0.1` is the app default). `snapshots` is a read
/// handle onto the driver's published [`ServerSnapshot`]; the server only ever
/// reads from it.
///
/// # Errors
///
/// - [`DashboardError::NonLoopbackBind`] if `bind` is not a loopback address.
/// - [`DashboardError::Bind`] if the address cannot be bound or the accept loop
///   fails.
pub async fn run(bind: SocketAddr, snapshots: SnapshotPublisher) -> Result<(), DashboardError> {
    let bind = ensure_loopback(bind).inspect_err(|_| {
        tracing::warn!(
            %bind,
            "refusing to bind the read-only ops dashboard to a non-loopback address; \
             it exposes player data — only 127.0.0.1/::1 are allowed"
        );
    })?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, router(snapshots)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(text: &str) -> SocketAddr {
        text.parse().expect("test address parses")
    }

    #[test]
    fn loopback_addresses_are_accepted() {
        // IPv4 127.0.0.1, the IPv6 ::1, and any address in the 127.0.0.0/8
        // loopback block are all allowed.
        for text in ["127.0.0.1:9090", "[::1]:9090", "127.0.0.5:9090"] {
            let bind = addr(text);
            assert_eq!(ensure_loopback(bind).expect("loopback is allowed"), bind);
        }
    }

    #[test]
    fn non_loopback_addresses_are_rejected() {
        // The any-address wildcards (which would expose the console off-host) and
        // a routable unicast address are all refused.
        for text in ["0.0.0.0:9090", "[::]:9090", "192.0.2.1:9090"] {
            let bind = addr(text);
            match ensure_loopback(bind) {
                Err(DashboardError::NonLoopbackBind(rejected)) => assert_eq!(rejected, bind),
                other => panic!("expected NonLoopbackBind for {text}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn run_rejects_a_non_loopback_bind_without_opening_a_socket() {
        // `0.0.0.0:0` is non-loopback; the guard runs before any bind, so `run`
        // returns the rejection rather than ever touching the network.
        let publisher = SnapshotPublisher::default();
        let err = run(addr("0.0.0.0:0"), publisher)
            .await
            .expect_err("a non-loopback bind is rejected");
        assert!(matches!(err, DashboardError::NonLoopbackBind(_)));
    }
}
