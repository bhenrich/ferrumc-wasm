//! End-to-end multiplayer-visibility test.
//!
//! Starts the real server on an ephemeral port and connects two real
//! [`tokio::net::TcpStream`] clients that log in offline and reach play. It then
//! asserts each client is told about the other — a `PlayerInfoUpdate` (player
//! list add) and a `SpawnEntity` (the other player's entity) — and that when one
//! client moves, the other receives the broadcast position.
//!
//! Determinism without wall-clock sleeps: the first client is driven through its
//! `JoinGame` before the second connects, which guarantees the server has
//! registered it, so the join the second client triggers always sees it. Every
//! step awaits the next frame and the whole flow is wrapped in a timeout guard.

#![allow(clippy::float_cmp)] // Broadcast coordinates are exact, representable values.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::timeout;
use uuid::Uuid;

use ferrumc_codec::{write_var_int, BoundedReader};
use ferrumc_core::PlayerId;
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, PlayerInfoUpdate, SetCreativeSlot, SetPlayerPosition,
};
use ferrumc_session::PLAYER_INFO_ADD;

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

/// `SetEquipment` slot id for the main hand (the lowest-id entry).
const EQUIP_MAIN_HAND: u8 = 0;
/// `SetEquipment` slot id for the helmet (the highest-id armor entry).
const EQUIP_HELMET: u8 = 5;
/// High bit of a `SetEquipment` entry's slot byte: set on every entry but the last.
const EQUIP_CONTINUATION: u8 = 0x80;

/// Window-0 inventory index of the worn helmet (the `SetCreativeSlot` wire slot).
const HELMET_INV_SLOT: i16 = 5;
/// Protocol item id of `minecraft:leather_helmet` and `minecraft:stone` (the held
/// kit item the main hand carries by default).
const LEATHER_HELMET_ITEM: i32 = 913;
const STONE_ITEM: i32 = 1;

/// Reads the UUID a `PlayerInfoUpdate` carries in this server's minimal entries
/// layout: a single count byte followed by the 16-byte UUID.
fn player_info_uuid(info: &PlayerInfoUpdate) -> Uuid {
    let entries = info.entries();
    assert_eq!(entries[0], 1, "exactly one player-info entry expected");
    Uuid::from_slice(&entries[1..17]).expect("a 16-byte UUID")
}

/// Reads play packets from `client` until it has seen both a player-list add and
/// an entity spawn for `target`, returning the network entity id of the spawn.
///
/// The spawn must carry the `minecraft:player` entity type (149) so the client
/// renders a player model, not a placeholder — this is what the original
/// entity-type bug (type 0 = a boat) got wrong.
async fn observe_appearance(client: &mut TestClient, target: Uuid) -> anyhow::Result<i32> {
    let mut info = false;
    let mut entity_id = None;
    while !(info && entity_id.is_some()) {
        match client.next_play().await? {
            ClientboundPlayPacket::PlayerInfoUpdate(p)
                if p.action() == PLAYER_INFO_ADD && player_info_uuid(&p) == target =>
            {
                info = true;
            }
            ClientboundPlayPacket::SpawnEntity(s) if s.entity_uuid() == target => {
                anyhow::ensure!(
                    s.entity_type() == 149,
                    "remote player must spawn as minecraft:player (149), got {}",
                    s.entity_type()
                );
                entity_id = Some(s.entity_id());
            }
            _ => {}
        }
    }
    Ok(entity_id.expect("entity id captured"))
}

/// Reads play packets from `client` until it sees `entity_id` teleported to
/// `expected` — the carrier for a move larger than 8 blocks.
async fn observe_teleport(
    client: &mut TestClient,
    entity_id: i32,
    expected: (f64, f64, f64),
) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::EntityTeleport(t) = client.next_play().await? {
            if t.entity_id() == entity_id && (t.x(), t.y(), t.z()) == expected {
                return Ok(());
            }
        }
    }
}

/// Reads play packets from `client` until it sees `entity_id` despawned via a
/// Remove Entities packet — the leave-side counterpart of the spawn.
async fn observe_despawn(client: &mut TestClient, entity_id: i32) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::RemoveEntities(r) = client.next_play().await? {
            if r.entity_ids().contains(&entity_id) {
                return Ok(());
            }
        }
    }
}

/// The body of the test, run under the timeout guard.
async fn run_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let saad = PlayerId::offline("Saad").as_uuid();
    let notch = PlayerId::offline("Notch").as_uuid();

    // First client joins alone, driven through JoinGame so it is registered.
    let mut c1 = login_to_play(addr, "Saad").await?;
    // Second client joins; the server now makes the two mutually visible.
    let mut c2 = login_to_play(addr, "Notch").await?;

    // Each client is told about the other (player-list add + entity spawn). Keep
    // Notch's entity id so c1 can match the relative/teleport move that follows.
    let notch_eid = observe_appearance(&mut c1, notch).await?;
    observe_appearance(&mut c2, saad).await?;

    // One client jumps far (spawn 8,64,8 -> 20,64,20, ~17 blocks); the other must
    // receive it as an absolute EntityTeleport (a relative move cannot encode it).
    let new_pos = (20.0_f64, 64.0_f64, 20.0_f64);
    c2.send_frame(&encode(|buf| {
        SetPlayerPosition::new(new_pos.0, new_pos.1, new_pos.2, 0).encode(buf)
    }))
    .await?;
    observe_teleport(&mut c1, notch_eid, new_pos).await?;

    // Notch leaves (drop the socket): Saad must see the entity despawn, not a
    // lingering ghost. This is the RemoveEntities the leave path now broadcasts.
    drop(c2);
    observe_despawn(&mut c1, notch_eid).await?;

    Ok(())
}

