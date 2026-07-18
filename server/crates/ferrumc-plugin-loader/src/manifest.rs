//! Parsing and validated owned values for `plugin.toml`.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::str;

use ferrumc_plugin_abi::{
    AbiVersion, FC_CAPABILITIES_V1, FC_CAPABILITY_READ_PERMISSIONS, FC_CAPABILITY_READ_WORLD,
    FC_CAPABILITY_RECEIVE_EVENTS, FC_CAPABILITY_REGISTER_COMMANDS, FC_CAPABILITY_STORAGE,
    FC_CAPABILITY_SUBMIT_INTENTS, FC_CAPABILITY_VETO_BLOCK_EDITS, FC_CAPABILITY_VETO_EVENTS,
};
use semver::{Version, VersionReq};
use serde::Deserialize;

/// One host-facade capability that a plugin manifest may request.
///
/// Names are stable, lowercase manifest identifiers. Bits are the exact ABI v1
/// assignments from `ferrumc-plugin-abi`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PluginCapability {
    /// Read world state through the host's read-only facade.
    ReadWorld,
    /// Submit bounded mutation intents to the host.
    SubmitIntents,
    /// Register commands during plugin initialization.
    RegisterCommands,
    /// Subscribe to and receive plugin events.
    ReceiveEvents,
    /// Read permission decisions through the host facade.
    ReadPermissions,
    /// Use the plugin's host-selected storage namespace.
    Storage,
    /// Participate in block-edit decision hooks.
    VetoBlockEdits,
    /// Participate in vetoable non-block event hooks.
    VetoEvents,
}

impl PluginCapability {
    /// Every defined capability in canonical ABI bit order.
    pub const ALL: [Self; 8] = [
        Self::ReadWorld,
        Self::SubmitIntents,
        Self::RegisterCommands,
        Self::ReceiveEvents,
        Self::ReadPermissions,
        Self::Storage,
        Self::VetoBlockEdits,
        Self::VetoEvents,
    ];

    /// Returns the exact lowercase name accepted in `plugin.toml`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadWorld => "read-world",
            Self::SubmitIntents => "submit-intents",
            Self::RegisterCommands => "register-commands",
            Self::ReceiveEvents => "receive-events",
            Self::ReadPermissions => "read-permissions",
            Self::Storage => "storage",
            Self::VetoBlockEdits => "veto-block-edits",
            Self::VetoEvents => "veto-events",
        }
    }

    /// Returns this capability's exact ABI v1 bit.
    pub const fn bit(self) -> u64 {
        match self {
            Self::ReadWorld => FC_CAPABILITY_READ_WORLD,
            Self::SubmitIntents => FC_CAPABILITY_SUBMIT_INTENTS,
            Self::RegisterCommands => FC_CAPABILITY_REGISTER_COMMANDS,
            Self::ReceiveEvents => FC_CAPABILITY_RECEIVE_EVENTS,
            Self::ReadPermissions => FC_CAPABILITY_READ_PERMISSIONS,
            Self::Storage => FC_CAPABILITY_STORAGE,
            Self::VetoBlockEdits => FC_CAPABILITY_VETO_BLOCK_EDITS,
            Self::VetoEvents => FC_CAPABILITY_VETO_EVENTS,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "read-world" => Some(Self::ReadWorld),
            "submit-intents" => Some(Self::SubmitIntents),
            "register-commands" => Some(Self::RegisterCommands),
            "receive-events" => Some(Self::ReceiveEvents),
            "read-permissions" => Some(Self::ReadPermissions),
            "storage" => Some(Self::Storage),
            "veto-block-edits" => Some(Self::VetoBlockEdits),
            "veto-events" => Some(Self::VetoEvents),
            _ => None,
        }
    }
}

impl fmt::Display for PluginCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An immutable set of capabilities requested by one plugin manifest.
///
/// Manifest parsing produces validated sets, while [`PluginCapabilities::all`]
/// and [`PluginCapabilities::with`] let callers build an explicit host policy.
/// Iteration always follows canonical ABI bit order, independent of insertion
/// or TOML declaration order.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PluginCapabilities {
    bits: u64,
}

