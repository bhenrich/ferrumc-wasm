//! End-to-end chat and command-feedback tests.
//!
//! Starts the real server on an ephemeral port and drives hand-rolled 1.21.8
//! clients over real sockets (the shared [`common`] harness) to assert the three
//! text-carrying paths this milestone wired:
//!
//! 1. a slash command's outcome reaches the issuer as a clientbound `SystemChat`
//!    (command feedback was previously computed and discarded);
//! 2. `/gamemode <id>` additionally emits a `GameEvent` with reason `3`
//!    (`change_game_mode`) so the client actually switches mode; and
//! 3. a serverbound chat message (`0x08`, previously dropped as an unknown id) is
//!    relayed to *every* player as an unsigned `SystemChat`.
//!
//! Determinism without wall-clock sleeps: every step awaits the next frame and the
//! whole flow is wrapped in a timeout guard.

#![allow(clippy::float_cmp)] // The game-mode id is an exact, representable float.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::timeout;

use ferrumc_codec::BoundedString;
use ferrumc_nbt::NbtTag;
use ferrumc_proto::generated::play::{ChatCommand, ChatMessage, ClientboundPlayPacket};

use ferrumc_app::AppConfig;

use common::{encode, login_to_play, TestClient};

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(15);

/// `GameEvent` reason for "change game mode".
const CHANGE_GAME_MODE: u8 = 3;

/// Extracts the literal `text` of a network-NBT text component (the `content` of a
/// `SystemChat`), panicking if it is not the expected `{ text: "..." }` compound.
fn nbt_text(content: &NbtTag) -> String {
    let NbtTag::Compound(root) = content else {
        panic!("system chat content must be a compound, got {content:?}");
    };
    match root.get("text") {
        Some(NbtTag::String(text)) => text.clone(),
        other => panic!("expected a `text` string in the component, got {other:?}"),
    }
}

/// Sends a serverbound `/command` (unsigned chat command).
async fn send_command(client: &mut TestClient, command: &str) -> anyhow::Result<()> {
    let command = BoundedString::<256>::new(command.to_string())?;
    client
        .send_frame(&encode(|buf| ChatCommand::new(command.clone()).encode(buf)))
        .await
}

/// Sends a serverbound plain chat message (`0x08`), with an empty signing tail
/// (the server ignores it: `enforces_secure_chat = false`).
async fn send_chat(client: &mut TestClient, message: &str) -> anyhow::Result<()> {
    let message = BoundedString::<256>::new(message.to_string())?;
    client
        .send_frame(&encode(|buf| {
            ChatMessage::new(message.clone(), Vec::new()).encode(buf)
        }))
        .await
}

/// Reads play packets from `client` until it sees a `SystemChat` whose text
/// contains `needle`, returning the full rendered text.
async fn read_system_chat_containing(
    client: &mut TestClient,
    needle: &str,
) -> anyhow::Result<String> {
    loop {
        if let ClientboundPlayPacket::SystemChat(chat) = client.next_play().await? {
            let text = nbt_text(chat.content());
            if text.contains(needle) {
                return Ok(text);
            }
        }
    }
}

/// `/gamemode` over the wire feeds back to the issuer AND switches their mode.
async fn gamemode_feedback_and_event(addr: SocketAddr) -> anyhow::Result<()> {
    // The issuing player must be an operator for `/gamemode` (level 2) to run.
    let mut player = login_to_play(addr, "GmTester").await?;
    send_command(&mut player, "gamemode 1").await?;

    // Two distinct deliveries on the issuer's own stream: the feedback SystemChat
    // and the GameEvent that actually changes the mode. Scan until both arrive
    // (the join also emits a GameEvent with reason 13, which is ignored here).
    let mut saw_feedback = false;
    let mut saw_event = false;
    while !(saw_feedback && saw_event) {
        match player.next_play().await? {
            ClientboundPlayPacket::SystemChat(chat) => {
                if nbt_text(chat.content()).contains("Game mode set to") {
                    saw_feedback = true;
                }
            }
            ClientboundPlayPacket::GameEvent(event) if event.reason() == CHANGE_GAME_MODE => {
                assert_eq!(
                    event.value(),
                    1.0,
                    "value carries the game-mode id (creative)"
                );
                saw_event = true;
            }
            _ => {}
        }
    }
    Ok(())
}

/// A non-operator's `/gamemode` is refused with a red feedback line, not silence.
async fn rejected_command_feedback(addr: SocketAddr) -> anyhow::Result<()> {
    // A plain player (not in `ops`) cannot run the operator-gated command.
    let mut player = login_to_play(addr, "Plebeian").await?;
    send_command(&mut player, "gamemode 1").await?;
    let text = read_system_chat_containing(&mut player, "permission denied").await?;
    assert!(
        text.contains("permission denied"),
        "a denied command must report why, got {text:?}"
    );
    Ok(())
}

