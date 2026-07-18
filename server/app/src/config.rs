//! Minimal server configuration: parsing, defaults, and validation.
//!
//! The vertical slice needs only a handful of knobs (where to bind, the spawn
//! area to keep resident, the play view distances, and a few transport limits).
//! [`AppConfig`] carries the validated, runtime-ready values; [`RawConfig`] is
//! the optional-field TOML shape that merges over the documented defaults.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ferrumc_config::{AccessConfig, PacketBudgetConfig, WorldConfig};
use ferrumc_math::Vec3;
use serde::Deserialize;

/// Default address the server binds to when the config omits one.
const DEFAULT_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 25_565);

/// Default address the read-only observability dashboard binds to. Loopback by
/// design: the dashboard is never exposed off-host unless the operator opts in.
/// A typed [`SocketAddr`] const so the default needs no runtime parse (and so the
/// [`Default`] impl carries no `parse().expect()`).
const DEFAULT_DASHBOARD_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9090);

/// Whether the read-only observability dashboard starts by default.
const DEFAULT_DASHBOARD_ENABLED: bool = true;

/// Default ceiling on concurrent connections.
const DEFAULT_MAX_CONNECTIONS: usize = 256;

/// Default per-I/O socket timeout, in seconds.
const DEFAULT_IO_TIMEOUT_SECS: u64 = 30;

/// Default play view distance, in chunks.
const DEFAULT_VIEW_DISTANCE: i32 = 10;

/// Default play simulation distance, in chunks.
const DEFAULT_SIMULATION_DISTANCE: i32 = 10;

/// Default world-spawn coordinate: chunk-centre, one block above the flat
/// grass surface at `y = 63`.
const DEFAULT_SPAWN: Vec3 = Vec3::new(8.0, 64.0, 8.0);

/// Default radius, in chunks, of the spawn area kept resident around the spawn
/// point (a `(2 * r + 1)` square).
const DEFAULT_SPAWN_CHUNK_RADIUS: u8 = 2;

/// Default simulation tick rate, in ticks per second.
const DEFAULT_TICKS_PER_SECOND: u32 = 20;

/// Whether the built-in plugin set is registered by default.
const DEFAULT_BUILTIN_PLUGINS: bool = true;

/// Default spawn-protection radius, in blocks. Zero disables spawn protection
/// entirely, which is the default so an unconfigured server protects nothing.
const DEFAULT_SPAWN_PROTECT_RADIUS: i32 = 0;

/// Default ceiling on the number of blocks a single `/fill` or `/replace` may
/// affect. `32_768` is a 32x32x32 cube — generous for shaping terrain on camera
/// while bounding the per-tick work one command can demand. Mirrors the built-in
/// default of [`ferrumc_sim::RegionLimits`].
const DEFAULT_MAX_REGION_FILL_VOLUME: u64 = 32_768;

/// Default number of undoable region edits retained per player before the oldest
/// is evicted. Bounds per-player `/undo` memory together with the volume cap.
const DEFAULT_REGION_UNDO_HISTORY: usize = 16;

/// Default play-phase keep-alive interval, in milliseconds.
///
/// A vanilla client disconnects if it hears no Keep Alive for 20 s, so the server
/// pings every 10 s. Configurable (in ms) so tests can drive a short interval
/// without a wall-clock wait.
const DEFAULT_KEEP_ALIVE_INTERVAL_MS: u64 = 10_000;

/// Default chunk-stream pump interval, in milliseconds.
///
/// How often a standing player's view is advanced toward the full advertised view
/// distance without waiting for a movement packet. One server tick (50 ms at the
/// 20 TPS default) keeps the backlog draining promptly while the per-pump load cap
/// bounds the burst.
const DEFAULT_CHUNK_STREAM_INTERVAL_MS: u64 = 50;

/// Default permission level granted to a non-operator player.
///
/// Zero is the vanilla "ordinary player" tier: it satisfies no operator gate, so
/// commands like `/gamemode` (which require an operator level) are refused for
/// everyone except the players listed in [`AppConfig::ops`]. This is what makes
/// the operator gate meaningful instead of granting every connection level 4.
const DEFAULT_PERMISSION_LEVEL: u8 = 0;

/// Largest supported concurrent-connection ceiling. This matches the bounded
/// observability roster and prevents a config typo from allocating an enormous
/// semaphore.
const MAX_CONNECTIONS: usize = 1_024;

/// Longest per-operation socket timeout accepted at startup.
#[allow(clippy::duration_suboptimal_units)] // `from_mins` is newer than the Rust 1.80 MSRV.
const MAX_IO_TIMEOUT: Duration = Duration::from_secs(300);

/// Largest compression threshold accepted by the largest configured frame cap.
const MAX_COMPRESSION_THRESHOLD: i32 = 2_097_152;

/// Largest advertised view or simulation distance supported by the streamer.
const MAX_PLAY_DISTANCE: i32 = 32;

/// Horizontal world-coordinate boundary used by movement and routing.
const MAX_HORIZONTAL_COORDINATE: f64 = 30_000_000.0;

/// Lowest valid overworld spawn Y.
const MIN_SPAWN_Y: f64 = -64.0;

/// Highest valid overworld spawn Y.
const MAX_SPAWN_Y: f64 = 319.0;

/// Largest resident spawn radius: at most a 17x17 (289 chunk) startup square.
const MAX_SPAWN_CHUNK_RADIUS: u8 = 8;

/// Fastest supported simulation rate, retaining a non-zero 1 ms tick period.
const MAX_TICKS_PER_SECOND: u32 = 1_000;

/// Largest meaningful Chebyshev spawn-protection radius.
const MAX_SPAWN_PROTECT_RADIUS: i32 = 30_000_000;

/// Largest single region edit an operator may retain for undo.
const MAX_REGION_FILL_VOLUME: u64 = 1_000_000;

/// Largest number of region undo entries retained per player.
const MAX_REGION_UNDO_HISTORY: usize = 64;

/// Combined prior-state ceiling retained by region undo history per player.
const MAX_RETAINED_REGION_CELLS: u64 = 1_048_576;

/// Accepted keep-alive cadence range.
const MIN_KEEP_ALIVE_INTERVAL: Duration = Duration::from_millis(100);
const MAX_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Accepted chunk-stream pump cadence range.
const MIN_CHUNK_STREAM_INTERVAL: Duration = Duration::from_millis(10);
#[allow(clippy::duration_suboptimal_units)] // `from_mins` is newer than the Rust 1.80 MSRV.
const MAX_CHUNK_STREAM_INTERVAL: Duration = Duration::from_secs(60);

/// Highest vanilla permission tier.
const MAX_PERMISSION_LEVEL: u8 = 4;

/// Packet-budget bounds that still admit at least one complete frame.
const MIN_PACKET_BUDGET: f64 = 1.0;
const MAX_PACKET_RATE: f64 = 10_000.0;
const MAX_PACKET_BURST: f64 = 20_000.0;

