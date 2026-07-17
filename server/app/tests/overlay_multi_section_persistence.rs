//! End-to-end regression test for cumulative chunk-overlay persistence.
//!
//! Mirrors `persistence_overlay::placed_block_survives_a_restart`, but proves the
//! fix for a data-loss bug: editing **two different sections of one chunk on two
//! different flush ticks** must not let the later flush overwrite the earlier
//! section away. The store's overlay save is last-write-wins per chunk key (both
//! the in-memory and redb backends overwrite), so correctness rests on every flush
//! capturing the chunk's *cumulative* set of edited sections — not just the
//! sections dirtied since the previous flush.
//!
//! Flow: a creative client breaks the grass surface at `(8, 63, 8)` (section 7) and
//! waits for its `AcknowledgeBlockChange` — proof the edit was applied and so
//! flushed on that tick — then places stone at `(9, 64, 8)` (section 8) on a later
//! tick and waits for its ack. The server is shut down gracefully (draining the
//! storage worker), and the SAME redb file is reopened to assert **both** edits
//! survived: the dug hole did not revert to the baseline grass, and the placed
//! stone is present. Without the cumulative-capture fix the second flush would
//! overwrite the first section's overlay and the dug hole would silently revert.
//!
//! Determinism without wall-clock sleeps: each edit waits for its ack before the
//! next is sent (so the two land on separate flush ticks), and the graceful
//! shutdown drains the storage worker, so the persisted state is settled by the
//! time `shutdown()` returns. The whole flow is wrapped in a timeout guard.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_core::{DimensionId, WorldId};
use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_proto::generated::play::{ClientboundPlayPacket, PlayerAction, UseItemOn};
use ferrumc_proto::types::BlockPosition;
use ferrumc_storage::{ChunkKey, RedbStore, WorldStore};
use ferrumc_world::{BlockStateId, FlatWorldGenerator};

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

/// `PlayerAction` status meaning "start destroying block" (creative insta-mine).
const START_DESTROY_BLOCK: i32 = 0;

/// `UseItemOn` face index for the top (`Up`) face.
const FACE_UP: i32 = 1;

/// Block-state id of `minecraft:stone`, the fixed block the server places.
const STONE_STATE: u32 = 1;

/// Block-state id of air (`0` in the pinned flat-world registry).
const AIR_STATE: u32 = 0;

/// The single overworld world/dimension the slice's shard owns.
const WORLD: WorldId = WorldId::new(0);
const DIMENSION: DimensionId = DimensionId::new(0);

/// The chunk the two edits land in (both positions are columns of `(0, 0)`).
const EDITED_CHUNK: ChunkPos = ChunkPos::new(0, 0);

/// The broken grass block at `y = 63` lives in section 7 (`(63 - (-64)) / 16`).
const DUG: BlockPos = BlockPos::new(8, 63, 8);
/// The placed stone at `y = 64` lives in section 8 (`(64 - (-64)) / 16`) — a
/// different section from [`DUG`], which is the whole point of the regression.
const PLACED: BlockPos = BlockPos::new(9, 64, 8);

/// Builds a server config that persists to `world_dir` on an ephemeral port with a
/// small resident spawn area.
fn persistent_config(world_dir: &Path) -> AppConfig {
    AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses")
        .with_world_dir(Some(world_dir.to_path_buf()))
        .expect("world directory preserves valid config")
}

/// Sends a dig-start `PlayerAction` breaking the block at `pos`, stamped with
/// `sequence`.
async fn send_break(
    client: &mut TestClient,
    pos: (i32, i32, i32),
    sequence: i32,
) -> anyhow::Result<()> {
    client
        .send_frame(&encode(|buf| {
            PlayerAction::new(
                START_DESTROY_BLOCK,
                BlockPosition::new(pos.0, pos.1, pos.2),
                1,
                sequence,
            )
            .encode(buf)
        }))
        .await
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
/// arrives, proving the server applied the edit (and so flushed it that tick).
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

/// Logs a client in, breaks the grass at [`DUG`] (section 7) and — only after that
/// edit is acked, so it flushed on its own tick — places stone at [`PLACED`]
/// (section 8) on a later tick, returning once both are acked.
async fn edit_two_sections(addr: SocketAddr) -> anyhow::Result<()> {
    let mut actor = login_to_play(addr, "Builder").await?;

    // Edit 1: break the grass directly under spawn (section 7). Waiting for the ack
    // guarantees this edit was applied — and so flushed by the end-of-tick flush —
    // before the second edit is even sent, so the two land on separate flush ticks.
    send_break(&mut actor, (DUG.x(), DUG.y(), DUG.z()), 1).await?;
    expect_ack(&mut actor, 1).await?;

    // Edit 2: place stone at (9, 64, 8) by clicking the top of (9, 63, 8) — a
    // DIFFERENT section (8). Under the pre-fix last-write-wins overwrite, this
    // flush would drop the section-7 overlay entirely.
    send_place_on_top(&mut actor, (PLACED.x(), PLACED.y() - 1, PLACED.z()), 2).await?;
    expect_ack(&mut actor, 2).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn edits_to_two_sections_on_separate_ticks_both_survive_a_restart() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = persistent_config(temp.path());

    // 1. Start the server, make two edits to two sections across two flush ticks,
    //    then shut down gracefully so the storage worker drains its final flush.
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();
    timeout(GUARD, edit_two_sections(addr))
        .await
        .expect("edit flow finished within the guard")
        .expect("edit flow succeeded");
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown within the guard")
        .expect("clean shutdown");

    // 2. Reopen the SAME store and reconstruct the chunk over a regenerated baseline.
    //    The overlay must carry BOTH edited sections (7 and 8) — the regression is a
    //    later flush overwriting the overlay with only section 8.
    let store = RedbStore::open(temp.path().join("world.redb")).expect("reopen store");
    let key = ChunkKey::new(WORLD, DIMENSION, EDITED_CHUNK);
    let overlay = store
        .load_chunk_overlay(key)
        .await
        .expect("overlay load")
        .expect("an overlay must exist for the edited chunk");
    let mask = overlay.dirty_section_mask();
    assert!(
        mask & (1 << 7) != 0 && mask & (1 << 8) != 0,
        "the overlay must carry BOTH edited sections (7 and 8); mask was {mask:#x} — a later \
         flush overwrote the earlier section, the exact data-loss bug",
    );

    let mut chunk = FlatWorldGenerator::new().generate(EDITED_CHUNK);
    overlay.apply_to_chunk(&mut chunk).expect("apply overlay");
    assert_eq!(
        chunk.get_block(DUG),
        Some(BlockStateId::new(AIR_STATE)),
        "the section-7 edit (the dug hole) must survive — it must NOT revert to the \
         baseline grass",
    );
    assert_eq!(
        chunk.get_block(PLACED),
        Some(BlockStateId::new(STONE_STATE)),
        "the section-8 edit (the placed stone) must survive",
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
