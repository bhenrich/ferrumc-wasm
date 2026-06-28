//! World-content configuration: where the world's initial terrain comes from.
//!
//! [`WorldConfig`] is the declarative `[world]` TOML table. By default the server
//! generates its built-in flat world; pointing `anvil_import_dir` at a vanilla
//! Anvil `region/` directory instead imports that prebuilt map into the world
//! store at startup. This type is pure data and performs no I/O — the application
//! layer reads the directory while bringing the world up, before it accepts any
//! connections.
//!
//! This is distinct from the application's top-level `world_dir` key, which names
//! where the durable world *database* is persisted: `world_dir` is the save
//! location, `[world].anvil_import_dir` is the one-time initial *content* source.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Declarative world-content configuration, deserialized from the `[world]` TOML
/// table.
///
/// Every field is optional, so an omitted `[world]` table — or any omitted field
/// within it — keeps the defaults: no Anvil import, i.e. the built-in flat world.
/// Unknown keys are rejected so a typo never silently does nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorldConfig {
    /// Optional path to a vanilla Anvil `region/` directory whose `r.<x>.<z>.mca`
    /// region files are imported into the world store at startup.
    ///
    /// `None` (the default — the key omitted) leaves the server on its built-in
    /// flat world. `Some(dir)` imports every region file found in `dir` *before*
    /// the server accepts connections, so a prebuilt map populates the world. A
    /// missing directory, a directory with no region files, or a malformed region
    /// file aborts startup with a clear error rather than silently serving flat
    /// terrain.
    pub anvil_import_dir: Option<PathBuf>,
}

impl WorldConfig {
    /// Returns the configured Anvil import directory, or `None` when the server
    /// should generate its built-in flat world.
    #[must_use]
    pub fn anvil_import_dir(&self) -> Option<&Path> {
        self.anvil_import_dir.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_imports_nothing() {
        let config = WorldConfig::default();
        assert_eq!(config.anvil_import_dir(), None);
    }

    #[test]
    fn an_omitted_table_keeps_the_defaults() {
        let parsed: WorldConfig = toml::from_str("").expect("empty table is valid");
        assert_eq!(parsed, WorldConfig::default());
    }

    #[test]
    fn anvil_import_dir_parses_from_toml() {
        let parsed: WorldConfig =
            toml::from_str(r#"anvil_import_dir = "/srv/maps/spawn/region""#).expect("valid table");
        assert_eq!(
            parsed.anvil_import_dir(),
            Some(Path::new("/srv/maps/spawn/region"))
        );
    }

    #[test]
    fn unknown_field_is_rejected() {
        assert!(toml::from_str::<WorldConfig>("bogus = 1").is_err());
    }
}
