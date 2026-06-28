//! `FerrumC` server binary: load config, start the server, and run until Ctrl-C.

use std::path::PathBuf;

use ferrumc_app::AppConfig;

/// Default world directory the shipping server persists to when the config does
/// not name one. This is what makes the durable redb store the runtime default;
/// tests construct [`AppConfig`] directly and leave `world_dir` unset (in-memory)
/// or point it at a temp directory.
const DEFAULT_WORLD_DIR: &str = "world";

/// Installs the tracing subscriber, honouring `RUST_LOG`, defaulting to `info`.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // `try_init` returns an error if a subscriber is already set; in `main` that
    // never happens, and ignoring it keeps startup panic-free.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Loads the config from the first CLI argument or `FERRUMC_CONFIG`, falling
/// back to the documented defaults when neither is set.
fn load_config() -> anyhow::Result<AppConfig> {
    let path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("FERRUMC_CONFIG").ok());
    let mut config = match path {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .map_err(|err| anyhow::anyhow!("reading config {path}: {err}"))?;
            AppConfig::from_toml_str(&text)?
        }
        None => AppConfig::default(),
    };
    // Make durable redb storage the runtime default: a real server persists its
    // world unless the operator names a directory explicitly. (Tests bypass this
    // by constructing `AppConfig` directly.)
    if config.world_dir.is_none() {
        config.world_dir = Some(PathBuf::from(DEFAULT_WORLD_DIR));
    }
    Ok(config)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = load_config()?;
    let server = ferrumc_app::run(&config).await?;
    tracing::info!(addr = %server.local_addr(), "ferrumc listening");

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutdown signal received; winding down");
    server.shutdown().await?;
    tracing::info!("shutdown complete");
    Ok(())
}
