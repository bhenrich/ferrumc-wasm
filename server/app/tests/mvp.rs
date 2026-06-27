//! The MVP acceptance suite: the ten-point vertical slice driven end to end by a
//! hand-rolled 1.21.8 fake client over a real socket, plus the spawn-protection
//! plugin loaded dynamically and enforced in-process.
//!
//! Each of the ten MVP points is asserted against the closest observable the fake
//! client (or the server's public API) can reach:
//!
//! 1. **status ping** — a `next_state = 1` handshake gets a `StatusResponse`
//!    advertising protocol 772, then a `PongResponse` echoing the ping payload.
//! 2. **offline login** — a client reaches play (`JoinGame`).
//! 3. **flat world** — the client receives `ChunkDataAndLight` for the spawn area.
//! 4. **movement updates sim** — one client moves and another sees the broadcast.
//! 5. **two clients see each other** — player-list add + entity spawn each way.
//! 6. **break/place updates chunk + viewers** — viewers receive `BlockUpdate`s.
//! 7. **/spawn** — a moved player is teleported back to spawn (viewer-observed).
//! 8. **/gamemode** — accepted over the wire without dropping the connection, and
//!    asserted directly against the command tree (no clientbound carrier exists
//!    for game mode in the pinned packet set).
//! 9. **spawn protection** — an unauthorized break near spawn is vetoed (no
//!    broadcast) while a bypassing player's edits go through.
//! 10. **clean shutdown** — the server winds down within the guard.
//!
//! Plus: the application loads the spawn-protection `cdylib` from a `/plugins`
//! directory across the C ABI ([`ferrumc_app::load_plugins`]).
//!
//! Determinism without wall-clock sleeps: every step awaits the next frame, uses
//! same-shard FIFO ordering as a fence (a vetoed edit is pipelined ahead of an
//! observable move), and the whole flow is wrapped in a timeout guard.

#![allow(clippy::float_cmp)] // Broadcast coordinates are exact, representable values.

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::time::timeout;
use uuid::Uuid;

use ferrumc_app::{build_command_tree, load_plugins, AppConfig};
use ferrumc_codec::{BoundedReader, BoundedString};
use ferrumc_command::{CommandError, CommandSource};
use ferrumc_core::PlayerId;
use ferrumc_proto::generated::handshake::Handshake;
use ferrumc_proto::generated::play::{
    ChatCommand, ClientboundPlayPacket, PlayerAction, PlayerInfoUpdate, SetPlayerPosition,
    UseItemOn,
};
use ferrumc_proto::generated::status::{ClientboundStatusPacket, PingRequest, StatusRequest};
use ferrumc_proto::types::BlockPosition;
use ferrumc_session::PLAYER_INFO_ADD;

use common::{encode, login_to_play, TestClient};

/// Protocol version for Minecraft 1.21.8.
const PROTOCOL_VERSION: i32 = 772;

/// Overall guard so a regression can never hang the suite.
const GUARD: Duration = Duration::from_secs(15);

/// `PlayerAction` status meaning "start destroying block" (creative insta-mine).
const START_DESTROY_BLOCK: i32 = 0;

/// `UseItemOn` face index for the top (`Up`) face.
const FACE_UP: i32 = 1;

/// Block-state id of `minecraft:stone`, the fixed block the server places.
const STONE_STATE: i32 = 1;

/// Spawn-protection radius, in blocks, the MVP server runs with.
const PROTECT_RADIUS: i32 = 16;

// --------------------------------------------------------------------------
// Building and staging the spawn-protection cdylib.
// --------------------------------------------------------------------------

/// The spawn-protect package name (its `cdylib` artifact backs the loader test).
const PLUGIN_PACKAGE: &str = "ferrumc-plugin-spawn-protect";

/// Builds the spawn-protect `cdylib` once per test process and returns its path.
fn plugin_dylib() -> &'static Path {
    static DYLIB: OnceLock<PathBuf> = OnceLock::new();
    DYLIB.get_or_init(build_plugin).as_path()
}

