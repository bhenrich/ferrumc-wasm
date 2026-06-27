//! End-to-end block break/place test.
//!
//! Starts the real server on an ephemeral port and connects two real
//! [`tokio::net::TcpStream`] clients that log in offline and reach play: an
//! `actor` that breaks and places blocks, and a `viewer` that must observe the
//! resulting `BlockUpdate` broadcasts.
//!
//! It asserts the slice's behaviours:
//! - a break of a nearby loaded block is accepted and broadcast (air), and the
//!   acting player receives an `AcknowledgeBlockChange` echoing its sequence;
//! - a place against a clicked face is accepted, broadcast (the fixed default
//!   block, `minecraft:stone` = state id `1`), and acked;
//! - an out-of-reach, unloaded-chunk break is rejected silently — proven by the
//!   actor sending it *before* a valid break and the viewer's first `BlockUpdate`
//!   being the valid block, never the rejected position;
//! - an out-of-reach break in a *loaded* chunk is rejected with a resync: the
//!   actor alone receives a `BlockUpdate` carrying the authoritative non-air
//!   state and then an `AcknowledgeBlockChange` echoing that sequence — the ack
//!   is what actually ends the client's prediction and reverts the ghost block.
//!
//! Determinism without wall-clock sleeps: the actor pipelines the rejected break
//! ahead of the valid break, so they share the shard inbox in order; readers scan
//! frames until the expected packet, and the whole flow is wrapped in a timeout
//! guard.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_proto::generated::play::{ClientboundPlayPacket, PlayerAction, UseItemOn};
use ferrumc_proto::types::BlockPosition;

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(10);

/// `PlayerAction` status meaning "start destroying block" (creative insta-mine).
const START_DESTROY_BLOCK: i32 = 0;

/// `UseItemOn` face index for the top (`Up`) face.
const FACE_UP: i32 = 1;

/// Block-state id of `minecraft:stone`, the fixed block the server places.
const STONE_STATE: i32 = 1;

/// Block-state id of air (a broken block), `0` in the pinned flat-world registry.
const AIR_STATE: i32 = 0;

/// Reads play packets from `client` until it sees a `BlockUpdate`, then asserts
/// that update is at `pos` carrying `state`.
///
/// The *first* `BlockUpdate` must match: a mismatch means a change that should
/// have been rejected (or a different edit) leaked through, which fails the test.
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
                "unexpected BlockUpdate at ({}, {}, {}); expected ({}, {}, {})",
                loc.x(),
                loc.y(),
                loc.z(),
                pos.0,
                pos.1,
                pos.2,
            );
            anyhow::ensure!(
                update.block_state() == state,
                "BlockUpdate at {pos:?} carried state {}; expected {state}",
                update.block_state(),
            );
            return Ok(());
        }
    }
}

/// Reads play packets from `client` until it sees an `AcknowledgeBlockChange`,
/// then asserts it echoes `expected_sequence`.
///
/// Non-ack packets (the actor's own broadcast `BlockUpdate`s, join visibility,
/// chunk data) are skipped; the first ack seen must carry the expected sequence.
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

/// Reads play packets from `client` until it sees the resync `BlockUpdate` at
/// `pos` carrying the authoritative non-air surface state, then asserts an
/// `AcknowledgeBlockChange` echoing `sequence` follows it.
///
/// A rejected edit heals on a real 1.21.8 client only when the ack ends the
/// pending prediction (endPredictionsUpTo) — the `BlockUpdate` alone is
/// swallowed while the prediction is pending. So both must arrive, in order: the
/// `BlockUpdate` sets the authoritative state and the ack reveals it. An ack
/// arriving *before* the resync fails the test.
async fn expect_resync_then_ack(
    client: &mut TestClient,
    pos: (i32, i32, i32),
    sequence: i32,
) -> anyhow::Result<()> {
    loop {
        match client.next_play().await? {
            ClientboundPlayPacket::AcknowledgeBlockChange(ack) => anyhow::bail!(
                "got an AcknowledgeBlockChange (sequence {}) before the resync BlockUpdate",
                ack.sequence(),
            ),
            ClientboundPlayPacket::BlockUpdate(update) => {
                let loc = update.location();
                if (loc.x(), loc.y(), loc.z()) == pos {
                    anyhow::ensure!(
                        update.block_state() != AIR_STATE,
                        "resync BlockUpdate at {pos:?} carried air; expected the authoritative \
                         non-air surface state",
                    );
                    break;
                }
                // A BlockUpdate at another position (e.g. an actor resync for an
                // out-of-reach edit in a different chunk) is not what we wait for.
            }
            _ => {}
        }
    }
    // The ack must follow the resync: it is what ends the client's prediction.
    expect_ack(client, sequence).await
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

/// Sends a `UseItemOn` clicking the top face of the block at `pos` (placing on
/// the block one step up), stamped with `sequence`.
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

/// The body of the test, run under the timeout guard.
async fn run_flow(addr: SocketAddr) -> anyhow::Result<()> {
    // The viewer joins first (driven through JoinGame so it is registered), then
    // the actor. Both spawn at the default (8, 64, 8), sharing the spawn chunk.
    let mut viewer = login_to_play(addr, "Viewer").await?;
    let mut actor = login_to_play(addr, "Actor").await?;

    // Rejected break: (100, 63, 8) is in an unloaded chunk and far out of reach
    // (radius-1 spawn keeps only chunks within one of spawn resident), so it heals
    // when the column streams in — no broadcast, no ack. Pipelined ahead of a valid
    // break of the grass surface directly under the actor.
    send_break(&mut actor, (100, 63, 8), 1).await?;
    send_break(&mut actor, (8, 63, 8), 2).await?;

    // The first BlockUpdate the viewer sees must be the valid break (air); if the
    // rejected break had leaked it would arrive first (it is earlier in the inbox)
    // and this would fail.
    expect_block_update(&mut viewer, (8, 63, 8), AIR_STATE).await?;
    // (a) Accepted break: the actor is acked with the break's sequence (it also
    // receives the broadcast BlockUpdate, so read until the ack).
    expect_ack(&mut actor, 2).await?;

    // Place: click the top of (9, 63, 8) -> stone appears at (9, 64, 8).
    send_place_on_top(&mut actor, (9, 63, 8), 3).await?;
    expect_block_update(&mut viewer, (9, 64, 8), STONE_STATE).await?;
    // (a) Accepted place: the actor is acked with the place's sequence.
    expect_ack(&mut actor, 3).await?;

    // (b) Rejected, in a LOADED chunk but out of reach: (8, 63, 30) is in chunk
    // (0, 1), resident under radius-1 spawn, ~22 blocks from spawn (> MAX_REACH).
    // The actor must be resynced with the authoritative non-air surface state and
    // then acked for sequence 4 (the ack is what reverts the predicted break).
    send_break(&mut actor, (8, 63, 30), 4).await?;
    expect_resync_then_ack(&mut actor, (8, 63, 30), 4).await?;

    Ok(())
}

#[tokio::test]
async fn break_and_place_broadcast_block_updates_and_reject_out_of_reach() {
    // Ephemeral port; radius-1 spawn keeps the resident area small so an
    // out-of-reach block is also genuinely unloaded.
    let config = AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, run_flow(addr))
        .await
        .expect("block flow finished within the timeout guard")
        .expect("block flow succeeded");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the timeout guard")
        .expect("clean shutdown");
}