/// A serverbound chat message is relayed to every player as a `SystemChat`.
async fn chat_relays_to_all(addr: SocketAddr) -> anyhow::Result<()> {
    let mut alice = login_to_play(addr, "Alice").await?;
    let mut bob = login_to_play(addr, "Bob").await?;

    send_chat(&mut alice, "hello world").await?;

    // Both the sender and the other player receive the formatted relay.
    let expected = "<Alice> hello world";
    let alice_seen = read_system_chat_containing(&mut alice, expected).await?;
    let bob_seen = read_system_chat_containing(&mut bob, expected).await?;
    assert!(alice_seen.contains(expected));
    assert!(bob_seen.contains(expected));
    Ok(())
}

/// Command feedback reaches the issuer and *only* the issuer: a rejection meant
/// for one player must never be broadcast to other connected players.
async fn command_feedback_reaches_only_the_issuer(addr: SocketAddr) -> anyhow::Result<()> {
    let mut alice = login_to_play(addr, "Alice").await?;
    let mut bob = login_to_play(addr, "Bob").await?;

    // Alice is not an operator, so `/gamemode` is refused; the rejection is
    // enqueued on Alice's own writer and is never routed to Bob.
    send_command(&mut alice, "gamemode 1").await?;
    // Confirm the rejection really reached Alice, so the negative check below is
    // meaningful rather than vacuous.
    let alice_seen = read_system_chat_containing(&mut alice, "permission denied").await?;
    assert!(alice_seen.contains("permission denied"));

    // Drive Bob's stream to a definite fence — his own relayed chat, which does
    // travel to everyone — and assert the rejection never appears on it. In
    // correct code the feedback is never routed to Bob, so this loop terminates
    // and passes deterministically; it can only fail if feedback regresses to
    // being broadcast.
    send_chat(&mut bob, "fence").await?;
    loop {
        if let ClientboundPlayPacket::SystemChat(chat) = bob.next_play().await? {
            let text = nbt_text(chat.content());
            assert!(
                !text.contains("permission denied"),
                "command feedback must reach only the issuer, but Bob saw {text:?}"
            );
            if text.contains("<Bob> fence") {
                return Ok(());
            }
        }
    }
}

/// Legacy section-sign (§) formatting codes in a player's message are stripped
/// from the relayed line, so a player cannot inject colour/obfuscation codes.
async fn section_codes_are_stripped_from_relay(addr: SocketAddr) -> anyhow::Result<()> {
    let mut player = login_to_play(addr, "Trickster").await?;
    // `§l` is bold and `§r` is reset. Only the section signs are removed; the
    // following code letters remain as ordinary text (the client applies no
    // formatting without the leading §).
    send_chat(&mut player, "\u{00A7}lbold \u{00A7}rnormal").await?;
    let text = read_system_chat_containing(&mut player, "bold").await?;
    assert!(
        !text.contains('\u{00A7}'),
        "no section sign may survive in the relayed line, got {text:?}"
    );
    assert_eq!(text, "<Trickster> lbold rnormal");
    Ok(())
}

#[tokio::test]
async fn command_feedback_and_gamemode_reach_the_issuer() {
    let config = AppConfig::from_toml_str(
        "bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1\nops = [\"GmTester\"]",
    )
    .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, gamemode_feedback_and_event(addr))
        .await
        .expect("gamemode flow finished within the guard")
        .expect("gamemode feedback + event delivered");

    timeout(GUARD, rejected_command_feedback(addr))
        .await
        .expect("rejection flow finished within the guard")
        .expect("rejected command feedback delivered");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the guard")
        .expect("clean shutdown");
}

#[tokio::test]
async fn serverbound_chat_relays_to_all_players() {
    let config = AppConfig::from_toml_str("bind = \"127.0.0.1:0\"\nspawn_chunk_radius = 1")
        .expect("config parses");
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    timeout(GUARD, chat_relays_to_all(addr))
        .await
        .expect("chat relay finished within the guard")
        .expect("chat relayed to all players");

    timeout(GUARD, command_feedback_reaches_only_the_issuer(addr))
        .await
        .expect("feedback-isolation flow finished within the guard")
        .expect("command feedback reached only the issuer");

    timeout(GUARD, section_codes_are_stripped_from_relay(addr))
        .await
        .expect("section-code flow finished within the guard")
        .expect("section codes stripped from relay");

    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the guard")
        .expect("clean shutdown");
}
