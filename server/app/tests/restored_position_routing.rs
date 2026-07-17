//! End-to-end restoration across every side of the spawn shard.
//!
//! The current runtime has one authoritative simulation owner. A player may move
//! beyond that owner's canonical 8x8-chunk home, leave, and later rejoin through
//! the world-covering route. This suite proves the position remains authoritative,
//! the target chunk is streamed, the routing entry accepts later movement and
//! leave, and the result survives both a same-process rejoin and a full restart.

mod common;

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::time::timeout;

use ferrumc_core::{GameMode, PlayerId};
use ferrumc_math::Vec3;
use ferrumc_observability::SnapshotPublisher;
use ferrumc_proto::generated::play::{ClientboundPlayPacket, SetPlayerPositionAndRotation};
use ferrumc_storage::{PlayerRecord, PlayerStore, RedbStore, SchemaVersion};

use ferrumc_app::{AppConfig, RunningServer};

use common::{encode, login_to_play, TestClient};

/// Overall guard so a missing route, packet, tick, or teardown cannot hang.
const GUARD: Duration = Duration::from_secs(20);

/// Current app-owned player payload schema.
const PLAYER_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Default configured spawn used by this regression.
const SPAWN: Vec3 = Vec3::new(8.0, 64.0, 8.0);

/// One spawn chunk remains resident through its permanent spawn ticket.
const SPAWN_RESIDENT_CHUNKS: usize = 1;

/// Exact float tolerance; every test coordinate is binary-representable.
const EPSILON: f64 = 1e-9;

/// One position/look marker persisted by a lifecycle stage.
#[derive(Debug, Clone, Copy)]
struct PlayerState {
    position: Vec3,
    yaw: f32,
    pitch: f32,
}

/// One cardinal side of the spawn shard and its three distinguishable saves.
#[derive(Debug, Clone, Copy)]
struct BoundaryCase {
    name: &'static str,
    chunk: (i32, i32),
    first: PlayerState,
    same_process: PlayerState,
    after_restart: PlayerState,
}

/// Every cardinal edge just outside shard `(0, 0)`, which spans blocks
/// `x,z = 0..128`.
const BOUNDARIES: [BoundaryCase; 4] = [
    BoundaryCase {
        name: "BoundaryWest",
        chunk: (-1, 4),
        first: PlayerState {
            position: Vec3::new(-0.5, 64.0, 64.5),
            yaw: 11.25,
            pitch: -3.25,
        },
        same_process: PlayerState {
            position: Vec3::new(-0.25, 64.25, 64.75),
            yaw: 22.5,
            pitch: -6.5,
        },
        after_restart: PlayerState {
            position: Vec3::new(-0.75, 64.5, 64.25),
            yaw: 33.75,
            pitch: -9.75,
        },
    },
    BoundaryCase {
        name: "BoundaryEast",
        chunk: (8, 4),
        first: PlayerState {
            position: Vec3::new(128.5, 64.0, 64.5),
            yaw: 12.25,
            pitch: -4.25,
        },
        same_process: PlayerState {
            position: Vec3::new(128.75, 64.25, 64.75),
            yaw: 23.5,
            pitch: -7.5,
        },
        after_restart: PlayerState {
            position: Vec3::new(128.25, 64.5, 64.25),
            yaw: 34.75,
            pitch: -10.75,
        },
    },
    BoundaryCase {
        name: "BoundaryNorth",
        chunk: (4, -1),
        first: PlayerState {
            position: Vec3::new(64.5, 64.0, -0.5),
            yaw: 13.25,
            pitch: -5.25,
        },
        same_process: PlayerState {
            position: Vec3::new(64.75, 64.25, -0.25),
            yaw: 24.5,
            pitch: -8.5,
        },
        after_restart: PlayerState {
            position: Vec3::new(64.25, 64.5, -0.75),
            yaw: 35.75,
            pitch: -11.75,
        },
    },
    BoundaryCase {
        name: "BoundarySouth",
        chunk: (4, 8),
        first: PlayerState {
            position: Vec3::new(64.5, 64.0, 128.5),
            yaw: 14.25,
            pitch: -6.25,
        },
        same_process: PlayerState {
            position: Vec3::new(64.75, 64.25, 128.75),
            yaw: 25.5,
            pitch: -9.5,
        },
        after_restart: PlayerState {
            position: Vec3::new(64.25, 64.5, 128.25),
            yaw: 36.75,
            pitch: -12.75,
        },
    },
];

