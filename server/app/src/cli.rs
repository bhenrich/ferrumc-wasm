//! Command-line interface and config bootstrap for the `ferrumc` binary.
//!
//! This module owns the [`clap`]-derived [`Cli`] argument parser, the rules for
//! resolving which config file to read, and the first-run logic that writes a
//! commented default config and then continues with defaults rather than dying.
//!
//! It lives in the library (not `main.rs`) so the integration tests in
//! `app/tests/` — which link against the lib only — can exercise argument
//! parsing, override precedence, and the first-run/template behaviour directly,
//! without spawning the binary.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::Parser;

use crate::config::AppConfig;

/// Config file resolved against the current working directory when neither
/// `--config` nor `$FERRUMC_CONFIG` is set.
const DEFAULT_CONFIG_FILE: &str = "config.toml";

/// World directory the shipping server persists to when the config names none.
///
/// This is what makes the durable redb store the runtime default. Tests construct
/// [`AppConfig`] directly and leave `world_dir` unset (in-memory) or point it at a
/// temp directory, so they never touch this.
const DEFAULT_WORLD_DIR: &str = "world";

/// The commented default config written verbatim on first run. Authored in
/// `app/config.template.toml`; every key is commented out, so the file parses to
/// the documented defaults until an operator edits it.
const CONFIG_TEMPLATE: &str = include_str!("../config.template.toml");

/// Parsed command-line arguments for the `ferrumc` server binary.
///
/// `clap` provides `--help` and `--version` automatically; the remaining flags
/// select the config file and optionally override the listen address. Invalid
/// values (a non-numeric `--port`, an unparseable `--bind`, an unknown flag) are
/// rejected by `clap` with a clear usage error.
#[derive(Debug, Parser)]
#[command(
    name = "ferrumc",
    version,
    about = "FerrumC: a Minecraft Java 1.21.8 (protocol 772) server",
    long_about = "FerrumC: a high-performance Minecraft Java 1.21.8 (protocol 772) server.\n\n\
                  Runs in offline mode by default. With no config file present, a commented \
                  config.toml is written next to it on first run and the server starts with \
                  defaults — edit it and restart to customise. Set RUST_LOG=debug for verbose logs."
)]
pub struct Cli {
    /// Path to the TOML config file. Falls back to `$FERRUMC_CONFIG`, then
    /// `./config.toml`. Written with commented defaults on first run if missing.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Override the listen port, replacing the port of the configured bind
    /// address. Applied after `--bind`, so it wins if both are given.
    #[arg(long, value_name = "PORT")]
    pub port: Option<u16>,
    /// Override the full bind address, e.g. `0.0.0.0:25565`. `--port` still
    /// refines the port afterwards if also supplied.
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<SocketAddr>,
}

impl Cli {
    /// Resolves which config file to use.
    ///
    /// Precedence: the `--config` flag, then the `FERRUMC_CONFIG` environment
    /// variable, then `./config.toml` relative to the current directory.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.config
            .clone()
            .or_else(|| std::env::var_os("FERRUMC_CONFIG").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILE))
    }
}

/// Loads the config at `path`, writing a commented default on first run, then
/// applies the CLI overrides and fills the runtime defaults.
///
/// Behaviour:
/// - **Missing file (first run):** writes [`CONFIG_TEMPLATE`] to `path`, logs a
///   clear message, and continues with [`AppConfig::default`]. The server never
///   dies just because no config exists yet. Using the defaults directly (rather
///   than re-parsing the file just written) guarantees first run cannot fail even
///   if the shipped template had a bug; the all-commented template parses to the
///   same defaults on the next run, so both starts converge.
/// - **Existing file:** read and parsed; a malformed document is the one allowed
///   startup-config error path — it returns `Err`, and `main` exits non-zero with
///   a message naming the offending key and the expected type (from serde/toml).
/// - **Overrides:** `bind` is applied first (full address), then `port` refines
///   the port, so an explicit `--port` always wins.
/// - **Runtime default:** an unset `world_dir` is filled with the shipping
///   default world directory so a real server persists by default.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read, if an existing file is
/// a malformed or invalid config, or if the template cannot be written to a
/// missing `path` (for example, an unwritable directory).
pub fn load_or_init_config(
    path: &Path,
    port: Option<u16>,
    bind: Option<SocketAddr>,
) -> anyhow::Result<AppConfig> {
    let mut config = if path.exists() {
        let text = std::fs::read_to_string(path)
            .map_err(|err| anyhow::anyhow!("reading config {}: {err}", path.display()))?;
        AppConfig::from_toml_str(&text)
            .map_err(|err| anyhow::anyhow!("invalid config {}: {err}", path.display()))?
    } else {
        std::fs::write(path, CONFIG_TEMPLATE).map_err(|err| {
            anyhow::anyhow!("writing default config to {}: {err}", path.display())
        })?;
        tracing::info!(
            path = %path.display(),
            "wrote a default config; edit and restart to customise — starting with defaults"
        );
        AppConfig::default()
    };

    // CLI overrides sit on top of the loaded (or default) config: the full bind
    // address first, then --port refines just the port so it wins over both.
    if let Some(addr) = bind {
        config.bind = addr;
    }
    if let Some(port) = port {
        config.bind.set_port(port);
    }

    // Make the durable redb store the runtime default unless the operator named a
    // world directory explicitly.
    if config.world_dir.is_none() {
        config.world_dir = Some(PathBuf::from(DEFAULT_WORLD_DIR));
    }

    Ok(config)
}
