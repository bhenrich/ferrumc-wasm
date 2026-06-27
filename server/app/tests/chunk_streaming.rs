//! End-to-end chunk-streaming test.
//!
//! Starts the real server on an ephemeral port, logs a real
//! [`tokio::net::TcpStream`] client in to play, drains the spawn batch, then
//! sends a `Set Player Position` that crosses a chunk boundary and asserts the
//! server streams the world to follow:
//!
//! - a `Set Center Chunk` for the new centre chunk;
//! - at least one **new** `Chunk Data and Light` for a column that was *not* in
//!   the spawn batch (a chunk newly within view distance);
//! - at least one `Unload Chunk` for a column that left the view radius;
//! - and never a duplicate `Chunk Data` for a column the client already holds.
//!
//! Determinism without wall-clock sleeps: the client drives every step by hand
//! and reads frames until the keystones arrive, and the whole flow is wrapped in
//! a timeout guard. View distance and spawn radius are pinned in config so the
//! crossing arithmetic is exact.

mod common;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_proto::generated::play::{ClientboundPlayPacket, SetPlayerPosition};

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

/// Spawn radius pinned in config: a `(2 * 1 + 1)` square == 9 spawn columns.
const SPAWN_CHUNKS: usize = 9;

/// Sends an absolute `Set Player Position` (flags `0`, no ground bit needed).
async fn send_position(client: &mut TestClient, x: f64, y: f64, z: f64) -> anyhow::Result<()> {
    client
        .send_frame(&encode(|buf| {
            SetPlayerPosition::new(x, y, z, 0).encode(buf)
        }))
        .await
}

/// The body of the test, run under the timeout guard.
async fn run_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let mut client = login_to_play(addr, "Walker").await?;

    // Every `Chunk Data` column the client is ever sent, to catch a double-send.
    let mut ever_sent: HashSet<(i32, i32)> = HashSet::new();
    // The columns delivered by the spawn batch (radius 1, centred on chunk 0,0).
    let mut spawn_columns: HashSet<(i32, i32)> = HashSet::new();

    // Drain the join sequence: the spawn batch is the last thing the join kit
    // sends, so reading until all 9 spawn columns arrive consumes it whole.
    while spawn_columns.len() < SPAWN_CHUNKS {
        if let ClientboundPlayPacket::ChunkDataAndLight(chunk) = client.next_play().await? {
            let pos = (chunk.x(), chunk.z());
            anyhow::ensure!(ever_sent.insert(pos), "spawn column {pos:?} sent twice");
            spawn_columns.insert(pos);
        }
    }

    // Cross one chunk boundary east: from spawn chunk (0, 0) into chunk (1, 0).
    // (x = 24 lands in chunk 1; y/z stay in the spawn column.)
    send_position(&mut client, 24.0, 64.0, 8.0).await?;

    // The crossing must re-centre the view, stream in at least one column that was
    // not already sent at spawn, and unload at least one column that left the
    // radius — all without re-sending a column the client already holds.
    let mut recentred = false;
    let mut streamed_new_column = false;
    let mut unloaded_departed_column = false;
    for _ in 0..64 {
        match client.next_play().await? {
            ClientboundPlayPacket::SetCenterChunk(center) => {
                anyhow::ensure!(
                    (center.chunk_x(), center.chunk_z()) == (1, 0),
                    "Set Center Chunk targeted ({}, {}); expected (1, 0)",
                    center.chunk_x(),
                    center.chunk_z(),
                );
                recentred = true;
            }
            ClientboundPlayPacket::ChunkDataAndLight(chunk) => {
                let pos = (chunk.x(), chunk.z());
                anyhow::ensure!(ever_sent.insert(pos), "column {pos:?} sent twice");
                // A column newly within view distance was not in the spawn batch.
                if !spawn_columns.contains(&pos) {
                    streamed_new_column = true;
                }
            }
            ClientboundPlayPacket::UnloadChunk(unload) => {
                let pos = (unload.chunk_x(), unload.chunk_z());
                // It must be a column that left the radius: one the client held
                // from the spawn batch but that is now outside the new view square.
                if spawn_columns.contains(&pos) {
                    unloaded_departed_column = true;
                }
            }
            _ => {}
        }
        if recentred && streamed_new_column && unloaded_departed_column {
            break;
        }
    }

    anyhow::ensure!(
        recentred,
        "server never sent Set Center Chunk for the crossing"
    );
    anyhow::ensure!(
        streamed_new_column,
        "server never streamed a new column outside the spawn batch",
    );
    anyhow::ensure!(
        unloaded_departed_column,
        "server never unloaded a column that left the view radius",
    );
    Ok(())
}

#[tokio::test]
async fn crossing_a_chunk_boundary_streams_and_unloads_chunks() {
    // Ephemeral port; a radius-1 spawn and view distance 1 make the crossing exact:
    // at spawn the client holds the 3x3 square around (0, 0); stepping one chunk
    // east loads the new x = 2 column and unloads the x = -1 column.
    let config = AppConfig::from_toml_str(
        "bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1\nview_distance = 1",
    )
    .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, run_flow(addr))
        .await
        .expect("stream flow finished within the timeout guard")
        .expect("stream flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the timeout guard")
        .expect("clean shutdown");
}
