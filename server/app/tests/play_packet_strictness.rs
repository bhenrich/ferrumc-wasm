//! Real-socket regressions for strict serverbound Play packet exhaustion.
//!
//! A malformed known frame is pipelined with a state-mutating creative-slot
//! carrier in one socket write. The server must classify and close on the first
//! frame, so the carrier can never execute. Protocol-valid but currently
//! unmodelled packet ids remain explicitly tolerated.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_codec::{write_var_int, BoundedReader, BoundedString};
use ferrumc_core::PlayerId;
use ferrumc_net::DisconnectReason;
use ferrumc_observability::{MetricsSnapshot, PacketState, SnapshotPublisher};
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, ConfirmTeleportation, ServerboundKeepAlive, SetCreativeSlot,
    TabCompleteRequest,
};

use ferrumc_app::{AppConfig, RunningServer};

use common::{encode, login_to_play, TestClient};

/// Overall guard so a connection or shutdown regression cannot hang the suite.
const GUARD: Duration = Duration::from_secs(20);

/// Protocol 772's valid, currently unmodelled, empty-body Tick End packet id.
const TICK_END_PACKET_ID: i32 = 0x0c;

/// An id outside protocol 772's complete serverbound Play range (`0x00..=0x41`).
const INVALID_PLAY_PACKET_ID: i32 = 0x77;

/// A transaction id no server-originated test traffic can accidentally match.
const SENTINEL_TRANSACTION: i32 = 0x5A17;

/// Empty inventory slot used by the pipelined stateful carrier.
const CARRIER_SLOT: i16 = 10;

/// Distinct slot kept populated to prove error teardown really persisted state.
const PERSISTENCE_MARKER_SLOT: i16 = 11;

/// Protocol item id of `minecraft:stone`.
const STONE_ITEM_ID: i32 = 1;

/// One malformed body and its canonical decode classification.
struct MalformedCase {
    name: &'static str,
    body: Vec<u8>,
    metric_label: &'static str,
    reason: DisconnectReason,
    rejects_creative_echo: bool,
}

/// Builds a server with minimal chunk traffic and returns its bound address.
async fn start_server() -> anyhow::Result<(RunningServer, SocketAddr)> {
    let config = AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 0")?;
    let server = ferrumc_app::run(&config).await?;
    let addr = server.local_addr();
    Ok((server, addr))
}

/// Waits for the driver-queued time packet emitted only after the join kit.
async fn wait_until_play_loop(client: &mut TestClient) -> anyhow::Result<()> {
    loop {
        if matches!(
            client.next_play().await?,
            ClientboundPlayPacket::UpdateTime(_)
        ) {
            return Ok(());
        }
    }
}

/// Encodes the valid tab-complete packet used as a liveness/fail-stop sentinel.
fn sentinel_body() -> Vec<u8> {
    encode(|buf| {
        TabCompleteRequest::new(
            SENTINEL_TRANSACTION,
            BoundedString::<32_767>::new("/".to_owned())?,
        )
        .encode(buf)
    })
}

/// Proves the sentinel path is live and drains its response.
async fn prove_sentinel(client: &mut TestClient) -> anyhow::Result<()> {
    client.send_frame(&sentinel_body()).await?;
    loop {
        if let ClientboundPlayPacket::TabCompleteResponse(response) = client.next_play().await? {
            if response.transaction_id() == SENTINEL_TRANSACTION {
                return Ok(());
            }
        }
    }
}

/// Reads until the peer closes, rejecting any flushed post-error effect.
async fn expect_closed_without_effects(
    client: &mut TestClient,
    case: &MalformedCase,
) -> anyhow::Result<()> {
    while let Some(packet) = client.next_play_or_closed().await? {
        match packet {
            ClientboundPlayPacket::SetContainerSlot(slot) if slot.slot() == CARRIER_SLOT => {
                anyhow::bail!(
                    "{} executed and echoed the pipelined stateful carrier",
                    case.name
                );
            }
            ClientboundPlayPacket::SetContainerSlot(slot)
                if case.rejects_creative_echo && slot.slot() == 9 =>
            {
                anyhow::bail!(
                    "{} mutated and echoed the creative slot before disconnecting",
                    case.name
                );
            }
            _ => {}
        }
    }
    Ok(())
}

