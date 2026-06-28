//! Minimal server configuration: parsing, defaults, and validation.
//!
//! The vertical slice needs only a handful of knobs (where to bind, the spawn
//! area to keep resident, the play view distances, and a few transport limits).
//! [`AppConfig`] carries the validated, runtime-ready values; [`RawConfig`] is
//! the optional-field TOML shape that merges over the documented defaults.

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

use ferrumc_math::Vec3;
use serde::Deserialize;

/// Default address the server binds to when the config omits one.
const DEFAULT_BIND: &str = "127.0.0.1:25565";

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

/// Default spawn-protection radius, in blocks. Zero disables spawn protection
/// entirely, which is the default so an unconfigured server protects nothing.
const DEFAULT_SPAWN_PROTECT_RADIUS: i32 = 0;

/// Default play-phase keep-alive interval, in milliseconds.
///
/// A vanilla client disconnects if it hears no Keep Alive for 20 s, so the server
/// pings every 10 s. Configurable (in ms) so tests can drive a short interval
/// without a wall-clock wait.
const DEFAULT_KEEP_ALIVE_INTERVAL_MS: u64 = 10_000;

/// Default permission level granted to a non-operator player.
///
/// Zero is the vanilla "ordinary player" tier: it satisfies no operator gate, so
/// commands like `/gamemode` (which require an operator level) are refused for
/// everyone except the players listed in [`AppConfig::ops`]. This is what makes
/// the operator gate meaningful instead of granting every connection level 4.
const DEFAULT_PERMISSION_LEVEL: u8 = 0;

/// Validated, runtime-ready server configuration.
///
/// Construct one with [`AppConfig::default`] for the documented defaults, or
/// parse and validate user input with [`AppConfig::from_toml_str`]. Every field
/// is already checked, so the rest of the application can consume it directly.
#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    /// The socket address the TCP listener binds to.
    pub bind: SocketAddr,
    /// The ceiling on simultaneously accepted connections.
    pub max_connections: usize,
    /// The deadline applied to each socket read and write.
    pub io_timeout: Duration,
    /// The packet-compression threshold in bytes, or `None` to leave compression
    /// disabled (the slice default — frames travel uncompressed).
    pub compression_threshold: Option<i32>,
    /// The play view distance advertised to clients, in chunks.
    pub view_distance: i32,
    /// The play simulation distance advertised to clients, in chunks.
    pub simulation_distance: i32,
    /// The world-spawn position players join at.
    pub spawn: Vec3,
    /// The radius, in chunks, of the spawn area kept resident.
    pub spawn_chunk_radius: u8,
    /// The simulation tick rate, in ticks per second.
    pub ticks_per_second: NonZeroU32,
    /// Directory scanned for dynamic (`cdylib`) plugins at startup, or `None` to
    /// skip dynamic plugin loading.
    pub plugins_dir: Option<PathBuf>,
    /// Spawn-protection radius, in blocks (Chebyshev) around the spawn column.
    /// Zero disables spawn protection.
    pub spawn_protect_radius: i32,
    /// Names of players granted the spawn-protection bypass permission.
    pub spawn_protect_bypass: Vec<String>,
    /// Interval between clientbound play-phase Keep Alive pings.
    pub keep_alive_interval: Duration,
    /// Names of players granted operator status (permission level 4), letting
    /// them run operator-gated commands such as `/gamemode`. Everyone else acts
    /// at [`default_permission_level`](Self::default_permission_level).
    pub ops: Vec<String>,
    /// Permission level granted to a player who is not an operator. Defaults to
    /// `0` (ordinary player), so the operator gate is meaningful.
    pub default_permission_level: u8,
    /// Where the persistent world database lives.
    ///
    /// `Some(path)` selects the durable redb-backed [`WorldStore`] at that path
    /// (the runtime default — `main` fills in a default directory when the config
    /// omits one); `None` selects the in-memory store, which keeps tests
    /// deterministic and file-free. The redb file is created under this directory.
    ///
    /// [`WorldStore`]: ferrumc_storage::WorldStore
    pub world_dir: Option<PathBuf>,
}

impl AppConfig {
    /// Parses and validates an [`AppConfig`] from a TOML document.
    ///
    /// Any field the document omits keeps its documented default. The bind
    /// address is parsed and the tick rate is checked to be non-zero; a malformed
    /// document, an unparseable address, or a zero tick rate is an error.
    ///
    /// # Errors
    ///
    /// Returns an error if the TOML cannot be parsed, contains an unknown field,
    /// carries an invalid bind address, or sets a tick rate of zero.
    pub fn from_toml_str(toml: &str) -> anyhow::Result<Self> {
        let raw: RawConfig = toml::from_str(toml)?;
        raw.into_config()
    }

