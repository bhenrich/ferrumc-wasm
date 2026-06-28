//! Access control for a public-facing server: a per-IP connection limit, a
//! ban list, and an optional whitelist.
//!
//! Two layers live here:
//!
//! - [`AccessConfig`] is the *declarative* shape an operator writes in TOML
//!   (inline lists and/or file paths, the per-IP cap, the whitelist toggle). It
//!   is pure data and performs no I/O when deserialized.
//! - [`ResolvedAccess`] is the *runtime* form: [`AccessConfig::resolve`] reads
//!   any referenced files, classifies every entry into names / UUIDs / IPs, and
//!   produces fast lookup sets. The hot-path predicates ([`login_decision`] and
//!   [`is_ip_banned`]) live on it.
//!
//! Entries are classified by shape, not by a prefix: a token that parses as a
//! [`Uuid`] is a UUID entry, a token that parses as an [`IpAddr`] (bans only) is
//! an IP entry, and anything else is a player name. Names are matched
//! case-insensitively (Minecraft usernames are not case-sensitive for op tooling),
//! so they are stored and compared lowercased.
//!
//! [`login_decision`]: ResolvedAccess::login_decision
//! [`is_ip_banned`]: ResolvedAccess::is_ip_banned

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use uuid::Uuid;

/// Default ceiling on concurrent connections from a single source IP.
///
/// A small cap that still lets a household behind one NAT join with a couple of
/// accounts, while denying a single host from exhausting connection slots. `0`
/// disables the per-IP limit entirely (see [`ResolvedAccess::per_ip_connection_limit`]).
pub const DEFAULT_PER_IP_CONNECTION_LIMIT: usize = 3;

/// Default whitelist toggle: disabled.
///
/// A local alpha runs offline and open; an operator opts into the whitelist
/// explicitly once the server faces the public internet.
pub const DEFAULT_WHITELIST_ENABLED: bool = false;

/// Declarative access-control configuration, deserialized from the `[access]`
/// TOML table.
///
/// Every field has a documented default, so an omitted `[access]` table (or any
/// omitted field within it) yields the safe defaults. Deserialization performs no
/// file I/O — call [`resolve`](Self::resolve) to read any referenced files and
/// build the runtime [`ResolvedAccess`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessConfig {
    /// Maximum number of concurrent connections allowed from a single source IP.
    /// `0` disables the per-IP limit.
    pub per_ip_connection_limit: usize,
    /// When `true`, only whitelisted players may complete login. When `false`
    /// (the default), the whitelist is ignored and any non-banned player may join.
    pub whitelist_enabled: bool,
    /// Inline allow-list of player names and/or UUIDs. Merged with
    /// [`whitelist_file`](Self::whitelist_file). Only consulted when
    /// [`whitelist_enabled`](Self::whitelist_enabled) is `true`.
    pub whitelist: Vec<String>,
    /// Optional path to a newline-delimited allow-list file (names and/or UUIDs,
    /// one per line; blank lines and `#` comments ignored). Resolved relative to
    /// the base directory passed to [`resolve`](Self::resolve).
    pub whitelist_file: Option<PathBuf>,
    /// Inline deny-list of player names, UUIDs, and/or IP addresses. Merged with
    /// [`ban_file`](Self::ban_file) and always enforced regardless of the
    /// whitelist toggle.
    pub bans: Vec<String>,
    /// Optional path to a newline-delimited deny-list file (names, UUIDs, and/or
    /// IPs, one per line; blank lines and `#` comments ignored). Resolved relative
    /// to the base directory passed to [`resolve`](Self::resolve).
    pub ban_file: Option<PathBuf>,
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            per_ip_connection_limit: DEFAULT_PER_IP_CONNECTION_LIMIT,
            whitelist_enabled: DEFAULT_WHITELIST_ENABLED,
            whitelist: Vec::new(),
            whitelist_file: None,
            bans: Vec::new(),
            ban_file: None,
        }
    }
}

