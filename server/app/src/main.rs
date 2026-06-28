//! `FerrumC` server binary: parse the CLI, load (or first-run init) the config,
//! start the server, log a startup banner, and run until Ctrl-C.

use clap::Parser;

use ferrumc_app::{load_or_init_config, AppConfig, Cli, RunningServer};

/// Installs the tracing subscriber, honouring `RUST_LOG`, defaulting to `info`.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // `try_init` returns an error if a subscriber is already set; in `main` that
    // never happens, and ignoring it keeps startup panic-free.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Emits the one-block startup banner: identity + protocol, the bound address,
/// the dashboard address when enabled, and a `RUST_LOG` discoverability hint.
///
/// Reads the *actual* bound address from the [`RunningServer`] so it is correct
/// even when the config binds port `0` (an OS-assigned ephemeral port).
fn log_startup_banner(config: &AppConfig, server: &RunningServer) {
    tracing::info!(
        "FerrumC {} — protocol {} (Minecraft {})",
        env!("CARGO_PKG_VERSION"),
        ferrumc_registry::PROTOCOL_VERSION,
        ferrumc_registry::MINECRAFT_VERSION,
    );
    tracing::info!(addr = %server.local_addr(), "listening for Minecraft clients");
    if config.dashboard_enabled {
        tracing::info!(addr = %config.dashboard_bind, "observability dashboard available");
    }
    tracing::info!("set RUST_LOG=debug for verbose logs (e.g. RUST_LOG=debug ferrumc)");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    // `clap` handles --help and --version (printing and exiting 0) before this
    // returns; anything else surfaces as a usage error.
    let cli = Cli::parse();
    let config_path = cli.config_path();
    let config = load_or_init_config(&config_path, cli.port, cli.bind)?;

    let server = ferrumc_app::run(&config).await?;
    log_startup_banner(&config, &server);

    // Start the read-only observability dashboard on its own task so it never
    // blocks or stalls the simulation tick. It reads the snapshot the driver
    // publishes through `server.snapshot_handle()` and binds loopback by default.
    if config.dashboard_enabled {
        let snapshots = server.snapshot_handle();
        let dashboard_bind = config.dashboard_bind;
        tokio::spawn(async move {
            if let Err(err) = ferrumc_dashboard::run(dashboard_bind, snapshots).await {
                tracing::warn!(%err, "dashboard server exited");
            }
        });
    }

    tokio::signal::ctrl_c().await?;
    tracing::info!("shutdown signal received; winding down");
    server.shutdown().await?;
    tracing::info!("shutdown complete");
    Ok(())
}