/// Server-wide sustained and immediate packet-work ceilings.
const MAX_GLOBAL_PACKET_RATE: f64 = 100_000.0;
const MAX_GLOBAL_PACKET_BURST: f64 = 200_000.0;

/// Maximum potential per-player view slots across the configured connection cap.
const MAX_TOTAL_VIEW_SLOTS: u64 = 1_100_000;

/// Maximum full-view scans the stream pump may request each second.
const MAX_VIEW_SCAN_UNITS_PER_SECOND: u64 = 3_000_000;

/// Validated, runtime-ready server configuration.
///
/// Construct one with [`AppConfig::default`] for the documented defaults, or
/// parse and validate user input with [`AppConfig::from_toml_str`]. Every field
/// is already checked, so the rest of the application can consume it directly.
///
/// Fields cannot be assembled or changed directly, so invalid values cannot
/// bypass the same validation used by TOML and command-line overrides.
///
/// ```compile_fail
/// use ferrumc_app::AppConfig;
///
/// let _ = AppConfig {
///     max_connections: 0,
///     ..AppConfig::default()
/// };
/// ```
///
/// ```compile_fail
/// use ferrumc_app::AppConfig;
///
/// let mut config = AppConfig::default();
/// config.max_connections = 0;
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    /// The socket address the TCP listener binds to.
    bind: SocketAddr,
    /// The ceiling on simultaneously accepted connections.
    max_connections: usize,
    /// The deadline applied to each socket read and write.
    io_timeout: Duration,
    /// The packet-compression threshold in bytes, or `None` to leave compression
    /// disabled (the slice default — frames travel uncompressed).
    compression_threshold: Option<i32>,
    /// The play view distance advertised to clients, in chunks.
    view_distance: i32,
    /// The play simulation distance advertised to clients, in chunks.
    simulation_distance: i32,
    /// The world-spawn position players join at.
    spawn: Vec3,
    /// The radius, in chunks, of the spawn area kept resident.
    spawn_chunk_radius: u8,
    /// The simulation tick rate, in ticks per second.
    ticks_per_second: u32,
    /// Directory containing strict trusted-native plugin bundles, or `None` to
    /// skip native plugin loading.
    plugins_dir: Option<PathBuf>,
    /// Whether the application registers its built-in plugin set.
    builtin_plugins: bool,
    /// Spawn-protection radius, in blocks (Chebyshev) around the spawn column.
    /// Zero disables spawn protection.
    spawn_protect_radius: i32,
    /// Names of players granted the spawn-protection bypass permission.
    spawn_protect_bypass: Vec<String>,
    /// Maximum number of blocks a single `/fill` or `/replace` command may affect.
    /// A larger region is rejected with a command error, bounding the per-tick work
    /// (and undo memory) one operator command can demand.
    max_region_fill_volume: u64,
    /// Number of undoable region edits retained per player before the oldest is
    /// evicted (the `/undo` history depth).
    region_undo_history: usize,
    /// Interval between clientbound play-phase Keep Alive pings.
    keep_alive_interval: Duration,
    /// How often a player's view is pumped toward the full advertised view
    /// distance. Movement only replaces the pending target; this fixed cadence
    /// consumes the latest target and advances a non-moving joiner's initial
    /// backlog. Each pump is bounded by the per-update load and unload caps, so a
    /// short interval drains work promptly without flooding the socket.
    chunk_stream_interval: Duration,
    /// Names of players granted operator status (permission level 4), letting
    /// them run operator-gated commands such as `/gamemode`. Everyone else acts
    /// at [`default_permission_level`](Self::default_permission_level).
    ops: Vec<String>,
    /// Permission level granted to a player who is not an operator. Defaults to
    /// `0` (ordinary player), so the operator gate is meaningful.
    default_permission_level: u8,
    /// Where the persistent world database lives.
    ///
    /// `Some(path)` selects the durable redb-backed [`WorldStore`] at that path
    /// (the runtime default — `main` fills in a default directory when the config
    /// omits one); `None` selects the in-memory store, which keeps tests
    /// deterministic and file-free. The redb file is created under this directory.
    ///
    /// [`WorldStore`]: ferrumc_storage::WorldStore
    world_dir: Option<PathBuf>,
    /// Whether the read-only observability dashboard starts alongside the server.
    dashboard_enabled: bool,
    /// The socket address the read-only dashboard binds to. Defaults to a loopback
    /// address so the dashboard is not reachable off-host unless reconfigured.
    dashboard_bind: SocketAddr,
    /// Access control for a public-facing server: the per-IP connection limit, the
    /// ban list, and the optional whitelist. Resolved (files read, entries
    /// classified) at startup; see [`ferrumc_config::AccessConfig::resolve`].
    access: AccessConfig,
    /// Per-connection serverbound packet budget (token-bucket sustained rate and
    /// burst) that throttles a flooding client: a sustained over-budget peer is
    /// dropped with `BudgetExceeded`. Validated at startup.
    budget: PacketBudgetConfig,
    /// World-content configuration: the source of the world's initial terrain.
    ///
    /// Defaults to the built-in flat world. When
    /// [`anvil_import_dir`](WorldConfig::anvil_import_dir) is set, a vanilla Anvil
    /// `region/` directory is imported into the world store at startup. This is
    /// separate from [`world_dir`](Self::world_dir), which is the *persistence*
    /// location; `[world]` selects the *initial content*.
    world: WorldConfig,
}

impl AppConfig {
    /// Parses and validates an [`AppConfig`] from a TOML document.
    ///
    /// Any field the document omits keeps its documented default. Socket
    /// addresses, numeric ranges, checked durations, and combined resource
    /// ceilings are all validated before the configuration is returned.
    ///
    /// # Errors
    ///
    /// Returns an error if the TOML cannot be parsed, contains an unknown field,
    /// carries an invalid socket address, or violates a numeric or cross-field
    /// resource ceiling.
    pub fn from_toml_str(toml: &str) -> anyhow::Result<Self> {
        let raw: RawConfig = toml::from_str(toml)?;
        raw.into_config()
    }

    /// Returns the socket address the TCP listener binds to.
    #[must_use]
    pub const fn bind(&self) -> SocketAddr {
        self.bind
    }

    /// Returns the ceiling on simultaneously accepted connections.
    #[must_use]
    pub const fn max_connections(&self) -> usize {
        self.max_connections
    }

    /// Returns the deadline applied to each socket read and write.
    #[must_use]
    pub const fn io_timeout(&self) -> Duration {
        self.io_timeout
    }

    /// Returns the compression threshold, or `None` when compression is disabled.
    #[must_use]
    pub const fn compression_threshold(&self) -> Option<i32> {
        self.compression_threshold
    }

    /// Returns the play view distance advertised to clients.
    #[must_use]
    pub const fn view_distance(&self) -> i32 {
        self.view_distance
    }

    /// Returns the play simulation distance advertised to clients.
    #[must_use]
    pub const fn simulation_distance(&self) -> i32 {
        self.simulation_distance
    }

    /// Returns the validated world-spawn position.
    #[must_use]
    pub const fn spawn(&self) -> Vec3 {
        self.spawn
    }

