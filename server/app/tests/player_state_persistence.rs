//! End-to-end player-state persistence across a full server restart.
//!
//! Proves the alpha-gate goal: a player's position, look (yaw + pitch), and
//! selected hotbar slot survive leaving the server AND a complete restart. The
//! flow logs a player in, moves + rotates them and changes their held slot, then
//! drops the client (driving the disconnect/save path) and shuts the server down.
//! A *fresh* server is then started on the same redb world directory and the same
//! player rejoins; the join `SynchronizePlayerPosition` and `ClientboundSetHeldItem`
//! must carry the values saved before the restart, not the spawn defaults.
//!
//! Determinism without wall-clock sleeps: the state-changing packets are followed
//! by a block place whose `AcknowledgeBlockChange` proves the server processed the
//! whole batch (the connection updates its local position/look/slot synchronously
//! while draining the read, before the place is routed) before the client drops.
//! Reacquiring every connection permit during shutdown guarantees the leave-save
//! has committed before the old server's redb file is released and the new one
//! opens it. The whole flow is wrapped in a timeout guard.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, ServerboundSetHeldItem, SetPlayerPositionAndRotation, UseItemOn,
};
use ferrumc_proto::types::BlockPosition;

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(15);

/// `UseItemOn` face index for the top (`Up`) face.
const FACE_UP: i32 = 1;

/// The distinctive state the player establishes before leaving — chosen so none of
/// it matches the spawn defaults (spawn look is `0.0`, default held slot is `0`).
const SAVED_X: f64 = 5.5;
const SAVED_Y: f64 = 64.0;
const SAVED_Z: f64 = 9.5;
const SAVED_YAW: f32 = 70.5;
const SAVED_PITCH: f32 = -22.5;
const SAVED_SLOT: i16 = 4;

/// Float comparison tolerance for the round-tripped position/look.
const EPS: f64 = 1e-6;

/// Builds a redb-persistent server config on an ephemeral port, with the spawn
/// area and view distance the rejoin needs to stream the player's chunk back in.
fn persistent_config(world_dir: &Path) -> AppConfig {
    AppConfig {
        bind: "127.0.0.1:0".parse().expect("loopback addr"),
        spawn_chunk_radius: 1,
        view_distance: 1,
        world_dir: Some(world_dir.to_path_buf()),
        ..AppConfig::default()
    }
}

/// Reads play packets until the first `SynchronizePlayerPosition`, returning its
/// position and look. Bounded by the outer timeout guard.
async fn read_sync(client: &mut TestClient) -> anyhow::Result<(f64, f64, f64, f32, f32)> {
    loop {
        if let ClientboundPlayPacket::SynchronizePlayerPosition(p) = client.next_play().await? {
            return Ok((p.x(), p.y(), p.z(), p.yaw(), p.pitch()));
        }
    }
}

/// Reads play packets until the first `ClientboundSetHeldItem`, returning its slot.
async fn read_held_slot(client: &mut TestClient) -> anyhow::Result<i32> {
    loop {
        if let ClientboundPlayPacket::ClientboundSetHeldItem(p) = client.next_play().await? {
            return Ok(p.slot());
        }
    }
}

/// Reads play packets until an `AcknowledgeBlockChange` echoing `sequence` arrives,
/// proving the server applied (or rejected) the edit — and therefore processed the
/// whole preceding batch.
async fn expect_ack(client: &mut TestClient, sequence: i32) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::AcknowledgeBlockChange(ack) = client.next_play().await? {
            anyhow::ensure!(
                ack.sequence() == sequence,
                "ack carried sequence {}, expected {sequence}",
                ack.sequence(),
            );
            return Ok(());
        }
    }
}

/// Logs `name` in, establishes the distinctive saved state, and drops the client so
/// the server runs its disconnect/save path.
async fn establish_state(addr: SocketAddr, name: &str) -> anyhow::Result<()> {
    let mut client = login_to_play(addr, name).await?;

    // Move + rotate, then change the held slot. Both are mirrored into the
    // connection's local state synchronously as the read drains.
    client
        .send_frame(&encode(|buf| {
            SetPlayerPositionAndRotation::new(SAVED_X, SAVED_Y, SAVED_Z, SAVED_YAW, SAVED_PITCH, 0)
                .encode(buf)
        }))
        .await?;
    client
        .send_frame(&encode(|buf| {
            ServerboundSetHeldItem::new(SAVED_SLOT).encode(buf)
        }))
        .await?;
    // Place a block at the spawn surface (within reach) purely as a barrier: its ack
    // proves the move + held-slot packets ahead of it were already processed.
    client
        .send_frame(&encode(|buf| {
            UseItemOn::new(
                0,
                BlockPosition::new(8, 63, 8),
                FACE_UP,
                0.5,
                1.0,
                0.5,
                false,
                false,
                1,
            )
            .encode(buf)
        }))
        .await?;
    expect_ack(&mut client, 1).await?;

    // Leaving: dropping the client closes the socket, so the server saves this
    // player's state on its disconnect path.
    drop(client);
    Ok(())
}

/// Rejoins as `name` and asserts the saved position, look, and held slot were
/// restored rather than reset to the spawn defaults.
async fn assert_restored(addr: SocketAddr, name: &str) -> anyhow::Result<()> {
    let mut client = login_to_play(addr, name).await?;

    let (x, y, z, yaw, pitch) = read_sync(&mut client).await?;
    anyhow::ensure!(
        (x - SAVED_X).abs() < EPS && (y - SAVED_Y).abs() < EPS && (z - SAVED_Z).abs() < EPS,
        "restored position ({x}, {y}, {z}) != saved ({SAVED_X}, {SAVED_Y}, {SAVED_Z})",
    );
    anyhow::ensure!(
        f64::from(yaw - SAVED_YAW).abs() < EPS && f64::from(pitch - SAVED_PITCH).abs() < EPS,
        "restored look (yaw {yaw}, pitch {pitch}) != saved (yaw {SAVED_YAW}, pitch {SAVED_PITCH})",
    );

    let slot = read_held_slot(&mut client).await?;
    anyhow::ensure!(
        slot == i32::from(SAVED_SLOT),
        "restored held slot {slot} != saved {SAVED_SLOT}",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn player_state_survives_a_full_restart() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = persistent_config(temp.path());

    // First boot: a player establishes distinctive state and leaves.
    let server = ferrumc_app::run(&config)
        .await
        .expect("first server starts");
    let addr = server.local_addr();
    timeout(GUARD, establish_state(addr, "Saad"))
        .await
        .expect("establish-state flow finished within the guard")
        .expect("establish-state flow succeeded");
    // Graceful shutdown drains the connection (its leave-save commits) and releases
    // the redb file before the next boot opens it.
    timeout(GUARD, server.shutdown())
        .await
        .expect("first shutdown within the guard")
        .expect("clean first shutdown");

    // Second boot on the SAME world directory: the player rejoins and their saved
    // state is restored from durable storage.
    let server = ferrumc_app::run(&config)
        .await
        .expect("second server starts");
    let addr = server.local_addr();
    timeout(GUARD, assert_restored(addr, "Saad"))
        .await
        .expect("restore-assert flow finished within the guard")
        .expect("restore-assert flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("second shutdown within the guard")
        .expect("clean second shutdown");
}
