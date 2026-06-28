//! End-to-end sign block-entity test.
//!
//! Starts the real server on an ephemeral port and connects two real clients that
//! log in offline and reach play: a `placer` that places and edits a sign, and a
//! `viewer` that must observe the rendered text.
//!
//! It drives the full sign loop over the wire:
//! - the placer puts an `oak_sign` item in its held hotbar slot (a creative slot
//!   write) and places it against a block face;
//! - the placer receives an `OpenSignEditor` for the new sign (the server created
//!   the block-entity), while the viewer sees the sign block appear via a
//!   `BlockUpdate`;
//! - the placer submits four lines with an `UpdateSign`;
//! - the viewer receives a `BlockEntityData` carrying the sign's NBT with the new
//!   front text — the sign renders for everyone in view.
//!
//! Determinism without wall-clock sleeps: the placer waits for the
//! `OpenSignEditor` (which proves the sign block-entity exists) before submitting
//! its text, and every reader scans frames until the expected packet under a
//! timeout guard.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_codec::{write_var_int, BoundedString};
use ferrumc_items::ItemId;
use ferrumc_nbt::NbtTag;
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, ServerboundSetHeldItem, SetCreativeSlot, UpdateSign, UseItemOn,
};
use ferrumc_proto::types::BlockPosition;

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
    // Select hotbar 0, then write the sign into its container slot.
    client
        .send_frame(&encode(|buf| ServerboundSetHeldItem::new(0).encode(buf)))
        .await?;
    client
        .send_frame(&encode(move |buf| {
            SetCreativeSlot::new(HOTBAR_SLOT_0, one_item_slot(sign_item)).encode(buf)
        }))
        .await
}

/// Sends a `UseItemOn` clicking the top face of `pos` (placing on the block one
/// step up), stamped with `sequence`.
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

/// Reads play packets until an `OpenSignEditor`, asserting it targets `pos`'s
/// front face.
async fn expect_open_sign_editor(
    client: &mut TestClient,
    pos: (i32, i32, i32),
) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::OpenSignEditor(editor) = client.next_play().await? {
            let loc = editor.location();
            anyhow::ensure!(
                (loc.x(), loc.y(), loc.z()) == pos,
                "OpenSignEditor at ({}, {}, {}); expected ({}, {}, {})",
                loc.x(),
                loc.y(),
                loc.z(),
                pos.0,
                pos.1,
                pos.2,
            );
            anyhow::ensure!(editor.is_front_text(), "editor opened the back face");
            return Ok(());
        }
    }
}

/// Reads play packets until a non-air `BlockUpdate` at `pos` (the placed sign
/// block), asserting it carries a non-air state.
async fn expect_sign_block(client: &mut TestClient, pos: (i32, i32, i32)) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::BlockUpdate(update) = client.next_play().await? {
            let loc = update.location();
            if (loc.x(), loc.y(), loc.z()) == pos {
                anyhow::ensure!(
                    update.block_state() != 0,
                    "the placed sign block update carried air",
                );
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

/// Reads play packets until a `BlockEntityData` at `pos`, asserting it is a sign
/// whose first front line is `expected_first_line`.
async fn expect_sign_text(
    client: &mut TestClient,
    pos: (i32, i32, i32),
    expected_first_line: &str,
) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::BlockEntityData(data) = client.next_play().await? {
            let loc = data.location();
            if (loc.x(), loc.y(), loc.z()) != pos {
                continue;
            }
            anyhow::ensure!(
                data.block_entity_type() == SIGN_BLOCK_ENTITY_TYPE,
                "BlockEntityData carried block-entity type {}; expected {SIGN_BLOCK_ENTITY_TYPE}",
                data.block_entity_type(),
            );
            let first = front_first_line(data.data())
                .ok_or_else(|| anyhow::anyhow!("sign NBT had no front_text/messages[0] string"))?;
            anyhow::ensure!(
                first == expected_first_line,
                "sign front line 1 was {first:?}; expected {expected_first_line:?}",
            );
            return Ok(());
        }
    }
}

/// The body of the test, run under the timeout guard.
async fn run_flow(addr: SocketAddr) -> anyhow::Result<()> {
    // The viewer joins first (so it is registered), then the placer; both spawn at
    // (8, 64, 8), sharing the spawn chunk so the viewer sees the placer's sign.
    let mut viewer = login_to_play(addr, "Viewer").await?;
    let mut placer = login_to_play(addr, "Placer").await?;

    // Equip a sign and place it: click the top of (10, 63, 8) -> sign at (10, 64, 8).
    let sign_pos = (10, 64, 8);
    equip_oak_sign(&mut placer).await?;
    send_place_on_top(&mut placer, (10, 63, 8), 1).await?;

    // The placer is shown the editor for the new sign (the block-entity exists),
    // and the viewer sees the sign block appear.
    expect_open_sign_editor(&mut placer, sign_pos).await?;
    expect_sign_block(&mut viewer, sign_pos).await?;

    // The placer submits text; the viewer receives the rendered sign.
    send_update_sign(&mut placer, sign_pos, ["Hello", "from", "FerrumC", ""]).await?;
    expect_sign_text(&mut viewer, sign_pos, "Hello").await?;

    Ok(())
}

#[tokio::test]
async fn place_edit_and_render_a_sign_for_a_viewer() {
    // Ephemeral port; radius-1 spawn keeps the resident area small.
    let config = AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, run_flow(addr))
        .await
        .expect("sign flow finished within the timeout guard")
        .expect("sign flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the timeout guard")
        .expect("clean shutdown");
}