    /// Returns the resident spawn-area radius in chunks.
    #[must_use]
    pub const fn spawn_chunk_radius(&self) -> u8 {
        self.spawn_chunk_radius
    }

    /// Returns the simulation rate in ticks per second.
    #[must_use]
    pub const fn ticks_per_second(&self) -> u32 {
        self.ticks_per_second
    }

    /// Returns the configured dynamic-plugin directory, if any.
    #[must_use]
    pub fn plugins_dir(&self) -> Option<&Path> {
        self.plugins_dir.as_deref()
    }

    /// Returns whether the built-in plugin set is enabled.
    #[must_use]
    pub const fn builtin_plugins(&self) -> bool {
        self.builtin_plugins
    }

    /// Returns the spawn-protection radius in blocks.
    #[must_use]
    pub const fn spawn_protect_radius(&self) -> i32 {
        self.spawn_protect_radius
    }

    /// Returns the names granted the spawn-protection bypass.
    #[must_use]
    pub fn spawn_protect_bypass(&self) -> &[String] {
        &self.spawn_protect_bypass
    }

    /// Returns the maximum block volume of one region-edit command.
    #[must_use]
    pub const fn max_region_fill_volume(&self) -> u64 {
        self.max_region_fill_volume
    }

    /// Returns the number of region undo entries retained per player.
    #[must_use]
    pub const fn region_undo_history(&self) -> usize {
        self.region_undo_history
    }

    /// Returns the clientbound keep-alive interval.
    #[must_use]
    pub const fn keep_alive_interval(&self) -> Duration {
        self.keep_alive_interval
    }

    /// Returns the chunk-stream pump interval.
    #[must_use]
    pub const fn chunk_stream_interval(&self) -> Duration {
        self.chunk_stream_interval
    }

    /// Returns the configured operator names.
    #[must_use]
    pub fn ops(&self) -> &[String] {
        &self.ops
    }

    /// Returns the default permission level for a non-operator player.
    #[must_use]
    pub const fn default_permission_level(&self) -> u8 {
        self.default_permission_level
    }

    /// Returns the persistent world directory, if one is configured.
    #[must_use]
    pub fn world_dir(&self) -> Option<&Path> {
        self.world_dir.as_deref()
    }

    /// Returns whether the read-only dashboard is enabled.
    #[must_use]
    pub const fn dashboard_enabled(&self) -> bool {
        self.dashboard_enabled
    }

    /// Returns the socket address used by the read-only dashboard.
    #[must_use]
    pub const fn dashboard_bind(&self) -> SocketAddr {
        self.dashboard_bind
    }

    /// Returns the validated access-control configuration.
    #[must_use]
    pub const fn access(&self) -> &AccessConfig {
        &self.access
    }

    /// Returns the validated per-connection packet budget.
    #[must_use]
    pub const fn budget(&self) -> PacketBudgetConfig {
        self.budget
    }

    /// Returns the initial-world-content configuration.
    #[must_use]
    pub const fn world(&self) -> &WorldConfig {
        &self.world
    }

    /// The nominal duration of one simulation tick.
    #[must_use]
    pub fn tick_period(&self) -> Duration {
        match self.checked_tick_period() {
            Ok(period) => period,
            // Public construction is sealed and `run` validates again. Retaining
            // a non-zero fallback makes this accessor panic-free even if future
            // crate-internal code temporarily assembles an invalid candidate.
            Err(_) => Duration::from_nanos(1),
        }
    }

    /// Returns a revalidated copy that uses `world_dir` for persistence.
    ///
    /// # Errors
    ///
    /// Returns an error if this configuration no longer satisfies a numeric or
    /// cross-field resource ceiling.
    pub fn with_world_dir(mut self, world_dir: Option<PathBuf>) -> anyhow::Result<Self> {
        self.world_dir = world_dir;
        self.validate()?;
        Ok(self)
    }

    /// Returns a revalidated copy that scans `plugins_dir` at startup.
    ///
    /// # Errors
    ///
    /// Returns an error if this configuration no longer satisfies a numeric or
    /// cross-field resource ceiling.
    pub fn with_plugins_dir(mut self, plugins_dir: Option<PathBuf>) -> anyhow::Result<Self> {
        self.plugins_dir = plugins_dir;
        self.validate()?;
        Ok(self)
    }

    /// Returns a revalidated copy carrying the supplied access-control policy.
    ///
    /// # Errors
    ///
    /// Returns an error if the per-IP connection limit exceeds the global
    /// connection ceiling, or if another config invariant is invalid.
    pub fn with_access(mut self, access: AccessConfig) -> anyhow::Result<Self> {
        self.access = access;
        self.validate()?;
        Ok(self)
    }

    /// Applies command-line and shipping-runtime defaults, then revalidates.
    pub(crate) fn with_runtime_overrides(
        mut self,
        bind: Option<SocketAddr>,
        port: Option<u16>,
        default_world_dir: PathBuf,
    ) -> anyhow::Result<Self> {
        if let Some(bind) = bind {
            self.bind = bind;
        }
        if let Some(port) = port {
            self.bind.set_port(port);
        }
        if self.world_dir.is_none() {
            self.world_dir = Some(default_world_dir);
        }
        self.validate()?;
        Ok(self)
    }

