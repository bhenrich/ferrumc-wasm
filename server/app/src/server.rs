//! Server bring-up and lifecycle: build the world, spawn the driver, bind the
//! listener, and run the accept loop until shutdown.
//!
//! [`run`] wires every layer together and returns a [`RunningServer`] handle the
//! caller drives. The accept loop spawns one connection task per socket, bounded
//! by a [`Semaphore`], and a watch channel fans a shutdown signal out to the
//! driver, the accept loop, and every in-flight connection so the whole tree
//! winds down cleanly.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::task::JoinHandle;

use ferrumc_net::ConnectionLimits;
use ferrumc_session::{shard_for_position, SessionRouter};

use crate::config::AppConfig;
use crate::connection::{handle_connection, ConnContext};
use crate::driver;
use crate::plugins::{build_play_policy, load_plugins};
use crate::registries::ConfigRegistries;
use crate::world::build_world;

/// Capacity of the bounded command channel from connections to the driver.
///
/// Comfortably above the per-tick command volume a handful of players produce;
/// reaching it means a connection task is forced to await (backpressure) rather
/// than the driver ever blocking.
const COMMAND_CHANNEL_CAPACITY: usize = 1024;

/// A bound, running server: a handle to its listening address and shutdown.
///
/// Obtained from [`run`]. Read the bound address with
/// [`local_addr`](Self::local_addr) (useful when binding to port `0`) and wind
/// the server down with [`shutdown`](Self::shutdown).
#[derive(Debug)]
pub struct RunningServer {
    /// The address the listener is bound to.
    local_addr: SocketAddr,
    /// The shutdown signal fanned out to every task.
    shutdown: watch::Sender<bool>,
    /// The accept-loop task.
    accept_task: JoinHandle<()>,
    /// The simulation/session driver task.
    driver_task: JoinHandle<()>,
}

impl RunningServer {
    /// The address the listener is actually bound to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Signals shutdown and waits for the accept loop and driver to finish.
    ///
    /// In-flight connection tasks observe the same signal and end on their own;
    /// they are detached, so this resolves once the two owned tasks complete.
    ///
    /// # Errors
    ///
    /// Returns an error only if a joined task panicked.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        // A send error means every receiver is already gone — also a clean stop.
        let _ = self.shutdown.send(true);
        self.accept_task.await?;
        self.driver_task.await?;
        Ok(())
    }
}

/// Builds the world, starts the simulation, binds the listener, and begins
/// accepting connections.
///
/// Returns once the server is listening; the accept loop and driver run on
/// spawned tasks. Drive the lifetime through the returned [`RunningServer`].
///
/// # Errors
///
/// Returns an error if the spawn area fails to load, the join payload cannot be
/// built, or the listener cannot bind the configured address.
pub async fn run(config: &AppConfig) -> anyhow::Result<RunningServer> {
    let shard_pos = shard_for_position(config.spawn);
    let setup = build_world(config, shard_pos).await?;

    // The configuration-phase registry payloads are identical for every
    // connection; build them once and share behind an `Arc`.
    let config_registries = Arc::new(ConfigRegistries::build()?);

    // Build the play policy (spawn-protection veto, bypass permissions, command
    // tree) by driving the in-process plugin's config round-trip through storage.
    let policy = Arc::new(build_play_policy(config)?);

    // Prove the dynamic loader: scan the configured plugins directory across the
    // C ABI. Failures are logged and never fatal to startup.
    if let Some(dir) = &config.plugins_dir {
        match load_plugins(dir) {
            Ok(count) => {
                tracing::info!(plugins = count, dir = %dir.display(), "loaded dynamic plugins");
            }
            Err(err) => {
                tracing::warn!(%err, dir = %dir.display(), "failed to scan plugins directory");
            }
        }
    }

    let mut router = SessionRouter::new();
    // Scope multiplayer visibility to the configured play view distance.
    router.set_view_distance(config.view_distance);
    let shard_rx = router.register_shard(shard_pos);

    let (commands_tx, commands_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let driver_task = tokio::spawn(driver::run(
        router,
        setup.shard,
        shard_rx,
        commands_rx,
        config.tick_period(),
        shutdown_rx.clone(),
    ));

    let listener = TcpListener::bind(config.bind).await?;
    let local_addr = listener.local_addr()?;

    let ctx = ConnContext {
        limits: ConnectionLimits::default(),
        io_timeout: config.io_timeout,
        compression_threshold: config.compression_threshold,
        join_kit: setup.join_kit,
        config: config_registries,
        keep_alive_interval: config.keep_alive_interval,
        commands: commands_tx,
        policy,
    };

    let accept_task = tokio::spawn(accept_loop(
        listener,
        ctx,
        config.max_connections,
        shutdown_rx,
    ));

    Ok(RunningServer {
        local_addr,
        shutdown: shutdown_tx,
        accept_task,
        driver_task,
    })
}

/// Accepts connections until shutdown, spawning one bounded task per socket.
async fn accept_loop(
    listener: TcpListener,
    ctx: ConnContext,
    max_connections: usize,
    mut shutdown: watch::Receiver<bool>,
) {
    let limiter = Arc::new(Semaphore::new(max_connections.max(1)));
    loop {
        tokio::select! {
            biased;
            _ = shutdown.changed() => break,
            accepted = listener.accept() => {
                let (stream, _addr) = match accepted {
                    Ok(pair) => pair,
                    Err(err) => {
                        tracing::warn!(%err, "accept failed");
                        continue;
                    }
                };
                // Hold a permit for the connection's lifetime to bound concurrency.
                let Ok(permit) = Arc::clone(&limiter).acquire_owned().await else {
                    break;
                };
                let ctx = ctx.clone();
                let conn_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(err) = handle_connection(stream, &ctx, conn_shutdown).await {
                        tracing::debug!(%err, "connection ended with an error");
                    }
                });
            }
        }
    }
}