impl PluginCapabilities {
    /// Returns an empty capability set.
    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    /// Returns the complete ABI v1 capability set.
    pub const fn all() -> Self {
        Self {
            bits: FC_CAPABILITIES_V1,
        }
    }

    /// Returns this set with `capability` included.
    #[must_use]
    pub const fn with(self, capability: PluginCapability) -> Self {
        Self {
            bits: self.bits | capability.bit(),
        }
    }

    /// Returns the exact ABI capability bitset.
    pub const fn bits(self) -> u64 {
        self.bits
    }

    /// Returns whether this set contains `capability`.
    pub const fn contains(self, capability: PluginCapability) -> bool {
        self.bits & capability.bit() != 0
    }

    /// Returns the number of requested capabilities.
    pub const fn len(self) -> u32 {
        self.bits.count_ones()
    }

    /// Returns whether no capabilities were requested.
    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// Iterates over requested capabilities in canonical ABI bit order.
    pub fn iter(self) -> impl Iterator<Item = PluginCapability> {
        PluginCapability::ALL
            .into_iter()
            .filter(move |capability| self.contains(*capability))
    }

    fn insert(&mut self, capability: PluginCapability) -> bool {
        let was_present = self.contains(capability);
        self.bits |= capability.bit();
        !was_present
    }
}

/// A fully parsed and field-validated `plugin.toml` manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginManifest {
    id: String,
    name: String,
    version: Version,
    abi_version: AbiVersion,
    server_api: VersionReq,
    library: PathBuf,
    library_sha256: [u8; 32],
    capabilities: PluginCapabilities,
}

impl PluginManifest {
    /// Returns the stable plugin identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the human-readable plugin name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the plugin's semantic version.
    pub const fn version(&self) -> &Version {
        &self.version
    }

    /// Returns the ABI major/minor pair declared by the manifest.
    pub const fn abi_version(&self) -> AbiVersion {
        self.abi_version
    }

    /// Returns the supported `FerrumC` server-API version range.
    pub const fn server_api(&self) -> &VersionReq {
        &self.server_api
    }

    /// Returns the validated relative path to the plugin library.
    pub fn library(&self) -> &Path {
        &self.library
    }

    /// Returns the expected SHA-256 digest of the plugin library.
    pub const fn library_sha256(&self) -> [u8; 32] {
        self.library_sha256
    }

    /// Returns the capabilities requested by the manifest.
    pub const fn capabilities(&self) -> PluginCapabilities {
        self.capabilities
    }
}

/// A `plugin.toml` document failed syntax or field validation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// The input byte sequence is not UTF-8.
    #[error("plugin manifest is not valid UTF-8: {source}")]
    InvalidUtf8 {
        /// The UTF-8 decoder failure.
        #[source]
        source: str::Utf8Error,
    },

    /// The UTF-8 document is not the exact required TOML shape.
    #[error("plugin manifest TOML is invalid: {source}")]
    InvalidToml {
        /// The TOML decoder failure.
        #[source]
        source: toml::de::Error,
    },

    /// The plugin identifier is empty.
    #[error("plugin manifest `id` must not be empty")]
    EmptyId,

    /// The plugin display name is empty.
    #[error("plugin manifest `name` must not be empty")]
    EmptyName,

    /// The plugin semantic version is invalid.
    #[error("plugin manifest version `{value}` is invalid: {source}")]
    InvalidVersion {
        /// The rejected version string.
        value: String,
        /// The semantic-version parser failure.
        #[source]
        source: semver::Error,
    },

    /// The server-API version requirement is invalid.
    #[error("plugin manifest server API range `{value}` is invalid: {source}")]
    InvalidServerApi {
        /// The rejected version-requirement string.
        value: String,
        /// The semantic-version-requirement parser failure.
        #[source]
        source: semver::Error,
    },

    /// The library path is empty.
    #[error("plugin manifest `library` must not be empty")]
    EmptyLibrary,

    /// The library path has a filesystem root or platform prefix.
    #[error("plugin manifest library path '{}' must be relative", path.display())]
    RootedOrPrefixedLibrary {
        /// The rejected library path.
        path: PathBuf,
    },

    /// The library path contains a parent-directory component.
    #[error(
        "plugin manifest library path '{}' contains a parent-directory component",
        path.display()
    )]
    ParentLibraryComponent {
        /// The rejected library path.
        path: PathBuf,
    },

    /// The library path contains a current-directory component.
    #[error(
        "plugin manifest library path '{}' contains a current-directory component",
        path.display()
    )]
    CurrentLibraryComponent {
        /// The rejected library path.
        path: PathBuf,
    },

    /// The checksum is not exactly 64 bytes.
    #[error("plugin manifest `library_sha256` must contain exactly 64 lowercase hex digits, got {found} bytes")]
    InvalidSha256Length {
        /// The rejected checksum's byte length.
        found: usize,
    },

    /// The checksum contains a byte outside lowercase hexadecimal.
    #[error(
        "plugin manifest `library_sha256` has noncanonical byte 0x{found:02x} at byte {index}"
    )]
    InvalidSha256Byte {
        /// The zero-based byte position.
        index: usize,
        /// The rejected byte.
        found: u8,
    },

    /// A requested capability name is not defined by ABI v1.
    #[error("plugin manifest requests unknown capability `{name}`")]
    UnknownCapability {
        /// The rejected capability name.
        name: String,
    },

    /// A capability appears more than once.
    #[error("plugin manifest requests capability `{capability}` more than once")]
    DuplicateCapability {
        /// The duplicated capability.
        capability: PluginCapability,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    id: String,
    name: String,
    version: String,
    abi_major: u16,
    abi_minor: u16,
    server_api: String,
    library: String,
    library_sha256: String,
    capabilities: Vec<String>,
}