    /// Validates every runtime-facing numeric and cross-field invariant.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=MAX_CONNECTIONS).contains(&self.max_connections),
            "max_connections must be in 1..={MAX_CONNECTIONS}, got {}",
            self.max_connections,
        );
        anyhow::ensure!(
            (Duration::from_secs(1)..=MAX_IO_TIMEOUT).contains(&self.io_timeout),
            "io_timeout_secs must be in 1..={} seconds, got {:?}",
            MAX_IO_TIMEOUT.as_secs(),
            self.io_timeout,
        );
        if let Some(threshold) = self.compression_threshold {
            anyhow::ensure!(
                (0..=MAX_COMPRESSION_THRESHOLD).contains(&threshold),
                "compression_threshold must disable compression or be in \
                 0..={MAX_COMPRESSION_THRESHOLD} bytes, got {threshold}",
            );
        }
        anyhow::ensure!(
            (0..=MAX_PLAY_DISTANCE).contains(&self.view_distance),
            "view_distance must be in 0..={MAX_PLAY_DISTANCE} chunks, got {}",
            self.view_distance,
        );
        anyhow::ensure!(
            (0..=MAX_PLAY_DISTANCE).contains(&self.simulation_distance),
            "simulation_distance must be in 0..={MAX_PLAY_DISTANCE} chunks, got {}",
            self.simulation_distance,
        );
        validate_spawn_axis(
            "x",
            self.spawn.x,
            -MAX_HORIZONTAL_COORDINATE,
            MAX_HORIZONTAL_COORDINATE,
        )?;
        validate_spawn_axis("y", self.spawn.y, MIN_SPAWN_Y, MAX_SPAWN_Y)?;
        validate_spawn_axis(
            "z",
            self.spawn.z,
            -MAX_HORIZONTAL_COORDINATE,
            MAX_HORIZONTAL_COORDINATE,
        )?;

        anyhow::ensure!(
            self.spawn_chunk_radius <= MAX_SPAWN_CHUNK_RADIUS,
            "spawn_chunk_radius must be in 0..={MAX_SPAWN_CHUNK_RADIUS} chunks, got {}",
            self.spawn_chunk_radius,
        );
        let spawn_side = u64::from(self.spawn_chunk_radius)
            .checked_mul(2)
            .and_then(|diameter| diameter.checked_add(1))
            .ok_or_else(|| anyhow::anyhow!("spawn_chunk_radius square overflow"))?;
        let spawn_chunks = spawn_side
            .checked_mul(spawn_side)
            .ok_or_else(|| anyhow::anyhow!("spawn_chunk_radius square overflow"))?;
        anyhow::ensure!(
            spawn_chunks <= 289,
            "spawn_chunk_radius requests {spawn_chunks} chunks; startup ceiling is 289",
        );

        anyhow::ensure!(
            (1..=MAX_TICKS_PER_SECOND).contains(&self.ticks_per_second),
            "ticks_per_second must be in 1..={MAX_TICKS_PER_SECOND}, got {}",
            self.ticks_per_second,
        );
        let _ = self.checked_tick_period()?;
        anyhow::ensure!(
            (0..=MAX_SPAWN_PROTECT_RADIUS).contains(&self.spawn_protect_radius),
            "spawn_protect_radius must be in 0..={MAX_SPAWN_PROTECT_RADIUS} blocks, got {}",
            self.spawn_protect_radius,
        );
        anyhow::ensure!(
            self.max_region_fill_volume <= MAX_REGION_FILL_VOLUME,
            "max_region_fill_volume must be in 0..={MAX_REGION_FILL_VOLUME} blocks, got {}",
            self.max_region_fill_volume,
        );
        anyhow::ensure!(
            self.region_undo_history <= MAX_REGION_UNDO_HISTORY,
            "region_undo_history must be in 0..={MAX_REGION_UNDO_HISTORY}, got {}",
            self.region_undo_history,
        );
        let undo_history = u64::try_from(self.region_undo_history)
            .map_err(|_| anyhow::anyhow!("region_undo_history does not fit in u64"))?;
        let retained_cells = self
            .max_region_fill_volume
            .checked_mul(undo_history)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "max_region_fill_volume * region_undo_history overflows the retained-cell count"
                )
            })?;
        anyhow::ensure!(
            retained_cells <= MAX_RETAINED_REGION_CELLS,
            "max_region_fill_volume * region_undo_history retains up to \
             {retained_cells} cells; ceiling is {MAX_RETAINED_REGION_CELLS}",
        );
        anyhow::ensure!(
            (MIN_KEEP_ALIVE_INTERVAL..=MAX_KEEP_ALIVE_INTERVAL).contains(&self.keep_alive_interval),
            "keep_alive_interval_ms must be in {}..={} milliseconds, got {:?}",
            MIN_KEEP_ALIVE_INTERVAL.as_millis(),
            MAX_KEEP_ALIVE_INTERVAL.as_millis(),
            self.keep_alive_interval,
        );
        anyhow::ensure!(
            (MIN_CHUNK_STREAM_INTERVAL..=MAX_CHUNK_STREAM_INTERVAL)
                .contains(&self.chunk_stream_interval),
            "chunk_stream_interval_ms must be in {}..={} milliseconds, got {:?}",
            MIN_CHUNK_STREAM_INTERVAL.as_millis(),
            MAX_CHUNK_STREAM_INTERVAL.as_millis(),
            self.chunk_stream_interval,
        );
        anyhow::ensure!(
            self.default_permission_level <= MAX_PERMISSION_LEVEL,
            "default_permission_level must be in 0..={MAX_PERMISSION_LEVEL}, got {}",
            self.default_permission_level,
        );
        anyhow::ensure!(
            self.access.per_ip_connection_limit == 0
                || self.access.per_ip_connection_limit <= self.max_connections,
            "access.per_ip_connection_limit must be 0 or no greater than max_connections \
             ({}), got {}",
            self.max_connections,
            self.access.per_ip_connection_limit,
        );

        self.budget.validate()?;
        anyhow::ensure!(
            (MIN_PACKET_BUDGET..=MAX_PACKET_RATE).contains(&self.budget.sustained_rate),
            "budget.sustained_rate must be in {MIN_PACKET_BUDGET}..={MAX_PACKET_RATE} \
             frames/second, got {}",
            self.budget.sustained_rate,
        );
        anyhow::ensure!(
            (MIN_PACKET_BUDGET..=MAX_PACKET_BURST).contains(&self.budget.burst),
            "budget.burst must be in {MIN_PACKET_BUDGET}..={MAX_PACKET_BURST} frames, got {}",
            self.budget.burst,
        );
        let connection_count = u32::try_from(self.max_connections)
            .map_err(|_| anyhow::anyhow!("max_connections does not fit in u32"))?;
        let global_packet_rate = f64::from(connection_count) * self.budget.sustained_rate;
        anyhow::ensure!(
            global_packet_rate <= MAX_GLOBAL_PACKET_RATE,
            "max_connections * budget.sustained_rate permits {global_packet_rate} \
             serverbound frames/second; ceiling is {MAX_GLOBAL_PACKET_RATE}",
        );
        let global_packet_burst = f64::from(connection_count) * self.budget.burst;
        anyhow::ensure!(
            global_packet_burst <= MAX_GLOBAL_PACKET_BURST,
            "max_connections * budget.burst permits {global_packet_burst} immediate \
             serverbound frames; ceiling is {MAX_GLOBAL_PACKET_BURST}",
        );

        let view_distance = u64::try_from(self.view_distance)
            .map_err(|_| anyhow::anyhow!("view_distance cannot be represented as u64"))?;
        let view_side = view_distance
            .checked_mul(2)
            .and_then(|diameter| diameter.checked_add(1))
            .ok_or_else(|| anyhow::anyhow!("view_distance square overflow"))?;
        let view_area = view_side
            .checked_mul(view_side)
            .ok_or_else(|| anyhow::anyhow!("view_distance square overflow"))?;
        let connections = u64::try_from(self.max_connections)
            .map_err(|_| anyhow::anyhow!("max_connections cannot be represented as u64"))?;
        let view_slots = connections
            .checked_mul(view_area)
            .ok_or_else(|| anyhow::anyhow!("max_connections * view_distance square overflow"))?;
        anyhow::ensure!(
            view_slots <= MAX_TOTAL_VIEW_SLOTS,
            "max_connections ({}) * view_distance square ({view_area}) requests \
             {view_slots} view slots; ceiling is {MAX_TOTAL_VIEW_SLOTS}",
            self.max_connections,
        );

        let interval_ms = u64::try_from(self.chunk_stream_interval.as_millis())
            .map_err(|_| anyhow::anyhow!("chunk_stream_interval_ms does not fit in u64"))?;
        let scan_numerator = view_slots
            .checked_mul(1_000)
            .ok_or_else(|| anyhow::anyhow!("view scan rate overflow"))?;
        let scans_per_second = scan_numerator
            .checked_add(interval_ms - 1)
            .ok_or_else(|| anyhow::anyhow!("view scan rate overflow"))?
            / interval_ms;
        anyhow::ensure!(
            scans_per_second <= MAX_VIEW_SCAN_UNITS_PER_SECOND,
            "max_connections, view_distance, and chunk_stream_interval_ms request \
             {scans_per_second} view-scan units/second; ceiling is \
             {MAX_VIEW_SCAN_UNITS_PER_SECOND}",
        );
        Ok(())
    }

    /// Computes the tick period without division truncating to zero.
    fn checked_tick_period(&self) -> anyhow::Result<Duration> {
        let period = Duration::from_secs(1)
            .checked_div(self.ticks_per_second)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "ticks_per_second must produce a representable duration, got {}",
                    self.ticks_per_second,
                )
            })?;
        anyhow::ensure!(
            !period.is_zero(),
            "ticks_per_second {} produces a zero-duration tick",
            self.ticks_per_second,
        );
        Ok(period)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        let config = Self {
            bind: DEFAULT_BIND,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            io_timeout: Duration::from_secs(DEFAULT_IO_TIMEOUT_SECS),
            compression_threshold: None,
            view_distance: DEFAULT_VIEW_DISTANCE,
            simulation_distance: DEFAULT_SIMULATION_DISTANCE,
            spawn: DEFAULT_SPAWN,
            spawn_chunk_radius: DEFAULT_SPAWN_CHUNK_RADIUS,
            ticks_per_second: DEFAULT_TICKS_PER_SECOND,
            plugins_dir: None,
            builtin_plugins: DEFAULT_BUILTIN_PLUGINS,
            spawn_protect_radius: DEFAULT_SPAWN_PROTECT_RADIUS,
            spawn_protect_bypass: Vec::new(),
            max_region_fill_volume: DEFAULT_MAX_REGION_FILL_VOLUME,
            region_undo_history: DEFAULT_REGION_UNDO_HISTORY,
            keep_alive_interval: Duration::from_millis(DEFAULT_KEEP_ALIVE_INTERVAL_MS),
            chunk_stream_interval: Duration::from_millis(DEFAULT_CHUNK_STREAM_INTERVAL_MS),
            ops: Vec::new(),
            default_permission_level: DEFAULT_PERMISSION_LEVEL,
            // None = in-memory, keeping `AppConfig::default()` deterministic and
            // file-free for tests. `main` substitutes a durable redb directory so
            // the shipping server persists by default.
            world_dir: None,
            dashboard_enabled: DEFAULT_DASHBOARD_ENABLED,
            dashboard_bind: DEFAULT_DASHBOARD_BIND,
            access: AccessConfig::default(),
            budget: PacketBudgetConfig::default(),
            // No Anvil import by default: the server generates its flat world.
            world: WorldConfig::default(),
        };
        debug_assert!(
            config.validate().is_ok(),
            "compile-time AppConfig defaults must satisfy validation",
        );
        config
    }
}

