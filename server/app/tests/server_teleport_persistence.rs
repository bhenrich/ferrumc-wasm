//! A server-driven teleport must be what a leave-save persists.
//!
//! Regression guard for a persistence bug: the leave-save mirror was advanced only
//! by CLIENT-reported movement (`SetPlayerPosition`/`Rotation`). A server-driven
//! absolute teleport — `/spawn`, a routed plugin `Teleport`, or an anti-cheat
//! correction — emits a clientbound `SynchronizePlayerPosition` but did NOT update
//! that mirror, so a player who teleported and then disconnected before reporting
//! any further movement was saved at their STALE pre-teleport position (and, for
//! `/spawn`, the look was additionally reset to `0.0/0.0`).
//!
//! This flow logs a player in, moves + rotates them to a distinctive spot via a
//! client packet (advancing the mirror to that spot), then runs `/spawn` so the
//! SERVER teleports them back to world spawn, and drops the client WITHOUT any
//! follow-up movement packet. A fresh server is started on the same redb world
//! directory and the player rejoins; the join `SynchronizePlayerPosition` must
//! carry the TELEPORTED position (world spawn) — not the pre-teleport one — and the
//! look the player was facing when `/spawn` ran (preserved across the snap).
//!
//! Determinism without wall-clock sleeps: the move is sent before the `/spawn`
//! command, and the connection processes serverbound frames strictly in order, so
//! the move's mirror update lands before `/spawn` overwrites it. Reading the
//! `/spawn` confirmation `SynchronizePlayerPosition` back is the barrier that proves
//! the command (and its mirror update) ran before the client drops. Reacquiring
//! every connection permit during shutdown guarantees the leave-save committed
//! before the old redb file is released and the new server opens it. The whole flow
//! is wrapped in a timeout guard.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_codec::BoundedString;
use ferrumc_proto::generated::play::{
    ChatCommand, ClientboundPlayPacket, SetPlayerPositionAndRotation,
};

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(15);

/// Distinctive look the player faces before `/spawn`. Chosen so it matches neither
/// the spawn default (`0.0`) nor the old `/spawn` reset value (also `0.0`), so a
/// preserved look is provably the moved-to one.
const MOVED_YAW: f32 = 88.0;
const MOVED_PITCH: f32 = -33.0;

/// Horizontal offset from world spawn for the pre-`/spawn` position, large enough
/// that the moved-to spot is unambiguously distinct from spawn.
const MOVED_DX: f64 = 6.0;
const MOVED_DZ: f64 = 9.0;

/// Float comparison tolerance for the round-tripped position/look.
const EPS: f64 = 1e-6;

/// Builds a redb-persistent server config on an ephemeral port, with a spawn area
/// and view distance the rejoin needs to stream the player's chunk back in.
fn persistent_config(world_dir: &Path) -> AppConfig {
    AppConfig {
        bind: "127.0.0.1:0".parse().expect("loopback addr"),
        spawn_chunk_radius: 1,
        view_distance: 1,
        world_dir: Some(world_dir.to_path_buf()),
        ..AppConfig::default()
    }
}

/// Reads play packets until the next `SynchronizePlayerPosition`, returning its
/// position and look. Bounded by the outer timeout guard.
async fn read_sync(client: &mut TestClient) -> anyhow::Result<(f64, f64, f64, f32, f32)> {
    loop {
        if let ClientboundPlayPacket::SynchronizePlayerPosition(p) = client.next_play().await? {
            return Ok((p.x(), p.y(), p.z(), p.yaw(), p.pitch()));
        }
    }
}

/// Reads play packets until a `SynchronizePlayerPosition` whose position is `spawn`
/// arrives — the `/spawn` confirmation — proving the command (and its mirror update)
/// ran. Any earlier sync (e.g. a movement correction snapping back to the moved-to
/// spot) is skipped.
async fn await_spawn_sync(client: &mut TestClient, spawn: (f64, f64, f64)) -> anyhow::Result<()> {
    loop {
        let (x, y, z, _, _) = read_sync(client).await?;
        if (x - spawn.0).abs() < EPS && (y - spawn.1).abs() < EPS && (z - spawn.2).abs() < EPS {
            return Ok(());
        }
    }
}

