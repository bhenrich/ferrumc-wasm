//! The `ferrumc-bench` runner.
//!
//! Runs the server-internal microbenchmarks, gathers reproducibility metadata
//! (commit, rustc, host, timestamp), writes a JSON artifact, and prints a
//! Markdown summary to stdout. This binary is the only part of the crate that
//! touches the outside world (clock, environment, subprocesses); the library
//! half is pure.
//!
//! ## Usage
//!
//! ```text
//! cargo run -p ferrumc-bench --release -- \
//!     --out target/bench-results.json \
//!     --timestamp "$(date -u +%FT%TZ)"
//! ```
//!
//! Flags: `--out <path>` (default `target/bench-results.json`), `--timestamp
//! <label>`, `--quick` (CI-fast iteration counts), `--filter <substr>` (run only
//! matching benchmarks).

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use ferrumc_bench::report::{to_markdown, SCHEMA_VERSION};
use ferrumc_bench::{run_all, BenchConfig, BenchReport, RunMetadata};

/// Parsed command-line arguments.
struct Args {
    out: PathBuf,
    timestamp: Option<String>,
    quick: bool,
    filter: Option<String>,
}

fn main() -> Result<()> {
    let args = parse_args()?;

    if cfg!(debug_assertions) {
        eprintln!("warning: this is a debug build; run with --release for meaningful numbers");
    }

    let mut config = if args.quick {
        BenchConfig::quick()
    } else {
        BenchConfig::default()
    };
    config.filter = args.filter;

    let metadata = gather_metadata(args.timestamp);
    let benchmarks = run_all(&config);

    let report = BenchReport {
        schema_version: SCHEMA_VERSION,
        metadata,
        benchmarks,
    };

    let json = serde_json::to_string_pretty(&report).context("serialize report to JSON")?;
    if let Some(parent) = args.out.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create output directory {}", parent.display()))?;
        }
    }
    fs::write(&args.out, &json)
        .with_context(|| format!("write JSON results to {}", args.out.display()))?;

    println!("{}", to_markdown(&report));
    eprintln!("wrote JSON results to {}", args.out.display());
    Ok(())
}

/// Parses `std::env::args`, returning the runner configuration.
fn parse_args() -> Result<Args> {
    let mut out = PathBuf::from("target/bench-results.json");
    let mut timestamp = None;
    let mut quick = false;
    let mut filter = None;

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--out" => {
                out = PathBuf::from(iter.next().context("--out requires a path argument")?);
            }
            "--timestamp" => {
                timestamp = Some(iter.next().context("--timestamp requires a value")?);
            }
            "--filter" => {
                filter = Some(iter.next().context("--filter requires a value")?);
            }
            "--quick" => quick = true,
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => bail!("unknown argument: {other} (try --help)"),
        }
    }

    Ok(Args {
        out,
        timestamp,
        quick,
        filter,
    })
}

/// Prints runner usage to stdout.
fn print_help() {
    println!("ferrumc-bench - server-internal microbenchmarks");
    println!();
    println!(
        "Usage: ferrumc-bench [--out <path>] [--timestamp <label>] [--quick] [--filter <substr>]"
    );
    println!();
    println!("  --out <path>        JSON output path (default: target/bench-results.json)");
    println!("  --timestamp <label> Free-form run timestamp label recorded in metadata");
    println!("  --quick             Use CI-fast iteration counts");
    println!("  --filter <substr>   Only run benchmarks whose group or name contains <substr>");
    println!("  -h, --help          Show this help");
}

/// Gathers reproducibility metadata from the environment.
fn gather_metadata(timestamp_label: Option<String>) -> RunMetadata {
    let commit_sha = env_or_command("FERRUMC_COMMIT", "git", &["rev-parse", "HEAD"])
        .unwrap_or_else(|| "unknown".to_owned());
    let commit_short = run_command("git", &["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|| commit_sha.chars().take(12).collect());
    let rustc_version =
        run_command("rustc", &["--version"]).unwrap_or_else(|| "unknown".to_owned());
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| run_command("hostname", &[]))
        .unwrap_or_else(|| "unknown".to_owned());
    let cpu_count = std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
    let timestamp_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|delta| delta.as_secs());

    RunMetadata {
        commit_sha,
        commit_short,
        rustc_version,
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        cpu_count,
        hostname,
        profile: if cfg!(debug_assertions) {
            "debug".to_owned()
        } else {
            "release".to_owned()
        },
        timestamp_unix,
        timestamp_label,
    }
}

/// Returns `var`'s value if set and non-empty, otherwise the trimmed stdout of
/// `cmd args`.
fn env_or_command(var: &str, cmd: &str, args: &[&str]) -> Option<String> {
    if let Ok(value) = std::env::var(var) {
        if !value.is_empty() {
            return Some(value);
        }
    }
    run_command(cmd, args)
}

/// Runs `cmd args` and returns its trimmed stdout, or `None` on any failure.
fn run_command(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}