/// Validates one spawn component against its finite inclusive range.
fn validate_spawn_axis(axis: &str, value: f64, min: f64, max: f64) -> anyhow::Result<()> {
    anyhow::ensure!(
        value.is_finite() && (min..=max).contains(&value),
        "spawn.{axis} must be finite and in {min}..={max}, got {value}",
    );
    Ok(())
}

/// The optional-field TOML shape that merges over [`AppConfig::default`].
///
/// Every field is optional so a config may set only what it wants to override;
/// [`RawConfig::into_config`] fills the rest from the defaults and validates.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    /// Override for [`AppConfig::bind`], as a parseable socket address.
    bind: Option<String>,
    /// Override for [`AppConfig::max_connections`].
    max_connections: Option<usize>,
    /// Override for [`AppConfig::io_timeout`], expressed in whole seconds.
    io_timeout_secs: Option<u64>,
    /// Override for [`AppConfig::compression_threshold`].
    compression_threshold: Option<i32>,
    /// Override for [`AppConfig::view_distance`].
    view_distance: Option<i32>,
    /// Override for [`AppConfig::simulation_distance`].
    simulation_distance: Option<i32>,
    /// Override for [`AppConfig::spawn`], as `[x, y, z]`.
    spawn: Option<[f64; 3]>,
    /// Override for [`AppConfig::spawn_chunk_radius`].
    spawn_chunk_radius: Option<u8>,
    /// Override for [`AppConfig::ticks_per_second`].
    ticks_per_second: Option<u32>,
    /// Override for [`AppConfig::plugins_dir`], as a filesystem path.
    plugins_dir: Option<String>,
    /// Override for [`AppConfig::builtin_plugins`].
    builtin_plugins: Option<bool>,
    /// Override for [`AppConfig::spawn_protect_radius`].
    spawn_protect_radius: Option<i32>,
    /// Override for [`AppConfig::spawn_protect_bypass`].
    spawn_protect_bypass: Option<Vec<String>>,
    /// Override for [`AppConfig::max_region_fill_volume`].
    max_region_fill_volume: Option<u64>,
    /// Override for [`AppConfig::region_undo_history`].
    region_undo_history: Option<usize>,
    /// Override for [`AppConfig::keep_alive_interval`], expressed in milliseconds.
    keep_alive_interval_ms: Option<u64>,
    /// Override for [`AppConfig::chunk_stream_interval`], expressed in milliseconds.
    chunk_stream_interval_ms: Option<u64>,
    /// Override for [`AppConfig::ops`].
    ops: Option<Vec<String>>,
    /// Override for [`AppConfig::default_permission_level`].
    default_permission_level: Option<u8>,
    /// Override for [`AppConfig::world_dir`], as a filesystem path. When set, the
    /// durable redb store is used at this directory.
    world_dir: Option<String>,
    /// Override for [`AppConfig::dashboard_enabled`].
    dashboard_enabled: Option<bool>,
    /// Override for [`AppConfig::dashboard_bind`], as a parseable socket address.
    dashboard_bind: Option<String>,
    /// The `[access]` table. Carries its own per-field defaults, so an omitted
    /// table (or any omitted field within it) falls back to the safe defaults.
    access: AccessConfig,
    /// The `[budget]` table. Carries its own per-field defaults (300/600), so an
    /// omitted table falls back to the safe serverbound packet budget.
    budget: PacketBudgetConfig,
    /// The `[world]` table. Carries its own defaults (no Anvil import), so an
    /// omitted table leaves the server on its built-in flat world.
    world: WorldConfig,
}

