//! End-to-end creative inventory test.
//!
//! Starts the real server on an ephemeral port and drives real
//! [`tokio::net::TcpStream`] clients through login to play. It asserts the
//! creative build loop:
//! - join sends a `SetContainerContent` for window 0 (state id 1) carrying the
//!   starter hotbar kit (stone in slot 36, glass in slot 39);
//! - selecting a hotbar slot (`Set Held Item`) then placing (`UseItemOn`) writes
//!   the *held* block — glass (block-state 562), not the old hardcoded stone —
//!   which the active block-rules plugin then rewrites to tinted glass (23377), the
//!   state that lands and is broadcast; and acks the sequence;
//! - a hostile `Set Creative Slot` (oversized count + a dangerous nested-item
//!   component) is normalized (count clamped, component stripped) and echoed back
//!   as a `Set Container Slot`;
//! - a non-creative player's `Set Creative Slot` is ignored (no echo);
//! - a `Click Container` on window 0 triggers a full container-content resync with
//!   a bumped state id.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_codec::{write_var_int, BoundedReader, BoundedString};
use ferrumc_proto::generated::play::{
    ChatCommand, ClientboundPlayPacket, ServerboundSetHeldItem, SetCreativeSlot, UseItemOn,
    WindowClick,
};
use ferrumc_proto::types::BlockPosition;

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

/// `UseItemOn` face index for the top (`Up`) face.
const FACE_UP: i32 = 1;

/// Player-inventory window id (window 0 is always open).
const WINDOW_ID: i32 = 0;

/// Hotbar inventory slot indices for the kit's stone and glass entries.
const STONE_SLOT: usize = 36;
const GLASS_SLOT: usize = 39;

/// Protocol item id of `minecraft:glass` and the block-state it places.
const GLASS_ITEM: i32 = 195;
const GLASS_STATE: i32 = 562;

/// Block-state of `minecraft:tinted_glass`: the state the active block-rules
/// plugin rewrites a glass placement (`GLASS_STATE`) to, so the block that
/// actually lands — and is broadcast — is tinted glass, not plain glass.
const TINTED_GLASS_STATE: i32 = 23377;

/// Protocol item id of `minecraft:stone`.
const STONE_ITEM: i32 = 1;

/// The `container` data-component type id (66): a nested item tree the server must
/// strip from a hostile creative slot.
const CONTAINER_COMPONENT: i32 = 66;

/// `Game Event` reason `3`: change game mode.
const GAME_EVENT_CHANGE_GAMEMODE: u8 = 3;

/// Reads play packets from `client` until it sees a `BlockUpdate`, then asserts it
/// is at `pos` carrying `state`. The first `BlockUpdate` must match.
async fn expect_block_update(
    client: &mut TestClient,
    pos: (i32, i32, i32),
    state: i32,
) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::BlockUpdate(update) = client.next_play().await? {
            let loc = update.location();
            anyhow::ensure!(
                (loc.x(), loc.y(), loc.z()) == pos,
                "unexpected BlockUpdate at ({}, {}, {})",
                loc.x(),
                loc.y(),
                loc.z(),
            );
            anyhow::ensure!(
                update.block_state() == state,
                "BlockUpdate carried state {}; expected {state}",
                update.block_state(),
            );
            return Ok(());
        }
    }
}

/// Reads play packets until an `AcknowledgeBlockChange`, asserting its sequence.
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

/// Reads play packets until the next `SetContainerContent` for window 0, returning
/// its `(state_id, payload)`.
async fn next_container_content(client: &mut TestClient) -> anyhow::Result<(i32, Vec<u8>)> {
    loop {
        if let ClientboundPlayPacket::SetContainerContent(content) = client.next_play().await? {
            anyhow::ensure!(content.window_id() == WINDOW_ID, "wrong window id");
            return Ok((content.state_id(), content.payload().to_vec()));
        }
    }
}