/// Waits until the driver publishes this player as authoritatively present.
async fn wait_for_player_presence(
    snapshots: &SnapshotPublisher,
    player: PlayerId,
) -> anyhow::Result<u64> {
    loop {
        let snapshot = snapshots.latest();
        if snapshot
            .players
            .iter()
            .any(|row| row.player_id == player.as_uuid().as_u128())
        {
            return Ok(snapshot.tick);
        }
        tokio::task::yield_now().await;
    }
}

/// Waits until a later snapshot proves the driver's leave was applied.
async fn wait_for_clean_leave(
    snapshots: &SnapshotPublisher,
    player: PlayerId,
    after_tick: u64,
) -> anyhow::Result<()> {
    loop {
        let snapshot = snapshots.latest();
        if snapshot.tick > after_tick
            && !snapshot
                .players
                .iter()
                .any(|row| row.player_id == player.as_uuid().as_u128())
        {
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
}

/// Reads the initial window-0 inventory payload from a joining client.
async fn next_inventory_payload(client: &mut TestClient) -> anyhow::Result<Vec<u8>> {
    loop {
        if let ClientboundPlayPacket::SetContainerContent(content) = client.next_play().await? {
            anyhow::ensure!(
                content.window_id() == 0,
                "initial inventory used wrong window"
            );
            return Ok(content.payload().to_vec());
        }
    }
}

/// Decodes the component-free trusted slots used by the default and saved kit.
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
        "strictness fixture expected component-free saved slots",
    );
    Ok(Some((count, item_id)))
}

/// Encodes one valid component-free stone item in untrusted-slot format.
fn stone_item() -> Vec<u8> {
    let mut item = Vec::new();
    write_var_int(&mut item, 1);
    write_var_int(&mut item, STONE_ITEM_ID);
    write_var_int(&mut item, 0);
    write_var_int(&mut item, 0);
    item
}

/// Encodes a valid Set Creative Slot body.
fn creative_slot_body(slot: i16, item: Vec<u8>) -> Vec<u8> {
    encode(|buf| SetCreativeSlot::new(slot, item).encode(buf))
}

/// Sends one creative-slot mutation and verifies its authoritative echo.
async fn set_slot_and_expect_echo(
    client: &mut TestClient,
    slot: i16,
    item: Vec<u8>,
    expected: Option<(i32, i32)>,
) -> anyhow::Result<()> {
    client.send_frame(&creative_slot_body(slot, item)).await?;
    loop {
        if let ClientboundPlayPacket::SetContainerSlot(echo) = client.next_play().await? {
            if echo.slot() != slot {
                continue;
            }
            let mut reader = BoundedReader::new(echo.item());
            let actual = read_trusted_slot(&mut reader)?;
            reader.finish()?;
            anyhow::ensure!(
                actual == expected,
                "slot {slot} echo was {actual:?}; expected {expected:?}",
            );
            return Ok(());
        }
    }
}

/// Proves the persisted-state carrier is live and leaves its target empty.
async fn prove_stateful_carrier(client: &mut TestClient) -> anyhow::Result<()> {
    set_slot_and_expect_echo(
        client,
        PERSISTENCE_MARKER_SLOT,
        stone_item(),
        Some((1, STONE_ITEM_ID)),
    )
    .await?;
    set_slot_and_expect_echo(client, CARRIER_SLOT, stone_item(), Some((1, STONE_ITEM_ID))).await?;
    set_slot_and_expect_echo(client, CARRIER_SLOT, vec![0], None).await
}

/// Returns one inventory slot from a `SetContainerContent` opaque payload.
fn slot_in_inventory(payload: &[u8], index: usize) -> anyhow::Result<Option<(i32, i32)>> {
    let mut reader = BoundedReader::new(payload);
    let count = usize::try_from(reader.read_var_int()?)?;
    anyhow::ensure!(index < count, "inventory payload omitted slot {index}");
    for current in 0..count {
        let slot = read_trusted_slot(&mut reader)?;
        if current == index {
            return Ok(slot);
        }
    }
    anyhow::bail!("inventory payload omitted slot {index}")
}

