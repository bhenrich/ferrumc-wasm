#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod emit;
mod error;
mod packets;
mod spec;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub use error::GenError;

use crate::emit::{emit, normalize_endings, normalize_rust};
use crate::packets::PacketSpec;
use crate::spec::Spec;

/// What [`run`] should do with the freshly generated output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Write changed files to disk (creating directories as needed).
    Write,
    /// Do not touch disk; fail with [`GenError::Drift`] if committed files differ.
    Check,
}

/// Filesystem locations the generator reads from and writes to.
///
/// Fields are private so the path layout stays an implementation detail across
/// the crate boundary; construct with [`GenPaths::new`].
#[derive(Debug, Clone)]
pub struct GenPaths {
    manifest: PathBuf,
    packets: PathBuf,
    generated_dir: PathBuf,
}

impl GenPaths {
    /// Builds a [`GenPaths`] from the pinned `manifest.toml`, the declarative
    /// `packets.toml`, and the output `generated/` directory (e.g.
    /// `crates/ferrumc-proto/src/generated`).
    pub fn new(
        manifest: impl Into<PathBuf>,
        packets: impl Into<PathBuf>,
        generated_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            manifest: manifest.into(),
            packets: packets.into(),
            generated_dir: generated_dir.into(),
        }
    }

    /// Path to the pinned spec (`manifest.toml`) the generator validates against.
    pub fn manifest(&self) -> &Path {
        &self.manifest
    }

    /// Path to the declarative packet spec (`packets.toml`).
    pub fn packets(&self) -> &Path {
        &self.packets
    }

    /// Directory the generated files are written to / checked against.
    pub fn generated_dir(&self) -> &Path {
        &self.generated_dir
    }
}

/// Outcome of a successful [`run`].
///
/// In [`Mode::Write`] this reports which files were (re)written; in
/// [`Mode::Check`] a success means nothing drifted, so the list is empty.
#[derive(Debug, Clone, Default)]
pub struct Report {
    changed: Vec<PathBuf>,
}

impl Report {
    /// Relative paths (under [`GenPaths::generated_dir`]) that were rewritten.
    pub fn changed(&self) -> &[PathBuf] {
        &self.changed
    }

    /// Whether generation made no changes (everything was already current).
    pub fn is_up_to_date(&self) -> bool {
        self.changed.is_empty()
    }
}

/// Generates the protocol code, then either writes it or checks it for drift.
///
/// The pipeline is identical for both modes: load + validate the pinned spec,
/// emit every file in memory, normalize each through `rustfmt`, then compare
/// against what is on disk. Only the final action differs ([`Mode::Write`]
/// writes changed files; [`Mode::Check`] reports drift via [`GenError::Drift`]).
///
/// # Errors
///
/// Returns a [`GenError`] if the spec is missing/invalid/mismatched, `rustfmt`
/// fails, a filesystem operation fails, or (in [`Mode::Check`]) the committed
/// output is stale.
pub fn run(paths: &GenPaths, mode: Mode) -> Result<Report, GenError> {
    let spec = Spec::load_pinned(paths.manifest())?;
    let packets = PacketSpec::load(paths.packets())?;
    let normalized = normalized_files(&spec, &packets)?;

    match mode {
        Mode::Write => write_files(paths.generated_dir(), &normalized),
        Mode::Check => check_files(paths.generated_dir(), &normalized),
    }
}

/// Emits and normalizes every generated file into `relative path -> source`.
fn normalized_files(
    spec: &Spec,
    packets: &PacketSpec,
) -> Result<BTreeMap<PathBuf, String>, GenError> {
    let mut normalized = BTreeMap::new();
    for (relative, raw) in emit(spec, packets) {
        let content = if relative.extension().is_some_and(|ext| ext == "rs") {
            normalize_rust(&raw)?
        } else {
            // Non-Rust artifacts still get LF + single trailing newline, but no
            // rustfmt. (There are none yet; this keeps the path future-proof.)
            normalize_endings(&raw)
        };
        normalized.insert(relative, content);
    }
    Ok(normalized)
}

