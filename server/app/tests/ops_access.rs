//! End-to-end access-control tests for the beta-gate ops features.
//!
//! Each test starts the real server on an ephemeral port and drives real
//! offline-login clients against it to assert the configured access rules:
//!
//! - the whitelist allows listed players and kicks everyone else,
//! - a banned player name is rejected at login while others join, and
//! - the per-IP connection limit drops a connection over the cap (all clients
//!   here share the loopback source IP).
//!
//! A denied login surfaces as an error from [`login_to_play`]: the server sends a
//! Login Disconnect (or, for the per-IP gate, simply drops the socket) instead of
//! a Login Success, so the client never reaches play and its frame stream ends.

mod common;

use std::time::Duration;

use tokio::time::timeout;

use ferrumc_app::AppConfig;

use common::{login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

#[tokio::test]
async fn whitelist_allows_listed_and_denies_others() {
    // Whitelist on, only "Saad" listed. radius-1 spawn keeps the join payload small.
    let config = AppConfig::from_toml_str(
        "bind = \"127.0.0.1:0\"\n\
         spawn_chunk_radius = 1\n\
         [access]\n\
         whitelist_enabled = true\n\
         whitelist = [\"Saad\"]\n",
    )
    .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    // The listed player reaches play.
    let allowed = timeout(GUARD, login_to_play(addr, "Saad"))
        .await
        .expect("whitelisted login finished within the guard");
    assert!(allowed.is_ok(), "whitelisted player must reach play");

    // A non-whitelisted player is kicked before play (server sends Login Disconnect
    // and closes, so the login flow errors).
    let denied = timeout(GUARD, login_to_play(addr, "Intruder"))
        .await
        .expect("denied login finished within the guard");
    assert!(
        denied.is_err(),
        "a non-whitelisted player must be rejected at login"
    );

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the guard")
        .expect("clean shutdown");
}

#[tokio::test]
async fn banned_name_is_denied_while_others_join() {
    // No whitelist; "Griefer" is banned. The ban is enforced regardless of the
    // (disabled) whitelist.
    let config = AppConfig::from_toml_str(
        "bind = \"127.0.0.1:0\"\n\
         spawn_chunk_radius = 1\n\
         [access]\n\
         bans = [\"Griefer\"]\n",
    )
    .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    let banned = timeout(GUARD, login_to_play(addr, "Griefer"))
        .await
        .expect("banned login finished within the guard");
    assert!(banned.is_err(), "a banned name must be rejected at login");

    let allowed = timeout(GUARD, login_to_play(addr, "Saad"))
        .await
        .expect("non-banned login finished within the guard");
    assert!(allowed.is_ok(), "a non-banned player must reach play");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the guard")
        .expect("clean shutdown");
}

#[tokio::test]
async fn per_ip_limit_rejects_excess_connections() {
    // Cap concurrent connections per IP at 2. All clients here are loopback, so
    // they share one source IP and contend for the same per-IP budget.
    let config = AppConfig::from_toml_str(
        "bind = \"127.0.0.1:0\"\n\
         spawn_chunk_radius = 1\n\
         [access]\n\
         per_ip_connection_limit = 2\n",
    )
    .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    // Two concurrent connections fill the per-IP budget; keep both alive. `_c2` is
    // held only to keep its connection open (never referenced again).
    let c1 = timeout(GUARD, login_to_play(addr, "Saad"))
        .await
        .expect("first login finished within the guard")
        .expect("first connection joins");
    let _c2 = timeout(GUARD, login_to_play(addr, "Notch"))
        .await
        .expect("second login finished within the guard")
        .expect("second connection joins");

    // The third connection from the same IP is dropped by the accept loop before
    // it can log in: the socket connects at TCP level, then the server closes it,
    // so the login flow errors.
    let third = timeout(GUARD, login_to_play(addr, "Trudy"))
        .await
        .expect("third login attempt finished within the guard");
    assert!(
        third.is_err(),
        "a third concurrent connection from one IP must be rejected"
    );

    // Once a slot frees, a new connection from the same IP is admitted again.
    drop(c1);
    let readmitted = timeout(GUARD, reconnect_until_admitted(addr, "Saad"))
        .await
        .expect("re-admission finished within the guard");
    assert!(
        readmitted.is_ok(),
        "a freed per-IP slot must admit a new connection"
    );

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the guard")
        .expect("clean shutdown");
}

/// Retries [`login_to_play`] until it succeeds, to bridge the tiny window between
/// the dropped connection's server-side task ending (which frees its per-IP slot)
/// and the retry. Bounded by the caller's timeout guard.
async fn reconnect_until_admitted(
    addr: std::net::SocketAddr,
    name: &str,
) -> anyhow::Result<TestClient> {
    loop {
        if let Ok(client) = login_to_play(addr, name).await {
            return Ok(client);
        }
        tokio::task::yield_now().await;
    }
}