/// Returns the current count for one exact Play decode-error label.
fn decode_error_count(snapshot: &MetricsSnapshot, label: &str) -> u64 {
    snapshot
        .packet_decode_error_total
        .entries
        .iter()
        .find(|entry| entry.state == PacketState::Play && entry.packet == label)
        .map_or(0, |entry| entry.count)
}

/// A complete Keep Alive body with optional trailing bytes.
fn keep_alive_body(trailing: &[u8]) -> Vec<u8> {
    let mut body = encode(|buf| ServerboundKeepAlive::new(9).encode(buf));
    body.extend_from_slice(trailing);
    body
}

/// Builds the malformed-input battery.
fn malformed_cases() -> Vec<MalformedCase> {
    let mut truncated_keep_alive = Vec::new();
    write_var_int(&mut truncated_keep_alive, ServerboundKeepAlive::PACKET_ID);
    truncated_keep_alive.extend_from_slice(&[0x00, 0x01, 0x02]);

    let mut overlong_field = Vec::new();
    write_var_int(&mut overlong_field, ConfirmTeleportation::PACKET_ID);
    overlong_field.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x00]);

    let mut invalid_id = Vec::new();
    write_var_int(&mut invalid_id, INVALID_PLAY_PACKET_ID);

    let mut valid_stone_with_junk = stone_item();
    valid_stone_with_junk.extend_from_slice(&[0xDE, 0xAD]);

    vec![
        MalformedCase {
            name: "missing_packet_id",
            body: Vec::new(),
            metric_label: "malformed_body",
            reason: DisconnectReason::MalformedPacket,
            rejects_creative_echo: false,
        },
        MalformedCase {
            name: "truncated_packet_id",
            body: vec![0x80],
            metric_label: "malformed_body",
            reason: DisconnectReason::MalformedPacket,
            rejects_creative_echo: false,
        },
        MalformedCase {
            name: "overlong_packet_id",
            body: vec![0x80, 0x80, 0x80, 0x80, 0x80, 0x00],
            metric_label: "malformed_body",
            reason: DisconnectReason::MalformedPacket,
            rejects_creative_echo: false,
        },
        MalformedCase {
            name: "truncated_known_body",
            body: truncated_keep_alive,
            metric_label: "malformed_body",
            reason: DisconnectReason::MalformedPacket,
            rejects_creative_echo: false,
        },
        MalformedCase {
            name: "overlong_known_field",
            body: overlong_field,
            metric_label: "malformed_body",
            reason: DisconnectReason::MalformedPacket,
            rejects_creative_echo: false,
        },
        MalformedCase {
            name: "known_packet_trailing_junk",
            body: keep_alive_body(&[0xDE, 0xAD]),
            metric_label: "trailing_bytes",
            reason: DisconnectReason::ProtocolViolation,
            rejects_creative_echo: false,
        },
        MalformedCase {
            name: "invalid_packet_id",
            body: invalid_id,
            metric_label: "unknown_packet",
            reason: DisconnectReason::ProtocolViolation,
            rejects_creative_echo: false,
        },
        MalformedCase {
            name: "creative_item_trailing_junk",
            body: encode(|buf| SetCreativeSlot::new(9, valid_stone_with_junk.clone()).encode(buf)),
            metric_label: "trailing_bytes",
            reason: DisconnectReason::ProtocolViolation,
            rejects_creative_echo: true,
        },
    ]
}