impl AccessConfig {
    /// Reads any referenced whitelist/ban files and builds the runtime
    /// [`ResolvedAccess`].
    ///
    /// Relative `whitelist_file` / `ban_file` paths are resolved against
    /// `base_dir` (typically the server's working directory); absolute paths are
    /// used verbatim. Inline list entries and file entries are merged.
    ///
    /// # Errors
    ///
    /// Returns [`AccessConfigError::ReadFile`] if a configured file cannot be
    /// read. A *missing* configured file is treated as an error (fail fast on
    /// operator misconfiguration) rather than silently starting with an empty
    /// list.
    pub fn resolve(&self, base_dir: &Path) -> Result<ResolvedAccess, AccessConfigError> {
        let mut whitelisted_names = HashSet::new();
        let mut whitelisted_uuids = HashSet::new();
        let whitelist_tokens = self
            .whitelist
            .iter()
            .cloned()
            .chain(read_file_tokens(base_dir, self.whitelist_file.as_deref())?);
        for token in whitelist_tokens {
            match classify_player(&token) {
                PlayerToken::Uuid(uuid) => {
                    whitelisted_uuids.insert(uuid);
                }
                PlayerToken::Name(name) => {
                    whitelisted_names.insert(name);
                }
            }
        }

        let mut banned_names = HashSet::new();
        let mut banned_uuids = HashSet::new();
        let mut banned_ips = HashSet::new();
        let ban_tokens = self
            .bans
            .iter()
            .cloned()
            .chain(read_file_tokens(base_dir, self.ban_file.as_deref())?);
        for token in ban_tokens {
            match classify_ban(&token) {
                BanToken::Uuid(uuid) => {
                    banned_uuids.insert(uuid);
                }
                BanToken::Ip(ip) => {
                    banned_ips.insert(ip);
                }
                BanToken::Name(name) => {
                    banned_names.insert(name);
                }
            }
        }

        Ok(ResolvedAccess {
            per_ip_connection_limit: self.per_ip_connection_limit,
            whitelist_enabled: self.whitelist_enabled,
            whitelisted_names,
            whitelisted_uuids,
            banned_names,
            banned_uuids,
            banned_ips,
        })
    }
}

/// A failure resolving an [`AccessConfig`] into a [`ResolvedAccess`].
#[derive(Debug, thiserror::Error)]
pub enum AccessConfigError {
    /// A configured whitelist or ban file could not be read.
    #[error("reading access-control file {path}: {source}")]
    ReadFile {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
}

/// Runtime, resolved access-control state with the hot-path lookup sets.
///
/// Built once at startup by [`AccessConfig::resolve`] and shared (behind an
/// `Arc`) across the acceptor and every connection task. Cheap to query: every
/// predicate is a set membership test.
#[derive(Debug, Clone)]
pub struct ResolvedAccess {
    per_ip_connection_limit: usize,
    whitelist_enabled: bool,
    whitelisted_names: HashSet<String>,
    whitelisted_uuids: HashSet<Uuid>,
    banned_names: HashSet<String>,
    banned_uuids: HashSet<Uuid>,
    banned_ips: HashSet<IpAddr>,
}

impl ResolvedAccess {
    /// The configured per-IP concurrent-connection cap; `0` means unlimited.
    #[must_use]
    pub fn per_ip_connection_limit(&self) -> usize {
        self.per_ip_connection_limit
    }

    /// Whether a connection from `ip` must be rejected because the IP is banned.
    #[must_use]
    pub fn is_ip_banned(&self, ip: IpAddr) -> bool {
        self.banned_ips.contains(&ip)
    }

    /// Decides whether a player identified by `name` and `uuid` may complete login.
    ///
    /// Bans take precedence over the whitelist: a banned name or UUID is denied
    /// even if it also appears on the whitelist. When the whitelist is enabled, a
    /// player matched by neither name nor UUID is denied as not whitelisted.
    /// Otherwise the login is allowed.
    #[must_use]
    pub fn login_decision(&self, name: &str, uuid: Uuid) -> LoginDecision {
        let lowered = name.to_lowercase();
        if self.banned_uuids.contains(&uuid) || self.banned_names.contains(&lowered) {
            return LoginDecision::Deny(DenyReason::Banned);
        }
        if self.whitelist_enabled
            && !self.whitelisted_uuids.contains(&uuid)
            && !self.whitelisted_names.contains(&lowered)
        {
            return LoginDecision::Deny(DenyReason::NotWhitelisted);
        }
        LoginDecision::Allow
    }
}

/// The outcome of a [`ResolvedAccess::login_decision`] check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginDecision {
    /// The player may proceed with login.
    Allow,
    /// The player is rejected; the variant carries why.
    Deny(DenyReason),
}