/// Writes only the files whose on-disk contents differ from `normalized`.
fn write_files(
    generated_dir: &Path,
    normalized: &BTreeMap<PathBuf, String>,
) -> Result<Report, GenError> {
    let mut changed = Vec::new();
    for (relative, content) in normalized {
        let target = generated_dir.join(relative);
        if read_existing(&target)?.as_deref() == Some(content) {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| GenError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(&target, content).map_err(|source| GenError::Io {
            path: target.clone(),
            source,
        })?;
        changed.push(relative.clone());
    }
    Ok(Report { changed })
}

/// Compares on-disk files to `normalized`, returning [`GenError::Drift`] on any
/// difference (missing file or mismatched contents).
fn check_files(
    generated_dir: &Path,
    normalized: &BTreeMap<PathBuf, String>,
) -> Result<Report, GenError> {
    let mut report = String::new();
    for (relative, expected) in normalized {
        let target = generated_dir.join(relative);
        match read_existing(&target)? {
            Some(actual) if actual == *expected => {}
            Some(actual) => {
                // Writing to a String is infallible; the Result is discarded.
                let _ = writeln!(report, "~ {} (out of date)", relative.display());
                report.push_str(&line_diff(&actual, expected));
            }
            None => {
                let _ = writeln!(report, "+ {} (missing)", relative.display());
            }
        }
    }

    if report.is_empty() {
        Ok(Report::default())
    } else {
        report.push_str("run `cargo xtask generate` to update the committed files.");
        Err(GenError::Drift(report))
    }
}

/// Reads a file if it exists, mapping a genuine I/O failure to [`GenError::Io`].
fn read_existing(path: &Path) -> Result<Option<String>, GenError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(GenError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Renders a minimal line-oriented diff: `-` for committed lines, `+` for the
/// freshly generated replacements. Enough for a reviewer to see what drifted.
fn line_diff(actual: &str, expected: &str) -> String {
    let actual_lines: Vec<&str> = actual.lines().collect();
    let expected_lines: Vec<&str> = expected.lines().collect();
    let max = actual_lines.len().max(expected_lines.len());

    let mut diff = String::new();
    for i in 0..max {
        let a = actual_lines.get(i).copied();
        let e = expected_lines.get(i).copied();
        if a == e {
            continue;
        }
        // Writing to a String is infallible; the Result is discarded.
        if let Some(a) = a {
            let _ = writeln!(diff, "    - {a}");
        }
        if let Some(e) = e {
            let _ = writeln!(diff, "    + {e}");
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emit::GENERATED_HEADER;
    use crate::spec::{EXPECTED_DATA_VERSION, EXPECTED_PROTOCOL_VERSION};

    /// Locates the vendored, real `manifest.toml` relative to this crate.
    fn pinned_manifest() -> PathBuf {
        // CARGO_MANIFEST_DIR = .../core/server/crates/ferrumc-proto-gen
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        crate_dir
            .ancestors()
            .nth(3)
            .expect("crate dir has a 3rd ancestor (repo root)")
            .join("fixtures/protocol/1_21_8/manifest.toml")
    }

    /// Locates the checked-in declarative packet spec relative to this crate.
    fn pinned_packets() -> PathBuf {
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        crate_dir
            .ancestors()
            .nth(3)
            .expect("crate dir has a 3rd ancestor (repo root)")
            .join("docs/protocol/1_21_8/packets.toml")
    }

    fn load_spec() -> Spec {
        Spec::load_pinned(&pinned_manifest()).expect("vendored manifest loads")
    }

    fn load_packets() -> PacketSpec {
        PacketSpec::load(&pinned_packets()).expect("checked-in packets.toml loads")
    }

    #[test]
    fn pinned_manifest_validates_to_supported_versions() {
        let spec = load_spec();
        assert_eq!(spec.protocol_version, EXPECTED_PROTOCOL_VERSION);
        assert_eq!(spec.data_version, EXPECTED_DATA_VERSION);
        assert!(!spec.source_commit.is_empty());
    }

    #[test]
    fn spec_rejects_protocol_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.toml");
        std::fs::write(
            &path,
            "[source]\ncommit = \"deadbeef\"\n[minecraft]\nprotocol_version = 770\ndata_version = 4440\n",
        )
        .expect("write manifest");

        let err = Spec::load_pinned(&path).expect_err("mismatch must fail");
        assert!(matches!(
            err,
            GenError::SpecMismatch {
                found_protocol: 770,
                ..
            }
        ));
    }

    #[test]
    fn spec_rejects_data_version_mismatch() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.toml");
        std::fs::write(
            &path,
            "[source]\ncommit = \"deadbeef\"\n[minecraft]\nprotocol_version = 772\ndata_version = 1\n",
        )
        .expect("write manifest");

        let err = Spec::load_pinned(&path).expect_err("mismatch must fail");
        assert!(matches!(err, GenError::SpecMismatch { found_data: 1, .. }));
    }

    #[test]
    fn spec_missing_file_is_classified() {
        let err = Spec::load_pinned(Path::new("/nonexistent/manifest.toml"))
            .expect_err("missing file must fail");
        assert!(matches!(err, GenError::SpecRead { .. }));
    }

    #[test]
    fn spec_garbage_is_parse_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("manifest.toml");
        std::fs::write(&path, "this is = not [valid").expect("write");
        let err = Spec::load_pinned(&path).expect_err("garbage must fail");
        assert!(matches!(err, GenError::SpecParse { .. }));
    }

    #[test]
    fn emit_is_deterministic() {
        let spec = load_spec();
        let packets = load_packets();
        assert_eq!(emit(&spec, &packets), emit(&spec, &packets));
    }

    /// Cross-checks every spec packet id against the vendored `minecraft-data`
    /// 772 `protocol.json`, when it is present.
    ///
    /// We compare *ids* (not names): each `(state, direction, id)` we declare
    /// must appear in the upstream id mapping. Names diverge between our
    /// `PascalCase` types and minecraft-data's `snake_case`, so a name-level
    /// assertion would be brittle; the id is the wire contract that matters.
    ///
    /// TODO: `protocol.json` is not vendored yet (only blocks/biomes/version
    /// were pulled in M04), so this test skips. Vendor
    /// `fixtures/protocol/1_21_8/protocol.json` to activate it.
    #[test]
    fn spec_ids_match_vendored_protocol_json() {
        use crate::packets::{Direction, State};

        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_root = crate_dir
            .ancestors()
            .nth(3)
            .expect("crate dir has a 3rd ancestor (repo root)");
        let protocol_json = repo_root.join("fixtures/protocol/1_21_8/protocol.json");

        if !protocol_json.exists() {
            eprintln!(
                "skipping: {} not vendored yet (see test TODO)",
                protocol_json.display()
            );
            return;
        }

        let raw = std::fs::read_to_string(&protocol_json).expect("read protocol.json");
        let root: serde_json::Value =
            serde_json::from_str(&raw).expect("protocol.json must be valid JSON");

        // minecraft-data names the handshake state "handshaking" and uses
        // toClient/toServer for direction.
        let state_key = |state: State| match state {
            State::Handshake => "handshaking",
            State::Status => "status",
            State::Login => "login",
            State::Configuration => "configuration",
        };
        let dir_key = |direction: Direction| match direction {
            Direction::Serverbound => "toServer",
            Direction::Clientbound => "toClient",
        };

        // The id->name mapper lives at
        // [state][direction].types.packet[1][0].type[1].mappings.
        let mappings = |state: State, direction: Direction| -> Vec<i64> {
            let node = &root[state_key(state)][dir_key(direction)]["types"]["packet"][1][0]["type"]
                [1]["mappings"];
            let obj = node.as_object().unwrap_or_else(|| {
                panic!(
                    "unexpected protocol.json shape for {}/{}; update this test",
                    state_key(state),
                    dir_key(direction)
                )
            });
            obj.keys()
                .map(|k| {
                    let hex = k.trim_start_matches("0x");
                    i64::from_str_radix(hex, 16).expect("packet id is hex")
                })
                .collect()
        };

        let packets = load_packets();
        for p in &packets.packets {
            let ids = mappings(p.state, p.direction);
            assert!(
                ids.contains(&i64::from(p.id)),
                "{} ({:?}/{:?}) id {:#04x} not found in vendored protocol.json",
                p.name,
                p.state,
                p.direction,
                p.id
            );
        }
    }

    #[test]
    fn mod_rs_carries_header_pin_and_protocol_constant() {
        let spec = load_spec();
        let packets = load_packets();
        let normalized = normalized_files(&spec, &packets).expect("normalize");
        let mod_rs = normalized
            .get(Path::new("mod.rs"))
            .expect("mod.rs is emitted");

        assert!(
            mod_rs.starts_with(GENERATED_HEADER),
            "first line must be the exact @generated header"
        );
        assert!(
            mod_rs.contains(&format!("// source: minecraft-data {}", spec.source_commit)),
            "source pin line must name the vendored commit"
        );
        assert!(mod_rs.contains("pub const PROTOCOL_VERSION: i32 = 772;"));
    }

    #[test]
    fn output_is_lf_with_single_trailing_newline() {
        let spec = load_spec();
        let packets = load_packets();
        let normalized = normalized_files(&spec, &packets).expect("normalize");
        for (path, content) in &normalized {
            assert!(!content.contains('\r'), "{path:?} must not contain CR");
            assert!(content.ends_with('\n'), "{path:?} must end with newline");
            assert!(
                !content.ends_with("\n\n"),
                "{path:?} must have exactly one trailing newline"
            );
        }
    }

    #[test]
    fn normalization_is_idempotent() {
        let spec = load_spec();
        let packets = load_packets();
        let once = normalized_files(&spec, &packets).expect("first pass");
        for content in once.values() {
            let twice = normalize_rust(content).expect("re-normalize");
            assert_eq!(&twice, content, "rustfmt + endings must be a fixed point");
        }
    }

    #[test]
    fn write_then_check_round_trips_clean() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = GenPaths::new(
            pinned_manifest(),
            pinned_packets(),
            dir.path().join("generated"),
        );

        let report = run(&paths, Mode::Write).expect("write succeeds");
        assert!(!report.is_up_to_date(), "first write creates files");
        assert!(report.changed().iter().any(|p| p == Path::new("mod.rs")));

        // Second write is a no-op; --check is clean.
        let again = run(&paths, Mode::Write).expect("idempotent write");
        assert!(again.is_up_to_date(), "second write changes nothing");
        run(&paths, Mode::Check).expect("check is clean after write");
    }

    #[test]
    fn check_detects_drift() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = GenPaths::new(
            pinned_manifest(),
            pinned_packets(),
            dir.path().join("generated"),
        );
        run(&paths, Mode::Write).expect("seed files");

        // Corrupt the committed file, then ensure --check fails with a diff.
        let mod_rs = paths.generated_dir().join("mod.rs");
        std::fs::write(&mod_rs, "// hand-edited\n").expect("corrupt");

        let err = run(&paths, Mode::Check).expect_err("drift must fail");
        match err {
            GenError::Drift(report) => {
                assert!(report.contains("mod.rs"));
                assert!(report.contains("out of date"));
            }
            other => panic!("expected drift, got {other:?}"),
        }
    }

    #[test]
    fn check_reports_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = GenPaths::new(
            pinned_manifest(),
            pinned_packets(),
            dir.path().join("generated"),
        );
        let err = run(&paths, Mode::Check).expect_err("missing files must fail");
        match err {
            GenError::Drift(report) => assert!(report.contains("missing")),
            other => panic!("expected drift, got {other:?}"),
        }
    }
}