impl RawConfig {
    /// Merges these overrides onto the defaults and validates the result.
    fn into_config(self) -> anyhow::Result<AppConfig> {
        let defaults = AppConfig::default();

        let bind = match self.bind {
            Some(text) => text
                .parse()
                .map_err(|err| anyhow::anyhow!("invalid bind address {text:?}: {err}"))?,
            None => defaults.bind,
        };

        let ticks_per_second = self.ticks_per_second.unwrap_or(defaults.ticks_per_second);

        let spawn = self
            .spawn
            .map_or(defaults.spawn, |[x, y, z]| Vec3::new(x, y, z));

        let dashboard_bind = match self.dashboard_bind {
            Some(text) => text
                .parse()
                .map_err(|err| anyhow::anyhow!("invalid dashboard_bind address {text:?}: {err}"))?,
            None => defaults.dashboard_bind,
        };

        // Every negative threshold already meant "compression disabled" at the
        // connection boundary. Normalize that spelling to the sealed `None`
        // representation while preserving the public key's established meaning.
        let compression_threshold = match self.compression_threshold {
            Some(threshold) if threshold < 0 => None,
            Some(threshold) => Some(threshold),
            None => defaults.compression_threshold,
        };
        // The spawn-protection plugin has always defined every non-positive
        // radius as disabled. Keep that public key meaning while sealing the
        // runtime representation to the validator's non-negative range.
        let spawn_protect_radius = self
            .spawn_protect_radius
            .unwrap_or(defaults.spawn_protect_radius)
            .max(0);

        let candidate = AppConfig {
            bind,
            max_connections: self.max_connections.unwrap_or(defaults.max_connections),
            io_timeout: self
                .io_timeout_secs
                .map_or(defaults.io_timeout, Duration::from_secs),
            compression_threshold,
            view_distance: self.view_distance.unwrap_or(defaults.view_distance),
            simulation_distance: self
                .simulation_distance
                .unwrap_or(defaults.simulation_distance),
            spawn,
            spawn_chunk_radius: self
                .spawn_chunk_radius
                .unwrap_or(defaults.spawn_chunk_radius),
            ticks_per_second,
            plugins_dir: self.plugins_dir.map(PathBuf::from).or(defaults.plugins_dir),
            builtin_plugins: self.builtin_plugins.unwrap_or(defaults.builtin_plugins),
            spawn_protect_radius,
            spawn_protect_bypass: self
                .spawn_protect_bypass
                .unwrap_or(defaults.spawn_protect_bypass),
            max_region_fill_volume: self
                .max_region_fill_volume
                .unwrap_or(defaults.max_region_fill_volume),
            region_undo_history: self
                .region_undo_history
                .unwrap_or(defaults.region_undo_history),
            keep_alive_interval: self
                .keep_alive_interval_ms
                .map_or(defaults.keep_alive_interval, Duration::from_millis),
            chunk_stream_interval: self
                .chunk_stream_interval_ms
                .map_or(defaults.chunk_stream_interval, Duration::from_millis),
            ops: self.ops.unwrap_or(defaults.ops),
            default_permission_level: self
                .default_permission_level
                .unwrap_or(defaults.default_permission_level),
            world_dir: self.world_dir.map(PathBuf::from).or(defaults.world_dir),
            dashboard_enabled: self.dashboard_enabled.unwrap_or(defaults.dashboard_enabled),
            dashboard_bind,
            access: self.access,
            budget: self.budget,
            world: self.world,
        };
        candidate.validate()?;
        Ok(candidate)
    }
}

#[cfg(test)]
mod tests {
    // The budget rate/burst under test are exact, representable literals, so exact
    // float comparison is intentional here.
    #![allow(clippy::float_cmp)]

    use super::*;

    #[test]
    fn empty_toml_yields_defaults() {
        let parsed = AppConfig::from_toml_str("").expect("empty config is valid");
        assert_eq!(parsed, AppConfig::default());
    }

    #[test]
    fn overrides_are_applied() {
        let toml = r#"
            bind = "0.0.0.0:0"
            max_connections = 4
            io_timeout_secs = 5
            compression_threshold = 256
            view_distance = 6
            simulation_distance = 7
            spawn = [1.0, 65.0, 2.0]
            spawn_chunk_radius = 1
            ticks_per_second = 10
            plugins_dir = "/srv/plugins"
            builtin_plugins = false
            spawn_protect_radius = 12
            spawn_protect_bypass = ["Admin", "Mod"]
            max_region_fill_volume = 4096
            region_undo_history = 3
            keep_alive_interval_ms = 250
            chunk_stream_interval_ms = 33
            ops = ["Admin"]
            default_permission_level = 1
            world_dir = "/srv/world"
            dashboard_enabled = false
            dashboard_bind = "127.0.0.1:8181"
        "#;
        let parsed = AppConfig::from_toml_str(toml).expect("valid config");
        assert_eq!(parsed.bind, "0.0.0.0:0".parse().unwrap());
        assert_eq!(parsed.max_connections, 4);
        assert_eq!(parsed.io_timeout, Duration::from_secs(5));
        assert_eq!(parsed.compression_threshold, Some(256));
        assert_eq!(parsed.view_distance, 6);
        assert_eq!(parsed.simulation_distance, 7);
        assert_eq!(parsed.spawn, Vec3::new(1.0, 65.0, 2.0));
        assert_eq!(parsed.spawn_chunk_radius, 1);
        assert_eq!(parsed.ticks_per_second, 10);
        assert_eq!(parsed.plugins_dir, Some(PathBuf::from("/srv/plugins")));
        assert!(!parsed.builtin_plugins);
        assert_eq!(parsed.spawn_protect_radius, 12);
        assert_eq!(parsed.spawn_protect_bypass, vec!["Admin", "Mod"]);
        assert_eq!(parsed.max_region_fill_volume, 4096);
        assert_eq!(parsed.region_undo_history, 3);
        assert_eq!(parsed.keep_alive_interval, Duration::from_millis(250));
        assert_eq!(parsed.chunk_stream_interval, Duration::from_millis(33));
        assert_eq!(parsed.ops, vec!["Admin"]);
        assert_eq!(parsed.default_permission_level, 1);
        assert_eq!(parsed.world_dir, Some(PathBuf::from("/srv/world")));
        assert!(!parsed.dashboard_enabled);
        assert_eq!(parsed.dashboard_bind, "127.0.0.1:8181".parse().unwrap());
    }

    #[test]
    fn dashboard_defaults_to_enabled_on_loopback() {
        let parsed = AppConfig::from_toml_str("").expect("empty config is valid");
        assert!(parsed.dashboard_enabled);
        assert_eq!(parsed.dashboard_bind, "127.0.0.1:9090".parse().unwrap());
    }

    #[test]
    fn chunk_stream_interval_defaults_to_one_tick() {
        let parsed = AppConfig::from_toml_str("").expect("empty config is valid");
        // Defaults to 50 ms (one tick at 20 TPS), so a standing joiner's view is
        // pumped toward full view distance promptly.
        assert_eq!(parsed.chunk_stream_interval, Duration::from_millis(50));
    }

    #[test]
    fn world_dir_defaults_to_in_memory() {
        let parsed = AppConfig::from_toml_str("").expect("empty config is valid");
        assert_eq!(parsed.world_dir, None);
    }

    #[test]
    fn spawn_protection_defaults_to_disabled() {
        let parsed = AppConfig::from_toml_str("").expect("empty config is valid");
        assert_eq!(parsed.spawn_protect_radius, 0);
        assert!(parsed.spawn_protect_bypass.is_empty());
        assert_eq!(parsed.plugins_dir, None);
    }

