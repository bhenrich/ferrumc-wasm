//! Developer task runner for the `FerrumC` workspace.
//!
//! Currently exposes protocol code generation:
//!
//! - `cargo xtask generate` — regenerate the checked-in protocol code.
//! - `cargo xtask generate --check` — fail (with a diff) if it has drifted.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use ferrumc_proto_gen::{GenPaths, Mode};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let result = match args.first().map(String::as_str) {
        Some("generate") => {
            let mode = if args.iter().skip(1).any(|a| a == "--check") {
                Mode::Check
            } else {
                Mode::Write
            };
            cmd_generate(mode)
        }
        other => {
            print_usage(other);
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err:?}");
            ExitCode::FAILURE
        }
    }
}

/// Runs protocol generation in the requested [`Mode`] against the workspace's
/// pinned spec and generated directory.
fn cmd_generate(mode: Mode) -> Result<()> {
    let paths = workspace_paths()?;

    match mode {
        Mode::Write => {
            let report =
                ferrumc_proto_gen::run(&paths, mode).context("protocol generation failed")?;
            if report.is_up_to_date() {
                println!("protocol code already up to date");
            } else {
                println!("protocol code: {} file(s) updated", report.changed().len());
                for relative in report.changed() {
                    println!("  {}", relative.display());
                }
            }
        }
        Mode::Check => {
            ferrumc_proto_gen::run(&paths, mode)
                .context("run `cargo xtask generate` to update the committed files")?;
            println!("protocol code up to date");
        }
    }

    Ok(())
}

/// Resolves the pinned spec and generated-output paths from this crate's
/// compile-time location, so the command works regardless of the caller's CWD.
fn workspace_paths() -> Result<GenPaths> {
    // CARGO_MANIFEST_DIR = <repo>/server/xtask
    let xtask_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let server_dir = xtask_dir
        .parent()
        .context("xtask directory has no parent (expected <repo>/server)")?;
    let repo_root = server_dir
        .parent()
        .context("server directory has no parent (expected <repo>)")?;

    let manifest: PathBuf = repo_root.join("fixtures/protocol/1_21_8/manifest.toml");
    let packets: PathBuf = repo_root.join("docs/protocol/1_21_8/packets.toml");
    let generated_dir: PathBuf = server_dir.join("crates/ferrumc-proto/src/generated");

    Ok(GenPaths::new(manifest, packets, generated_dir))
}

/// Prints CLI usage to stderr, noting the unknown command when there was one.
fn print_usage(unknown: Option<&str>) {
    if let Some(cmd) = unknown {
        eprintln!("xtask: unknown command `{cmd}`");
    }
    eprintln!("Usage: cargo xtask <command>");
    eprintln!("Commands:");
    eprintln!("  generate          Regenerate checked-in protocol code");
    eprintln!("  generate --check  Verify generated code is up to date (CI gate)");
}