/// Logs `name` in, moves them to a distinctive spot, runs `/spawn` to teleport them
/// back, and drops the client so the server runs its disconnect/save path. Returns
/// `(spawn, moved)` — the world spawn the teleport targeted and the pre-`/spawn`
/// position — so the rejoin can assert which one was persisted.
async fn establish_teleported_state(
    addr: SocketAddr,
    name: &str,
) -> anyhow::Result<((f64, f64, f64), (f64, f64, f64))> {
    let mut client = login_to_play(addr, name).await?;

    // The join `SynchronizePlayerPosition` carries the world spawn; capture it as the
    // teleport target the rejoin must restore.
    let (sx, sy, sz, _, _) = read_sync(&mut client).await?;
    let spawn = (sx, sy, sz);
    let moved = (sx + MOVED_DX, sy, sz + MOVED_DZ);

    // Client-driven move + rotate: this advances the leave-save mirror to `moved`.
    client
        .send_frame(&encode(|buf| {
            SetPlayerPositionAndRotation::new(moved.0, moved.1, moved.2, MOVED_YAW, MOVED_PITCH, 0)
                .encode(buf)
        }))
        .await?;

    // Server-driven teleport back to spawn. The fix must mirror this into the
    // persistence state even though the client never reports a follow-up move.
    let command = BoundedString::<256>::new("spawn".to_string())?;
    client
        .send_frame(&encode(|buf| ChatCommand::new(command.clone()).encode(buf)))
        .await?;
    // Barrier: the `/spawn` confirmation proves the command was processed (and the
    // mirror updated) before the client drops below.
    await_spawn_sync(&mut client, spawn).await?;

    // Leaving with NO follow-up movement packet: dropping the client closes the
    // socket, so the server saves this player's state on its disconnect path.
    drop(client);
    Ok((spawn, moved))
}

/// Rejoins as `name` and asserts the persisted position is the teleport target
/// (`spawn`), not the pre-teleport `moved` spot, and that the look the player faced
/// when `/spawn` ran was preserved.
async fn assert_restored_to_spawn(
    addr: SocketAddr,
    name: &str,
    spawn: (f64, f64, f64),
    moved: (f64, f64, f64),
) -> anyhow::Result<()> {
    let mut client = login_to_play(addr, name).await?;

    let (x, y, z, yaw, pitch) = read_sync(&mut client).await?;
    anyhow::ensure!(
        (x - spawn.0).abs() < EPS && (y - spawn.1).abs() < EPS && (z - spawn.2).abs() < EPS,
        "restored position ({x}, {y}, {z}) != teleport target spawn {spawn:?}",
    );
    anyhow::ensure!(
        (x - moved.0).abs() >= EPS || (z - moved.2).abs() >= EPS,
        "restored position ({x}, {y}, {z}) is the STALE pre-teleport spot {moved:?}",
    );
    anyhow::ensure!(
        f64::from(yaw - MOVED_YAW).abs() < EPS && f64::from(pitch - MOVED_PITCH).abs() < EPS,
        "restored look (yaw {yaw}, pitch {pitch}) != look at teleport (yaw {MOVED_YAW}, pitch {MOVED_PITCH})",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn server_teleport_is_persisted_over_a_restart() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = persistent_config(temp.path());

    // First boot: a player moves, is teleported back to spawn by `/spawn`, and leaves
    // without reporting any further movement.
    let server = ferrumc_app::run(&config)
        .await
        .expect("first server starts");
    let addr = server.local_addr();
    let (spawn, moved) = timeout(GUARD, establish_teleported_state(addr, "Saad"))
        .await
        .expect("establish-state flow finished within the guard")
        .expect("establish-state flow succeeded");
    // Graceful shutdown drains the connection (its leave-save commits) and releases
    // the redb file before the next boot opens it.
    timeout(GUARD, server.shutdown())
        .await
        .expect("first shutdown within the guard")
        .expect("clean first shutdown");

    // Second boot on the SAME world directory: the player rejoins and their persisted
    // state must be the teleported position, not the pre-teleport one.
    let server = ferrumc_app::run(&config)
        .await
        .expect("second server starts");
    let addr = server.local_addr();
    timeout(GUARD, assert_restored_to_spawn(addr, "Saad", spawn, moved))
        .await
        .expect("restore-assert flow finished within the guard")
        .expect("restore-assert flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("second shutdown within the guard")
        .expect("clean second shutdown");
}
