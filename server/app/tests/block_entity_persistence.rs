//! End-to-end block-entity persistence test over the durable redb store.
//!
//! Mirrors `persistence_overlay::placed_block_survives_a_restart` but for a block
//! *entity*: a real client places an `oak_sign` and submits front-face text, the
//! server is shut down gracefully (draining the storage worker), and the SAME redb
//! file is reopened to assert the sign's text survived — reconstructed from the
//! chunk overlay over a regenerated flat baseline. It then restarts the server to
//! prove the persisted database reopens cleanly.
//!
//! This exercises the whole new path end to end: client → session → the sim's
//! `apply_sign_update` (which now marks the chunk persist-dirty) → the tick/flush
//! flush → the storage worker → the v3 overlay codec (block-entity section) →
//! reopen → decode → `apply_to_chunk` block-entity reconstruction.
//!
//! Determinism without wall-clock sleeps: a viewer waits for the `BlockEntityData`
//! carrying the new sign text (proof the edit was applied) before the shutdown, and
//! the graceful shutdown drains the storage worker, so the persisted state is
//! settled by the time `shutdown()` returns. The whole flow is wrapped in a timeout
//! guard.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_codec::{write_var_int, BoundedString};
use ferrumc_core::{DimensionId, WorldId};
use ferrumc_items::ItemId;
use ferrumc_math::{BlockPos, ChunkPos};
use ferrumc_nbt::NbtTag;
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, ServerboundSetHeldItem, SetCreativeSlot, UpdateSign, UseItemOn,
};
use ferrumc_proto::types::BlockPosition;
use ferrumc_storage::{ChunkKey, RedbStore, SchemaVersion, WorldStore};
use ferrumc_world::{BlockEntity, FlatWorldGenerator};

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

/// `UseItemOn` face index for the top (`Up`) face.
const FACE_UP: i32 = 1;

/// Container slot index of hotbar slot 0 (the default selected hotbar slot).
const HOTBAR_SLOT_0: i16 = 36;

/// The `minecraft:sign` block-entity-type id (1.21.8 / protocol 772).
const SIGN_BLOCK_ENTITY_TYPE: i32 = 7;

/// The single overworld world/dimension the slice's shard owns.
const WORLD: WorldId = WorldId::new(0);
const DIMENSION: DimensionId = DimensionId::new(0);

/// The chunk holding the placed sign, and the sign's absolute position.
const SIGN_CHUNK: ChunkPos = ChunkPos::new(0, 0);
const SIGN_X: i32 = 10;
const SIGN_Y: i32 = 64;
const SIGN_Z: i32 = 8;

/// The first front-face line the placer submits and we assert survives.
const SIGN_LINE_1: &str = "Persisted";

/// Builds a server config that persists to `world_dir` on an ephemeral port with a
/// small resident spawn area.
fn persistent_config(world_dir: &Path) -> AppConfig {
    AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses")
        .with_world_dir(Some(world_dir.to_path_buf()))
        .expect("world directory preserves valid config")
}

/// Builds a trusted (component-free) inventory slot holding one `item_id`.
fn one_item_slot(item_id: i32) -> Vec<u8> {
    let mut buf = Vec::new();
    write_var_int(&mut buf, 1); // count
    write_var_int(&mut buf, item_id); // itemId
    write_var_int(&mut buf, 0); // componentsToAdd
    write_var_int(&mut buf, 0); // componentsToRemove
    buf
}

/// Puts an `oak_sign` item in the placer's held hotbar slot.
async fn equip_oak_sign(client: &mut TestClient) -> anyhow::Result<()> {
    let sign_item = ItemId::from_name("oak_sign")
        .expect("oak_sign is a registry item")
        .id();
    client
        .send_frame(&encode(|buf| ServerboundSetHeldItem::new(0).encode(buf)))
        .await?;
    client
        .send_frame(&encode(move |buf| {
            SetCreativeSlot::new(HOTBAR_SLOT_0, one_item_slot(sign_item)).encode(buf)
        }))
        .await
}

/// Sends a `UseItemOn` clicking the top face of `pos` (placing one step up).
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

/// Sends an `UpdateSign` setting the four front-face lines of the sign at `pos`.
async fn send_update_sign(
    client: &mut TestClient,
    pos: (i32, i32, i32),
    lines: [&str; 4],
) -> anyhow::Result<()> {
    let l1 = BoundedString::<384>::new(lines[0].to_string())?;
    let l2 = BoundedString::<384>::new(lines[1].to_string())?;
    let l3 = BoundedString::<384>::new(lines[2].to_string())?;
    let l4 = BoundedString::<384>::new(lines[3].to_string())?;
    client
        .send_frame(&encode(move |buf| {
            UpdateSign::new(
                BlockPosition::new(pos.0, pos.1, pos.2),
                true,
                l1,
                l2,
                l3,
                l4,
            )
            .encode(buf)
        }))
        .await
}

