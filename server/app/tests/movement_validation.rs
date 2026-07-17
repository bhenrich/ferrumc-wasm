//! End-to-end trust-boundary coverage for serverbound movement.
//!
//! A valid move first establishes one exact stream/simulation/persistence state.
//! A hostile batch then mixes non-finite coordinates, coordinates beyond the
//! simulation boundary, and non-finite look values. A driver-owned command reply
//! and a later authoritative snapshot fence the whole batch. None of those
//! rejected observations may recenter the stream, churn chunk tickets, reach the
//! shard/router, or replace the valid leave-save candidate.

mod common;

use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::time::timeout;
use uuid::Uuid;

use ferrumc_codec::BoundedString;
use ferrumc_core::PlayerId;
use ferrumc_math::{ChunkPos, Vec3};
use ferrumc_nbt::NbtTag;
use ferrumc_observability::{ServerSnapshot, SnapshotPublisher};
use ferrumc_proto::generated::play::{
    ChatCommand, ClientboundPlayPacket, PlayerInfoUpdate, SetPlayerPositionAndRotation,
};
use ferrumc_session::PLAYER_INFO_ADD;
use ferrumc_storage::{PlayerRecord, PlayerStore, RedbStore};

use ferrumc_app::{AppConfig, RunningServer};

use common::{encode, login_to_play, TestClient};

/// Overall guard so a missing packet, tick, or teardown cannot hang the suite.
const GUARD: Duration = Duration::from_secs(20);

/// Default configured spawn.
const SPAWN: Vec3 = Vec3::new(8.0, 64.0, 8.0);

/// One accepted state shared by the stream, simulation, router, and save mirror.
const ACCEPTED: Vec3 = Vec3::new(40.5, 64.0, 8.5);
const ACCEPTED_YAW: f32 = 31.5;
const ACCEPTED_PITCH: f32 = -17.25;
const ACCEPTED_CHUNK: ChunkPos = ChunkPos::new(2, 0);

/// Radius-two spawn square plus the ten new columns introduced by a two-chunk shift.
const ACCEPTED_RESIDENT_CHUNKS: usize = 35;
/// The radius-two spawn square retained by spawn tickets after both players leave.
const CLEAN_RESIDENT_CHUNKS: usize = 25;
/// Two 25-column join kits plus the accepted shift's ten entered columns.
const ACCEPTED_SENT_MIN: u64 = 60;
/// The accepted two-chunk shift releases ten columns from the actor's old square.
const ACCEPTED_UNLOADED_MIN: u64 = 10;

/// Exact float tolerance; every accepted coordinate is binary-representable.
const EPSILON: f64 = 1e-9;

/// Creates an isolated world directory under the repository-owned scratch root.
fn temp_world() -> tempfile::TempDir {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".codex-tmp");
    std::fs::create_dir_all(&scratch).expect("create repository scratch directory");
    tempfile::Builder::new()
        .prefix("packet26-movement-validation-")
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