/// Why a login was denied, with a player-facing kick message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The player's name or UUID is on the ban list.
    Banned,
    /// The whitelist is enabled and the player is not on it.
    NotWhitelisted,
}

impl DenyReason {
    /// The default player-facing kick message for this denial.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::Banned => "You are banned from this server.",
            Self::NotWhitelisted => "You are not whitelisted on this server.",
        }
    }
}

/// A classified whitelist entry.
enum PlayerToken {
    /// A parsed UUID.
    Uuid(Uuid),
    /// A lowercased player name.
    Name(String),
}

/// A classified ban entry.
enum BanToken {
    /// A parsed UUID.
    Uuid(Uuid),
    /// A parsed IP address.
    Ip(IpAddr),
    /// A lowercased player name.
    Name(String),
}

/// Classifies a whitelist token: a UUID if it parses as one, else a (lowercased)
/// player name.
fn classify_player(token: &str) -> PlayerToken {
    match Uuid::parse_str(token) {
        Ok(uuid) => PlayerToken::Uuid(uuid),
        Err(_) => PlayerToken::Name(token.to_lowercase()),
    }
}

/// Classifies a ban token: a UUID, then an IP, else a (lowercased) player name.
fn classify_ban(token: &str) -> BanToken {
    if let Ok(uuid) = Uuid::parse_str(token) {
        return BanToken::Uuid(uuid);
    }
    if let Ok(ip) = token.parse::<IpAddr>() {
        return BanToken::Ip(ip);
    }
    BanToken::Name(token.to_lowercase())
}

/// Reads `path` (joined onto `base_dir` when relative) and returns its non-blank,
/// non-comment, trimmed lines. Returns an empty `Vec` when `path` is `None`.
fn read_file_tokens(
    base_dir: &Path,
    path: Option<&Path>,
) -> Result<Vec<String>, AccessConfigError> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let resolved = base_dir.join(path);
    let contents =
        std::fs::read_to_string(&resolved).map_err(|source| AccessConfigError::ReadFile {
            path: resolved,
            source,
        })?;
    Ok(parse_tokens(&contents))
}

