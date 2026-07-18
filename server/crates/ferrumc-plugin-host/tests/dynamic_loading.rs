//! Integration tests for dynamic plugin loading against a *real* `cdylib`.
//!
//! These build the `ferrumc-plugin-fixture` crate into a dynamic library, then
//! drive [`PluginLoader`] over it to cover the milestone's required scenarios:
//!
//! 1. a well-formed plugin loads, registers, and enables successfully;
//! 2. an ABI-version mismatch is rejected;
//! 3. a missing entrypoint symbol is rejected;
//! 4. a plugin whose `init` fails is contained — the host survives and other
//!    plugins keep working.
//!
//! The fixture is built on demand (see [`fixture_dylib`]). If that build cannot
//! run, the test fails loudly rather than silently passing; the ABI-parsing
//! logic itself also has in-crate unit tests that do not need a dylib.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use ferrumc_core::PluginId;
use ferrumc_plugin_api::Version;
use ferrumc_plugin_host::{HostError, LoadError, PluginHost, PluginLoader};

/// The fixture package name (its `cdylib` artifact backs these tests).
const FIXTURE_PACKAGE: &str = "ferrumc-plugin-fixture";

/// Builds the fixture `cdylib` once per test process and returns its path.
///
/// Uses `cargo build --message-format=json` so the exact artifact path is read
/// from cargo rather than guessed. Cached in a `OnceLock` so the (slow) build
/// happens at most once even though several tests need it.
fn fixture_dylib() -> &'static Path {
    static DYLIB: OnceLock<PathBuf> = OnceLock::new();
    DYLIB.get_or_init(build_fixture).as_path()
}

/// Runs `cargo build -p <fixture> --message-format=json` and extracts the
/// dynamic-library artifact path from the JSON output.
fn build_fixture() -> PathBuf {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let workspace_root = workspace_root();

    let output = Command::new(&cargo)
        .current_dir(&workspace_root)
        .args([
            "build",
            "-p",
            FIXTURE_PACKAGE,
            "--message-format=json-render-diagnostics",
        ])
        .output()
        .expect("failed to spawn cargo to build the plugin fixture");

    assert!(
        output.status.success(),
        "building the plugin fixture failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let dll_suffix = std::env::consts::DLL_SUFFIX;
    let stdout = String::from_utf8(output.stdout).expect("cargo json output is utf-8");

    // Each line is a JSON object; the compiler-artifact for the fixture lists its
    // output filenames. We avoid a JSON dependency by scanning for the artifact
    // filename, which is unambiguous (it ends in the platform DLL suffix and
    // contains the fixture's normalized crate name).
    let needle = FIXTURE_PACKAGE.replace('-', "_");
    for line in stdout.lines() {
        if !line.contains("\"compiler-artifact\"") || !line.contains(FIXTURE_PACKAGE) {
            continue;
        }
        if let Some(path) = extract_artifact_path(line, &needle, dll_suffix) {
            return path;
        }
    }

    panic!("could not find the fixture {dll_suffix} artifact in cargo output:\n{stdout}");
}

/// Pulls the first `"...<needle>...<dll_suffix>"` JSON string value out of a
/// cargo `compiler-artifact` line.
fn extract_artifact_path(line: &str, needle: &str, dll_suffix: &str) -> Option<PathBuf> {
    // Filenames live in `"filenames":["...","..."]`; scan every quoted string
    // and take the one that looks like our dynamic library.
    for candidate in line.split('"') {
        if candidate.contains(needle) && candidate.ends_with(dll_suffix) {
            return Some(PathBuf::from(candidate));
        }
    }
    None
}

/// Returns the Cargo workspace root (the `server/` directory).
fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = .../server/crates/ferrumc-plugin-host
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .nth(2)
        .expect("manifest dir has a workspace-root ancestor")
        .to_path_buf()
}

#[test]
fn loads_and_enables_a_real_plugin_dylib() {
    let dylib = fixture_dylib();
    let mut host = PluginHost::in_memory();
    let loader = PluginLoader::new();

    let id = loader
        .load_file(dylib, &mut host)
        .expect("the well-formed fixture must load");
    assert_eq!(id, PluginId::new("ferrumc-fixture"));

    // Metadata was read across the ABI.
    let metadata = host.metadata(&id).expect("registered plugin has metadata");
    assert_eq!(metadata.name(), "Fixture Plugin");
    assert_eq!(metadata.version(), &Version::new(1, 2, 3));

    // It registers but is not auto-enabled; enabling runs `init` across the ABI.
    assert!(!host.is_enabled(&id));
    host.enable(&id).expect("fixture init succeeds");
    assert!(host.is_enabled(&id));

    // And it disables cleanly (shutdown crosses the ABI).
    host.disable(&id).expect("fixture disables");
    assert!(!host.is_enabled(&id));
}

