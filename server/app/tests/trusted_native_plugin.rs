//! Production-socket regression for a pure trusted-native plugin deployment.
//!
//! The test builds the real block-rules dynamic artifact, packages it as a
//! strict hash-pinned bundle, disables every built-in plugin, and starts the
//! shipping app path. A glass placement must still be rewritten to tinted glass,
//! proving the loaded native instance remains active in the host shared by live
//! connections.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ferrumc_app::AppConfig;
use ferrumc_proto::generated::play::{ClientboundPlayPacket, ServerboundSetHeldItem, UseItemOn};
use ferrumc_proto::types::BlockPosition;
use sha2::{Digest, Sha256};
use tokio::time::timeout;

use common::{encode, login_to_play, TestClient};

const BLOCK_RULES_PACKAGE: &str = "ferrumc-plugin-block-rules";
const MANIFEST_TEMPLATE: &str =
    include_str!("../../plugins/ferrumc-plugin-block-rules/plugin.toml.in");
const GUARD: Duration = Duration::from_secs(15);
const FACE_UP: i32 = 1;
const TINTED_GLASS_STATE: i32 = 23_377;
const SEQUENCE: i32 = 66;

#[tokio::test]
async fn dynamic_block_rules_rewrites_glass_on_the_production_socket_path() {
    let plugins_root = tokio::task::spawn_blocking(build_strict_bundle)
        .await
        .expect("bundle build task completes");
    let plugins_path = plugins_root
        .to_str()
        .expect("repository plugin scratch path is UTF-8");
    let config = AppConfig::from_toml_str(&format!(
        "bind = \"127.0.0.1:0\"\n\
         spawn_chunk_radius = 1\n\
         builtin_plugins = false\n\
         plugins_dir = {plugins_path:?}\n"
    ))
    .expect("pure-mode native plugin config parses");

    // The dynamic plugin has the same stable id as the built-in block-rules
    // plugin. Successful startup therefore also proves the built-in set is off.
    let server = ferrumc_app::run(&config)
        .await
        .expect("server starts with the strict native bundle");
    let address = server.local_addr();

    timeout(GUARD, place_glass_and_observe_rewrite(address))
        .await
        .expect("native plugin flow finishes within the guard")
        .expect("native block-rules rewrite reaches the socket");
    timeout(GUARD, server.shutdown())
        .await
        .expect("shutdown finishes within the guard")
        .expect("server shuts down cleanly");

    tokio::task::spawn_blocking(move || cleanup_loaded_scratch(&scratch_root(&plugins_root)))
        .await
        .expect("scratch cleanup task completes");
}

async fn place_glass_and_observe_rewrite(address: std::net::SocketAddr) -> anyhow::Result<()> {
    let mut viewer = login_to_play(address, "NativeViewer").await?;
    let mut builder = login_to_play(address, "NativeBuilder").await?;

    // Starter-kit hotbar index 3 is glass. The production handler resolves that
    // held item to glass state 562 before consulting the plugin host.
    builder
        .send_frame(&encode(|buf| ServerboundSetHeldItem::new(3).encode(buf)))
        .await?;
    builder
        .send_frame(&encode(|buf| {
            UseItemOn::new(
                0,
                BlockPosition::new(9, 63, 8),
                FACE_UP,
                0.5,
                1.0,
                0.5,
                false,
                false,
                SEQUENCE,
            )
            .encode(buf)
        }))
        .await?;

    expect_tinted_glass(&mut viewer).await?;
    expect_ack(&mut builder).await
}

async fn expect_tinted_glass(client: &mut TestClient) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::BlockUpdate(update) = client.next_play().await? {
            let position = update.location();
            anyhow::ensure!(
                (position.x(), position.y(), position.z()) == (9, 64, 8),
                "unexpected block update position"
            );
            anyhow::ensure!(
                update.block_state() == TINTED_GLASS_STATE,
                "native block-rules produced state {}, expected {TINTED_GLASS_STATE}",
                update.block_state()
            );
            return Ok(());
        }
    }
}

async fn expect_ack(client: &mut TestClient) -> anyhow::Result<()> {
    loop {
        if let ClientboundPlayPacket::AcknowledgeBlockChange(ack) = client.next_play().await? {
            anyhow::ensure!(
                ack.sequence() == SEQUENCE,
                "block-change ack carried sequence {}, expected {SEQUENCE}",
                ack.sequence()
            );
            return Ok(());
        }
    }
}

fn build_strict_bundle() -> PathBuf {
    let repo = repo_root();
    let scratch = repo
        .join(".codex-tmp")
        .join(format!("p66-app-native-{}", std::process::id()));
    remove_if_present(&scratch);
    fs::create_dir_all(&scratch).expect("create repository-local plugin scratch");

    let target = scratch.join("target");
    let output = Command::new(env!("CARGO"))
        .arg("build")
        .arg("--manifest-path")
        .arg(repo.join("server/Cargo.toml"))
        .arg("--locked")
        .arg("--offline")
        .arg("--jobs")
        .arg("1")
        .arg("-p")
        .arg(BLOCK_RULES_PACKAGE)
        .arg("--lib")
        .arg("--release")
        .arg("--no-default-features")
        .arg("--features")
        .arg("dynamic")
        .arg("--target-dir")
        .arg(&target)
        .output()
        .expect("run nested dynamic block-rules build");
    assert!(
        output.status.success(),
        "nested dynamic block-rules build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let filename = format!(
        "{}ferrumc_plugin_block_rules{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let artifact = target.join("release").join(&filename);
    assert!(
        artifact.is_file(),
        "dynamic block-rules artifact missing at {}",
        artifact.display()
    );

    let plugins_root = scratch.join("plugins");
    let bundle = plugins_root.join("block-rules");
    fs::create_dir_all(&bundle).expect("create strict block-rules bundle");
    let library = bundle.join(&filename);
    fs::copy(&artifact, &library).expect("copy block-rules library into strict bundle");
    let hash = hex_digest(Sha256::digest(
        fs::read(&library).expect("read copied block-rules library"),
    ));
    let manifest = MANIFEST_TEMPLATE
        .replace("{{SERVER_API}}", env!("CARGO_PKG_VERSION"))
        .replace("{{LIBRARY}}", &filename)
        .replace("{{LIBRARY_SHA256}}", &hash);
    assert!(
        !manifest.contains("{{"),
        "all plugin manifest placeholders are resolved"
    );
    fs::write(bundle.join("plugin.toml"), manifest).expect("write strict plugin manifest");
    fs::remove_dir_all(target).expect("remove nested Cargo target after packaging");
    plugins_root
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("app crate is nested under the server workspace")
        .to_path_buf()
}

fn scratch_root(plugins_root: &Path) -> PathBuf {
    plugins_root
        .parent()
        .expect("plugins directory is nested in its scratch root")
        .to_path_buf()
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let bytes = digest.as_ref();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn remove_if_present(path: &Path) {
    if let Err(error) = fs::remove_dir_all(path) {
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
}

#[cfg(not(target_os = "windows"))]
fn cleanup_loaded_scratch(path: &Path) {
    remove_if_present(path);
}

#[cfg(target_os = "windows")]
fn cleanup_loaded_scratch(_path: &Path) {
    // Trusted native libraries remain mapped until process exit, so Windows
    // cannot remove this repository-local scratch directory during the test.
}