    /// The nominal duration of one simulation tick.
    #[must_use]
    pub fn tick_period(&self) -> Duration {
        Duration::from_nanos(1_000_000_000 / u64::from(self.ticks_per_second.get()))
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        // `expect` is confined to building the compile-time defaults, the
        // documented startup-config exception: these literals are known good.
        Self {
            bind: DEFAULT_BIND.parse().expect("default bind address is valid"),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            io_timeout: Duration::from_secs(DEFAULT_IO_TIMEOUT_SECS),
            compression_threshold: None,
            view_distance: DEFAULT_VIEW_DISTANCE,
            simulation_distance: DEFAULT_SIMULATION_DISTANCE,
            spawn: DEFAULT_SPAWN,
            spawn_chunk_radius: DEFAULT_SPAWN_CHUNK_RADIUS,
            ticks_per_second: NonZeroU32::new(DEFAULT_TICKS_PER_SECOND)
                .expect("default tick rate is non-zero"),
            plugins_dir: None,
            spawn_protect_radius: DEFAULT_SPAWN_PROTECT_RADIUS,
            spawn_protect_bypass: Vec::new(),
            keep_alive_interval: Duration::from_millis(DEFAULT_KEEP_ALIVE_INTERVAL_MS),
            ops: Vec::new(),
            default_permission_level: DEFAULT_PERMISSION_LEVEL,
            // None = in-memory, keeping `AppConfig::default()` deterministic and
            // file-free for tests. `main` substitutes a durable redb directory so
            // the shipping server persists by default.
            world_dir: None,
        }
    }
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
    /// Override for [`AppConfig::spawn_protect_radius`].
    spawn_protect_radius: Option<i32>,
    /// Override for [`AppConfig::spawn_protect_bypass`].
    spawn_protect_bypass: Option<Vec<String>>,
    /// Override for [`AppConfig::keep_alive_interval`], expressed in milliseconds.
    keep_alive_interval_ms: Option<u64>,
    /// Override for [`AppConfig::ops`].
    ops: Option<Vec<String>>,
    /// Override for [`AppConfig::default_permission_level`].
    default_permission_level: Option<u8>,
    /// Override for [`AppConfig::world_dir`], as a filesystem path. When set, the
    /// durable redb store is used at this directory.
    world_dir: Option<String>,
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

        let ticks_per_second = match self.ticks_per_second {
            Some(value) => NonZeroU32::new(value)
                .ok_or_else(|| anyhow::anyhow!("ticks_per_second must be greater than zero"))?,
            None => defaults.ticks_per_second,
        };

        let spawn = self
            .spawn
            .map_or(defaults.spawn, |[x, y, z]| Vec3::new(x, y, z));

        Ok(AppConfig {
            bind,
            max_connections: self.max_connections.unwrap_or(defaults.max_connections),
            io_timeout: self
                .io_timeout_secs
                .map_or(defaults.io_timeout, Duration::from_secs),
            compression_threshold: self
                .compression_threshold
                .or(defaults.compression_threshold),
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
            spawn_protect_radius: self
                .spawn_protect_radius
                .unwrap_or(defaults.spawn_protect_radius),
            spawn_protect_bypass: self
                .spawn_protect_bypass
                .unwrap_or(defaults.spawn_protect_bypass),
            keep_alive_interval: self
                .keep_alive_interval_ms
                .map_or(defaults.keep_alive_interval, Duration::from_millis),
            ops: self.ops.unwrap_or(defaults.ops),
            default_permission_level: self
                .default_permission_level
                .unwrap_or(defaults.default_permission_level),
            world_dir: self.world_dir.map(PathBuf::from).or(defaults.world_dir),
        })
    }
}

#[cfg(test)]
mod tests {
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
            spawn_protect_radius = 12
            spawn_protect_bypass = ["Admin", "Mod"]
            keep_alive_interval_ms = 250
            ops = ["Admin"]
            default_permission_level = 1
            world_dir = "/srv/world"
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
        assert_eq!(parsed.ticks_per_second, NonZeroU32::new(10).unwrap());
        assert_eq!(parsed.plugins_dir, Some(PathBuf::from("/srv/plugins")));
        assert_eq!(parsed.spawn_protect_radius, 12);
        assert_eq!(parsed.spawn_protect_bypass, vec!["Admin", "Mod"]);
        assert_eq!(parsed.keep_alive_interval, Duration::from_millis(250));
        assert_eq!(parsed.ops, vec!["Admin"]);
        assert_eq!(parsed.default_permission_level, 1);
        assert_eq!(parsed.world_dir, Some(PathBuf::from("/srv/world")));
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
    fn operators_default_to_empty_with_player_level_zero() {
        let parsed = AppConfig::from_toml_str("").expect("empty config is valid");
        assert!(parsed.ops.is_empty());
        assert_eq!(parsed.default_permission_level, 0);
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
            ticks_per_second: NonZeroU32::new(20).unwrap(),
            ..AppConfig::default()
        };
        assert_eq!(config.tick_period(), Duration::from_millis(50));
    }
}
