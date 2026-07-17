//! End-to-end canonical offline-identity regression.
//!
//! One case-sensitive username is admitted by a UUID-only whitelist, receives
//! the same UUID in Login Success and both remote-player appearance packets, is
//! persisted under that UUID, and is rejected by a UUID-only ban on a second
//! boot. This crosses every app-owned identity handoff without relying on an
//! internal implementation detail.

mod common;

use std::path::Path;
use std::time::Duration;

use tokio::time::timeout;
use uuid::Uuid;

use ferrumc_app::AppConfig;
use ferrumc_codec::BoundedReader;
use ferrumc_core::PlayerId;
use ferrumc_proto::generated::play::{ClientboundPlayPacket, PlayerInfoUpdate};
use ferrumc_session::PLAYER_INFO_ADD;
use ferrumc_storage::{PlayerStore, RedbStore};

use common::{login_to_play, TestClient};

/// Overall guard so a network or shutdown regression cannot hang the suite.
const GUARD: Duration = Duration::from_secs(15);

/// Username whose identity is followed through every surface.
const TARGET_NAME: &str = "IdentityProbe";

/// A second whitelisted player who observes the target's session appearance.
const VIEWER_NAME: &str = "IdentityViewer";

/// Builds the first-boot configuration with a UUID-only whitelist and durable
/// player storage.
fn whitelisted_config(world_dir: &Path, target: Uuid, viewer: Uuid) -> AppConfig {
    let mut config = AppConfig::from_toml_str(&format!(
        "bind = \"127.0.0.1:0\"\n\
         spawn_chunk_radius = 1\n\
         view_distance = 1\n\
         [access]\n\
         whitelist_enabled = true\n\
         whitelist = [\"{target}\", \"{viewer}\"]\n",
    ))
    .expect("UUID-only whitelist config parses");
    config.world_dir = Some(world_dir.to_path_buf());
    config
}

/// Builds the second-boot configuration with a UUID-only ban for the target.
fn banned_config(target: Uuid) -> AppConfig {
    AppConfig::from_toml_str(&format!(
        "bind = \"127.0.0.1:0\"\n\
         spawn_chunk_radius = 1\n\
         [access]\n\
         bans = [\"{target}\"]\n",
    ))
    .expect("UUID-only ban config parses")
}

/// Extracts the first entry UUID from this server's player-info payload.
fn player_info_uuid(info: &PlayerInfoUpdate) -> anyhow::Result<Uuid> {
    let mut reader = BoundedReader::new(info.entries());
    anyhow::ensure!(
        reader.read_var_int()? == 1,
        "expected exactly one player-info entry",
    );
    Ok(Uuid::from_slice(reader.read_bytes(16)?)?)
}

/// Observes both clientbound identity-bearing appearance packets for `target`.
async fn observe_target_identity(client: &mut TestClient, target: Uuid) -> anyhow::Result<()> {
    let mut saw_player_info = false;
    let mut saw_spawn = false;

    while !(saw_player_info && saw_spawn) {
        match client.next_play().await? {
            ClientboundPlayPacket::PlayerInfoUpdate(info)
                if info.action() == PLAYER_INFO_ADD && player_info_uuid(&info)? == target =>
            {
                saw_player_info = true;
            }
            ClientboundPlayPacket::SpawnEntity(spawn) if spawn.entity_uuid() == target => {
                saw_spawn = true;
            }
            _ => {}
        }
    }

    Ok(())
}

/// Waits for the driver-queued world-time packet that is drained only after the
/// complete join kit and the connection's pre-loop streaming pass. It is a
/// deterministic fence before dropping the socket into the shared leave-save
/// teardown.
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

#[tokio::test(flavor = "multi_thread")]
async fn one_username_has_one_canonical_identity_everywhere() {
    let temp = tempfile::tempdir().expect("temporary world directory");
    let target = PlayerId::offline(TARGET_NAME);
    let target_uuid = target.as_uuid();
    let viewer_uuid = PlayerId::offline(VIEWER_NAME).as_uuid();

    // UUID-only whitelist acceptance proves access control receives the same
    // canonical value, not an independently derived login-only identity.
    let server = ferrumc_app::run(&whitelisted_config(temp.path(), target_uuid, viewer_uuid))
        .await
        .expect("whitelist server starts");
    let addr = server.local_addr();

    let mut viewer = timeout(GUARD, login_to_play(addr, VIEWER_NAME))
        .await
        .expect("viewer login finishes within the guard")
        .expect("UUID-whitelisted viewer reaches play");
    let mut target_client = timeout(GUARD, login_to_play(addr, TARGET_NAME))
        .await
        .expect("target login finishes within the guard")
        .expect("UUID-whitelisted target reaches play");

    assert_eq!(
        target_client.login_uuid(),
        Some(target_uuid),
        "Login Success must expose the canonical PlayerId UUID",
    );
    timeout(GUARD, observe_target_identity(&mut viewer, target_uuid))
        .await
        .expect("appearance packets arrive within the guard")
        .expect("player-info and spawn packets carry the canonical UUID");
    timeout(GUARD, wait_until_play_loop(&mut target_client))
        .await
        .expect("target reaches the steady Play loop within the guard")
        .expect("target completes its join kit");

    // Graceful shutdown drains both connection teardowns and commits their
    // player records before releasing the redb file.
    drop(target_client);
    drop(viewer);
    timeout(GUARD, server.shutdown())
        .await
        .expect("whitelist server shuts down within the guard")
        .expect("whitelist server shuts down cleanly");

    let store =
        RedbStore::open(temp.path().join("world.redb")).expect("reopen durable world store");
    assert!(
        store
            .load_player(target)
            .await
            .expect("load canonical player key")
            .is_some(),
        "disconnect persistence must be keyed by the canonical PlayerId",
    );
    drop(store);

    // The same UUID used by whitelist/login/session/persistence must also match
    // a UUID-only ban; the username itself is deliberately absent from the list.
    let server = ferrumc_app::run(&banned_config(target_uuid))
        .await
        .expect("ban server starts");
    let denied = timeout(GUARD, login_to_play(server.local_addr(), TARGET_NAME))
        .await
        .expect("banned login finishes within the guard");
    assert!(
        denied.is_err(),
        "canonical UUID ban must reject the username"
    );

    timeout(GUARD, server.shutdown())
        .await
        .expect("ban server shuts down within the guard")
        .expect("ban server shuts down cleanly");
}
