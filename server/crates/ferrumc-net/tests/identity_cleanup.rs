//! Repository-level regression for the one-way offline-identity cleanup.

use std::fs;
use std::path::{Path, PathBuf};

fn collect_source_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(directory)
        .expect("read workspace source directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("read every workspace source entry");
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() != "target" {
                collect_source_files(&path, files);
            }
        } else if path.file_name().is_some_and(|name| name == "Cargo.toml")
            || path.extension().is_some_and(|extension| extension == "rs")
        {
            files.push(path);
        }
    }
}

#[test]
fn legacy_identity_compatibility_surface_is_absent() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(
        !crate_root.join("src/offline.rs").exists(),
        "the compatibility adapter module must be deleted"
    );

    let workspace_root = crate_root
        .parent()
        .and_then(Path::parent)
        .expect("net crate is nested under the workspace");

    // Build the adapter identifier at runtime so this regression does not match
    // its own source, then prove it is absent throughout the net crate.
    let adapter_identifier = ["offline", "_uuid"].concat();
    let mut net_files = Vec::new();
    collect_source_files(crate_root, &mut net_files);
    net_files.sort();
    for path in net_files {
        let source = fs::read_to_string(&path).expect("net Rust/TOML source is UTF-8");
        assert!(
            !source.contains(&adapter_identifier),
            "legacy identity adapter remains in {}",
            path.display()
        );
    }

    // These implementation markers must be absent from every Rust/TOML source in
    // the workspace. Canonical core code and its vanilla vectors remain allowed.
    let forbidden = [
        ["OFFLINE", "_NAMESPACE"].concat(),
        ["OFFLINE", "_SEED_PREFIX"].concat(),
    ];
    let digest_markers = [["sha", "1::"].concat(), ["Sha", "1"].concat()];
    let offline_markers = [
        ["Offline", "Player:"].concat(),
        ["OFFLINE", "_NAMESPACE"].concat(),
        ["OFFLINE", "_SEED_PREFIX"].concat(),
    ];
    let version_three_constructor = ["Uuid::new", "_v3"].concat();
    let offline_function = ["fn ", "offline"].concat();

    let mut workspace_files = Vec::new();
    collect_source_files(workspace_root, &mut workspace_files);
    workspace_files.sort();
    for path in workspace_files {
        let source = fs::read_to_string(&path).expect("workspace Rust/TOML source is UTF-8");
        for marker in &forbidden {
            assert!(
                !source.contains(marker),
                "legacy identity marker {marker:?} remains in {}",
                path.display()
            );
        }
        if path.extension().is_some_and(|extension| extension == "rs") {
            let uses_legacy_digest = digest_markers.iter().any(|marker| source.contains(marker));
            let derives_offline_identity =
                offline_markers.iter().any(|marker| source.contains(marker));
            assert!(
                !(uses_legacy_digest && derives_offline_identity),
                "SHA-1 remains coupled to offline identity derivation in {}",
                path.display()
            );
            assert!(
                !(source.contains(&version_three_constructor)
                    && source.contains(&offline_function)),
                "a custom UUID namespace remains coupled to offline identity derivation in {}",
                path.display()
            );
        }
    }
}
