//! Leave-then-rejoin persistence tests over a *running* server (no restart).
//!
//! These guard the two combining bugs that made a placed block vanish when a
//! player left and rejoined while the server kept running, yet reappear after a
//! full restart:
//!
//! - `placed_spawn_block_survives_rejoin` (Bug B): the join kit used to replay a
//!   chunk snapshot captured once at startup, so a block placed in a spawn chunk
//!   was never reflected to a later joiner. The fix fetches the spawn-area chunk
//!   blobs live from the resident shard chunks at join time; this test logs a
//!   builder in, places a block in the spawn chunk, drops it, then logs a fresh
//!   client in and asserts the chunk data that client receives carries the block.
//! - `placed_streamed_block_survives_rejoin` (Bug A): the unload flush on
//!   disconnect was fire-and-forget, so a fast rejoin could read the stale
//!   baseline before the overlay commit landed. With `spawn_chunk_radius = 0` the
//!   edited chunk is non-resident after the builder leaves, so the rejoiner's
//!   streamed copy must come from the durably committed overlay — proving the
//!   release path waited for the commit before dropping the chunk's tickets.
//!
//! Neither test pre-writes an overlay synchronously: both exercise the real
//! place -> release (leave) -> reacquire (rejoin) path so the async store race is
//! not masked. The placed-block assertion compares the received chunk-section blob
//! byte-for-byte against a freshly encoded reference chunk (paletted-container
//! encoding is deterministic) rather than a hardcoded blob.
//!
//! Determinism without wall-clock sleeps: every edit is confirmed by the server's
//! `AcknowledgeBlockChange`, a move is sequenced before a reach-dependent place by
//! an intervening in-reach place whose ack proves the move tick completed (a move
//! is applied only at the end of its tick, *after* edits in that tick), and the
//! whole flow is wrapped in a timeout guard.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_proto::generated::play::{ClientboundPlayPacket, SetPlayerPosition, UseItemOn};
use ferrumc_proto::types::BlockPosition;
use ferrumc_world::{encode_chunk_section_data, BlockStateId, Chunk, FlatWorldGenerator};

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

/// `UseItemOn` face index for the top (`Up`) face.
const FACE_UP: i32 = 1;

/// Block-state id of `minecraft:stone`, the block the server places (the fixed
/// default the creative starter kit's first slot resolves to).
const STONE_STATE: u32 = 1;