/// Creates an isolated world directory under the repository-owned scratch root.
fn temp_world() -> tempfile::TempDir {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".codex-tmp");
    std::fs::create_dir_all(&scratch).expect("create repository scratch directory");
    tempfile::Builder::new()
        .prefix("packet25-restored-position-")
        .tempdir_in(scratch)
        .expect("create test-local world directory")
}

/// Builds the multi-thread runtime used only for async server/store operations.
fn test_runtime() -> Runtime {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build test runtime")
}

/// Builds a durable single-spawn-chunk server on an ephemeral port.
fn persistent_config(world_dir: &Path) -> AppConfig {
    AppConfig::from_toml_str(
        "bind = \"127.0.0.1:0\"\n\
         spawn_chunk_radius = 0\n\
         view_distance = 0\n",
    )
    .expect("restored-position config parses")
    .with_world_dir(Some(world_dir.to_path_buf()))
    .expect("world directory preserves a valid config")
}

/// Runs `future` under the suite's diagnostic timeout guard.
async fn guarded<T>(
    label: &str,
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    timeout(GUARD, future)
        .await
        .map_err(|_| anyhow::anyhow!("{label} exceeded the timeout guard"))?
}

/// Returns whether two positions are exactly equal for this test's coordinates.
fn same_position(actual: Vec3, expected: Vec3) -> bool {
    (actual.x - expected.x).abs() < EPSILON
        && (actual.y - expected.y).abs() < EPSILON
        && (actual.z - expected.z).abs() < EPSILON
}

/// Sends a complete position/look marker through the real Play connection.
async fn send_state(client: &mut TestClient, state: PlayerState) -> anyhow::Result<()> {
    client
        .send_frame(&encode(|buf| {
            SetPlayerPositionAndRotation::new(
                state.position.x,
                state.position.y,
                state.position.z,
                state.yaw,
                state.pitch,
                0,
            )
            .encode(buf)
        }))
        .await
}

