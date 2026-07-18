//! Static plugin identity and version declarations.

use crate::{CapabilityManifest, DeclarationError};

/// Maximum accepted byte length of a stable plugin identifier.
pub const MAX_PLUGIN_ID_BYTES: usize = 128;

/// Maximum accepted byte length of a plugin display name.
pub const MAX_PLUGIN_NAME_BYTES: usize = 256;

/// A fixed-width semantic plugin version.
///
/// Numeric components map directly to the stable plugin descriptor used by
/// the trusted native plugin packaging adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PluginVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl PluginVersion {
    /// Creates a semantic version from its three numeric components.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Returns the major version.
    pub const fn major(self) -> u32 {
        self.major
    }

    /// Returns the minor version.
    pub const fn minor(self) -> u32 {
        self.minor
    }

    /// Returns the patch version.
    pub const fn patch(self) -> u32 {
        self.patch
    }
}

/// Compile-time-reachable identity and capability declaration for a plugin.
///
/// The declaration is one source of truth for both built-in and
/// trusted native plugin packaging. Adapters call [`PluginDeclaration::validate`]
/// before registration or export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginDeclaration {
    id: &'static str,
    name: &'static str,
    version: PluginVersion,
    requested_capabilities: CapabilityManifest,
}

impl PluginDeclaration {
    /// Creates a static plugin declaration.
    pub const fn new(
        id: &'static str,
        name: &'static str,
        version: PluginVersion,
        requested_capabilities: CapabilityManifest,
    ) -> Self {
        Self {
            id,
            name,
            version,
            requested_capabilities,
        }
    }

    /// Returns the stable plugin identifier.
    pub const fn id(self) -> &'static str {
        self.id
    }

    /// Returns the human-readable display name.
    pub const fn name(self) -> &'static str {
        self.name
    }

    /// Returns the plugin version.
    pub const fn version(self) -> PluginVersion {
        self.version
    }

    /// Returns the capabilities requested by the plugin.
    pub const fn requested_capabilities(self) -> CapabilityManifest {
        self.requested_capabilities
    }

    /// Validates identity fields before an adapter exposes the plugin.
    ///
    /// Identifiers are nonempty and bounded. Character-policy alignment across
    /// loaders, metrics, and existing compiled registrations remains a host
    /// admission decision rather than an SDK packaging difference.
    pub fn validate(self) -> Result<(), DeclarationError> {
        validate_id(self.id)?;
        if self.name.is_empty() {
            return Err(DeclarationError::EmptyName);
        }
        if self.name.len() > MAX_PLUGIN_NAME_BYTES {
            return Err(DeclarationError::NameTooLong {
                len: self.name.len(),
                max: MAX_PLUGIN_NAME_BYTES,
            });
        }
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), DeclarationError> {
    if id.is_empty() {
        return Err(DeclarationError::EmptyId);
    }
    if id.len() > MAX_PLUGIN_ID_BYTES {
        return Err(DeclarationError::IdTooLong {
            len: id.len(),
            max: MAX_PLUGIN_ID_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Capability;

    #[test]
    fn declaration_is_const_reachable_and_validated() {
        const DECLARATION: PluginDeclaration = PluginDeclaration::new(
            "region-guard",
            "Region Guard",
            PluginVersion::new(1, 2, 3),
            CapabilityManifest::empty().with(Capability::ReadWorld),
        );

        assert_eq!(DECLARATION.id(), "region-guard");
        assert_eq!(DECLARATION.version().minor(), 2);
        assert!(DECLARATION
            .requested_capabilities()
            .grants(Capability::ReadWorld));
        assert_eq!(DECLARATION.validate(), Ok(()));
    }

    #[test]
    fn declaration_rejects_empty_id_and_bounds_names() {
        let bad = PluginDeclaration::new(
            "",
            "Bad",
            PluginVersion::new(1, 0, 0),
            CapabilityManifest::empty(),
        );
        assert_eq!(bad.validate(), Err(DeclarationError::EmptyId));

        let long_name: &'static str =
            Box::leak("x".repeat(MAX_PLUGIN_NAME_BYTES + 1).into_boxed_str());
        let bad_name = PluginDeclaration::new(
            "valid",
            long_name,
            PluginVersion::new(1, 0, 0),
            CapabilityManifest::empty(),
        );
        assert!(matches!(
            bad_name.validate(),
            Err(DeclarationError::NameTooLong { .. })
        ));
    }
}