/// Parses one complete `plugin.toml` byte sequence.
pub(crate) fn parse_manifest(input: &[u8]) -> Result<PluginManifest, ManifestError> {
    let text = str::from_utf8(input).map_err(|source| ManifestError::InvalidUtf8 { source })?;
    let raw: RawManifest =
        toml::from_str(text).map_err(|source| ManifestError::InvalidToml { source })?;

    if raw.id.is_empty() {
        return Err(ManifestError::EmptyId);
    }
    if raw.name.is_empty() {
        return Err(ManifestError::EmptyName);
    }

    let version = Version::parse(&raw.version).map_err(|source| ManifestError::InvalidVersion {
        value: raw.version.clone(),
        source,
    })?;
    let server_api =
        VersionReq::parse(&raw.server_api).map_err(|source| ManifestError::InvalidServerApi {
            value: raw.server_api.clone(),
            source,
        })?;
    let library = validate_library_path(&raw.library)?;
    let library_sha256 = parse_sha256(&raw.library_sha256)?;
    let capabilities = parse_capabilities(raw.capabilities)?;

    Ok(PluginManifest {
        id: raw.id,
        name: raw.name,
        version,
        abi_version: AbiVersion::new(raw.abi_major, raw.abi_minor),
        server_api,
        library,
        library_sha256,
        capabilities,
    })
}

fn validate_library_path(value: &str) -> Result<PathBuf, ManifestError> {
    if value.is_empty() {
        return Err(ManifestError::EmptyLibrary);
    }

    let path = PathBuf::from(value);
    if contains_path_segment(value, "..") {
        return Err(ManifestError::ParentLibraryComponent { path });
    }
    if contains_path_segment(value, ".") {
        return Err(ManifestError::CurrentLibraryComponent { path });
    }

    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::ParentDir => {
                return Err(ManifestError::ParentLibraryComponent { path });
            }
            Component::CurDir => {
                return Err(ManifestError::CurrentLibraryComponent { path });
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ManifestError::RootedOrPrefixedLibrary { path });
            }
        }
    }

    Ok(path)
}

#[cfg(windows)]
fn contains_path_segment(value: &str, expected: &str) -> bool {
    value
        .split(['/', '\\'])
        .any(|component| component == expected)
}

#[cfg(not(windows))]
fn contains_path_segment(value: &str, expected: &str) -> bool {
    value.split('/').any(|component| component == expected)
}