/// Builds a durable radius-two stream on an ephemeral port.
fn persistent_config(world_dir: &Path) -> AppConfig {
    AppConfig {
        bind: "127.0.0.1:0".parse().expect("loopback address"),
        spawn_chunk_radius: 2,
        view_distance: 2,
        world_dir: Some(world_dir.to_path_buf()),
        ops: vec!["MovementGuard".to_string(), "MovementObserver".to_string()],
        ..AppConfig::default()
    }
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

/// Returns whether two positions match exactly for this test's coordinates.
fn same_position(actual: Vec3, expected: Vec3) -> bool {
    (actual.x - expected.x).abs() < EPSILON
        && (actual.y - expected.y).abs() < EPSILON
        && (actual.z - expected.z).abs() < EPSILON
}

/// Extracts plain text from the command reply's network-NBT component.
fn nbt_text(content: &NbtTag) -> Option<&str> {
    let NbtTag::Compound(root) = content else {
        return None;
    };
    let Some(NbtTag::String(text)) = root.get("text") else {
        return None;
    };
    Some(text)
}

/// Drains a fresh join through its spawn centre, sync, initial time, and one
/// representative column from the radius-two kit.
async fn expect_fresh_spawn(client: &mut TestClient) -> anyhow::Result<()> {
    let mut centered = false;
    let mut synchronized = false;
    let mut chunk = false;
    let mut initial_time = false;
    while !(centered && synchronized && chunk && initial_time) {
        match client.next_play().await? {
            ClientboundPlayPacket::SetCenterChunk(center)
                if ChunkPos::new(center.chunk_x(), center.chunk_z()) == ChunkPos::ORIGIN =>
            {
                centered = true;
            }
            ClientboundPlayPacket::SynchronizePlayerPosition(sync)
                if same_position(Vec3::new(sync.x(), sync.y(), sync.z()), SPAWN) =>
            {
                synchronized = true;
            }
            ClientboundPlayPacket::ChunkDataAndLight(column)
                if ChunkPos::new(column.x(), column.z()) == ChunkPos::ORIGIN =>
            {
                chunk = true;
            }
            ClientboundPlayPacket::UpdateTime(_) => initial_time = true,
            _ => {}
        }
    }
    Ok(())
}

/// Reads the one-entry UUID carried by this server's player-list add.
fn player_info_uuid(info: &PlayerInfoUpdate) -> anyhow::Result<Uuid> {
    let entries = info.entries();
    anyhow::ensure!(
        entries.first() == Some(&1) && entries.len() >= 17,
        "expected one complete player-info entry",
    );
    Ok(Uuid::from_slice(&entries[1..17])?)
}

/// Waits until `client` sees the target player and returns their network entity id.
async fn observe_appearance(client: &mut TestClient, target: Uuid) -> anyhow::Result<i32> {
    let mut info = false;
    let mut entity_id = None;
    while !(info && entity_id.is_some()) {
        match client.next_play().await? {
            ClientboundPlayPacket::PlayerInfoUpdate(packet)
                if packet.action() == PLAYER_INFO_ADD && player_info_uuid(&packet)? == target =>
            {
                info = true;
            }
            ClientboundPlayPacket::SpawnEntity(packet) if packet.entity_uuid() == target => {
                entity_id = Some(packet.entity_id());
            }
            _ => {}
        }
    }
    entity_id.ok_or_else(|| anyhow::anyhow!("target entity id was not observed"))
}

/// Sends one complete accepted position/look observation.
async fn send_accepted(client: &mut TestClient) -> anyhow::Result<()> {
    client
        .send_frame(&encode(|buf| {
            SetPlayerPositionAndRotation::new(
                ACCEPTED.x,
                ACCEPTED.y,
                ACCEPTED.z,
                ACCEPTED_YAW,
                ACCEPTED_PITCH,
                0,
            )
            .encode(buf)
        }))
        .await
}

/// Waits for the accepted move's exact stream centre and all ten entered/departed
/// columns from its two-chunk shift.
async fn expect_accepted_stream(client: &mut TestClient) -> anyhow::Result<()> {
    let mut centered = false;
    let mut entered = BTreeSet::new();
    let mut departed = BTreeSet::new();
    while !(centered && entered.len() == 10 && departed.len() == 10) {
        match client.next_play().await? {
            ClientboundPlayPacket::SetCenterChunk(center)
                if ChunkPos::new(center.chunk_x(), center.chunk_z()) == ACCEPTED_CHUNK =>
            {
                centered = true;
            }
            ClientboundPlayPacket::ChunkDataAndLight(column) if column.x() >= 3 => {
                entered.insert(ChunkPos::new(column.x(), column.z()));
            }
            ClientboundPlayPacket::UnloadChunk(column) if column.chunk_x() < 0 => {
                departed.insert(ChunkPos::new(column.chunk_x(), column.chunk_z()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Proves the accepted position/look reached the router's viewer carrier exactly.
async fn expect_accepted_router(
    observer: &mut TestClient,
    actor_entity_id: i32,
) -> anyhow::Result<()> {
    let mut teleported = false;
    let mut head_rotated = false;
    while !(teleported && head_rotated) {
        match observer.next_play().await? {
            ClientboundPlayPacket::EntityTeleport(packet)
                if packet.entity_id() == actor_entity_id =>
            {
                anyhow::ensure!(
                    same_position(Vec3::new(packet.x(), packet.y(), packet.z()), ACCEPTED),
                    "router broadcast the wrong accepted position",
                );
                anyhow::ensure!(
                    packet.yaw() == ACCEPTED_YAW && packet.pitch() == ACCEPTED_PITCH,
                    "router broadcast the wrong accepted look",
                );
                teleported = true;
            }
            ClientboundPlayPacket::SetHeadRotation(packet)
                if packet.entity_id() == actor_entity_id =>
            {
                head_rotated = true;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Waits for a later snapshot to expose the accepted authoritative state.
async fn wait_for_accepted_snapshot(
    snapshots: &SnapshotPublisher,
    after_tick: u64,
    player: PlayerId,
) -> anyhow::Result<ServerSnapshot> {
    loop {
        let snapshot = snapshots.latest();
        if snapshot.tick > after_tick
            && snapshot.chunks_loaded == ACCEPTED_RESIDENT_CHUNKS
            && snapshot.chunk_sent_total >= ACCEPTED_SENT_MIN
            && snapshot.chunk_unloaded_total >= ACCEPTED_UNLOADED_MIN
        {
            if let Some(row) = snapshot
                .players
                .iter()
                .find(|row| row.player_id == player.as_uuid().as_u128())
            {
                let actual = Vec3::new(row.position.x, row.position.y, row.position.z);
                if same_position(actual, ACCEPTED) {
                    let actual_chunk = ChunkPos::new(row.chunk.x, row.chunk.z);
                    anyhow::ensure!(
                        actual_chunk == ACCEPTED_CHUNK,
                        "authoritative chunk {actual_chunk:?} != {ACCEPTED_CHUNK:?}",
                    );
                    return Ok((*snapshot).clone());
                }
            }
        }
        tokio::task::yield_now().await;
    }
}

/// Sends every hostile movement class, ending with a finite but out-of-range
/// position/look marker that the old leave mirror could serialize.
async fn send_hostile_batch(client: &mut TestClient) -> anyhow::Result<()> {
    let hostile = [
        (Vec3::new(f64::NAN, 64.0, 8.5), 41.0, -11.0),
        (Vec3::new(40.5, f64::INFINITY, 8.5), 42.0, -12.0),
        (Vec3::new(40.5, 64.0, f64::NEG_INFINITY), 43.0, -13.0),
        (Vec3::new(30_000_001.0, 64.0, 8.5), 44.0, -14.0),
        (Vec3::new(40.5, -30_000_001.0, 8.5), 45.0, -15.0),
        (Vec3::new(40.5, 64.0, 30_000_001.0), 46.0, -16.0),
        (ACCEPTED, f32::NAN, -17.0),
        (ACCEPTED, 48.0, f32::INFINITY),
        (Vec3::new(-30_000_001.0, 64.0, 8.5), 149.0, -39.0),
    ];
    for (position, yaw, pitch) in hostile {
        client
            .send_frame(&encode(|buf| {
                SetPlayerPositionAndRotation::new(position.x, position.y, position.z, yaw, pitch, 0)
                    .encode(buf)
            }))
            .await?;
    }
    Ok(())
}

/// Uses a driver-owned command reply as a FIFO fence after the hostile frames.
async fn fence_driver(client: &mut TestClient) -> anyhow::Result<()> {
    let command = BoundedString::<256>::new("time query daytime".to_string())?;
    client
        .send_frame(&encode(|buf| ChatCommand::new(command.clone()).encode(buf)))
        .await?;
    loop {
        match client.next_play().await? {
            ClientboundPlayPacket::SystemChat(chat)
                if nbt_text(chat.content()).is_some_and(|text| text.contains("The time is")) =>
            {
                return Ok(());
            }
            ClientboundPlayPacket::SynchronizePlayerPosition(_) => {
                anyhow::bail!("hostile movement reached sim and caused an actor correction")
            }
            ClientboundPlayPacket::SetCenterChunk(_)
            | ClientboundPlayPacket::ChunkDataAndLight(_)
            | ClientboundPlayPacket::UnloadChunk(_) => {
                anyhow::bail!("hostile movement changed the actor's chunk stream")
            }
            _ => {}
        }
    }
}

/// Reads through actor traffic until the next periodic world-time update.
async fn next_world_age(client: &mut TestClient) -> anyhow::Result<i64> {
    loop {
        if let ClientboundPlayPacket::UpdateTime(update) = client.next_play().await? {
            return Ok(update.world_age());
        }
    }
}

/// Crosses at least one fixed chunk-stream cadence after the hostile batch.
///
/// Periodic updates are one second apart, while the stream cadence is one tick.
/// Requiring an age newer than the update read immediately before the hostile
/// batch proves the timer crossed after that batch, even on a slow test runner,
/// and scans every actor packet along the way.
async fn fence_stream_cadence(
    client: &mut TestClient,
    before_hostile_age: i64,
) -> anyhow::Result<()> {
    loop {
        match client.next_play().await? {
            ClientboundPlayPacket::UpdateTime(update)
                if update.world_age() > before_hostile_age =>
            {
                return Ok(());
            }
            ClientboundPlayPacket::SynchronizePlayerPosition(_) => {
                anyhow::bail!("hostile movement reached sim after the command fence")
            }
            ClientboundPlayPacket::SetCenterChunk(_)
            | ClientboundPlayPacket::ChunkDataAndLight(_)
            | ClientboundPlayPacket::UnloadChunk(_) => {
                anyhow::bail!("a rejected target survived into a later stream cadence")
            }
            _ => {}
        }
    }
}

/// Fences and scans the observer's route so rejected position/look never leaks.
async fn fence_observer(observer: &mut TestClient, actor_entity_id: i32) -> anyhow::Result<()> {
    let command = BoundedString::<256>::new("time query daytime".to_string())?;
    observer
        .send_frame(&encode(|buf| ChatCommand::new(command.clone()).encode(buf)))
        .await?;
    loop {
        match observer.next_play().await? {
            ClientboundPlayPacket::SystemChat(chat)
                if nbt_text(chat.content()).is_some_and(|text| text.contains("The time is")) =>
            {
                return Ok(());
            }
            ClientboundPlayPacket::EntityTeleport(packet)
                if packet.entity_id() == actor_entity_id =>
            {
                anyhow::bail!("rejected position leaked to the router observer")
            }
            ClientboundPlayPacket::UpdateEntityPositionAndRotation(packet)
                if packet.entity_id() == actor_entity_id =>
            {
                anyhow::bail!("rejected position/look leaked to the router observer")
            }
            ClientboundPlayPacket::UpdateEntityRotation(packet)
                if packet.entity_id() == actor_entity_id =>
            {
                anyhow::bail!("rejected look leaked to the router observer")
            }
            ClientboundPlayPacket::SetHeadRotation(packet)
                if packet.entity_id() == actor_entity_id =>
            {
                anyhow::bail!("rejected look leaked to the router observer")
            }
            _ => {}
        }
    }
}

/// Waits for the first post-fence tick and proves no rejected observation changed
/// authoritative position, residency, or stream counters.
async fn assert_no_hostile_side_effects(
    snapshots: &SnapshotPublisher,
    after_tick: u64,
    player: PlayerId,
    baseline: &ServerSnapshot,
) -> anyhow::Result<()> {
    loop {
        let snapshot = snapshots.latest();
        if snapshot.tick > after_tick {
            let row = snapshot
                .players
                .iter()
                .find(|row| row.player_id == player.as_uuid().as_u128())
                .ok_or_else(|| anyhow::anyhow!("movement player disappeared"))?;
            let actual = Vec3::new(row.position.x, row.position.y, row.position.z);
            anyhow::ensure!(
                same_position(actual, ACCEPTED),
                "hostile movement reached simulation/router state: {actual:?}"
            );
            anyhow::ensure!(
                ChunkPos::new(row.chunk.x, row.chunk.z) == ACCEPTED_CHUNK,
                "hostile movement changed the authoritative chunk"
            );
            anyhow::ensure!(
                snapshot.chunks_loaded == baseline.chunks_loaded,
                "hostile movement changed resident tickets: {} -> {}",
                baseline.chunks_loaded,
                snapshot.chunks_loaded,
            );
            anyhow::ensure!(
                snapshot.chunk_sent_total == baseline.chunk_sent_total,
                "hostile movement caused chunk reads/encodes: {} -> {}",
                baseline.chunk_sent_total,
                snapshot.chunk_sent_total,
            );
            anyhow::ensure!(
                snapshot.chunk_unloaded_total == baseline.chunk_unloaded_total,
                "hostile movement caused unload/ticket churn: {} -> {}",
                baseline.chunk_unloaded_total,
                snapshot.chunk_unloaded_total,
            );
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
}

/// Waits until leave routing removes the player and releases their target ticket.
async fn wait_for_clean_leave(
    snapshots: &SnapshotPublisher,
    after_tick: u64,
) -> anyhow::Result<()> {
    loop {
        let snapshot = snapshots.latest();
        if snapshot.tick > after_tick
            && snapshot.players_online == 0
            && snapshot.players.is_empty()
            && snapshot.chunks_loaded == CLEAN_RESIDENT_CHUNKS
        {
            return Ok(());
        }
        tokio::task::yield_now().await;
    }
}

/// Asserts one app-owned persisted record retained the accepted state and look.
fn assert_saved_state(record: &PlayerRecord) {
    let payload: serde_json::Value =
        serde_json::from_slice(record.data()).expect("decode saved player payload");
    let number = |field: &str| {
        payload[field]
            .as_f64()
            .unwrap_or_else(|| panic!("{field} is a JSON number"))
    };
    let actual = Vec3::new(number("x"), number("y"), number("z"));
    assert!(
        same_position(actual, ACCEPTED),
        "saved position {actual:?} != accepted {ACCEPTED:?}"
    );
    assert!(
        (number("yaw") - f64::from(ACCEPTED_YAW)).abs() < EPSILON,
        "saved yaw {} != {ACCEPTED_YAW}",
        number("yaw")
    );
    assert!(
        (number("pitch") - f64::from(ACCEPTED_PITCH)).abs() < EPSILON,
        "saved pitch {} != {ACCEPTED_PITCH}",
        number("pitch")
    );
}

/// Reads the restored synchronization and verifies the exact accepted state.
async fn expect_restored_state(client: &mut TestClient) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::SynchronizePlayerPosition(sync) = client.next_play().await? {
            let actual = Vec3::new(sync.x(), sync.y(), sync.z());
            anyhow::ensure!(
                same_position(actual, ACCEPTED),
                "restored position {actual:?} != accepted {ACCEPTED:?}"
            );
            anyhow::ensure!(
                (sync.yaw() - ACCEPTED_YAW).abs() < f32::EPSILON
                    && (sync.pitch() - ACCEPTED_PITCH).abs() < f32::EPSILON,
                "restored look ({}, {}) != ({ACCEPTED_YAW}, {ACCEPTED_PITCH})",
                sync.yaw(),
                sync.pitch(),
            );
            return Ok(());
        }
    }
}

/// Exercises one valid state, a hostile movement flood, leave persistence, and
/// restart restoration through the shipping socket path.
async fn run_first_server(server: &RunningServer, player: PlayerId) -> anyhow::Result<()> {
    let snapshots = server.snapshot_handle();
    let mut client = login_to_play(server.local_addr(), "MovementGuard").await?;
    expect_fresh_spawn(&mut client).await?;
    let mut observer = login_to_play(server.local_addr(), "MovementObserver").await?;
    let actor_entity_id = observe_appearance(&mut observer, player.as_uuid()).await?;

    let before_move = snapshots.latest().tick;
    send_accepted(&mut client).await?;
    expect_accepted_stream(&mut client).await?;
    expect_accepted_router(&mut observer, actor_entity_id).await?;
    let baseline = wait_for_accepted_snapshot(&snapshots, before_move, player).await?;
    let before_hostile_age = next_world_age(&mut client).await?;

    send_hostile_batch(&mut client).await?;
    fence_driver(&mut client).await?;
    fence_stream_cadence(&mut client, before_hostile_age).await?;
    fence_observer(&mut observer, actor_entity_id).await?;
    let after_fence = snapshots.latest().tick;
    assert_no_hostile_side_effects(&snapshots, after_fence, player, &baseline).await?;

    let before_leave = snapshots.latest().tick;
    drop(observer);
    drop(client);
    wait_for_clean_leave(&snapshots, before_leave).await
}

#[test]
fn hostile_movement_cannot_mutate_stream_sim_router_or_saved_state() {
    let temp = temp_world();
    let config = persistent_config(temp.path());
    let database_path = temp.path().join("world.redb");
    let runtime = test_runtime();
    let player = PlayerId::offline("MovementGuard");

    runtime
        .block_on(async {
            let server = ferrumc_app::run(&config).await?;
            guarded("hostile movement flow", run_first_server(&server, player)).await?;
            guarded("first server shutdown", server.shutdown()).await
        })
        .expect("hostile movement has no connection-side effect");

    let store = RedbStore::open(&database_path).expect("open player store");
    let record = runtime
        .block_on(store.load_player(player))
        .expect("load player record")
        .expect("player record exists");
    assert_saved_state(&record);
    drop(store);

    runtime
        .block_on(async {
            let server = ferrumc_app::run(&config).await?;
            let mut client = guarded(
                "restored movement login",
                login_to_play(server.local_addr(), "MovementGuard"),
            )
            .await?;
            guarded(
                "restored accepted state",
                expect_restored_state(&mut client),
            )
            .await?;
            drop(client);
            guarded("restart shutdown", server.shutdown()).await
        })
        .expect("accepted movement survives restart exactly");
}
