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
/// an entity spawn for `target`, returning the network entity id of the spawn.
///
/// The spawn must carry the `minecraft:player` entity type (149) so the client
/// renders a player model, not a placeholder — this is what the original
/// entity-type bug (type 0 = a boat) got wrong.
async fn observe_appearance(client: &mut TestClient, target: Uuid) -> anyhow::Result<i32> {
    let mut info = false;
    let mut entity_id = None;
    while !(info && entity_id.is_some()) {
        match client.next_play().await? {
            ClientboundPlayPacket::PlayerInfoUpdate(p)
                if p.action() == PLAYER_INFO_ADD && player_info_uuid(&p) == target =>
            {
                info = true;
            }
            ClientboundPlayPacket::SpawnEntity(s) if s.entity_uuid() == target => {
                anyhow::ensure!(
                    s.entity_type() == 149,
                    "remote player must spawn as minecraft:player (149), got {}",
                    s.entity_type()
                );
                entity_id = Some(s.entity_id());
            }
            _ => {}
        }
    }
    Ok(entity_id.expect("entity id captured"))
}

/// Reads play packets from `client` until it sees `entity_id` teleported to
/// `expected` — the carrier for a move larger than 8 blocks.
async fn observe_teleport(
    client: &mut TestClient,
    entity_id: i32,
    expected: (f64, f64, f64),
) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::EntityTeleport(t) = client.next_play().await? {
            if t.entity_id() == entity_id && (t.x(), t.y(), t.z()) == expected {
                return Ok(());
            }
        }
    }
}

/// Reads play packets from `client` until it sees `entity_id` despawned via a
/// Remove Entities packet — the leave-side counterpart of the spawn.
async fn observe_despawn(client: &mut TestClient, entity_id: i32) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::RemoveEntities(r) = client.next_play().await? {
            if r.entity_ids().contains(&entity_id) {
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

    // Each client is told about the other (player-list add + entity spawn). Keep
    // Notch's entity id so c1 can match the relative/teleport move that follows.
    let notch_eid = observe_appearance(&mut c1, notch).await?;
    observe_appearance(&mut c2, saad).await?;

    // One client jumps far (spawn 8,64,8 -> 20,64,20, ~17 blocks); the other must
    // receive it as an absolute EntityTeleport (a relative move cannot encode it).
    let new_pos = (20.0_f64, 64.0_f64, 20.0_f64);
    c2.send_frame(&encode(|buf| {
        SetPlayerPosition::new(new_pos.0, new_pos.1, new_pos.2, 0).encode(buf)
    }))
    .await?;
    observe_teleport(&mut c1, notch_eid, new_pos).await?;

    // Notch leaves (drop the socket): Saad must see the entity despawn, not a
    // lingering ghost. This is the RemoveEntities the leave path now broadcasts.
    drop(c2);
    observe_despawn(&mut c1, notch_eid).await?;

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
