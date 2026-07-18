#![forbid(unsafe_code)]

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use ferrumc_plugin_block_rules::{
    DENIED_BLOCK_STATE_ID, GLASS_BLOCK_STATE_ID, PLUGIN_ID, PLUGIN_NAME,
    TINTED_GLASS_BLOCK_STATE_ID,
};
use ferrumc_plugin_loader::{PluginCapabilities, PluginCapability, PluginLoader};
use ferrumc_plugin_sdk::{
    BlockDecision, BlockEvent, BlockPos, Capability, CapabilityManifest, Event, EventDecision,
    EventKind, Feedback, PlaceAttempt, PlayerId, Tick,
};
use ferrumc_testkit::{PluginEffect, PluginTestHost, ScheduledPluginEvent};
use sha2::{Digest, Sha256};

const MANIFEST_TEMPLATE: &str = include_str!("../plugin.toml.in");
const DENIED_MESSAGE: &str = "You cannot place that block here.";
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[test]
fn strict_bundle_loads_and_dynamic_decisions_match_the_compiled_policy() {
    let repo = repo_root();
    let scratch = repo
        .join(".codex-tmp")
        .join(format!("p66-block-rules-{}", std::process::id()));
    remove_if_present(&scratch);
    fs::create_dir_all(&scratch).expect("create repository-local plugin scratch");

    let built = build_dynamic_plugin(&repo, &scratch);
    let plugins_root = scratch.join("plugins");
    let library = package_bundle(&built, &plugins_root);

    let capabilities = PluginCapabilities::empty().with(PluginCapability::VetoBlockEdits);
    let bundle_loader =
        PluginLoader::current(capabilities).expect("construct current plugin loader");
    let plugins = bundle_loader
        .load_directory(&plugins_root)
        .expect("load strict block-rules bundle");
    assert_eq!(plugins.len(), 1);
    let plugin = plugins.get(PLUGIN_ID).expect("loaded block-rules id");
    let manifest = plugin.manifest();
    let metadata = plugin.metadata();
    assert_eq!(manifest.id(), PLUGIN_ID);
    assert_eq!(manifest.name(), PLUGIN_NAME);
    assert_eq!(manifest.version().to_string(), "0.1.0");
    assert_eq!(manifest.capabilities(), capabilities);
    assert_eq!(metadata.id(), PLUGIN_ID);
    assert_eq!(metadata.name(), PLUGIN_NAME);
    assert_eq!(metadata.version().major(), 0);
    assert_eq!(metadata.version().minor(), 1);
    assert_eq!(metadata.version().patch(), 0);
    assert_eq!(metadata.requested_capabilities(), capabilities.bits());

    let player = PlayerId::offline("BlockRulesDynamicFixture");
    let events = [
        scheduled(
            1,
            Event::BlockPlaceAttempt(PlaceAttempt::new(
                player,
                BlockPos::new(1, 64, 1),
                DENIED_BLOCK_STATE_ID,
            )),
        ),
        scheduled(
            2,
            Event::BlockPlaceAttempt(PlaceAttempt::new(
                player,
                BlockPos::new(2, 64, 2),
                GLASS_BLOCK_STATE_ID,
            )),
        ),
        scheduled(
            3,
            Event::BlockPlaceAttempt(PlaceAttempt::new(
                player,
                BlockPos::new(3, 64, 3),
                ferrumc_registry::block_state::ids::STONE,
            )),
        ),
        scheduled(
            4,
            Event::BlockBreakAttempt(BlockEvent::new(player, BlockPos::new(4, 64, 4))),
        ),
    ];
    let granted = CapabilityManifest::empty().with(Capability::VetoBlockEdits);
    let host = PluginTestHost::new(granted, 1).expect("valid exact-capability host");
    let run = host
        .replay_dynamic(&library, &events)
        .expect("replay trusted-native block rules");

    assert_eq!(
        run.effects(),
        [
            PluginEffect::BlockDecision(BlockDecision::Deny(Some(
                Feedback::new(DENIED_MESSAGE).expect("static feedback is bounded")
            ))),
            PluginEffect::BlockDecision(BlockDecision::Replace(TINTED_GLASS_BLOCK_STATE_ID)),
            PluginEffect::BlockDecision(BlockDecision::Allow),
            PluginEffect::EventDecision {
                kind: EventKind::BlockBreakAttempt,
                decision: EventDecision::Allow,
            },
        ]
    );
    assert!(run.diagnostics().is_empty());
    assert!(run.state().messages().is_empty());

    cleanup_loaded_scratch(&scratch);
}

