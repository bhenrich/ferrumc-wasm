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
use ferrumc_observability::{CounterRegistry, ServerClock, SnapshotPublisher};
use ferrumc_session::{shard_for_position, SessionRouter};

use crate::config::AppConfig;
use crate::connection::{build_status_response, handle_connection, ConnContext};
use crate::driver;
use crate::plugins::{build_play_policy, load_plugins};
use crate::registries::ConfigRegistries;
use crate::storage_worker::{run_storage_worker, StorageFlushRequest};
use crate::world::build_world;

/// Capacity of the bounded command channel from connections to the driver.
///
/// Comfortably above the per-tick command volume a handful of players produce;
/// reaching it means a connection task is forced to await (backpressure) rather
/// than the driver ever blocking.
const COMMAND_CHANNEL_CAPACITY: usize = 1024;

/// Capacity of the bounded storage-flush channel from the driver to the storage
/// worker.
///
/// Each slot is one tick's (or one chunk-release's) batch of overlays + journal
/// entries. Sized so the worker has ample headroom under normal edit volume; when
/// it does fill, the driver's end-of-tick flush *defers* (keeps the chunks
/// persist-dirty and retries next tick) rather than blocking the tick.
const STORAGE_FLUSH_CHANNEL_CAPACITY: usize = 256;

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
    /// The off-tick storage worker task that persists chunk overlays and the
    /// mutation journal. Awaited *after* the driver on shutdown so the driver's
    /// final flush is committed before the server returns.
    storage_worker_task: JoinHandle<()>,
    /// The shared metric registry, fed by the driver and every connection task.
    /// Exposed for an on-demand metrics snapshot (see [`metrics`](Self::metrics)
    /// and [`dump_metrics`](Self::dump_metrics)).
    metrics: Arc<CounterRegistry>,
    /// The read side of the per-tick [`ServerSnapshot`] the driver publishes.
    /// Handed to the read-only dashboard task via [`snapshot_handle`].
    ///
    /// [`ServerSnapshot`]: ferrumc_observability::ServerSnapshot
    /// [`snapshot_handle`]: Self::snapshot_handle
    snapshots: SnapshotPublisher,
}

impl RunningServer {
    /// The address the listener is actually bound to.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The shared metric registry.
    ///
    /// Call [`snapshot`](ferrumc_observability::CounterRegistry::snapshot) on it
    /// for an owned, serializable view of every metric (the on-demand JSON
    /// snapshot surface a future exporter or admin command can scrape).
    #[must_use]
    pub fn metrics(&self) -> &CounterRegistry {
        &self.metrics
    }

    /// Emits the current metrics as one structured tracing event carrying JSON.
    ///
    /// A convenience over [`metrics`](Self::metrics) for operators who just want
    /// the snapshot in the logs (for example on a future SIGUSR1 or admin hook).
    pub fn dump_metrics(&self) {
        self.metrics.dump();
    }

    /// A read handle onto the per-tick [`ServerSnapshot`] the driver publishes.
    ///
    /// Cloned and handed to the read-only dashboard task; the handle only ever
    /// reads the latest snapshot and never mutates server state.
    ///
    /// [`ServerSnapshot`]: ferrumc_observability::ServerSnapshot
    #[must_use]
    pub fn snapshot_handle(&self) -> SnapshotPublisher {
        self.snapshots.clone()
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
        // The driver performs its final flush and then drops its storage sender;
        // await it first so that send completes, then await the worker so it
        // observes the closed channel, drains everything pending, and exits. This
        // ordering is what makes a graceful shutdown durable.
        self.driver_task.await?;
        self.storage_worker_task.await?;
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

    // Build the play policy (bypass permissions, command tree, spawn) and the
    // long-lived block-event dispatcher that owns the plugin host. The dispatcher
    // is shared by every connection task so the plugins' `before_block_*` decision
    // hooks run at the intent boundary, off the simulation tick.
    let (policy, block_events) = build_play_policy(config)?;
    let policy = Arc::new(policy);
    let block_events = Arc::new(block_events);

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

    // Shared observability state: one atomic-backed metric registry fed by the
    // driver and every connection task, and a single-writer server clock the
    // driver publishes each tick so connection tasks can stamp packet traces.
    let metrics = Arc::new(CounterRegistry::new());
    let clock = ServerClock::new();

    // The driver publishes a read-only snapshot here every tick; the dashboard
    // task reads it through a clone of this handle. Seeded empty until the first
    // tick lands.
    let snapshots = SnapshotPublisher::default();

    // The driver emits persistence work onto this bounded channel; the storage
    // worker owns the world store and commits it off the tick. The store is shared
    // (the driver also reads it on the chunk load-or-generate path).
    let store = setup.store;
    let (storage_tx, storage_rx) =
        mpsc::channel::<StorageFlushRequest>(STORAGE_FLUSH_CHANNEL_CAPACITY);
    let storage_worker_task = tokio::spawn(run_storage_worker(
        storage_rx,
        Arc::clone(&store),
        Arc::clone(&metrics),
        config.tick_period(),
    ));

    let driver_task = tokio::spawn(driver::run(
        router,
        setup.shard,
        store,
        setup.generator,
        shard_rx,
        commands_rx,
        config.tick_period(),
        Arc::clone(&metrics),
        clock.clone(),
        storage_tx,
        snapshots.clone(),
        shutdown_rx.clone(),
    ));

    // A port clash is the common operator mistake, so name the port and the fix
    // instead of surfacing a bare "Address already in use (os error 48)".
    let listener = match TcpListener::bind(config.bind).await {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            return Err(anyhow::anyhow!(
                "port {} is already in use (bind {}). Another server may be running — \
                 pick a different port with --port <PORT> (or set `bind` in config.toml), \
                 or find what holds it with `lsof -i :{}`.",
                config.bind.port(),
                config.bind,
                config.bind.port(),
            ));
        }
        Err(err) => return Err(anyhow::anyhow!("binding to {}: {err}", config.bind)),
    };
    let local_addr = listener.local_addr()?;

    // Render the server-list status response once; it advertises the connection
    // ceiling as the player max and never changes for the server's lifetime. A
    // ceiling above `u32::MAX` saturates rather than wrapping to a tiny max.
    let max_players = u32::try_from(config.max_connections).unwrap_or(u32::MAX);
    let status_response = Arc::new(build_status_response(max_players)?);

    let ctx = ConnContext {
        limits: ConnectionLimits::default(),
        io_timeout: config.io_timeout,
        compression_threshold: config.compression_threshold,
        join_kit: setup.join_kit,
        config: config_registries,
        keep_alive_interval: config.keep_alive_interval,
        chunk_stream_interval: config.chunk_stream_interval,
        commands: commands_tx,
        policy,
        block_events,
        status_response,
        view_distance: config.view_distance,
        metrics: Arc::clone(&metrics),
        clock,
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
        storage_worker_task,
        metrics,
        snapshots,
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
