//! End-to-end world-persistence tests over the durable redb store.
//!
//! Each test runs the real server bound to an ephemeral port with its world store
//! pointed at a throwaway temp directory, then reopens that same store after a
//! graceful shutdown to assert what was (and was not) persisted:
//!
//! - `placed_block_survives_a_restart` — a creative place is captured as a chunk
//!   overlay, survives a graceful shutdown, and is reconstructed (over a
//!   regenerated flat baseline) with the placed block intact when the store is
//!   reopened on the same directory; the overlay carries the v3 schema version
//!   (so `schema_version` round-trips, the acceptance test's third property).
//! - `untouched_generated_chunks_persist_nothing` — a session that generates the
//!   spawn area but edits nothing leaves **zero** overlay records: every spawn
//!   chunk reads back as `None`, proving generated terrain occupies no storage.
//!
//! Determinism without wall-clock sleeps: the placing client waits for the
//! server's `AcknowledgeBlockChange` (proof the edit was applied) before the
//! shutdown, and the graceful shutdown drains the storage worker, so the persisted
//! state is settled by the time `shutdown()` returns. The whole flow is wrapped in
//! a timeout guard.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_core::{DimensionId, WorldId};
use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_proto::generated::play::{ClientboundPlayPacket, UseItemOn};
use ferrumc_proto::types::BlockPosition;
use ferrumc_storage::{ChunkKey, RedbStore, SchemaVersion, WorldStore};
use ferrumc_world::{BlockStateId, FlatWorldGenerator};

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

/// `UseItemOn` face index for the top (`Up`) face.
const FACE_UP: i32 = 1;

/// Block-state id of `minecraft:stone`, the fixed block the server places.
const STONE_STATE: u32 = 1;

/// The single overworld world/dimension the slice's shard owns.
const WORLD: WorldId = WorldId::new(0);
const DIMENSION: DimensionId = DimensionId::new(0);

/// Builds a server config that persists to `world_dir` on an ephemeral port with
/// a small resident spawn area.
fn persistent_config(world_dir: &Path) -> AppConfig {
    AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses")
        .with_world_dir(Some(world_dir.to_path_buf()))
        .expect("world directory preserves valid config")
}

/// Sends a `UseItemOn` clicking the top face of the block at `pos` (placing on the
/// block one step up), stamped with `sequence`.
async fn send_place_on_top(
    client: &mut TestClient,
    pos: (i32, i32, i32),
    sequence: i32,
) -> anyhow::Result<()> {
    client
        .send_frame(&encode(|buf| {
            UseItemOn::new(
                0,
                BlockPosition::new(pos.0, pos.1, pos.2),
                FACE_UP,
                0.5,
                1.0,
                0.5,
                false,
                false,
                sequence,
            )
            .encode(buf)
        }))
        .await
}

/// Reads play packets until an `AcknowledgeBlockChange` echoing `expected_sequence`
/// arrives, proving the server applied the edit.
async fn expect_ack(client: &mut TestClient, expected_sequence: i32) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::AcknowledgeBlockChange(ack) = client.next_play().await? {
            anyhow::ensure!(
                ack.sequence() == expected_sequence,
                "AcknowledgeBlockChange carried sequence {}; expected {expected_sequence}",
                ack.sequence(),
            );
            return Ok(());
        }
    }
}

/// Logs a client in and places stone on top of the spawn surface at (8, 63, 8),
/// returning once the place has been acknowledged by the server.
async fn place_block_at_spawn(addr: SocketAddr) -> anyhow::Result<()> {
    let mut actor = login_to_play(addr, "Builder").await?;
    // Spawn is (8, 64, 8); clicking the top of the grass at (8, 63, 8) places
    // stone at (8, 64, 8), comfortably within reach and in the resident spawn
    // chunk (0, 0).
    send_place_on_top(&mut actor, (8, 63, 8), 1).await?;
    expect_ack(&mut actor, 1).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn placed_block_survives_a_restart() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = persistent_config(temp.path());

    // 1. Start the server, place a block, and shut down gracefully so the storage
    //    worker drains its final flush to the redb file.
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();
    timeout(GUARD, place_block_at_spawn(addr))
        .await
        .expect("place flow finished within the guard")
        .expect("place flow succeeded");
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown within the guard")
        .expect("clean shutdown");

    // 2. Reopen the SAME store directly and assert the placed block persisted as a
    //    chunk overlay (schema v3), reconstructed over a regenerated flat baseline.
    let store = RedbStore::open(temp.path().join("world.redb")).expect("reopen store");
    let placed_chunk = ChunkPos::new(0, 0);
    let key = ChunkKey::new(WORLD, DIMENSION, placed_chunk);
    let overlay = store
        .load_chunk_overlay(key)
        .await
        .expect("overlay load")
        .expect("an overlay must exist for the edited chunk");
    assert_eq!(
        overlay.schema_version(),
        SchemaVersion::new(3),
        "overlay schema_version must round-trip as v3 (block-entity-carrying)",
    );

    let mut chunk = FlatWorldGenerator::new().generate(placed_chunk);
    overlay.apply_to_chunk(&mut chunk).expect("apply overlay");
    assert_eq!(
        chunk.get_block(BlockPos::new(8, 64, 8)),
        Some(BlockStateId::new(STONE_STATE)),
        "the placed stone must survive the restart",
    );

    // 3. The server must also reopen the persisted database cleanly and serve a
    //    reconnecting client.
    drop(store); // release the redb file lock before the server reopens it
    let server2 = ferrumc_app::run(&config).await.expect("server reopens db");
    let addr2 = server2.local_addr();
    timeout(GUARD, login_to_play(addr2, "Returner"))
        .await
        .expect("reconnect within the guard")
        .expect("client reconnects after restart");
    timeout(GUARD, server2.shutdown())
        .await
        .expect("second shutdown within the guard")
        .expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn untouched_generated_chunks_persist_nothing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = persistent_config(temp.path());

    // A session that generates the spawn area but edits nothing.
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();
    let _client = timeout(GUARD, login_to_play(addr, "Tourist"))
        .await
        .expect("login within the guard")
        .expect("client reaches play");
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown within the guard")
        .expect("clean shutdown");

    // Every chunk in the (radius-1) spawn square was generated; none was edited,
    // so the overlay store must hold nothing for any of them.
    let store = RedbStore::open(temp.path().join("world.redb")).expect("reopen store");
    for z in -1..=1 {
        for x in -1..=1 {
            let key = ChunkKey::new(WORLD, DIMENSION, ChunkPos::new(x, z));
            assert!(
                store
                    .load_chunk_overlay(key)
                    .await
                    .expect("overlay load")
                    .is_none(),
                "generated, unedited chunk ({x}, {z}) must produce no overlay record",
            );
        }
    }
}