/// Exercises every fatal body through a real socket and one-write pipeline.
async fn strictness_flow(server: &RunningServer, addr: SocketAddr) -> anyhow::Result<()> {
    let snapshots = server.snapshot_handle();
    for (index, case) in malformed_cases().iter().enumerate() {
        // The reason is part of each case, and its policy must be the immediate
        // peer-fault path rather than a clean server-shutdown close.
        anyhow::ensure!(
            case.reason.policy() == ferrumc_net::DisconnectPolicy::Immediate,
            "{} did not select immediate disconnect policy",
            case.name
        );

        let before = decode_error_count(&server.metrics().snapshot(), case.metric_label);
        let name = format!("Strict{index}");
        let player = PlayerId::offline(&name);
        let mut client = login_to_play(addr, &name).await?;
        wait_until_play_loop(&mut client).await?;
        prove_sentinel(&mut client).await?;
        prove_stateful_carrier(&mut client).await?;

        let before_disconnect_tick = wait_for_player_presence(&snapshots, player).await?;
        let carrier = creative_slot_body(CARRIER_SLOT, stone_item());
        client
            .send_frames(&[case.body.as_slice(), carrier.as_slice()])
            .await?;
        expect_closed_without_effects(&mut client, case).await?;
        wait_for_clean_leave(&snapshots, player, before_disconnect_tick).await?;

        let after = decode_error_count(&server.metrics().snapshot(), case.metric_label);
        anyhow::ensure!(
            after == before + 1,
            "{} recorded {} {} time(s); expected exactly one",
            case.name,
            case.metric_label,
            after.saturating_sub(before),
        );

        // A queued response can be lost when a later error skips `flush_writer`,
        // so absence on the socket does not prove the carrier was never run.
        // Rejoin every identity and inspect the leave-save. The retained marker
        // proves teardown really persisted this session rather than falling back
        // to a fresh default record; the carrier slot must still be empty.
        let mut rejoined = login_to_play(addr, &name).await?;
        anyhow::ensure!(
            rejoined.login_uuid() == Some(player.as_uuid()),
            "stateful-carrier rejoin used a different identity",
        );
        let payload = next_inventory_payload(&mut rejoined).await?;
        anyhow::ensure!(
            slot_in_inventory(&payload, usize::try_from(PERSISTENCE_MARKER_SLOT)?)?
                == Some((1, STONE_ITEM_ID)),
            "{} did not persist the positive-control marker",
            case.name,
        );
        anyhow::ensure!(
            slot_in_inventory(&payload, usize::try_from(CARRIER_SLOT)?)?.is_none(),
            "{} persisted the pipelined carrier, so the later frame ran",
            case.name,
        );
        if case.rejects_creative_echo {
            anyhow::ensure!(
                slot_in_inventory(&payload, 9)?.is_none(),
                "{} persisted its malformed creative-slot mutation",
                case.name,
            );
        }

        let before_rejoin_leave = wait_for_player_presence(&snapshots, player).await?;
        drop(rejoined);
        wait_for_clean_leave(&snapshots, player, before_rejoin_leave).await?;
    }

    Ok(())
}

/// A real protocol-772 packet omitted from the generated slice stays compatible.
async fn unmodelled_packet_flow(server: &RunningServer, addr: SocketAddr) -> anyhow::Result<()> {
    let mut client = login_to_play(addr, "Unmodelled").await?;
    wait_until_play_loop(&mut client).await?;
    prove_sentinel(&mut client).await?;

    let before = decode_error_count(&server.metrics().snapshot(), "unknown_play");
    let mut tick_end = Vec::new();
    write_var_int(&mut tick_end, TICK_END_PACKET_ID);
    let sentinel = sentinel_body();
    client
        .send_frames(&[tick_end.as_slice(), sentinel.as_slice()])
        .await?;
    prove_sentinel_response(&mut client).await?;
    let after = decode_error_count(&server.metrics().snapshot(), "unknown_play");
    anyhow::ensure!(
        after == before + 1,
        "the valid unmodelled packet was not classified separately",
    );
    Ok(())
}

/// Waits for the already-sent sentinel response.
async fn prove_sentinel_response(client: &mut TestClient) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::TabCompleteResponse(response) = client.next_play().await? {
            if response.transaction_id() == SENTINEL_TRANSACTION {
                return Ok(());
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_play_packets_disconnect_before_pipelined_work() {
    let (server, addr) = start_server().await.expect("server starts");
    let flow = timeout(GUARD, async {
        strictness_flow(&server, addr).await?;
        unmodelled_packet_flow(&server, addr).await
    })
    .await;
    let shutdown = timeout(GUARD, server.shutdown()).await;

    flow.expect("strictness flow finished within the guard")
        .expect("strictness flow succeeded");
    shutdown
        .expect("shutdown finished within the guard")
        .expect("server shut down cleanly");
}