/// Reads play packets until an `OpenSignEditor` at `pos` (proof the sign
/// block-entity exists).
async fn expect_open_sign_editor(
    client: &mut TestClient,
    pos: (i32, i32, i32),
) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::OpenSignEditor(editor) = client.next_play().await? {
            let loc = editor.location();
            if (loc.x(), loc.y(), loc.z()) == pos {
                return Ok(());
            }
        }
    }
}

/// Returns the first front-face message line of a sign's `BlockEntityData` NBT.
fn front_first_line(data: &NbtTag) -> Option<String> {
    let NbtTag::Compound(root) = data else {
        return None;
    };
    let NbtTag::Compound(front) = root.get("front_text")? else {
        return None;
    };
    let NbtTag::List(messages) = front.get("messages")? else {
        return None;
    };
    match messages.first()? {
        NbtTag::String(line) => Some(line.clone()),
        _ => None,
    }
}

/// Reads play packets until a `BlockEntityData` sign at `pos` whose first front
/// line is `expected` (proof the edit was applied server-side).
async fn expect_sign_text(
    client: &mut TestClient,
    pos: (i32, i32, i32),
    expected: &str,
) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::BlockEntityData(data) = client.next_play().await? {
            let loc = data.location();
            if (loc.x(), loc.y(), loc.z()) != pos {
                continue;
            }
            anyhow::ensure!(
                data.block_entity_type() == SIGN_BLOCK_ENTITY_TYPE,
                "BlockEntityData type {}; expected {SIGN_BLOCK_ENTITY_TYPE}",
                data.block_entity_type(),
            );
            let first = front_first_line(data.data())
                .ok_or_else(|| anyhow::anyhow!("sign NBT had no front_text/messages[0]"))?;
            anyhow::ensure!(
                first == expected,
                "sign line 1 was {first:?}; expected {expected:?}"
            );
            return Ok(());
        }
    }
}

/// Logs a viewer and placer in, places a sign, and edits its front text, returning
/// once a viewer has observed the rendered text (so the edit is applied).
async fn place_and_edit_sign(addr: SocketAddr) -> anyhow::Result<()> {
    let mut viewer = login_to_play(addr, "Viewer").await?;
    let mut placer = login_to_play(addr, "Placer").await?;

    let sign_pos = (SIGN_X, SIGN_Y, SIGN_Z);
    equip_oak_sign(&mut placer).await?;
    send_place_on_top(&mut placer, (SIGN_X, SIGN_Y - 1, SIGN_Z), 1).await?;
    expect_open_sign_editor(&mut placer, sign_pos).await?;

    send_update_sign(&mut placer, sign_pos, [SIGN_LINE_1, "by", "FerrumC", ""]).await?;
    // The viewer observing the new text proves the server applied (and so marked
    // persist-dirty) the edit before we shut down.
    expect_sign_text(&mut viewer, sign_pos, SIGN_LINE_1).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_text_survives_a_restart() {
    let temp = tempfile::tempdir().expect("temp dir");
    let config = persistent_config(temp.path());

    // 1. Start the server, place + edit a sign, and shut down gracefully so the
    //    storage worker drains its final flush to the redb file.
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();
    timeout(GUARD, place_and_edit_sign(addr))
        .await
        .expect("sign flow finished within the guard")
        .expect("sign flow succeeded");
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown within the guard")
        .expect("clean shutdown");

    // 2. Reopen the SAME store and assert the sign block-entity persisted into the
    //    chunk overlay (now schema v3) with its text, reconstructed over a baseline.
    let store = RedbStore::open(temp.path().join("world.redb")).expect("reopen store");
    let key = ChunkKey::new(WORLD, DIMENSION, SIGN_CHUNK);
    let overlay = store
        .load_chunk_overlay(key)
        .await
        .expect("overlay load")
        .expect("an overlay must exist for the edited chunk");
    assert_eq!(
        overlay.schema_version(),
        SchemaVersion::new(3),
        "overlay schema must round-trip as v3 (block-entity-carrying)",
    );
    assert!(
        overlay.block_entity_count() >= 1,
        "the overlay must carry the sign block entity",
    );

    let mut chunk = FlatWorldGenerator::new().generate(SIGN_CHUNK);
    overlay.apply_to_chunk(&mut chunk).expect("apply overlay");
    let sign_pos = BlockPos::new(SIGN_X, SIGN_Y, SIGN_Z);
    match chunk.block_entity(sign_pos) {
        Some(BlockEntity::Sign(sign)) => {
            assert_eq!(
                sign.front().lines()[0],
                SIGN_LINE_1,
                "the persisted sign's front line 1 must survive the restart",
            );
        }
        other => panic!("expected a reconstructed sign block-entity, got {other:?}"),
    }

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