/// Splits file `contents` into entry tokens: trims each line, drops blank lines
/// and lines whose first non-whitespace character is `#`.
fn parse_tokens(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    /// A fixed, valid UUID used across the classification tests.
    const SAMPLE_UUID: &str = "069a79f4-44e9-4726-a5be-fca90e38aaf5";

    fn resolve(config: &AccessConfig) -> ResolvedAccess {
        // No file paths set, so the base dir is never touched.
        config
            .resolve(Path::new("."))
            .expect("resolve without files cannot fail")
    }

    #[test]
    fn defaults_are_open_with_per_ip_three() {
        let resolved = resolve(&AccessConfig::default());
        assert_eq!(resolved.per_ip_connection_limit(), 3);
        // Whitelist disabled => any non-banned player is allowed.
        assert_eq!(
            resolved.login_decision("Anyone", Uuid::nil()),
            LoginDecision::Allow
        );
    }

    #[test]
    fn whitelist_disabled_allows_everyone() {
        let config = AccessConfig {
            whitelist_enabled: false,
            whitelist: vec!["Saad".to_string()],
            ..AccessConfig::default()
        };
        let resolved = resolve(&config);
        assert_eq!(
            resolved.login_decision("Intruder", Uuid::nil()),
            LoginDecision::Allow
        );
    }

    #[test]
    fn whitelist_enabled_allows_only_listed_names() {
        let config = AccessConfig {
            whitelist_enabled: true,
            whitelist: vec!["Saad".to_string()],
            ..AccessConfig::default()
        };
        let resolved = resolve(&config);
        // Case-insensitive name match.
        assert_eq!(
            resolved.login_decision("saad", Uuid::nil()),
            LoginDecision::Allow
        );
        assert_eq!(
            resolved.login_decision("Intruder", Uuid::nil()),
            LoginDecision::Deny(DenyReason::NotWhitelisted)
        );
    }

    #[test]
    fn whitelist_enabled_allows_listed_uuid() {
        let uuid = Uuid::parse_str(SAMPLE_UUID).expect("valid uuid");
        let config = AccessConfig {
            whitelist_enabled: true,
            whitelist: vec![SAMPLE_UUID.to_string()],
            ..AccessConfig::default()
        };
        let resolved = resolve(&config);
        // Allowed by UUID even though the name is not listed.
        assert_eq!(
            resolved.login_decision("NameDoesNotMatter", uuid),
            LoginDecision::Allow
        );
        assert_eq!(
            resolved.login_decision("Other", Uuid::nil()),
            LoginDecision::Deny(DenyReason::NotWhitelisted)
        );
    }

    #[test]
    fn ban_by_name_denies_even_if_whitelisted() {
        let config = AccessConfig {
            whitelist_enabled: true,
            whitelist: vec!["Griefer".to_string()],
            bans: vec!["Griefer".to_string()],
            ..AccessConfig::default()
        };
        let resolved = resolve(&config);
        // Ban wins over whitelist.
        assert_eq!(
            resolved.login_decision("Griefer", Uuid::nil()),
            LoginDecision::Deny(DenyReason::Banned)
        );
    }

    #[test]
    fn ban_by_uuid_denies() {
        let uuid = Uuid::parse_str(SAMPLE_UUID).expect("valid uuid");
        let config = AccessConfig {
            bans: vec![SAMPLE_UUID.to_string()],
            ..AccessConfig::default()
        };
        let resolved = resolve(&config);
        assert_eq!(
            resolved.login_decision("AnyName", uuid),
            LoginDecision::Deny(DenyReason::Banned)
        );
    }

    #[test]
    fn ip_ban_classification_and_lookup() {
        let config = AccessConfig {
            bans: vec!["10.0.0.5".to_string(), "Griefer".to_string()],
            ..AccessConfig::default()
        };
        let resolved = resolve(&config);
        assert!(resolved.is_ip_banned(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))));
        assert!(!resolved.is_ip_banned(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 6))));
        // The name token must not have been mistaken for an IP.
        assert_eq!(
            resolved.login_decision("Griefer", Uuid::nil()),
            LoginDecision::Deny(DenyReason::Banned)
        );
    }

    #[test]
    fn parse_tokens_skips_blanks_and_comments() {
        let contents = "\n  # a comment\nSaad\n   \n  Notch  \n# trailing comment\n";
        let tokens = parse_tokens(contents);
        assert_eq!(tokens, vec!["Saad".to_string(), "Notch".to_string()]);
    }

    #[test]
    fn resolve_reads_whitelist_and_ban_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("whitelist.txt"), "# allow\nSaad\n").expect("write");
        std::fs::write(dir.path().join("bans.txt"), "Griefer\n10.0.0.5\n").expect("write");
        let config = AccessConfig {
            whitelist_enabled: true,
            whitelist_file: Some(PathBuf::from("whitelist.txt")),
            ban_file: Some(PathBuf::from("bans.txt")),
            ..AccessConfig::default()
        };
        let resolved = config.resolve(dir.path()).expect("resolve reads files");
        assert_eq!(
            resolved.login_decision("Saad", Uuid::nil()),
            LoginDecision::Allow
        );
        assert_eq!(
            resolved.login_decision("Griefer", Uuid::nil()),
            LoginDecision::Deny(DenyReason::Banned)
        );
        assert!(resolved.is_ip_banned(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))));
    }

    #[test]
    fn missing_file_is_an_error() {
        let config = AccessConfig {
            ban_file: Some(PathBuf::from("does-not-exist.txt")),
            ..AccessConfig::default()
        };
        let err = config
            .resolve(Path::new("/nonexistent-base"))
            .expect_err("missing file must fail fast");
        assert!(matches!(err, AccessConfigError::ReadFile { .. }));
    }

    #[test]
    fn deny_reasons_have_messages() {
        assert!(!DenyReason::Banned.message().is_empty());
        assert!(!DenyReason::NotWhitelisted.message().is_empty());
    }
}