/// Decodes one *trusted* slot (the component-free form the kit and validated
/// creative slots use): `None` for an empty slot, else `(count, item_id)`.
fn read_trusted_slot(reader: &mut BoundedReader<'_>) -> anyhow::Result<Option<(i32, i32)>> {
    let count = reader.read_var_int()?;
    if count == 0 {
        return Ok(None);
    }
    let item_id = reader.read_var_int()?;
    let added = reader.read_var_int()?;
    let removed = reader.read_var_int()?;
    anyhow::ensure!(
        added == 0 && removed == 0,
        "test only decodes component-free slots (added={added}, removed={removed})",
    );
    Ok(Some((count, item_id)))
}

/// Walks a `SetContainerContent` payload and returns the slot at `index`.
fn slot_in_container(payload: &[u8], index: usize) -> anyhow::Result<Option<(i32, i32)>> {
    let mut reader = BoundedReader::new(payload);
    let count = usize::try_from(reader.read_var_int()?)?;
    let mut found = None;
    for i in 0..count {
        let slot = read_trusted_slot(&mut reader)?;
        if i == index {
            found = slot;
        }
    }
    Ok(found)
}

/// Builds a hostile untrusted glass slot: an oversized count plus a dangerous
/// `container` component, both of which validation must normalize away.
fn hostile_glass_slot() -> Vec<u8> {
    let mut buf = Vec::new();
    write_var_int(&mut buf, 200); // itemCount (clamps to glass's max stack, 64)
    write_var_int(&mut buf, GLASS_ITEM); // itemId
    write_var_int(&mut buf, 1); // addedCount
    write_var_int(&mut buf, 0); // removedCount
    write_var_int(&mut buf, CONTAINER_COMPONENT); // dangerous -> stripped
    write_var_int(&mut buf, 3); // component data length (ByteArray)
    buf.extend_from_slice(&[1, 2, 3]);
    buf
}

/// Sends a `Set Held Item` selecting hotbar index `slot`.
async fn send_held_item(client: &mut TestClient, slot: i16) -> anyhow::Result<()> {
    client
        .send_frame(&encode(|buf| ServerboundSetHeldItem::new(slot).encode(buf)))
        .await
}

/// Sends a `UseItemOn` clicking the top face of `pos`, stamped with `sequence`.
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

/// Builds a server with the given extra config lines and returns its address.
async fn start_server(extra: &str) -> (ferrumc_app::RunningServer, SocketAddr) {
    let toml = format!("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1\n{extra}");
    let config = AppConfig::from_toml_str(&toml).expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();
    (server, addr)
}

async fn join_kit_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let mut viewer = login_to_play(addr, "Viewer").await?;
    let mut builder = login_to_play(addr, "Builder").await?;

    // The join container content carries the starter kit at state id 1.
    let (state_id, payload) = next_container_content(&mut builder).await?;
    anyhow::ensure!(
        state_id == 1,
        "join container state id was {state_id}; expected 1"
    );
    anyhow::ensure!(
        slot_in_container(&payload, STONE_SLOT)? == Some((64, STONE_ITEM)),
        "hotbar slot 36 is not a full stone stack",
    );
    anyhow::ensure!(
        slot_in_container(&payload, GLASS_SLOT)? == Some((64, GLASS_ITEM)),
        "hotbar slot 39 is not a full glass stack",
    );

    // Select the glass hotbar slot (index 3 -> inventory slot 39), then place.
    send_held_item(&mut builder, 3).await?;
    send_place_on_top(&mut builder, (9, 63, 8), 7).await?;
    // The held item must resolve to glass (562), not the old stone default (1):
    // proven end to end because the active block-rules plugin only rewrites a glass
    // placement, so the block that lands and is broadcast is tinted glass (23377).
    // A wrong held-resolution (or the old default) would never trigger the rewrite.
    assert_ne!(
        GLASS_STATE, TINTED_GLASS_STATE,
        "the rewrite must change the placed state"
    );
    expect_block_update(&mut viewer, (9, 64, 8), TINTED_GLASS_STATE).await?;
    expect_ack(&mut builder, 7).await?;

    Ok(())
}

