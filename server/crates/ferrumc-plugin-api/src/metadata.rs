//! Static description a plugin reports about itself.

use ferrumc_core::PluginId;
use semver::Version;

use crate::capability::CapabilityManifest;

/// Immutable, self-reported description of a plugin.
///
/// The host reads this once at registration to learn the plugin's identity, its
/// human-readable name and version, and the [`CapabilityManifest`] it requests.
/// The host decides what to actually grant; the requested manifest is only the
/// plugin's declaration of intent. Fields are private; use the accessors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMetadata {
    id: PluginId,
    name: String,
    version: Version,
    description: Option<String>,
    requested_capabilities: CapabilityManifest,
}

impl PluginMetadata {
    /// Builds metadata from a stable `id`, a display `name`, a semantic
    /// `version`, and the `requested_capabilities` the plugin needs.
    pub fn new(
        id: PluginId,
        name: impl Into<String>,
        version: Version,
        requested_capabilities: CapabilityManifest,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            version,
            description: None,
            requested_capabilities,
        }
    }

    /// Returns a copy of this metadata with `description` attached.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Returns the plugin's stable identifier.
    pub const fn id(&self) -> &PluginId {
        &self.id
    }

    /// Returns the plugin's human-readable name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the plugin's semantic version.
    pub const fn version(&self) -> &Version {
        &self.version
    }

    /// Returns the plugin's optional description, if one was set.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// Returns the capabilities the plugin declares it needs.
    pub const fn requested_capabilities(&self) -> CapabilityManifest {
        self.requested_capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;

    #[test]
    fn metadata_round_trips_its_fields() {
        let meta = PluginMetadata::new(
            PluginId::new("spawn-protect"),
            "Spawn Protection",
            Version::new(1, 2, 3),
            CapabilityManifest::empty().with(Capability::ReadWorld),
        )
        .with_description("Protects spawn from grief");

        assert_eq!(meta.id().as_str(), "spawn-protect");
        assert_eq!(meta.name(), "Spawn Protection");
        assert_eq!(meta.version(), &Version::new(1, 2, 3));
        assert_eq!(meta.description(), Some("Protects spawn from grief"));
        assert!(meta.requested_capabilities().grants(Capability::ReadWorld));
        assert!(!meta.requested_capabilities().grants(Capability::Storage));
    }

    #[test]
    fn description_defaults_to_none() {
        let meta = PluginMetadata::new(
            PluginId::new("x"),
            "X",
            Version::new(0, 1, 0),
            CapabilityManifest::empty(),
        );
        assert_eq!(meta.description(), None);
    }
}