#[test]
fn rejects_abi_version_mismatch() {
    let dylib = fixture_dylib();
    let mut host = PluginHost::in_memory();
    let loader = PluginLoader::new();

    let err = loader
        .load_file_with_entry(dylib, c"ferrumc_plugin_entry_bad_abi", &mut host)
        .expect_err("a bad ABI version must be rejected");

    match err {
        LoadError::AbiMismatch {
            found, expected, ..
        } => {
            assert_ne!(found, expected);
            assert_eq!(expected, ferrumc_plugin_api::abi::ABI_VERSION);
        }
        other => panic!("expected AbiMismatch, got {other:?}"),
    }
    // Nothing was registered.
    assert!(host.is_empty());
}

#[test]
fn rejects_missing_entrypoint() {
    let dylib = fixture_dylib();
    let mut host = PluginHost::in_memory();
    let loader = PluginLoader::new();

    let err = loader
        .load_file_with_entry(dylib, c"ferrumc_plugin_entry_does_not_exist", &mut host)
        .expect_err("a missing entrypoint must be rejected");

    assert!(
        matches!(err, LoadError::MissingEntrypoint { .. }),
        "expected MissingEntrypoint, got {err:?}"
    );
    assert!(host.is_empty());
}

#[test]
fn returns_null_vtable_error() {
    let dylib = fixture_dylib();
    let mut host = PluginHost::in_memory();
    let loader = PluginLoader::new();

    let err = loader
        .load_file_with_entry(dylib, c"ferrumc_plugin_entry_null", &mut host)
        .expect_err("a null vtable must be rejected");
    assert!(
        matches!(err, LoadError::NullVTable { .. }),
        "expected NullVTable, got {err:?}"
    );
}

#[test]
fn plugin_init_failure_is_contained() {
    let dylib = fixture_dylib();
    let mut host = PluginHost::in_memory();
    let loader = PluginLoader::new();

    // Load the good plugin and the fail-init plugin into the same host.
    let good = loader
        .load_file(dylib, &mut host)
        .expect("good plugin loads");

    let bad = loader
        .load_file_with_entry(dylib, c"ferrumc_plugin_entry_failinit", &mut host)
        .expect("fail-init plugin still *loads* (its metadata is valid)");
    assert_eq!(bad, PluginId::new("ferrumc-fixture-fail-init"));

    // Enabling the bad plugin fails, but the failure is contained.
    let err = host
        .enable(&bad)
        .expect_err("init failure surfaces as an enable error");
    assert!(
        matches!(err, HostError::PluginFailed { .. }),
        "expected PluginFailed, got {err:?}"
    );
    assert!(!host.is_enabled(&bad));

    // The host survived: the good plugin still enables and works.
    host.enable(&good).expect("good plugin still enables");
    assert!(host.is_enabled(&good));
    assert_eq!(host.len(), 2);
}

#[test]
fn load_dir_scans_and_registers_libraries() {
    // Build the fixture, then copy it into a dedicated directory and scan that,
    // so the scan sees a clean plugins folder rather than the whole target dir.
    let dylib = fixture_dylib();
    let dir = std::env::temp_dir().join(format!(
        "ferrumc-plugin-host-loaddir-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create plugins dir");
    let dest = dir.join(dylib.file_name().expect("dylib has a file name"));
    std::fs::copy(dylib, &dest).expect("copy fixture into plugins dir");
    // A non-library file that must be ignored by the scan.
    std::fs::write(dir.join("README.txt"), b"not a plugin").expect("write decoy");

    let mut host = PluginHost::in_memory();
    let report = PluginLoader::new()
        .load_dir(&dir, &mut host)
        .expect("directory scan succeeds");

    assert_eq!(report.loaded_count(), 1, "exactly one plugin loaded");
    assert_eq!(
        report.failure_count(),
        0,
        "no failures: {:?}",
        report.failed()
    );
    assert_eq!(report.loaded(), &[PluginId::new("ferrumc-fixture")]);

    let _ = std::fs::remove_dir_all(&dir);
}
