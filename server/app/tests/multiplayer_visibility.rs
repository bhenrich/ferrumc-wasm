//! End-to-end multiplayer-visibility test.
//!
//! Starts the real server on an ephemeral port and connects two real
//! [`tokio::net::TcpStream`] clients that log in offline and reach play. It then
//! asserts each client is told about the other — a `PlayerInfoUpdate` (player
//! list add) and a `SpawnEntity` (the other player's entity) — and that when one
//! client moves, the other receives the broadcast position.
//!
//! Determinism without wall-clock sleeps: the first client is driven through its
//! `JoinGame` before the second connects, which guarantees the server has
//! registered it, so the join the second client triggers always sees it. Every
//! step awaits the next frame and the whole flow is wrapped in a timeout guard.

#![allow(clippy::float_cmp)] // Broadcast coordinates are exact, representable values.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::timeout;
use uuid::Uuid;

use ferrumc_core::PlayerId;
use ferrumc_proto::generated::play::{ClientboundPlayPacket, PlayerInfoUpdate, SetPlayerPosition};
use ferrumc_session::PLAYER_INFO_ADD;

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

/// Reads the UUID a `PlayerInfoUpdate` carries in this server's minimal entries
/// layout: a single count byte followed by the 16-byte UUID.
fn player_info_uuid(info: &PlayerInfoUpdate) -> Uuid {
    let entries = info.entries();
    assert_eq!(entries[0], 1, "exactly one player-info entry expected");
    Uuid::from_slice(&entries[1..17]).expect("a 16-byte UUID")
}

/// Reads play packets from `client` until it has seen both a player-list add and
/// an entity spawn for `target`.
async fn observe_appearance(client: &mut TestClient, target: Uuid) -> anyhow::Result<()> {
    let mut info = false;
    let mut spawn = false;
    while !(info && spawn) {
        match client.next_play().await? {
            ClientboundPlayPacket::PlayerInfoUpdate(p)
                if p.action() == PLAYER_INFO_ADD && player_info_uuid(&p) == target =>
            {
                info = true;
            }
            ClientboundPlayPacket::SpawnEntity(s) if s.entity_uuid() == target => spawn = true,
            _ => {}
        }
    }
    Ok(())
}

/// Reads play packets from `client` until it sees `target` at `expected`,
/// conveyed as a (re)spawn — the milestone's position-broadcast carrier.
async fn observe_move(
    client: &mut TestClient,
    target: Uuid,
    expected: (f64, f64, f64),
) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::SpawnEntity(s) = client.next_play().await? {
            if s.entity_uuid() == target && (s.x(), s.y(), s.z()) == expected {
                return Ok(());
            }
        }
    }
}

/// The body of the test, run under the timeout guard.
async fn run_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let saad = PlayerId::offline("Saad").as_uuid();
    let notch = PlayerId::offline("Notch").as_uuid();

    // First client joins alone, driven through JoinGame so it is registered.
    let mut c1 = login_to_play(addr, "Saad").await?;
    // Second client joins; the server now makes the two mutually visible.
    let mut c2 = login_to_play(addr, "Notch").await?;

    // Each client is told about the other (player-list add + entity spawn).
    observe_appearance(&mut c1, notch).await?;
    observe_appearance(&mut c2, saad).await?;

    // One client moves; the other must receive the broadcast position.
    let new_pos = (20.0_f64, 64.0_f64, 20.0_f64);
    c2.send_frame(&encode(|buf| {
        SetPlayerPosition::new(new_pos.0, new_pos.1, new_pos.2, 0).encode(buf)
    }))
    .await?;
    observe_move(&mut c1, notch, new_pos).await?;

    Ok(())
}

#[tokio::test]
async fn two_clients_see_each_other_and_movement() {
    // Ephemeral port; radius-1 spawn keeps the chunk payload small. Both players
    // join at the default spawn, so they start in view distance of each other.
    let config = AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, run_flow(addr))
        .await
        .expect("multiplayer flow finished within the timeout guard")
        .expect("multiplayer flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the timeout guard")
        .expect("clean shutdown");
}
