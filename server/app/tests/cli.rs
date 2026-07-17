//! CLI parsing + config-bootstrap acceptance tests.
//!
//! These link against the `ferrumc_app` library only (not the binary), exercising
//! the public [`Cli`] parser and [`load_or_init_config`] entry points directly:
//!
//! - argument parsing: `--port`/`--config`/`--bind` produce the right values, and
//!   bad input (non-numeric port, garbage bind, unknown flag, missing value) is
//!   rejected;
//! - `--version` is recognised by clap and prints rather than running;
//! - first run writes a commented template and continues with defaults;
//! - a subsequent start reads that template back to the identical config;
//! - a missing config path never panics;
//! - CLI overrides layer correctly (`--bind` then `--port`, port wins);
//! - a malformed existing config is rejected (the allowed startup-config error).
//!
//! Every filesystem test uses a `tempfile::tempdir`, so nothing is written into
//! the crate's working directory.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;

use ferrumc_app::{load_or_init_config, AppConfig, Cli};

/// `--port` parses into `Some(port)`.
#[test]
fn parses_port_override() {
    let cli = Cli::try_parse_from(["ferrumc", "--port", "25566"]).expect("valid args");
    assert_eq!(cli.port, Some(25566));
    assert_eq!(cli.bind, None);
    assert_eq!(cli.config, None);
}

/// `--config` parses into a path and `config_path` returns exactly it (the flag
/// takes precedence over the env fallback and the default).
#[test]
fn parses_config_path_override() {
    let cli = Cli::try_parse_from(["ferrumc", "--config", "/etc/ferrumc/server.toml"])
        .expect("valid args");
    assert_eq!(cli.config_path(), PathBuf::from("/etc/ferrumc/server.toml"));
}

/// `--bind` parses a full socket address.
#[test]
fn parses_bind_override() {
    let cli = Cli::try_parse_from(["ferrumc", "--bind", "0.0.0.0:25565"]).expect("valid args");
    assert_eq!(cli.bind, "0.0.0.0:25565".parse::<SocketAddr>().ok());
}

/// Invalid argument forms are all rejected by clap rather than silently accepted.
#[test]
fn rejects_bad_arguments() {
    assert!(Cli::try_parse_from(["ferrumc", "--port", "notanum"]).is_err());
    assert!(Cli::try_parse_from(["ferrumc", "--port", "70000"]).is_err()); // outside u16
    assert!(Cli::try_parse_from(["ferrumc", "--bind", "garbage"]).is_err());
    assert!(Cli::try_parse_from(["ferrumc", "--nope"]).is_err());
    assert!(Cli::try_parse_from(["ferrumc", "--config"]).is_err()); // flag with no value
}

/// `--version` is recognised by clap; parsing reports the version-display "error"
/// kind (clap models help/version as a non-fatal early exit) instead of running.
#[test]
fn version_flag_is_recognised() {
    let err = Cli::try_parse_from(["ferrumc", "--version"]).expect_err("version short-circuits");
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
}

/// First run on a missing path writes the commented template and continues with
/// defaults (never panics, never dies). The runtime world-dir default is filled.
#[test]
fn first_run_writes_template_and_uses_defaults() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    assert!(!path.exists());

    let config = load_or_init_config(&path, None, None).expect("first run never fails");

    assert!(path.exists(), "template must be written on first run");
    let written = std::fs::read_to_string(&path).expect("read back template");
    assert!(
        written.contains("FerrumC server configuration"),
        "written file must be the commented template"
    );

    // Defaults, plus the runtime world-dir fill.
    let expected = AppConfig::default()
        .with_world_dir(Some(PathBuf::from("world")))
        .expect("runtime world directory preserves valid defaults");
    assert_eq!(config, expected);
}

/// A second start reads the just-written template back to the identical config:
/// the all-commented template parses to the same defaults the first run used.
#[test]
fn subsequent_start_reads_written_template() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");

    let first = load_or_init_config(&path, None, None).expect("first run");
    let second = load_or_init_config(&path, None, None).expect("second run reads the file");
    assert_eq!(first, second);
}

/// A missing config path resolves to defaults without panicking even when the CLI
/// supplies no overrides (the core "runs with no config" guarantee).
#[test]
fn missing_config_does_not_panic() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("nested").join("does-not-exist.toml");
    // The parent dir does not exist, so the template write fails cleanly with an
    // error (never a panic); a writable location succeeds. Prove the writable case
    // here; the unwritable case is just an `Err`, asserted below.
    let writable = dir.path().join("config.toml");
    assert!(load_or_init_config(&writable, None, None).is_ok());
    assert!(load_or_init_config(&path, None, None).is_err());
}

/// `--port` overrides only the port of the configured bind address.
#[test]
fn port_override_replaces_only_the_port() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "bind = \"127.0.0.1:25565\"\n").expect("write config");

    let config = load_or_init_config(&path, Some(40000), None).expect("valid config");
    assert_eq!(
        config.bind(),
        "127.0.0.1:40000".parse::<SocketAddr>().unwrap()
    );
}

/// `--bind` overrides the full address.
#[test]
fn bind_override_replaces_the_full_address() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "bind = \"127.0.0.1:25565\"\n").expect("write config");

    let bind: SocketAddr = "0.0.0.0:1".parse().unwrap();
    let config = load_or_init_config(&path, None, Some(bind)).expect("valid config");
    assert_eq!(config.bind(), bind);
}

/// When both are supplied, `--bind` applies first and `--port` wins the port.
#[test]
fn port_wins_over_bind_port() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "bind = \"127.0.0.1:25565\"\n").expect("write config");

    let bind: SocketAddr = "0.0.0.0:1".parse().unwrap();
    let config = load_or_init_config(&path, Some(7), Some(bind)).expect("valid config");
    assert_eq!(config.bind(), "0.0.0.0:7".parse::<SocketAddr>().unwrap());
}

/// A malformed existing config is the one allowed startup-config error path: it is
/// rejected with an error (not a panic, not silent defaults).
#[test]
fn malformed_existing_config_is_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "ticks_per_second = 0\n").expect("write config");

    let err = load_or_init_config(&path, None, None).expect_err("zero tick rate is invalid");
    assert!(err.to_string().contains("invalid config"));
}
