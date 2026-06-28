//! `FerrumC` server binary: parse the CLI, load (or first-run init) the config,
//! start the server, log a startup banner, and run until a shutdown signal.
//!
//! Shutdown is triggered by Ctrl-C and, on Unix, also by `SIGTERM` and `SIGHUP`
//! (so `kill`, container stop, and `systemd stop` flush player/world state
//! instead of losing recent edits). `SIGKILL` cannot be caught, so it is not
//! handled; the periodic per-tick/timer storage flush bounds the data lost to a
//! hard kill.

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

    let signal = wait_for_shutdown_signal().await?;
    tracing::info!(signal, "shutdown signal received; winding down");
    server.shutdown().await?;
    tracing::info!("shutdown complete");
    Ok(())
}

/// Awaits the first OS shutdown signal and returns its name for logging.
///
/// On Unix this selects over Ctrl-C (`SIGINT`), `SIGTERM`, and `SIGHUP` so that
/// `kill`, container stop, and `systemd stop` run the same graceful flush path
/// as an interactive Ctrl-C. On non-Unix platforms only Ctrl-C is available.
///
/// `SIGKILL` is uncatchable by design and is therefore not handled; the periodic
/// per-tick/timer storage flush bounds how much state a hard kill can lose.
#[cfg(unix)]
async fn wait_for_shutdown_signal() -> anyhow::Result<&'static str> {
    use tokio::signal::unix::{signal, SignalKind};

    // Registration only fails on a malformed signal kind or exhausted resources,
    // both of which are unrecoverable at startup — matching the existing
    // startup-time `expect` style.
    let mut sigterm = signal(SignalKind::terminate()).expect("register SIGTERM handler");
    let mut sighup = signal(SignalKind::hangup()).expect("register SIGHUP handler");

    let signal = tokio::select! {
        result = tokio::signal::ctrl_c() => {
            result?;
            "SIGINT"
        }
        _ = sigterm.recv() => "SIGTERM",
        _ = sighup.recv() => "SIGHUP",
    };
    Ok(signal)
}

/// Awaits Ctrl-C, the only portable shutdown signal on non-Unix platforms.
#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> anyhow::Result<&'static str> {
    tokio::signal::ctrl_c().await?;
    Ok("CTRL-C")
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::{Context, Waker};

    use super::*;

    /// Registering the signal handlers must not, by itself, complete the future:
    /// it has to stay pending until a real signal is delivered, or the server
    /// would shut itself down on startup. Polling once with a no-op waker inside
    /// a runtime context (the tokio signal driver needs one) proves the
    /// selection compiles, registers without panicking, and parks — with no
    /// wall-clock wait and no risk of hanging the suite.
    ///
    /// Actual signal *delivery* (SIGINT/SIGTERM/SIGHUP → graceful flush) is
    /// process-global and is verified by manual integration testing.
    #[test]
    fn shutdown_signal_pends_until_delivered() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build current-thread test runtime");

        runtime.block_on(async {
            let mut future = std::pin::pin!(wait_for_shutdown_signal());
            let waker = Waker::noop();
            let mut cx = Context::from_waker(waker);
            assert!(
                future.as_mut().poll(&mut cx).is_pending(),
                "shutdown signal future resolved without a delivered signal",
            );
        });
    }
}