/// Drains a fresh player's one-column spawn kit before their first movement.
async fn expect_fresh_spawn(client: &mut TestClient) -> anyhow::Result<()> {
    let mut centered = false;
    let mut synchronized = false;
    let mut chunk = false;
    while !(centered && synchronized && chunk) {
        match client.next_play().await? {
            ClientboundPlayPacket::SetCenterChunk(center)
                if (center.chunk_x(), center.chunk_z()) == (0, 0) =>
            {
                centered = true;
            }
            ClientboundPlayPacket::SynchronizePlayerPosition(sync) => {
                let actual = Vec3::new(sync.x(), sync.y(), sync.z());
                anyhow::ensure!(
                    same_position(actual, SPAWN),
                    "fresh sync {actual:?} != spawn {SPAWN:?}"
                );
                synchronized = true;
            }
            ClientboundPlayPacket::ChunkDataAndLight(column)
                if (column.x(), column.z()) == (0, 0) =>
            {
                chunk = true;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Requires recentering, the target column, and release of the spawn view.
async fn expect_far_stream(client: &mut TestClient, target: (i32, i32)) -> anyhow::Result<()> {
    let mut centered = false;
    let mut chunk = false;
    let mut spawn_unloaded = false;
    while !(centered && chunk && spawn_unloaded) {
        match client.next_play().await? {
            ClientboundPlayPacket::SetCenterChunk(center)
                if (center.chunk_x(), center.chunk_z()) == target =>
            {
                centered = true;
            }
            ClientboundPlayPacket::ChunkDataAndLight(column)
                if (column.x(), column.z()) == target =>
            {
                chunk = true;
            }
            ClientboundPlayPacket::UnloadChunk(column)
                if (column.chunk_x(), column.chunk_z()) == (0, 0) =>
            {
                spawn_unloaded = true;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Requires exact restored state plus the chunk stream for its position.
async fn expect_restored_join(
    client: &mut TestClient,
    expected: PlayerState,
    target: (i32, i32),
    game_mode: GameMode,
    selected_slot: i32,
) -> anyhow::Result<()> {
    let mut synchronized = false;
    let mut centered = false;
    let mut chunk = false;
    let mut spawn_unloaded = target == (0, 0);
    let mut mode_restored = false;
    let mut slot_restored = false;
    while !(synchronized && centered && chunk && spawn_unloaded && mode_restored && slot_restored) {
        match client.next_play().await? {
            ClientboundPlayPacket::SynchronizePlayerPosition(sync) if !synchronized => {
                let actual = Vec3::new(sync.x(), sync.y(), sync.z());
                anyhow::ensure!(
                    same_position(actual, expected.position),
                    "restored sync {actual:?} != expected {:?}",
                    expected.position,
                );
                anyhow::ensure!(
                    f64::from(sync.yaw() - expected.yaw).abs() < EPSILON
                        && f64::from(sync.pitch() - expected.pitch).abs() < EPSILON,
                    "restored look ({}, {}) != expected ({}, {})",
                    sync.yaw(),
                    sync.pitch(),
                    expected.yaw,
                    expected.pitch,
                );
                synchronized = true;
            }
            ClientboundPlayPacket::SetCenterChunk(center)
                if (center.chunk_x(), center.chunk_z()) == target =>
            {
                centered = true;
            }
            ClientboundPlayPacket::ChunkDataAndLight(column)
                if (column.x(), column.z()) == target =>
            {
                chunk = true;
            }
            ClientboundPlayPacket::UnloadChunk(column)
                if (column.chunk_x(), column.chunk_z()) == (0, 0) =>
            {
                spawn_unloaded = true;
            }
            ClientboundPlayPacket::GameEvent(event) if event.reason() == 3 => {
                anyhow::ensure!(
                    f64::from(event.value() - f32::from(game_mode.as_id())).abs() < EPSILON,
                    "restored game mode {} != expected {}",
                    event.value(),
                    game_mode.as_id(),
                );
                mode_restored = true;
            }
            ClientboundPlayPacket::ClientboundSetHeldItem(slot) => {
                anyhow::ensure!(
                    slot.slot() == selected_slot,
                    "restored selected slot {} != expected {selected_slot}",
                    slot.slot(),
                );
                slot_restored = true;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Waits for a later snapshot to expose the exact simulation-owned position.
async fn wait_for_authoritative_position(
    snapshots: &SnapshotPublisher,
    after_tick: u64,
    player: PlayerId,
    expected: PlayerState,
    expected_chunk: (i32, i32),
) -> anyhow::Result<()> {
    loop {
        let snapshot = snapshots.latest();
        if snapshot.tick > after_tick {
            if let Some(row) = snapshot
                .players
                .iter()
                .find(|row| row.player_id == player.as_uuid().as_u128())
            {
                let actual = Vec3::new(row.position.x, row.position.y, row.position.z);
                if same_position(actual, expected.position) {
                    anyhow::ensure!(
                        (row.chunk.x, row.chunk.z) == expected_chunk,
                        "snapshot chunk ({}, {}) != expected {expected_chunk:?}",
                        row.chunk.x,
                        row.chunk.z,
                    );
                    return Ok(());
                }
            }
        }
        tokio::task::yield_now().await;
    }
}

/// Waits until leave routing removed the player and released their target ticket.
async fn wait_for_clean_leave(
    snapshots: &SnapshotPublisher,
    after_tick: u64,
) -> anyhow::Result<()> {
    loop {
        let snapshot = snapshots.latest();
        if snapshot.tick > after_tick
            && snapshot.players_online == 0
            && snapshot.players.is_empty()
            && snapshot.chunks_loaded == SPAWN_RESIDENT_CHUNKS
        {
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
}

/// Establishes one far state through live movement, then proves its leave.
async fn establish_boundary(server: &RunningServer, case: BoundaryCase) -> anyhow::Result<()> {
    let snapshots = server.snapshot_handle();
    let player = PlayerId::offline(case.name);
    let mut client = guarded(
        "fresh boundary login",
        login_to_play(server.local_addr(), case.name),
    )
    .await?;
    guarded("fresh spawn kit", expect_fresh_spawn(&mut client)).await?;

    let before_move = snapshots.latest().tick;
    send_state(&mut client, case.first).await?;
    guarded(
        "far movement chunk stream",
        expect_far_stream(&mut client, case.chunk),
    )
    .await?;
    guarded(
        "authoritative far movement",
        wait_for_authoritative_position(&snapshots, before_move, player, case.first, case.chunk),
    )
    .await?;

    let before_leave = snapshots.latest().tick;
    drop(client);
    guarded(
        "far movement leave cleanup",
        wait_for_clean_leave(&snapshots, before_leave),
    )
    .await
}

/// Restores one state, routes a distinguishable same-chunk move, then leaves.
async fn restore_advance_and_leave(
    server: &RunningServer,
    case: BoundaryCase,
    expected: PlayerState,
    next: PlayerState,
) -> anyhow::Result<()> {
    let snapshots = server.snapshot_handle();
    let player = PlayerId::offline(case.name);
    let before_join = snapshots.latest().tick;
    let mut client = guarded(
        "restored boundary login",
        login_to_play(server.local_addr(), case.name),
    )
    .await?;
    guarded(
        "restored boundary join stream",
        expect_restored_join(&mut client, expected, case.chunk, GameMode::Creative, 0),
    )
    .await?;
    guarded(
        "restored authoritative position",
        wait_for_authoritative_position(&snapshots, before_join, player, expected, case.chunk),
    )
    .await?;

    let before_move = snapshots.latest().tick;
    send_state(&mut client, next).await?;
    guarded(
        "post-restore routed movement",
        wait_for_authoritative_position(&snapshots, before_move, player, next, case.chunk),
    )
    .await?;

    let before_leave = snapshots.latest().tick;
    drop(client);
    guarded(
        "post-restore leave cleanup",
        wait_for_clean_leave(&snapshots, before_leave),
    )
    .await
}

/// Restores a player without changing the recovered position, then leaves cleanly.
async fn restore_and_leave(
    server: &RunningServer,
    name: &str,
    expected: PlayerState,
    game_mode: GameMode,
    selected_slot: i32,
) -> anyhow::Result<()> {
    let snapshots = server.snapshot_handle();
    let player = PlayerId::offline(name);
    let before_join = snapshots.latest().tick;
    let mut client = guarded(
        "recovered-position login",
        login_to_play(server.local_addr(), name),
    )
    .await?;
    guarded(
        "recovered-position join stream",
        expect_restored_join(&mut client, expected, (0, 0), game_mode, selected_slot),
    )
    .await?;
    guarded(
        "recovered authoritative position",
        wait_for_authoritative_position(&snapshots, before_join, player, expected, (0, 0)),
    )
    .await?;

    let before_leave = snapshots.latest().tick;
    drop(client);
    guarded(
        "recovered-position leave cleanup",
        wait_for_clean_leave(&snapshots, before_leave),
    )
    .await
}

/// Loads one durable player record after the app has released the database.
fn load_player(runtime: &Runtime, database_path: &Path, player: PlayerId) -> PlayerRecord {
    let store = RedbStore::open(database_path).expect("open player store");
    runtime
        .block_on(store.load_player(player))
        .expect("load player record")
        .expect("player record remains present")
}

/// Asserts the position/look stored in one app-owned JSON payload.
fn assert_record_state(record: &PlayerRecord, expected: PlayerState) {
    let payload: serde_json::Value =
        serde_json::from_slice(record.data()).expect("decode saved player payload");
    let coordinate = |field: &str| {
        payload[field]
            .as_f64()
            .unwrap_or_else(|| panic!("{field} is a JSON number"))
    };
    let actual = Vec3::new(coordinate("x"), coordinate("y"), coordinate("z"));
    assert!(
        same_position(actual, expected.position),
        "saved position {actual:?} != expected {:?}",
        expected.position,
    );
    assert!(
        (coordinate("yaw") - f64::from(expected.yaw)).abs() < EPSILON,
        "saved yaw {} != expected {}",
        coordinate("yaw"),
        expected.yaw,
    );
    assert!(
        (coordinate("pitch") - f64::from(expected.pitch)).abs() < EPSILON,
        "saved pitch {} != expected {}",
        coordinate("pitch"),
        expected.pitch,
    );
}

#[test]
fn every_spawn_shard_side_survives_same_process_rejoin_and_restart() {
    let temp = temp_world();
    let config = persistent_config(temp.path());
    let runtime = test_runtime();

    runtime
        .block_on(async {
            let server = ferrumc_app::run(&config).await?;
            for case in BOUNDARIES {
                establish_boundary(&server, case).await?;
                restore_advance_and_leave(&server, case, case.first, case.same_process).await?;
            }
            guarded("first server shutdown", server.shutdown()).await?;

            let server = ferrumc_app::run(&config).await?;
            for case in BOUNDARIES {
                restore_advance_and_leave(&server, case, case.same_process, case.after_restart)
                    .await?;
            }
            guarded("restart server shutdown", server.shutdown()).await
        })
        .expect("all boundary lifecycle flows succeed");

    let database_path = temp.path().join("world.redb");
    for case in BOUNDARIES {
        let record = load_player(&runtime, &database_path, PlayerId::offline(case.name));
        assert_eq!(record.schema_version(), PLAYER_SCHEMA_VERSION);
        assert_eq!(record.game_mode(), GameMode::Creative);
        assert_record_state(&record, case.after_restart);
    }
}

#[test]
fn out_of_range_record_recovers_at_spawn_and_stays_healed_after_restart() {
    let temp = temp_world();
    let config = persistent_config(temp.path());
    let database_path = temp.path().join("world.redb");
    let runtime = test_runtime();
    let name = "InvalidPosition";
    let player = PlayerId::offline(name);
    let expected = PlayerState {
        position: SPAWN,
        yaw: 47.5,
        pitch: -16.25,
    };
    let empty_slots = vec![serde_json::json!({"item_id": 0, "count": 0}); 46];
    let original_payload = serde_json::json!({
        "x": 30_000_001.0,
        "y": 64.0,
        "z": 8.0,
        "yaw": expected.yaw,
        "pitch": expected.pitch,
        "selected_slot": 2,
        "slots": empty_slots,
    });
    let original = PlayerRecord::new(
        PLAYER_SCHEMA_VERSION,
        GameMode::Survival,
        serde_json::to_vec(&original_payload).expect("encode invalid-position fixture"),
    )
    .expect("bounded invalid-position fixture");
    let store = RedbStore::open(&database_path).expect("open player store");
    runtime
        .block_on(store.save_player(player, original))
        .expect("seed invalid-position record");
    drop(store);

    runtime
        .block_on(async {
            let server = ferrumc_app::run(&config).await?;
            restore_and_leave(&server, name, expected, GameMode::Survival, 2).await?;
            guarded("recovery server shutdown", server.shutdown()).await
        })
        .expect("invalid position recovers and leaves cleanly");

    let first_healed = load_player(&runtime, &database_path, player);
    assert_eq!(first_healed.schema_version(), PLAYER_SCHEMA_VERSION);
    assert_eq!(first_healed.game_mode(), GameMode::Survival);
    assert_record_state(&first_healed, expected);

    runtime
        .block_on(async {
            let server = ferrumc_app::run(&config).await?;
            restore_and_leave(&server, name, expected, GameMode::Survival, 2).await?;
            guarded("healed restart shutdown", server.shutdown()).await
        })
        .expect("healed position restores without another repair");

    let healed = load_player(&runtime, &database_path, player);
    assert_eq!(healed.schema_version(), PLAYER_SCHEMA_VERSION);
    assert_eq!(healed.game_mode(), GameMode::Survival);
    assert_record_state(&healed, expected);
    let payload: serde_json::Value =
        serde_json::from_slice(healed.data()).expect("decode healed player payload");
    assert_eq!(payload["selected_slot"], 2);
    assert_eq!(
        payload["slots"], original_payload["slots"],
        "position recovery preserves the decoded inventory layout",
    );
}
