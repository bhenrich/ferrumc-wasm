use std::io;
use std::path::PathBuf;

use ferrumc_plugin_abi::AbiVersion;
use ferrumc_plugin_abi_sys::{AbiRecord, LoadError as BoundaryLoadError, PluginSemanticVersion};
use semver::{Version, VersionReq};
use thiserror::Error;

use crate::manifest::{ManifestError, PluginCapabilities};

/// Building a loader policy from `FerrumC`'s compile-time metadata failed.
#[derive(Debug, Error)]
pub enum LoaderConfigError {
    /// The workspace package version is not a semantic version.
    #[error("compiled server API version `{value}` is invalid: {source}")]
    InvalidServerApi {
        /// The rejected compile-time package version.
        value: &'static str,
        /// The semantic-version parser failure.
        source: semver::Error,
    },
    /// A loader policy was given an empty target triple.
    #[error("plugin loader target triple cannot be empty")]
    EmptyTarget,
}

/// Discovery or validation of a trusted native plugin failed.
#[derive(Debug, Error)]
pub enum PluginLoadError {
    /// The configured plugins directory could not be read.
    #[error("failed to read plugin directory {}: {source}", root.display())]
    ReadDirectory {
        /// The configured plugins directory.
        root: PathBuf,
        /// The filesystem failure.
        source: io::Error,
    },
    /// One entry in the configured plugins directory could not be read.
    #[error("failed to read an entry in plugin directory {}: {source}", root.display())]
    ReadDirectoryEntry {
        /// The configured plugins directory.
        root: PathBuf,
        /// The filesystem failure.
        source: io::Error,
    },
    /// The immediate-child scan exceeded its fixed entry ceiling.
    #[error(
        "plugin directory {} contains more than the {maximum} permitted immediate entries",
        root.display()
    )]
    TooManyDirectoryEntries {
        /// The configured plugins directory.
        root: PathBuf,
        /// The fixed maximum number of scanned entries.
        maximum: usize,
    },
    /// Discovery found more manifests than the fixed plugin ceiling.
    #[error(
        "plugin directory {} contains more than the {maximum} permitted manifests",
        root.display()
    )]
    TooManyPlugins {
        /// The configured plugins directory.
        root: PathBuf,
        /// The fixed maximum number of manifests.
        maximum: usize,
    },
    /// Metadata for a possible immediate-child manifest could not be read.
    #[error("failed to inspect plugin manifest {}: {source}", path.display())]
    InspectManifest {
        /// The possible manifest path.
        path: PathBuf,
        /// The filesystem failure.
        source: io::Error,
    },
    /// A manifest could not be read.
    #[error("failed to read plugin manifest {}: {source}", path.display())]
    ReadManifest {
        /// The manifest path.
        path: PathBuf,
        /// The filesystem failure.
        source: io::Error,
    },
    /// A manifest exceeded the fixed input ceiling.
    #[error(
        "plugin manifest {} exceeds the {maximum}-byte limit",
        path.display()
    )]
    ManifestTooLarge {
        /// The manifest path.
        path: PathBuf,
        /// The fixed maximum accepted length.
        maximum: usize,
    },
    /// A bounded manifest was malformed.
    #[error("plugin manifest {} is invalid: {source}", path.display())]
    InvalidManifest {
        /// The manifest path.
        path: PathBuf,
        /// The classified manifest failure.
        source: ManifestError,
    },
    /// Two manifests declared the same plugin identifier.
    #[error(
        "duplicate plugin id `{id}` in {} and {}",
        first.display(),
        duplicate.display()
    )]
    DuplicatePluginId {
        /// The duplicated plugin identifier.
        id: String,
        /// The first manifest in deterministic path order.
        first: PathBuf,
        /// The duplicate manifest.
        duplicate: PathBuf,
    },
    /// A manifest or loaded record declares the wrong ABI major.
    #[error("plugin `{id}` declares ABI {plugin}, but this host requires major {}", host.major())]
    WrongAbiMajor {
        /// The plugin identifier.
        id: String,
        /// The host ABI version.
        host: AbiVersion,
        /// The rejected plugin ABI version.
        plugin: AbiVersion,
    },
    /// A manifest or loaded record requires a newer ABI minor.
    #[error("plugin `{id}` declares ABI {plugin}, newer than host ABI {host}")]
    AbiMinorTooNew {
        /// The plugin identifier.
        id: String,
        /// The host ABI version.
        host: AbiVersion,
        /// The rejected plugin ABI version.
        plugin: AbiVersion,
    },
    /// The running server API is outside a manifest's declared range.
    #[error("plugin `{id}` requires server API `{requirement}`, but this server is {server}")]
    UnsupportedServerApi {
        /// The plugin identifier.
        id: String,
        /// The running server API version.
        server: Version,
        /// The rejected requirement.
        requirement: VersionReq,
    },
    /// The manifest version's numeric core cannot fit ABI v1.
    #[error("plugin `{id}` version {version} has a component larger than ABI v1 can represent")]
    ManifestVersionOutOfRange {
        /// The plugin identifier.
        id: String,
        /// The rejected manifest version.
        version: Version,
    },
    /// A requested capability is unavailable from this loader policy.
    #[error("plugin `{id}` requests unavailable capability bits 0x{missing:016x}")]
    UnavailableCapabilities {
        /// The plugin identifier.
        id: String,
        /// The requested capability set.
        requested: PluginCapabilities,
        /// The host-available capability set.
        available: PluginCapabilities,
        /// Bits requested but unavailable.
        missing: u64,
    },
    /// A library path could not be resolved to a regular file.
    #[error("plugin `{id}` library {} is not a regular file", path.display())]
    LibraryNotRegular {
        /// The plugin identifier.
        id: String,
        /// The rejected library path.
        path: PathBuf,
    },
    /// A bundle or library path could not be canonicalized.
    #[error("failed to resolve plugin `{id}` library {}: {source}", path.display())]
    ResolveLibrary {
        /// The plugin identifier.
        id: String,
        /// The unresolved path.
        path: PathBuf,
        /// The filesystem failure.
        source: io::Error,
    },
    /// A canonical library path escaped its manifest bundle.
    #[error(
        "plugin `{id}` library {} resolves outside bundle {}",
        library.display(),
        bundle.display()
    )]
    LibraryEscapesBundle {
        /// The plugin identifier.
        id: String,
        /// The canonical bundle path.
        bundle: PathBuf,
        /// The canonical library path.
        library: PathBuf,
    },
    /// The library file could not be read while hashing.
    #[error("failed to hash plugin `{id}` library {}: {source}", path.display())]
    ReadLibrary {
        /// The plugin identifier.
        id: String,
        /// The library path.
        path: PathBuf,
        /// The filesystem failure.
        source: io::Error,
    },
    /// The library bytes do not match the manifest checksum.
    #[error(
        "plugin `{id}` library {} has SHA-256 {actual}, expected {expected}",
        path.display()
    )]
    LibraryHashMismatch {
        /// The plugin identifier.
        id: String,
        /// The library path.
        path: PathBuf,
        /// The canonical expected digest.
        expected: String,
        /// The canonical observed digest.
        actual: String,
    },
    /// The library bytes changed while the native boundary was opening it.
    #[error(
        "plugin `{id}` library {} changed during native loading: before {before}, after {after}",
        path.display()
    )]
    LibraryChangedDuringLoad {
        /// The plugin identifier.
        id: String,
        /// The library path.
        path: PathBuf,
        /// Digest observed immediately before native loading.
        before: String,
        /// Digest observed immediately after native loading.
        after: String,
    },
    /// A raw ABI record is shorter than its required prefix.
    #[error("plugin `{id}` {record} declares {declared} bytes, but {required} are required")]
    ShortAbiRecord {
        /// The plugin identifier.
        id: String,
        /// The rejected record.
        record: AbiRecord,
        /// The declared byte length.
        declared: u32,
        /// The required byte length.
        required: u32,
    },
    /// A required bootstrap result or function slot was null.
    #[error("plugin `{id}` {record} has a null required `{slot}` pointer")]
    NullRequiredPointer {
        /// The plugin identifier.
        id: String,
        /// The rejected record.
        record: AbiRecord,
        /// The stable pointer or slot name.
        slot: &'static str,
    },
    /// The native boundary rejected the library for another classified reason.
    #[error("plugin `{id}` library {} failed native validation: {source}", path.display())]
    NativeBoundary {
        /// The plugin identifier.
        id: String,
        /// The library path.
        path: PathBuf,
        /// The underlying boundary failure.
        source: BoundaryLoadError,
    },
    /// String metadata disagrees between manifest and loaded descriptor.
    #[error("plugin `{id}` manifest {field} `{manifest}` does not match descriptor `{binary}`")]
    ManifestMetadataMismatch {
        /// The plugin identifier used for diagnostics.
        id: String,
        /// The mismatched field.
        field: &'static str,
        /// The manifest value.
        manifest: String,
        /// The loaded descriptor value.
        binary: String,
    },
    /// The manifest and descriptor semantic-version cores disagree.
    #[error(
        "plugin `{id}` manifest version {manifest} does not match descriptor {}.{}.{}",
        binary.major(),
        binary.minor(),
        binary.patch()
    )]
    ManifestVersionMismatch {
        /// The plugin identifier.
        id: String,
        /// The manifest semantic version.
        manifest: Version,
        /// The loaded numeric semantic-version core.
        binary: PluginSemanticVersion,
    },
    /// The manifest and descriptor ABI versions disagree.
    #[error("plugin `{id}` manifest ABI {manifest} does not match descriptor ABI {binary}")]
    ManifestAbiMismatch {
        /// The plugin identifier.
        id: String,
        /// The manifest ABI version.
        manifest: AbiVersion,
        /// The loaded descriptor ABI version.
        binary: AbiVersion,
    },
    /// The descriptor requested capability bits unknown to ABI v1.
    #[error("plugin `{id}` descriptor requests unknown capability bits 0x{unknown:016x}")]
    UnsupportedCapabilityBits {
        /// The plugin identifier.
        id: String,
        /// The complete descriptor bit mask.
        requested: u64,
        /// Bits not assigned by ABI v1.
        unknown: u64,
    },
    /// The manifest and descriptor capability requests disagree.
    #[error(
        "plugin `{id}` manifest capability bits 0x{manifest:016x} do not match descriptor bits 0x{binary:016x}"
    )]
    CapabilityMismatch {
        /// The plugin identifier.
        id: String,
        /// The manifest capability bits.
        manifest: u64,
        /// The loaded descriptor capability bits.
        binary: u64,
    },
    /// The descriptor target differs from the exact host target.
    #[error("plugin `{id}` targets `{plugin}`, but this host target is `{host}`")]
    WrongTarget {
        /// The plugin identifier.
        id: String,
        /// The exact host target triple.
        host: String,
        /// The loaded descriptor target triple.
        plugin: String,
    },
}