    #[test]
    fn builtin_plugins_default_on_and_can_be_disabled() {
        assert!(AppConfig::default().builtin_plugins());
        let parsed =
            AppConfig::from_toml_str("builtin_plugins = false").expect("valid plugin toggle");
        assert!(!parsed.builtin_plugins());
    }

    #[test]
    fn region_edit_limits_default_to_a_32x32x32_cube_and_16_undos() {
        let parsed = AppConfig::from_toml_str("").expect("empty config is valid");
        assert_eq!(parsed.max_region_fill_volume, 32_768);
        assert_eq!(parsed.region_undo_history, 16);
    }

    #[test]
    fn operators_default_to_empty_with_player_level_zero() {
        let parsed = AppConfig::from_toml_str("").expect("empty config is valid");
        assert!(parsed.ops.is_empty());
        assert_eq!(parsed.default_permission_level, 0);
    }

    #[test]
    fn access_defaults_to_open_with_per_ip_three() {
        let parsed = AppConfig::from_toml_str("").expect("empty config is valid");
        assert_eq!(parsed.access, AccessConfig::default());
        assert_eq!(parsed.access.per_ip_connection_limit, 3);
        assert!(!parsed.access.whitelist_enabled);
        assert!(parsed.access.whitelist.is_empty());
        assert!(parsed.access.bans.is_empty());
    }

    #[test]
    fn access_table_overrides_parse() {
        let toml = "\
            bind = \"127.0.0.1:0\"\n\
            [access]\n\
            per_ip_connection_limit = 5\n\
            whitelist_enabled = true\n\
            whitelist = [\"Saad\"]\n\
            bans = [\"Griefer\", \"10.0.0.5\"]\n\
        ";
        let parsed = AppConfig::from_toml_str(toml).expect("valid access config");
        assert_eq!(parsed.access.per_ip_connection_limit, 5);
        assert!(parsed.access.whitelist_enabled);
        assert_eq!(parsed.access.whitelist, vec!["Saad"]);
        assert_eq!(parsed.access.bans, vec!["Griefer", "10.0.0.5"]);
    }

    #[test]
    fn access_unknown_field_is_rejected() {
        assert!(AppConfig::from_toml_str("[access]\nbogus = 1").is_err());
    }

    #[test]
    fn packet_budget_defaults_to_three_hundred_over_six_hundred() {
        let parsed = AppConfig::from_toml_str("").expect("empty config is valid");
        assert_eq!(parsed.budget, PacketBudgetConfig::default());
        assert_eq!(parsed.budget.sustained_rate, 300.0);
        assert_eq!(parsed.budget.burst, 600.0);
    }

    #[test]
    fn budget_table_overrides_parse() {
        let toml = "\
            bind = \"127.0.0.1:0\"\n\
            [budget]\n\
            sustained_rate = 150.0\n\
            burst = 450.0\n\
        ";
        let parsed = AppConfig::from_toml_str(toml).expect("valid budget config");
        assert_eq!(parsed.budget.sustained_rate, 150.0);
        assert_eq!(parsed.budget.burst, 450.0);
    }

    #[test]
    fn a_degenerate_budget_is_rejected_at_startup() {
        let err = AppConfig::from_toml_str("[budget]\nsustained_rate = 0.0")
            .expect_err("a zero sustained rate must be rejected");
        assert!(err.to_string().contains("sustained_rate"));
    }

    #[test]
    fn budget_unknown_field_is_rejected() {
        assert!(AppConfig::from_toml_str("[budget]\nbogus = 1").is_err());
    }

    #[test]
    fn world_defaults_to_no_anvil_import() {
        let parsed = AppConfig::from_toml_str("").expect("empty config is valid");
        assert_eq!(parsed.world, WorldConfig::default());
        assert_eq!(parsed.world.anvil_import_dir(), None);
    }

    #[test]
    fn world_table_overrides_parse() {
        let toml = "\
            bind = \"127.0.0.1:0\"\n\
            [world]\n\
            anvil_import_dir = \"/srv/maps/spawn/region\"\n\
        ";
        let parsed = AppConfig::from_toml_str(toml).expect("valid world config");
        assert_eq!(
            parsed.world.anvil_import_dir(),
            Some(std::path::Path::new("/srv/maps/spawn/region"))
        );
    }

    #[test]
    fn world_unknown_field_is_rejected() {
        assert!(AppConfig::from_toml_str("[world]\nbogus = 1").is_err());
    }

    #[test]
    fn zero_tick_rate_is_rejected() {
        let err = AppConfig::from_toml_str("ticks_per_second = 0").expect_err("zero is invalid");
        assert!(err.to_string().contains("ticks_per_second"));
    }

    #[test]
    fn invalid_bind_is_rejected() {
        let err = AppConfig::from_toml_str("bind = \"not-an-address\"").expect_err("bad address");
        assert!(err.to_string().contains("invalid bind address"));
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(AppConfig::from_toml_str("nonsense = 1").is_err());
    }

    #[test]
    fn tick_period_tracks_rate() {
        let config = AppConfig {
            ticks_per_second: 20,
            ..AppConfig::default()
        };
        assert_eq!(config.tick_period(), Duration::from_millis(50));
    }

