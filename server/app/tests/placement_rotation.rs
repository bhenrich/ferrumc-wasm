//! End-to-end placement-rotation tests over a *running* server.
//!
//! These prove the place path computes the *correct* placed block-state (not the
//! held item's bare default) and that the computed state replicates to a second
//! client and survives a leave/rejoin:
//!
//! - `side_face_log_is_rotated_and_seen_by_second_client`: an actor equips an oak
//!   log via the creative-slot path, places it against an east face, and the
//!   placement rule rotates it to `axis=x` (state `136`, not the default vertical
//!   `137`). A second client in range observes the rotated state in the broadcast
//!   `BlockUpdate`.
//! - `placed_rotated_log_survives_rejoin`: the rotated log placed in the spawn
//!   chunk is present in the chunk data a *later* joiner receives after the placer
//!   has left the still-running server — proving the computed state persists, not
//!   just the held default.
//!
//! Determinism without wall-clock sleeps: every edit is confirmed by the server's
//! `AcknowledgeBlockChange`, and the whole flow is wrapped in a timeout guard.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_codec::write_var_int;
use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, ServerboundSetHeldItem, SetCreativeSlot, UseItemOn,
};
use ferrumc_proto::types::BlockPosition;
use ferrumc_world::{encode_chunk_section_data, BlockStateId, Chunk, FlatWorldGenerator};

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

/// `UseItemOn` face index for the east (`+X`) face (Minecraft canonical order).
const FACE_EAST: i32 = 5;

/// Protocol item id of `minecraft:oak_log`.
const OAK_LOG_ITEM: i32 = 134;

/// The block-state of `oak_log` with `axis=x` — the state a log placed against an
/// east/west face must take, distinct from the default vertical `axis=y` (137).
const OAK_LOG_AXIS_X: u32 = 136;

/// Hotbar inventory slot index 0 (the first hotbar slot).
const HOTBAR_SLOT_0: i16 = 36;

/// Encodes a trusted, component-free creative item stack of one `item_id`.
fn single_item(item_id: i32) -> Vec<u8> {
    let mut buf = Vec::new();
    write_var_int(&mut buf, 1); // itemCount
    write_var_int(&mut buf, item_id); // itemId
    write_var_int(&mut buf, 0); // addedCount (no components)
    write_var_int(&mut buf, 0); // removedCount
    buf
}

/// Equips an oak log in hotbar slot 0 and selects it (requires creative mode,
/// which the connection seeds for every joiner).
async fn equip_oak_log(client: &mut TestClient) -> anyhow::Result<()> {
    client
        .send_frame(&encode(|buf| {
            SetCreativeSlot::new(HOTBAR_SLOT_0, single_item(OAK_LOG_ITEM)).encode(buf)
        }))
        .await?;
    client
        .send_frame(&encode(|buf| ServerboundSetHeldItem::new(0).encode(buf)))
        .await
}

/// Sends a `UseItemOn` clicking the east face of `clicked` (placing one step east
/// of it), stamped with `sequence`.
async fn place_on_east_face(
    client: &mut TestClient,
    clicked: (i32, i32, i32),
    sequence: i32,
) -> anyhow::Result<()> {
    client
        .send_frame(&encode(|buf| {
            UseItemOn::new(
                0,
                BlockPosition::new(clicked.0, clicked.1, clicked.2),
                FACE_EAST,
                0.5,
                0.5,
                0.5,
                false,
                false,
                sequence,
            )
            .encode(buf)
        }))
        .await
}

/// Reads play packets until a `BlockUpdate` at `pos`, asserting it carries `state`.
async fn expect_block_update(
    client: &mut TestClient,
    pos: (i32, i32, i32),
    state: i32,
) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::BlockUpdate(update) = client.next_play().await? {
            let loc = update.location();
            if (loc.x(), loc.y(), loc.z()) != pos {
                // A different position (e.g. a neighbour update) is not the one we
                // wait for; keep scanning.
                continue;
            }
            anyhow::ensure!(
                update.block_state() == state,
                "BlockUpdate at {pos:?} carried state {}; expected {state}",
                update.block_state(),
            );
            return Ok(());
        }
    }
}