/// Builds a server config that persists to `world_dir` on an ephemeral port with
/// an explicit spawn radius and view distance, so the resident vs. streamed split
/// is exact.
fn persistent_config(world_dir: &Path, spawn_chunk_radius: u8, view_distance: i32) -> AppConfig {
    AppConfig::from_toml_str(&format!(
        "bind = \"127.0.0.1:0\"\nspawn_chunk_radius = {spawn_chunk_radius}\nview_distance = {view_distance}"
    ))
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

/// Sends an absolute `Set Player Position` (flags `0`, no ground bit needed).
async fn send_position(client: &mut TestClient, x: f64, y: f64, z: f64) -> anyhow::Result<()> {
    client
        .send_frame(&encode(|buf| {
            SetPlayerPosition::new(x, y, z, 0).encode(buf)
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

/// Reads play packets until a `ChunkDataAndLight` for the `target` column arrives,
/// returning its section-data blob. Relies on the outer timeout guard to bound a
/// column that never arrives.
async fn read_chunk_blob(client: &mut TestClient, target: (i32, i32)) -> anyhow::Result<Vec<u8>> {
    loop {
        if let ClientboundPlayPacket::ChunkDataAndLight(chunk) = client.next_play().await? {
            if (chunk.x(), chunk.z()) == target {
                return Ok(chunk.chunk_data().as_slice().to_vec());
            }
        }
    }
}

/// The freshly encoded section-data blob for `pos` after applying `edit` to a
/// regenerated flat baseline — the byte-exact reference a rejoiner should receive.
fn encoded_with_edit(pos: ChunkPos, edit: impl FnOnce(&mut Chunk)) -> Vec<u8> {
    let mut chunk = FlatWorldGenerator::new().generate(pos);
    edit(&mut chunk);
    encode_chunk_section_data(&chunk).expect("reference chunk encodes")
}

/// The freshly encoded section-data blob for the untouched flat baseline at `pos`.
fn encoded_baseline(pos: ChunkPos) -> Vec<u8> {
    encode_chunk_section_data(&FlatWorldGenerator::new().generate(pos)).expect("baseline encodes")
}

/// Bug B: a block placed in a spawn chunk is present in the chunk data a *later*
/// joiner receives, while the original placer has already left the running server.
async fn spawn_block_flow(addr: SocketAddr) -> anyhow::Result<()> {
    // 1. Builder joins, places stone at (8, 64, 8) in spawn chunk (0, 0) (clicking
    //    the top of the grass at (8, 63, 8), well within reach of spawn), and waits
    //    for the ack proving the edit landed in the resident chunk.
    let mut builder = login_to_play(addr, "Builder").await?;
    send_place_on_top(&mut builder, (8, 63, 8), 1).await?;
    expect_ack(&mut builder, 1).await?;

    // 2. Builder leaves: dropping the client closes the socket, so the server runs
    //    its disconnect/release path (the spawn chunk stays resident via its Spawn
    //    ticket, carrying the live edit).
    drop(builder);

    // 3. A FRESH client joins the still-running server. Pre-fix, its join kit
    //    replayed the snapshot captured at startup (before any edit), so it would
    //    receive the baseline; post-fix the spawn chunk is fetched live.
    let mut returner = login_to_play(addr, "Returner").await?;
    let blob = read_chunk_blob(&mut returner, (0, 0)).await?;

    let expected = encoded_with_edit(ChunkPos::new(0, 0), |chunk| {
        chunk
            .set_block(BlockPos::new(8, 64, 8), BlockStateId::new(STONE_STATE))
            .expect("set reference block");
    });
    anyhow::ensure!(
        blob == expected,
        "rejoiner's spawn chunk (0, 0) did not carry the placed block",
    );
    anyhow::ensure!(
        blob != encoded_baseline(ChunkPos::new(0, 0)),
        "rejoiner's spawn chunk (0, 0) was the unedited baseline (the placed block was lost)",
    );
    Ok(())
}

/// Bug A: a block placed in a *streamed* (non-spawn) chunk survives the
/// disconnect's committed overlay flush and is reconstructed for a rejoiner from
/// the durable store after the chunk has unloaded.
async fn streamed_block_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let edited = ChunkPos::new(1, 0);

    // 1. Builder joins (spawn chunk (0, 0) only is resident). Move to (12, 64, 8)
    //    so the target in chunk (1, 0) is within reach, then place an in-reach
    //    block at spawn first: its ack proves the move tick completed (a move is
    //    applied only at the end of its tick, after edits), so the *next* place
    //    sees the updated position. View distance 1 makes chunk (1, 0) resident via
    //    streaming, but with no Spawn ticket — so it unloads when the builder
    //    leaves.
    let mut builder = login_to_play(addr, "Streamer").await?;
    send_position(&mut builder, 12.0, 64.0, 8.0).await?;
    send_place_on_top(&mut builder, (9, 63, 8), 1).await?;
    expect_ack(&mut builder, 1).await?;

    // 2. Now place at (16, 64, 8) in chunk (1, 0): ~4.6 blocks from (12, 64, 8)
    //    (in reach), and the chunk is resident from the view-distance-1 stream.
    send_place_on_top(&mut builder, (16, 63, 8), 2).await?;
    expect_ack(&mut builder, 2).await?;

    // 3. Builder leaves. The disconnect/release path must commit the overlay before
    //    releasing the chunk's tickets, and (radius 0) chunk (1, 0) then unloads —
    //    so the edit now lives only in the durable store.
    drop(builder);

    // 4. A fresh client joins and streams chunk (1, 0) back in (view distance 1
    //    covers it from spawn); a nudge forces an immediate streaming pass. The
    //    reloaded copy must carry the block, which is only possible if the overlay
    //    commit had landed before the chunk unloaded.
    let mut returner = login_to_play(addr, "Rejoiner").await?;
    send_position(&mut returner, 8.0, 64.0, 8.0).await?;
    let blob = read_chunk_blob(&mut returner, (edited.x(), edited.z())).await?;

    let expected = encoded_with_edit(edited, |chunk| {
        chunk
            .set_block(BlockPos::new(16, 64, 8), BlockStateId::new(STONE_STATE))
            .expect("set reference block");
    });
    anyhow::ensure!(
        blob == expected,
        "rejoiner's streamed chunk (1, 0) did not carry the placed block",
    );
    anyhow::ensure!(
        blob != encoded_baseline(edited),
        "rejoiner's streamed chunk (1, 0) was the unedited baseline (the unload flush raced)",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn placed_spawn_block_survives_rejoin() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = persistent_config(temp.path(), 1, 1);

    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, spawn_block_flow(addr))
        .await
        .expect("spawn-block rejoin flow finished within the guard")
        .expect("spawn-block rejoin flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown within the guard")
        .expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn placed_streamed_block_survives_rejoin() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = persistent_config(temp.path(), 0, 1);

    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, streamed_block_flow(addr))
        .await
        .expect("streamed-block rejoin flow finished within the guard")
        .expect("streamed-block rejoin flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown within the guard")
        .expect("clean shutdown");
}