    #[test]
    fn every_numeric_config_field_rejects_unsafe_values_with_its_name() {
        let cases = [
            ("max_connections = 0", "max_connections"),
            ("max_connections = 1025", "max_connections"),
            ("io_timeout_secs = 0", "io_timeout"),
            ("io_timeout_secs = 301", "io_timeout"),
            ("compression_threshold = 2097153", "compression_threshold"),
            ("view_distance = -1", "view_distance"),
            ("view_distance = 33", "view_distance"),
            ("simulation_distance = -1", "simulation_distance"),
            ("simulation_distance = 33", "simulation_distance"),
            ("spawn = [nan, 64.0, 8.0]", "spawn"),
            ("spawn = [8.0, inf, 8.0]", "spawn"),
            ("spawn = [8.0, 64.0, -inf]", "spawn"),
            ("spawn = [30000001.0, 64.0, 8.0]", "spawn"),
            ("spawn = [-30000001.0, 64.0, 8.0]", "spawn"),
            ("spawn = [8.0, -65.0, 8.0]", "spawn"),
            ("spawn = [8.0, 320.0, 8.0]", "spawn"),
            ("spawn = [8.0, 64.0, 30000001.0]", "spawn"),
            ("spawn = [8.0, 64.0, -30000001.0]", "spawn"),
            ("spawn_chunk_radius = 9", "spawn_chunk_radius"),
            ("ticks_per_second = 0", "ticks_per_second"),
            ("ticks_per_second = 1001", "ticks_per_second"),
            ("spawn_protect_radius = 30000001", "spawn_protect_radius"),
            ("max_region_fill_volume = 1000001", "max_region_fill_volume"),
            ("region_undo_history = 65", "region_undo_history"),
            (
                "max_region_fill_volume = 32769\nregion_undo_history = 32",
                "max_region_fill_volume",
            ),
            ("keep_alive_interval_ms = 99", "keep_alive_interval"),
            ("keep_alive_interval_ms = 15001", "keep_alive_interval"),
            ("chunk_stream_interval_ms = 9", "chunk_stream_interval"),
            ("chunk_stream_interval_ms = 60001", "chunk_stream_interval"),
            ("default_permission_level = 5", "default_permission_level"),
            (
                "max_connections = 1\n[access]\nper_ip_connection_limit = 2",
                "per_ip_connection_limit",
            ),
            (
                "max_connections = 1024\nview_distance = 32\n\
                 [budget]\nsustained_rate = 1.0\nburst = 1.0",
                "max_connections",
            ),
            (
                "view_distance = 32\nchunk_stream_interval_ms = 10",
                "chunk_stream_interval_ms",
            ),
            (
                "[budget]\nsustained_rate = 0.5\nburst = 1.0",
                "sustained_rate",
            ),
            (
                "[budget]\nsustained_rate = nan\nburst = 1.0",
                "sustained_rate",
            ),
            ("[budget]\nsustained_rate = 1.0\nburst = 0.5", "burst"),
            ("[budget]\nsustained_rate = 1.0\nburst = inf", "burst"),
            ("[budget]\nsustained_rate = 2.0\nburst = 1.0", "burst"),
            (
                "[budget]\nsustained_rate = 10001.0\nburst = 10001.0",
                "sustained_rate",
            ),
            (
                "[budget]\nsustained_rate = 10000.0\nburst = 20001.0",
                "burst",
            ),
        ];

        for (toml, field) in cases {
            let Err(error) = AppConfig::from_toml_str(toml) else {
                panic!("{field} case unexpectedly parsed: {toml:?}");
            };
            assert!(
                error.to_string().contains(field),
                "{field} rejection was not actionable: {error:#}",
            );
        }
    }

    #[test]
    fn declared_minimum_and_maximum_resource_boundaries_are_accepted() {
        let cases = [
            "max_connections = 1\n[access]\nper_ip_connection_limit = 0",
            "max_connections = 1024\nview_distance = 0\n\
             [budget]\nsustained_rate = 1.0\nburst = 1.0",
            "io_timeout_secs = 1",
            "io_timeout_secs = 300",
            "compression_threshold = 0",
            "compression_threshold = 2097152",
            "view_distance = 0",
            "max_connections = 1\nview_distance = 32\n\
             [access]\nper_ip_connection_limit = 0",
            "simulation_distance = 0",
            "simulation_distance = 32",
            "spawn = [-30000000.0, -64.0, -30000000.0]",
            "spawn = [30000000.0, 319.0, 30000000.0]",
            "spawn_chunk_radius = 0",
            "spawn_chunk_radius = 8",
            "ticks_per_second = 1",
            "ticks_per_second = 1000",
            "spawn_protect_radius = 0",
            "spawn_protect_radius = 30000000",
            "max_region_fill_volume = 0\nregion_undo_history = 64",
            "max_region_fill_volume = 1000000\nregion_undo_history = 1",
            "max_region_fill_volume = 16384\nregion_undo_history = 64",
            "keep_alive_interval_ms = 100",
            "keep_alive_interval_ms = 15000",
            "view_distance = 0\nchunk_stream_interval_ms = 10",
            "chunk_stream_interval_ms = 60000",
            "default_permission_level = 0",
            "default_permission_level = 4",
            "[access]\nper_ip_connection_limit = 0",
            "[access]\nper_ip_connection_limit = 256",
            "[budget]\nsustained_rate = 1.0\nburst = 1.0",
            "max_connections = 10\n\
             [budget]\nsustained_rate = 10000.0\nburst = 20000.0",
        ];

        for toml in cases {
            let config = AppConfig::from_toml_str(toml)
                .unwrap_or_else(|error| panic!("declared boundary failed: {toml:?}: {error:#}"));
            assert!(!config.tick_period().is_zero());
        }

        let maximum_spawn = AppConfig::from_toml_str("spawn_chunk_radius = 8")
            .expect("maximum spawn radius is valid");
        assert_eq!(
            (2 * usize::from(maximum_spawn.spawn_chunk_radius) + 1).pow(2),
            289,
            "accepted spawn configuration has a declared startup ceiling",
        );
        let maximum_view = AppConfig::from_toml_str(
            "max_connections = 1\nview_distance = 32\n\
             [access]\nper_ip_connection_limit = 0",
        )
        .expect("maximum view is valid");
        assert_eq!(
            (2 * usize::try_from(maximum_view.view_distance).unwrap() + 1).pow(2),
            4_225,
            "accepted per-player view has a declared residency ceiling",
        );
    }

    #[test]
    fn negative_compression_threshold_preserves_disabled_meaning() {
        let config =
            AppConfig::from_toml_str("compression_threshold = -7").expect("negative disables");
        assert_eq!(config.compression_threshold, None);
    }

    #[test]
    fn negative_spawn_protection_preserves_disabled_meaning() {
        let config = AppConfig::from_toml_str("spawn_protect_radius = -7")
            .expect("negative spawn protection disables");
        assert_eq!(config.spawn_protect_radius, 0);
    }

    #[test]
    fn accepted_discrete_ranges_keep_derived_work_nonzero_and_bounded() {
        for ticks_per_second in 1..=MAX_TICKS_PER_SECOND {
            let config = AppConfig {
                ticks_per_second,
                ..AppConfig::default()
            };
            config.validate().expect("accepted tick rate validates");
            assert!(!config.tick_period().is_zero());
        }

        for radius in 0..=MAX_SPAWN_CHUNK_RADIUS {
            let side = 2 * u64::from(radius) + 1;
            assert!(side * side <= 289);
        }
        for distance in 0..=MAX_PLAY_DISTANCE {
            let side = 2 * u64::try_from(distance).unwrap() + 1;
            assert!(side * side <= 4_225);
        }
    }

    #[test]
    fn multiplicative_resource_maxima_are_rejected_actionably() {
        let cases = [
            (
                "max_connections = 1024\nview_distance = 32\n\
                 [budget]\nsustained_rate = 1.0\nburst = 1.0",
                "view slots",
            ),
            (
                "view_distance = 32\nchunk_stream_interval_ms = 10",
                "view-scan units/second",
            ),
            (
                "max_region_fill_volume = 1000000\nregion_undo_history = 64",
                "retains up to",
            ),
            (
                "max_connections = 11\n\
                 [budget]\nsustained_rate = 10000.0\nburst = 20000.0",
                "serverbound frames/second",
            ),
            (
                "max_connections = 11\n\
                 [budget]\nsustained_rate = 1.0\nburst = 20000.0",
                "immediate serverbound frames",
            ),
        ];
        for (toml, diagnostic) in cases {
            let error = AppConfig::from_toml_str(toml)
                .expect_err("combined resource maxima must be rejected");
            assert!(
                error.to_string().contains(diagnostic),
                "missing {diagnostic:?} in {error:#}",
            );
        }
    }
}