async fn creative_slot_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let mut builder = login_to_play(addr, "Builder").await?;
    // Consume the join container content (state id 1).
    let (state_id, _) = next_container_content(&mut builder).await?;
    anyhow::ensure!(state_id == 1, "join state id was {state_id}");

    // A hostile glass stack into a main slot: oversized count + dangerous component.
    builder
        .send_frame(&encode(|buf| {
            SetCreativeSlot::new(9, hostile_glass_slot()).encode(buf)
        }))
        .await?;

    // The server echoes a normalized Set Container Slot for slot 9.
    let echo = loop {
        if let ClientboundPlayPacket::SetContainerSlot(slot) = builder.next_play().await? {
            break slot;
        }
    };
    anyhow::ensure!(echo.window_id() == WINDOW_ID, "echo for wrong window");
    anyhow::ensure!(
        echo.slot() == 9,
        "echo for slot {}; expected 9",
        echo.slot()
    );
    anyhow::ensure!(
        echo.state_id() == 2,
        "state id {}; expected bump to 2",
        echo.state_id()
    );
    let mut reader = BoundedReader::new(echo.item());
    let decoded = read_trusted_slot(&mut reader)?;
    // Count clamped 200 -> 64; the dangerous container component is stripped.
    anyhow::ensure!(
        decoded == Some((64, GLASS_ITEM)),
        "echoed slot was {decoded:?}; expected normalized glass x64 with no components",
    );

    // A click on window 0 triggers a resync with a bumped state id (now 3).
    builder
        .send_frame(&encode(|buf| {
            WindowClick::new(WINDOW_ID, 2, Vec::new()).encode(buf)
        }))
        .await?;
    let (resync_state, _) = next_container_content(&mut builder).await?;
    anyhow::ensure!(
        resync_state == 3,
        "resync state id was {resync_state}; expected 3",
    );

    Ok(())
}

async fn non_creative_ignored_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let mut builder = login_to_play(addr, "Builder").await?;
    // Consume the join container content first so the barrier below only sees the
    // resync, never the join content.
    let _ = next_container_content(&mut builder).await?;

    // Switch the (operator) player to survival; wait for the confirming GameEvent.
    builder
        .send_frame(&encode(|buf| {
            ChatCommand::new(BoundedString::<256>::new("gamemode 0".to_string()).unwrap())
                .encode(buf)
        }))
        .await?;
    loop {
        if let ClientboundPlayPacket::GameEvent(event) = builder.next_play().await? {
            if event.reason() == GAME_EVENT_CHANGE_GAMEMODE {
                anyhow::ensure!(event.value() == 0.0, "expected survival (0.0)");
                break;
            }
        }
    }

    // A creative slot from a now-survival player must be ignored (no echo). A click
    // afterward still forces a resync, which acts as a barrier: if the creative
    // slot HAD been processed, a Set Container Slot would arrive before the resync.
    builder
        .send_frame(&encode(|buf| {
            SetCreativeSlot::new(9, hostile_glass_slot()).encode(buf)
        }))
        .await?;
    builder
        .send_frame(&encode(|buf| {
            WindowClick::new(WINDOW_ID, 1, Vec::new()).encode(buf)
        }))
        .await?;
    loop {
        match builder.next_play().await? {
            ClientboundPlayPacket::SetContainerSlot(_) => {
                anyhow::bail!("a non-creative Set Creative Slot was processed (got an echo)");
            }
            ClientboundPlayPacket::SetContainerContent(_) => break, // the resync barrier
            _ => {}
        }
    }

    Ok(())
}

#[tokio::test]
async fn join_kit_and_held_block_placement() {
    let (server, addr) = start_server("").await;
    timeout(GUARD, join_kit_flow(addr))
        .await
        .expect("flow finished within the guard")
        .expect("flow succeeded");
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown within the guard")
        .expect("clean shutdown");
}

#[tokio::test]
async fn creative_slot_normalizes_echoes_and_click_resyncs() {
    let (server, addr) = start_server("").await;
    timeout(GUARD, creative_slot_flow(addr))
        .await
        .expect("flow finished within the guard")
        .expect("flow succeeded");
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown within the guard")
        .expect("clean shutdown");
}

#[tokio::test]
async fn non_creative_player_cannot_set_creative_slot() {
    // Builder must be an operator to run /gamemode.
    let (server, addr) = start_server("ops = [\"Builder\"]").await;
    timeout(GUARD, non_creative_ignored_flow(addr))
        .await
        .expect("flow finished within the guard")
        .expect("flow succeeded");
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown within the guard")
        .expect("clean shutdown");
}