/// Decodes a `SetEquipment` body into `(equipment-slot id, item id)` pairs, with
/// `None` for an air (empty) slot. Test slots are component-free, so each non-air
/// Slot is `count, item_id, 0 added, 0 removed`.
fn decode_equipment(body: &[u8]) -> anyhow::Result<Vec<(u8, Option<i32>)>> {
    let mut reader = BoundedReader::new(body);
    let mut entries = Vec::new();
    loop {
        let slot_byte = reader.read_u8()?;
        let more = slot_byte & EQUIP_CONTINUATION != 0;
        let slot = slot_byte & !EQUIP_CONTINUATION;
        let count = reader.read_var_int()?;
        let item = if count == 0 {
            None
        } else {
            let id = reader.read_var_int()?;
            let added = reader.read_var_int()?;
            let removed = reader.read_var_int()?;
            anyhow::ensure!(
                added == 0 && removed == 0,
                "test only decodes component-free equipment slots",
            );
            Some(id)
        };
        entries.push((slot, item));
        if !more {
            break;
        }
    }
    Ok(entries)
}

/// Builds a component-free `Set Creative Slot` item body for `item` (count 1), or
/// the empty/air slot when `item` is `None`.
fn creative_item_bytes(item: Option<i32>) -> Vec<u8> {
    let mut buf = Vec::new();
    match item {
        None => write_var_int(&mut buf, 0), // itemCount 0 = air
        Some(id) => {
            write_var_int(&mut buf, 1); // itemCount
            write_var_int(&mut buf, id); // itemId
            write_var_int(&mut buf, 0); // addedCount
            write_var_int(&mut buf, 0); // removedCount
        }
    }
    buf
}

/// Reads play packets from `client` until it sees a `SetEquipment` for `entity_id`
/// whose helmet entry (equip slot 5) carries `expected_helmet`, returning that
/// packet's decoded entries so the caller can assert the rest of the set.
async fn observe_equipment(
    client: &mut TestClient,
    entity_id: i32,
    expected_helmet: Option<i32>,
) -> anyhow::Result<Vec<(u8, Option<i32>)>> {
    loop {
        if let ClientboundPlayPacket::SetEquipment(equip) = client.next_play().await? {
            if equip.entity_id() != entity_id {
                continue;
            }
            let entries = decode_equipment(equip.equipments())?;
            let helmet = entries.iter().find(|(slot, _)| *slot == EQUIP_HELMET);
            if helmet.map(|(_, item)| *item) == Some(expected_helmet) {
                return Ok(entries);
            }
        }
    }
}

/// The body of the armor-broadcast test, run under the timeout guard.
async fn run_armor_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let saad = PlayerId::offline("Saad").as_uuid();
    let notch = PlayerId::offline("Notch").as_uuid();

    let mut c1 = login_to_play(addr, "Saad").await?;
    let mut c2 = login_to_play(addr, "Notch").await?;

    // Each client sees the other; keep Saad's entity id so Notch can match Saad's
    // equipment broadcasts.
    let saad_eid = observe_appearance(&mut c2, saad).await?;
    observe_appearance(&mut c1, notch).await?;

    // Saad (creative by default) puts a leather helmet in armor slot 5. Notch must
    // receive a SetEquipment whose helmet entry carries the leather helmet, and
    // whose full set still reports Saad's held stone in the main hand.
    c1.send_frame(&encode(|buf| {
        SetCreativeSlot::new(
            HELMET_INV_SLOT,
            creative_item_bytes(Some(LEATHER_HELMET_ITEM)),
        )
        .encode(buf)
    }))
    .await?;
    let entries = observe_equipment(&mut c2, saad_eid, Some(LEATHER_HELMET_ITEM)).await?;
    // The broadcast is the full six-slot set, not just the helmet.
    anyhow::ensure!(entries.len() == 6, "expected the full equipment set");
    let main_hand = entries
        .iter()
        .find(|(slot, _)| *slot == EQUIP_MAIN_HAND)
        .and_then(|(_, item)| *item);
    anyhow::ensure!(
        main_hand == Some(STONE_ITEM),
        "main hand should still carry Saad's held stone, got {main_hand:?}",
    );

    // Clearing the helmet broadcasts an air entry, not a stale render.
    c1.send_frame(&encode(|buf| {
        SetCreativeSlot::new(HELMET_INV_SLOT, creative_item_bytes(None)).encode(buf)
    }))
    .await?;
    observe_equipment(&mut c2, saad_eid, None).await?;

    Ok(())
}

#[tokio::test]
async fn two_clients_see_each_other_and_movement() {
    // Ephemeral port; radius-1 spawn keeps the chunk payload small. Both players
    // join at the default spawn, so they start in view distance of each other.
    let config = AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, run_flow(addr))
        .await
        .expect("multiplayer flow finished within the timeout guard")
        .expect("multiplayer flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the timeout guard")
        .expect("clean shutdown");
}

#[tokio::test]
async fn worn_armor_is_broadcast_to_viewers() {
    // Two players in view of each other; one wears (then removes) a helmet and the
    // other must see the SetEquipment broadcast carrying it in the helmet slot.
    let config = AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, run_armor_flow(addr))
        .await
        .expect("armor flow finished within the timeout guard")
        .expect("armor flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the timeout guard")
        .expect("clean shutdown");
}