fn scheduled(tick: u64, event: Event) -> ScheduledPluginEvent {
    ScheduledPluginEvent::new(Tick::new(tick), event)
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("plugin crate is nested under server/plugins")
        .to_path_buf()
}

fn build_dynamic_plugin(repo: &Path, scratch: &Path) -> PathBuf {
    let target = scratch.join("target");
    let output = Command::new(env!("CARGO"))
        .arg("build")
        .arg("--manifest-path")
        .arg(repo.join("server/Cargo.toml"))
        .arg("-p")
        .arg("ferrumc-plugin-block-rules")
        .arg("--lib")
        .arg("--locked")
        .arg("--offline")
        .arg("--jobs")
        .arg("1")
        .arg("--no-default-features")
        .arg("--features")
        .arg("dynamic")
        .arg("--target-dir")
        .arg(&target)
        .arg("--message-format=json-render-diagnostics")
        .output()
        .expect("run nested dynamic block-rules build");
    assert!(
        output.status.success(),
        "nested dynamic block-rules build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let built = dynamic_artifact_from_messages(&output.stdout);
    let artifact = scratch.join(format!(
        "{}ferrumc_plugin_block_rules{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    fs::copy(&built, &artifact).unwrap_or_else(|error| {
        panic!(
            "copy block-rules artifact {} to {}: {error}",
            built.display(),
            artifact.display()
        )
    });
    fs::remove_dir_all(&target)
        .unwrap_or_else(|error| panic!("remove nested target {}: {error}", target.display()));
    artifact
}

fn dynamic_artifact_from_messages(stdout: &[u8]) -> PathBuf {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .filter(|message| message["target"]["name"] == "ferrumc_plugin_block_rules")
        .filter(|message| {
            message["target"]["crate_types"]
                .as_array()
                .is_some_and(|types| types.iter().any(|kind| kind == "cdylib"))
        })
        .flat_map(|message| message["filenames"].as_array().cloned().unwrap_or_default())
        .filter_map(|filename| filename.as_str().map(PathBuf::from))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(std::env::consts::DLL_SUFFIX))
        })
        .expect("Cargo reported the block-rules cdylib artifact")
}

fn package_bundle(library: &Path, plugins_root: &Path) -> PathBuf {
    let filename = library
        .file_name()
        .and_then(|name| name.to_str())
        .expect("dynamic artifact has a UTF-8 filename");
    assert!(
        filename
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "dynamic artifact filename is safe for the TOML template"
    );

    let bundle = plugins_root.join(PLUGIN_ID);
    fs::create_dir_all(&bundle).expect("create block-rules bundle");
    let copied = bundle.join(filename);
    fs::copy(library, &copied).expect("copy block-rules artifact into bundle");
    let manifest = MANIFEST_TEMPLATE
        .replace("{{SERVER_API}}", env!("CARGO_PKG_VERSION"))
        .replace("{{LIBRARY}}", filename)
        .replace("{{LIBRARY_SHA256}}", &hex_digest(sha256_file(&copied)));
    assert!(!manifest.contains("{{"));
    let temporary = bundle.join(".plugin.toml.tmp");
    fs::write(&temporary, manifest).expect("write temporary block-rules manifest");
    fs::rename(temporary, bundle.join("plugin.toml")).expect("publish block-rules manifest");
    copied
}

fn sha256_file(path: &Path) -> [u8; 32] {
    let mut file = File::open(path).expect("open copied block-rules artifact");
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .expect("hash copied block-rules artifact");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher.finalize().into()
}

fn hex_digest(digest: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(64);
    for byte in digest {
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
    // The ABI policy keeps the DLL resident until process exit, and Windows
    // does not permit deleting the mapped library in this process.
}