fn parse_sha256(value: &str) -> Result<[u8; 32], ManifestError> {
    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return Err(ManifestError::InvalidSha256Length { found: bytes.len() });
    }

    let high_bytes = bytes.iter().step_by(2);
    let low_bytes = bytes.iter().skip(1).step_by(2);
    let mut digest = [0_u8; 32];
    for (index, (output, (high, low))) in
        digest.iter_mut().zip(high_bytes.zip(low_bytes)).enumerate()
    {
        let high = lowercase_hex_nibble(*high, index * 2)?;
        let low = lowercase_hex_nibble(*low, index * 2 + 1)?;
        *output = (high << 4) | low;
    }
    Ok(digest)
}

fn lowercase_hex_nibble(byte: u8, index: usize) -> Result<u8, ManifestError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(ManifestError::InvalidSha256Byte { index, found: byte }),
    }
}

fn parse_capabilities(names: Vec<String>) -> Result<PluginCapabilities, ManifestError> {
    let mut capabilities = PluginCapabilities::empty();
    for name in names {
        let Some(capability) = PluginCapability::from_name(&name) else {
            return Err(ManifestError::UnknownCapability { name });
        };
        if !capabilities.insert(capability) {
            return Err(ManifestError::DuplicateCapability { capability });
        }
    }
    Ok(capabilities)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ferrumc_plugin_abi::{
        AbiVersion, FC_CAPABILITIES_V1, FC_CAPABILITY_READ_PERMISSIONS, FC_CAPABILITY_READ_WORLD,
        FC_CAPABILITY_RECEIVE_EVENTS, FC_CAPABILITY_REGISTER_COMMANDS, FC_CAPABILITY_STORAGE,
        FC_CAPABILITY_SUBMIT_INTENTS, FC_CAPABILITY_VETO_BLOCK_EDITS, FC_CAPABILITY_VETO_EVENTS,
    };
    use semver::{Version, VersionReq};

    use super::{
        parse_manifest, ManifestError, PluginCapabilities, PluginCapability, PluginManifest,
    };

    const CHECKSUM: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn manifest(library: &str, checksum: &str, capabilities: &str, extra: &str) -> String {
        format!(
            r#"id = "example.plugin"
name = "Example Plugin"
version = "1.2.3-alpha.1+build.7"
abi_major = 1
abi_minor = 0
server_api = ">=0.2.0-dev, <0.3.0"
library = "{library}"
library_sha256 = "{checksum}"
capabilities = {capabilities}
{extra}"#
        )
    }

    fn valid_manifest() -> String {
        manifest(
            "native/linux/libexample.so",
            CHECKSUM,
            r#"["veto-events", "storage", "read-world", "register-commands", "veto-block-edits", "receive-events", "submit-intents", "read-permissions"]"#,
            "",
        )
    }

    fn parse(input: &str) -> Result<PluginManifest, ManifestError> {
        parse_manifest(input.as_bytes())
    }

    #[test]
    fn valid_manifest_round_trips_every_field_and_canonicalizes_capability_order() {
        let parsed = parse(&valid_manifest()).expect("valid manifest");

        assert_eq!(parsed.id(), "example.plugin");
        assert_eq!(parsed.name(), "Example Plugin");
        assert_eq!(
            parsed.version(),
            &Version::parse("1.2.3-alpha.1+build.7").expect("valid expected version")
        );
        assert_eq!(parsed.abi_version(), AbiVersion::new(1, 0));
        assert_eq!(
            parsed.server_api(),
            &VersionReq::parse(">=0.2.0-dev, <0.3.0").expect("valid expected range")
        );
        assert_eq!(parsed.library(), Path::new("native/linux/libexample.so"));

        let expected_digest = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ];
        assert_eq!(parsed.library_sha256(), expected_digest);
        assert_eq!(parsed.capabilities().bits(), FC_CAPABILITIES_V1);
        assert_eq!(parsed.capabilities().len(), 8);
        assert_eq!(
            parsed.capabilities().iter().collect::<Vec<_>>(),
            PluginCapability::ALL
        );
    }

    #[test]
    fn capability_names_and_bits_are_exact_abi_assignments() {
        let expected = [
            (
                PluginCapability::ReadWorld,
                "read-world",
                FC_CAPABILITY_READ_WORLD,
            ),
            (
                PluginCapability::SubmitIntents,
                "submit-intents",
                FC_CAPABILITY_SUBMIT_INTENTS,
            ),
            (
                PluginCapability::RegisterCommands,
                "register-commands",
                FC_CAPABILITY_REGISTER_COMMANDS,
            ),
            (
                PluginCapability::ReceiveEvents,
                "receive-events",
                FC_CAPABILITY_RECEIVE_EVENTS,
            ),
            (
                PluginCapability::ReadPermissions,
                "read-permissions",
                FC_CAPABILITY_READ_PERMISSIONS,
            ),
            (PluginCapability::Storage, "storage", FC_CAPABILITY_STORAGE),
            (
                PluginCapability::VetoBlockEdits,
                "veto-block-edits",
                FC_CAPABILITY_VETO_BLOCK_EDITS,
            ),
            (
                PluginCapability::VetoEvents,
                "veto-events",
                FC_CAPABILITY_VETO_EVENTS,
            ),
        ];

        for (capability, name, bit) in expected {
            assert_eq!(capability.as_str(), name);
            assert_eq!(capability.to_string(), name);
            assert_eq!(capability.bit(), bit);
            assert_eq!(PluginCapability::from_name(name), Some(capability));
        }
    }

    #[test]
    fn empty_capabilities_are_valid_and_have_empty_accessors() {
        let parsed = parse(&manifest("plugin.so", CHECKSUM, "[]", "")).expect("empty set is valid");
        let capabilities = parsed.capabilities();
        assert_eq!(capabilities, PluginCapabilities::empty());
        assert_eq!(capabilities.bits(), 0);
        assert_eq!(capabilities.len(), 0);
        assert!(capabilities.is_empty());
        assert_eq!(capabilities.iter().next(), None);
        assert!(!capabilities.contains(PluginCapability::Storage));
    }

    #[test]
    fn invalid_utf8_is_distinct_from_invalid_toml() {
        assert!(matches!(
            parse_manifest(&[0xff]),
            Err(ManifestError::InvalidUtf8 { .. })
        ));
        assert!(matches!(
            parse("id = ["),
            Err(ManifestError::InvalidToml { .. })
        ));
    }

    #[test]
    fn every_required_field_is_required_and_unknown_fields_are_rejected() {
        let valid = valid_manifest();
        for field in [
            "id",
            "name",
            "version",
            "abi_major",
            "abi_minor",
            "server_api",
            "library",
            "library_sha256",
            "capabilities",
        ] {
            let prefix = format!("{field} =");
            let without_field = valid
                .lines()
                .filter(|line| !line.starts_with(&prefix))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                matches!(
                    parse(&without_field),
                    Err(ManifestError::InvalidToml { .. })
                ),
                "missing field {field} must fail"
            );
        }

        assert!(matches!(
            parse(&manifest("plugin.so", CHECKSUM, "[]", "unexpected = true")),
            Err(ManifestError::InvalidToml { .. })
        ));
    }

    #[test]
    fn empty_identity_and_library_fields_have_specific_errors() {
        assert!(matches!(
            parse(&valid_manifest().replace(r#"id = "example.plugin""#, r#"id = """#)),
            Err(ManifestError::EmptyId)
        ));
        assert!(matches!(
            parse(&valid_manifest().replace(r#"name = "Example Plugin""#, r#"name = """#)),
            Err(ManifestError::EmptyName)
        ));
        assert!(matches!(
            parse(&manifest("", CHECKSUM, "[]", "")),
            Err(ManifestError::EmptyLibrary)
        ));
    }

    #[test]
    fn malformed_semantic_versions_have_specific_errors() {
        assert!(matches!(
            parse(&valid_manifest().replace(
                r#"version = "1.2.3-alpha.1+build.7""#,
                r#"version = "one.two.three""#
            )),
            Err(ManifestError::InvalidVersion { .. })
        ));
        assert!(matches!(
            parse(&valid_manifest().replace(
                r#"server_api = ">=0.2.0-dev, <0.3.0""#,
                r#"server_api = "definitely not a range""#
            )),
            Err(ManifestError::InvalidServerApi { .. })
        ));
    }

    #[test]
    fn abi_components_must_fit_u16() {
        assert!(matches!(
            parse(&valid_manifest().replace("abi_major = 1", "abi_major = 65536")),
            Err(ManifestError::InvalidToml { .. })
        ));
        assert!(matches!(
            parse(&valid_manifest().replace("abi_minor = 0", "abi_minor = -1")),
            Err(ManifestError::InvalidToml { .. })
        ));
    }

    #[test]
    fn checksum_requires_exact_lowercase_hex() {
        assert!(matches!(
            parse(&manifest("plugin.so", &CHECKSUM[..62], "[]", "")),
            Err(ManifestError::InvalidSha256Length { found: 62 })
        ));
        assert!(matches!(
            parse(&manifest(
                "plugin.so",
                &format!("A{}", &CHECKSUM[1..]),
                "[]",
                ""
            )),
            Err(ManifestError::InvalidSha256Byte {
                index: 0,
                found: b'A'
            })
        ));
        assert!(matches!(
            parse(&manifest(
                "plugin.so",
                &format!("{}g", &CHECKSUM[..63]),
                "[]",
                ""
            )),
            Err(ManifestError::InvalidSha256Byte {
                index: 63,
                found: b'g'
            })
        ));
        assert!(matches!(
            parse(&manifest("plugin.so", &"é".repeat(32), "[]", "")),
            Err(ManifestError::InvalidSha256Byte { index: 0, .. })
        ));
    }

    #[test]
    fn capability_names_are_exact_case_sensitive_and_unique() {
        assert!(matches!(
            parse(&manifest(
                "plugin.so",
                CHECKSUM,
                r#"["ReadWorld"]"#,
                ""
            )),
            Err(ManifestError::UnknownCapability { name }) if name == "ReadWorld"
        ));
        assert!(matches!(
            parse(&manifest(
                "plugin.so",
                CHECKSUM,
                r#"["storage", "storage"]"#,
                ""
            )),
            Err(ManifestError::DuplicateCapability {
                capability: PluginCapability::Storage
            })
        ));
    }

    #[test]
    fn library_path_rejects_root_parent_and_current_components() {
        assert!(matches!(
            parse(&manifest("/native/plugin.so", CHECKSUM, "[]", "")),
            Err(ManifestError::RootedOrPrefixedLibrary { .. })
        ));

        for library in ["../plugin.so", "native/../plugin.so"] {
            assert!(
                matches!(
                    parse(&manifest(library, CHECKSUM, "[]", "")),
                    Err(ManifestError::ParentLibraryComponent { .. })
                ),
                "{library} must reject parent traversal"
            );
        }

        for library in [".", "./plugin.so", "native/./plugin.so"] {
            assert!(
                matches!(
                    parse(&manifest(library, CHECKSUM, "[]", "")),
                    Err(ManifestError::CurrentLibraryComponent { .. })
                ),
                "{library} must reject current-directory components"
            );
        }
    }

    #[test]
    fn normal_nested_library_components_are_preserved() {
        let parsed = parse(&manifest(
            "targets/aarch64/libexample.so",
            CHECKSUM,
            "[]",
            "",
        ))
        .expect("normal nested path");
        assert_eq!(parsed.library(), Path::new("targets/aarch64/libexample.so"));
    }

    #[test]
    fn duplicate_toml_keys_and_wrong_field_types_are_rejected() {
        let duplicate = format!("{}\nid = \"second\"", valid_manifest());
        assert!(matches!(
            parse(&duplicate),
            Err(ManifestError::InvalidToml { .. })
        ));

        assert!(matches!(
            parse(&valid_manifest().replace("abi_major = 1", r#"abi_major = "1""#)),
            Err(ManifestError::InvalidToml { .. })
        ));
        assert!(matches!(
            parse(&valid_manifest().replace(
                r#"capabilities = ["veto-events", "storage", "read-world", "register-commands", "veto-block-edits", "receive-events", "submit-intents", "read-permissions"]"#,
                r#"capabilities = "storage""#
            )),
            Err(ManifestError::InvalidToml { .. })
        ));
    }
}