/// Reads play packets until an `AcknowledgeBlockChange` echoing `sequence`.
async fn expect_ack(client: &mut TestClient, sequence: i32) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::AcknowledgeBlockChange(ack) = client.next_play().await? {
            anyhow::ensure!(
                ack.sequence() == sequence,
                "ack carried sequence {}; expected {sequence}",
                ack.sequence(),
            );
            return Ok(());
        }
    }
}

/// Reads play packets until a `ChunkDataAndLight` for `target`, returning its
/// section-data blob.
async fn read_chunk_blob(client: &mut TestClient, target: (i32, i32)) -> anyhow::Result<Vec<u8>> {
    loop {
        if let ClientboundPlayPacket::ChunkDataAndLight(chunk) = client.next_play().await? {
            if (chunk.x(), chunk.z()) == target {
                return Ok(chunk.chunk_data().as_slice().to_vec());
            }
        }
    }
}

/// The section-data blob for `pos` after applying `edit` to a regenerated flat
/// baseline — the byte-exact reference a rejoiner should receive.
fn encoded_with_edit(pos: ChunkPos, edit: impl FnOnce(&mut Chunk)) -> Vec<u8> {
    let mut chunk = FlatWorldGenerator::new().generate(pos);
    edit(&mut chunk);
    encode_chunk_section_data(&chunk).expect("reference chunk encodes")
}

/// A second client sees the rotated state a log placed against a side face takes.
async fn rotated_log_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let mut viewer = login_to_play(addr, "Viewer").await?;
    let mut actor = login_to_play(addr, "Actor").await?;

    equip_oak_log(&mut actor).await?;

    // Click the east face of (7, 65, 8): the log lands at (8, 65, 8), in reach of
    // spawn and in the resident spawn chunk. An east/west face -> axis=x (136).
    place_on_east_face(&mut actor, (7, 65, 8), 1).await?;

    // The viewer's broadcast carries the rotated state, not the default 137.
    let rotated_wire = i32::try_from(OAK_LOG_AXIS_X).expect("state fits i32");
    expect_block_update(&mut viewer, (8, 65, 8), rotated_wire).await?;
    assert_ne!(OAK_LOG_AXIS_X, 137, "the rotation must change the state");
    expect_ack(&mut actor, 1).await?;

    Ok(())
}

/// The rotated log placed in the spawn chunk is present in a later joiner's chunk
/// data after the placer has left the running server.
async fn rejoin_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let mut builder = login_to_play(addr, "Builder").await?;
    equip_oak_log(&mut builder).await?;
    place_on_east_face(&mut builder, (7, 65, 8), 1).await?;
    expect_ack(&mut builder, 1).await?;

    // Builder leaves; the spawn chunk stays resident (Spawn ticket), carrying the
    // live rotated edit.
    drop(builder);

    // A fresh client joins and must receive the rotated log in the spawn chunk.
    let mut returner = login_to_play(addr, "Returner").await?;
    let blob = read_chunk_blob(&mut returner, (0, 0)).await?;

    let expected = encoded_with_edit(ChunkPos::new(0, 0), |chunk| {
        chunk
            .set_block(BlockPos::new(8, 65, 8), BlockStateId::new(OAK_LOG_AXIS_X))
            .expect("set reference block");
    });
    anyhow::ensure!(
        blob == expected,
        "rejoiner's spawn chunk did not carry the rotated log (axis=x)",
    );
    let baseline =
        encode_chunk_section_data(&FlatWorldGenerator::new().generate(ChunkPos::new(0, 0)))
            .expect("baseline encodes");
    anyhow::ensure!(
        blob != baseline,
        "rejoiner's spawn chunk was the unedited baseline (the placed log was lost)",
    );
    Ok(())
}

#[tokio::test]
async fn side_face_log_is_rotated_and_seen_by_second_client() {
    let config = AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, rotated_log_flow(addr))
        .await
        .expect("rotated-log flow finished within the guard")
        .expect("rotated-log flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown within the guard")
        .expect("clean shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn placed_rotated_log_survives_rejoin() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = AppConfig {
        bind: "127.0.0.1:0".parse().expect("loopback addr"),
        spawn_chunk_radius: 1,
        view_distance: 1,
        world_dir: Some(temp.path().to_path_buf()),
        ..AppConfig::default()
    };

    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, rejoin_flow(addr))
        .await
        .expect("rejoin flow finished within the guard")
        .expect("rejoin flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown within the guard")
        .expect("clean shutdown");
}