/// Runs `cargo build -p <plugin>` and extracts the dynamic-library artifact path.
fn build_plugin() -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(&cargo)
        .current_dir(workspace_root())
        .args([
            "build",
            "-p",
            PLUGIN_PACKAGE,
            "--message-format=json-render-diagnostics",
        ])
        .output()
        .expect("failed to spawn cargo to build the spawn-protect plugin");
    assert!(
        output.status.success(),
        "building the spawn-protect plugin failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dll_suffix = std::env::consts::DLL_SUFFIX;
    let stdout = String::from_utf8(output.stdout).expect("cargo json output is utf-8");
    let needle = PLUGIN_PACKAGE.replace('-', "_");
    for line in stdout.lines() {
        if !line.contains("\"compiler-artifact\"") || !line.contains(PLUGIN_PACKAGE) {
            continue;
        }
        for candidate in line.split('"') {
            if candidate.contains(&needle) && candidate.ends_with(dll_suffix) {
                return PathBuf::from(candidate);
            }
        }
    }
    panic!("could not find the {dll_suffix} artifact in cargo output:\n{stdout}");
}

/// Returns the Cargo workspace root (the `server/` directory).
fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../server/app
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("app manifest dir has a workspace-root parent")
        .to_path_buf()
}

/// Copies the built plugin into a fresh, isolated `/plugins` directory.
fn stage_plugins_dir() -> tempfile::TempDir {
    let dylib = plugin_dylib();
    let dir = tempfile::tempdir().expect("create plugins dir");
    let dest = dir
        .path()
        .join(dylib.file_name().expect("dylib has a name"));
    std::fs::copy(dylib, &dest).expect("copy plugin into plugins dir");
    // A non-library file the scan must ignore.
    std::fs::write(dir.path().join("README.txt"), b"not a plugin").expect("write decoy");
    dir
}

// --------------------------------------------------------------------------
// Fake-client read helpers.
// --------------------------------------------------------------------------

/// Reads play packets until `pred` matches, returning the matching packet.
async fn read_until<F>(
    client: &mut TestClient,
    mut pred: F,
) -> anyhow::Result<ClientboundPlayPacket>
where
    F: FnMut(&ClientboundPlayPacket) -> bool,
{
    loop {
        let packet = client.next_play().await?;
        if pred(&packet) {
            return Ok(packet);
        }
    }
}

/// Reads the single UUID a minimal `PlayerInfoUpdate` carries (count byte + 16).
fn info_uuid(info: &PlayerInfoUpdate) -> Option<Uuid> {
    let entries = info.entries();
    if entries.first() != Some(&1) || entries.len() < 17 {
        return None;
    }
    Uuid::from_slice(&entries[1..17]).ok()
}

/// Reads until `client` has seen both a player-list add and an entity spawn for
/// `target` — i.e. the two players are mutually visible.
async fn observe_appearance(client: &mut TestClient, target: Uuid) -> anyhow::Result<()> {
    let mut info = false;
    let mut spawn = false;
    while !(info && spawn) {
        match client.next_play().await? {
            ClientboundPlayPacket::PlayerInfoUpdate(p)
                if p.action() == PLAYER_INFO_ADD && info_uuid(&p) == Some(target) =>
            {
                info = true;
            }
            ClientboundPlayPacket::SpawnEntity(s) if s.entity_uuid() == target => spawn = true,
            _ => {}
        }
    }
    Ok(())
}

/// Reads until `client` sees `target` at `expected` (conveyed as a spawn shell).
async fn observe_move(
    client: &mut TestClient,
    target: Uuid,
    expected: (f64, f64, f64),
) -> anyhow::Result<()> {
    read_until(client, |packet| {
        matches!(packet, ClientboundPlayPacket::SpawnEntity(s)
            if s.entity_uuid() == target && (s.x(), s.y(), s.z()) == expected)
    })
    .await
    .map(|_| ())
}

/// Reads until `client` sees a `BlockUpdate` at `pos` carrying `state`.
async fn observe_block_update(
    client: &mut TestClient,
    pos: (i32, i32, i32),
    state: i32,
) -> anyhow::Result<()> {
    read_until(client, |packet| {
        if let ClientboundPlayPacket::BlockUpdate(u) = packet {
            let loc = u.location();
            (loc.x(), loc.y(), loc.z()) == pos && u.block_state() == state
        } else {
            false
        }
    })
    .await
    .map(|_| ())
}

/// Reads until `client` sees `target` move to `move_to`, asserting that **no**
/// `BlockUpdate` at `forbidden` arrives first.
///
/// Because a vetoed break is pipelined ahead of the move from the same actor (and
/// both share the shard inbox FIFO), a leaked edit would surface before the move.
async fn move_without_block_update(
    client: &mut TestClient,
    target: Uuid,
    move_to: (f64, f64, f64),
    forbidden: (i32, i32, i32),
) -> anyhow::Result<()> {
    loop {
        match client.next_play().await? {
            ClientboundPlayPacket::BlockUpdate(u) => {
                let loc = u.location();
                anyhow::ensure!(
                    (loc.x(), loc.y(), loc.z()) != forbidden,
                    "spawn protection leaked: BlockUpdate at the protected position {forbidden:?}",
                );
            }
            ClientboundPlayPacket::SpawnEntity(s)
                if s.entity_uuid() == target && (s.x(), s.y(), s.z()) == move_to =>
            {
                return Ok(());
            }
            _ => {}
        }
    }
}

// --------------------------------------------------------------------------
// Fake-client write helpers.
// --------------------------------------------------------------------------

async fn send_move(client: &mut TestClient, pos: (f64, f64, f64)) -> anyhow::Result<()> {
    client
        .send_frame(&encode(|buf| {
            SetPlayerPosition::new(pos.0, pos.1, pos.2, 0).encode(buf)
        }))
        .await
}

async fn send_break(client: &mut TestClient, pos: (i32, i32, i32)) -> anyhow::Result<()> {
    client
        .send_frame(&encode(|buf| {
            PlayerAction::new(
                START_DESTROY_BLOCK,
                BlockPosition::new(pos.0, pos.1, pos.2),
                1,
                0,
            )
            .encode(buf)
        }))
        .await
}

async fn send_place_on_top(client: &mut TestClient, pos: (i32, i32, i32)) -> anyhow::Result<()> {
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
                0,
            )
            .encode(buf)
        }))
        .await
}

async fn send_command(client: &mut TestClient, command: &str) -> anyhow::Result<()> {
    let command = BoundedString::<256>::new(command.to_string())?;
    client
        .send_frame(&encode(|buf| ChatCommand::new(command.clone()).encode(buf)))
        .await
}

// --------------------------------------------------------------------------
// Point 1: status ping returns a server-list response and pong.
// --------------------------------------------------------------------------

/// The ping payload the status exchange must echo back verbatim.
const STATUS_PING_PAYLOAD: i64 = 0x5151_5151_5151_5151;

/// Drives a `next_state = 1` (status) handshake plus Status Request and Ping
/// Request, asserting the server answers with a `StatusResponse` advertising
/// protocol 772 and a `PongResponse` echoing the ping payload — what a real
/// client needs to render the server in its multiplayer list.
async fn status_ping_responds(addr: SocketAddr) -> anyhow::Result<()> {
    let mut client = TestClient::connect(addr).await?;
    let address = BoundedString::<255>::new("127.0.0.1".to_string())?;
    client
        .send_frame(&encode(|buf| {
            Handshake::new(PROTOCOL_VERSION, address.clone(), addr.port(), 1).encode(buf)
        }))
        .await?;

    // Status Request -> Status Response advertising the 1.21.8 protocol.
    client
        .send_frame(&encode(|buf| StatusRequest.encode(buf)))
        .await?;
    let frame = client.next_frame().await?;
    let mut reader = BoundedReader::new(&frame);
    let id = reader.read_var_int()?;
    let ClientboundStatusPacket::StatusResponse(response) =
        ClientboundStatusPacket::decode(id, &mut reader)?
    else {
        anyhow::bail!("expected a StatusResponse");
    };
    anyhow::ensure!(
        response.json().as_str().contains("\"protocol\":772"),
        "status JSON must advertise protocol 772"
    );

    // Ping Request -> Pong Response echoing the exact payload.
    client
        .send_frame(&encode(|buf| {
            PingRequest::new(STATUS_PING_PAYLOAD).encode(buf)
        }))
        .await?;
    let frame = client.next_frame().await?;
    let mut reader = BoundedReader::new(&frame);
    let id = reader.read_var_int()?;
    let ClientboundStatusPacket::PongResponse(pong) =
        ClientboundStatusPacket::decode(id, &mut reader)?
    else {
        anyhow::bail!("expected a PongResponse");
    };
    anyhow::ensure!(
        pong.payload() == STATUS_PING_PAYLOAD,
        "pong must echo the ping payload"
    );
    Ok(())
}

// --------------------------------------------------------------------------
// Points 2-9: the main end-to-end flow.
// --------------------------------------------------------------------------

async fn run_flow(addr: SocketAddr) -> anyhow::Result<()> {
    let admin_uuid = PlayerId::offline("Admin").as_uuid();
    let viewer_uuid = PlayerId::offline("Viewer").as_uuid();
    let griefer_uuid = PlayerId::offline("Griefer").as_uuid();

    // Point 2 (offline login) + Point 3 (flat world): the viewer reaches play and
    // receives spawn-area chunks.
    let mut viewer = login_to_play(addr, "Viewer").await?;
    read_until(&mut viewer, |p| {
        matches!(p, ClientboundPlayPacket::ChunkDataAndLight(_))
    })
    .await?;

    // A bypassing admin and a non-bypassing griefer also join.
    let mut admin = login_to_play(addr, "Admin").await?;
    let mut griefer = login_to_play(addr, "Griefer").await?;

    // Point 5: the viewer and admin are mutually visible.
    observe_appearance(&mut viewer, admin_uuid).await?;
    observe_appearance(&mut admin, viewer_uuid).await?;

    // Point 4: the admin moves and the viewer sees the broadcast position.
    send_move(&mut admin, (10.0, 64.0, 9.0)).await?;
    observe_move(&mut viewer, admin_uuid, (10.0, 64.0, 9.0)).await?;

    // Point 6 + Point 9 (bypass allowed): the admin breaks and places inside the
    // protected area; both go through because the admin holds bypass.
    send_break(&mut admin, (8, 63, 8)).await?;
    observe_block_update(&mut viewer, (8, 63, 8), 0).await?;
    send_place_on_top(&mut admin, (9, 63, 8)).await?;
    observe_block_update(&mut viewer, (9, 64, 8), STONE_STATE).await?;

    // Point 9 (veto): the griefer's break inside the protected area is vetoed.
    // Pipelined ahead of a move so the move fences the (absent) broadcast.
    send_break(&mut griefer, (10, 63, 8)).await?;
    send_move(&mut griefer, (20.0, 64.0, 20.0)).await?;
    move_without_block_update(&mut viewer, griefer_uuid, (20.0, 64.0, 20.0), (10, 63, 8)).await?;

    // Point 7: /spawn teleports the admin back to spawn; the viewer sees it.
    send_command(&mut admin, "spawn").await?;
    observe_move(&mut viewer, admin_uuid, (8.0, 64.0, 8.0)).await?;

    // Point 8: /gamemode is accepted over the wire (the connection survives), and
    // a follow-up move still broadcasts — proving the command was consumed
    // cleanly. Game mode itself has no clientbound carrier this slice.
    send_command(&mut admin, "gamemode 1").await?;
    send_move(&mut admin, (12.0, 64.0, 12.0)).await?;
    observe_move(&mut viewer, admin_uuid, (12.0, 64.0, 12.0)).await?;

    Ok(())
}

#[tokio::test]
async fn mvp_end_to_end() {
    // The plugins directory holds the freshly-built spawn-protect cdylib.
    let plugins = stage_plugins_dir();

    // Point (dynamic loading): the application loads the cdylib across the C ABI.
    let loaded = load_plugins(plugins.path()).expect("plugins dir scans");
    assert_eq!(loaded, 1, "the spawn-protect cdylib must load");

    // Point 8 (direct, server-side): /gamemode dispatches as a server command.
    let tree = build_command_tree();
    let op = CommandSource::for_player(PlayerId::offline("Admin"), "Admin", 4);
    assert!(tree
        .dispatch("gamemode 1", &op)
        .expect("gamemode dispatches")
        .is_success());
    assert!(matches!(
        tree.dispatch("gamemode 9", &op),
        Err(CommandError::IntegerOutOfRange { .. })
    ));

    // A radius-1 spawn keeps the resident area small; spawn protection is on with
    // the admin granted bypass.
    let config = AppConfig {
        bind: "127.0.0.1:0".parse().expect("valid bind"),
        spawn_chunk_radius: 1,
        spawn_protect_radius: PROTECT_RADIUS,
        spawn_protect_bypass: vec!["Admin".to_string()],
        plugins_dir: Some(plugins.path().to_path_buf()),
        ..AppConfig::default()
    };
    let server = ferrumc_app::run(&config).await.expect("server starts");
    let addr = server.local_addr();

    // Point 1: status ping returns a server-list response and pong.
    timeout(GUARD, status_ping_responds(addr))
        .await
        .expect("status ping finished within the guard")
        .expect("status ping answered");

    // Points 2-9: the full play flow.
    timeout(GUARD, run_flow(addr))
        .await
        .expect("flow finished within the guard")
        .expect("flow succeeded");

    // Point 10: clean shutdown.
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finished within the guard")
        .expect("clean shutdown");
}
