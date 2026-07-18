use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use ferrumc_plugin_abi::{AbiVersion, AbiVersionError, CURRENT_ABI, FC_CAPABILITIES_V1};
use ferrumc_plugin_abi_sys::{
    load as load_boundary, LoadError as BoundaryLoadError, LoadedAbiPlugin, OwnedPluginMetadata,
    PluginSemanticVersion, ValidationError,
};
use semver::Version;

use crate::error::{LoaderConfigError, PluginLoadError};
use crate::lifecycle::{LoadedPlugin, LoadedPlugins};
use crate::manifest::{parse_manifest, PluginCapabilities, PluginManifest};
use crate::sha256::digest_file;

/// Largest accepted `plugin.toml` byte length.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// Maximum number of plugin manifests accepted from one directory.
pub const MAX_PLUGINS: usize = 256;

/// Maximum immediate directory entries examined during one scan.
pub const MAX_DIRECTORY_ENTRIES: usize = 4096;

/// Exact target triple of this `FerrumC` build.
pub const HOST_TARGET: &str = env!("FERRUMC_HOST_TARGET");

/// Semantic version used as this build's server API version.
pub const SERVER_API_VERSION: &str = env!("CARGO_PKG_VERSION");

const MANIFEST_READ_LIMIT: u64 = (MAX_MANIFEST_BYTES as u64) + 1;

/// Fixed compatibility and capability policy for plugin loading.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoaderConfig {
    server_api: Version,
    target: String,
    available_capabilities: PluginCapabilities,
}

impl LoaderConfig {
    fn new(
        server_api: Version,
        target: impl Into<String>,
        available_capabilities: PluginCapabilities,
    ) -> Result<Self, LoaderConfigError> {
        let target = target.into();
        if target.is_empty() {
            return Err(LoaderConfigError::EmptyTarget);
        }
        Ok(Self {
            server_api,
            target,
            available_capabilities,
        })
    }

    /// Creates a policy for this compiled `FerrumC` target and package version.
    pub fn current(available_capabilities: PluginCapabilities) -> Result<Self, LoaderConfigError> {
        let server_api = Version::parse(SERVER_API_VERSION).map_err(|source| {
            LoaderConfigError::InvalidServerApi {
                value: SERVER_API_VERSION,
                source,
            }
        })?;
        Self::new(server_api, HOST_TARGET, available_capabilities)
    }

    /// Returns the running server API version.
    pub const fn server_api(&self) -> &Version {
        &self.server_api
    }

    /// Returns the exact accepted plugin target triple.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Returns the capabilities this host can provide.
    pub const fn available_capabilities(&self) -> PluginCapabilities {
        self.available_capabilities
    }
}

/// Deterministic safe loader for trusted native plugins.
pub struct PluginLoader {
    config: LoaderConfig,
}

impl PluginLoader {
    /// Creates a loader with a validated immutable policy.
    pub const fn new(config: LoaderConfig) -> Self {
        Self { config }
    }

    /// Creates a loader for this build's target and server API.
    pub fn current(available_capabilities: PluginCapabilities) -> Result<Self, LoaderConfigError> {
        Ok(Self::new(LoaderConfig::current(available_capabilities)?))
    }

    /// Returns this loader's immutable policy.
    pub const fn config(&self) -> &LoaderConfig {
        &self.config
    }

    /// Discovers and validates every immediate-child plugin bundle.
    ///
    /// No plugin initialization callback runs. Successful libraries remain
    /// resident until process exit even if this returned set is dropped.
    pub fn load_directory(&self, root: &Path) -> Result<LoadedPlugins, PluginLoadError> {
        let validated = self.load_directory_with(root, load_boundary)?;
        Ok(LoadedPlugins::new(
            validated
                .into_iter()
                .map(|plugin| LoadedPlugin::new(plugin.manifest, plugin.boundary))
                .collect(),
        ))
    }

    fn load_directory_with<P, F>(
        &self,
        root: &Path,
        mut native_load: F,
    ) -> Result<Vec<ValidatedPlugin<P>>, PluginLoadError>
    where
        P: BoundaryPlugin,
        F: FnMut(&Path) -> Result<P, BoundaryLoadError>,
    {
        let candidates = discover_candidates(root)?;
        let mut loaded = Vec::with_capacity(candidates.len());

        for candidate in candidates {
            validate_manifest_policy(&self.config, &candidate.manifest)?;
            let library = resolve_library(&candidate)?;
            let before = digest_file(&library).map_err(|source| PluginLoadError::ReadLibrary {
                id: candidate.manifest.id().to_owned(),
                path: library.clone(),
                source,
            })?;
            let expected = candidate.manifest.library_sha256();
            if before != expected {
                return Err(PluginLoadError::LibraryHashMismatch {
                    id: candidate.manifest.id().to_owned(),
                    path: library,
                    expected: hex_digest(expected),
                    actual: hex_digest(before),
                });
            }

            let boundary = native_load(&library)
                .map_err(|source| map_boundary_error(&candidate.manifest, &library, source))?;

            let after = digest_file(&library).map_err(|source| PluginLoadError::ReadLibrary {
                id: candidate.manifest.id().to_owned(),
                path: library.clone(),
                source,
            })?;
            if after != before {
                return Err(PluginLoadError::LibraryChangedDuringLoad {
                    id: candidate.manifest.id().to_owned(),
                    path: library,
                    before: hex_digest(before),
                    after: hex_digest(after),
                });
            }

            validate_boundary_metadata(&self.config, &candidate.manifest, boundary.metadata())?;
            loaded.push(ValidatedPlugin {
                manifest: candidate.manifest,
                boundary,
            });
        }

        Ok(loaded)
    }
}

struct Candidate {
    bundle: PathBuf,
    manifest_path: PathBuf,
    manifest: PluginManifest,
}

fn discover_candidates(root: &Path) -> Result<Vec<Candidate>, PluginLoadError> {
    let entries = fs::read_dir(root).map_err(|source| PluginLoadError::ReadDirectory {
        root: root.to_path_buf(),
        source,
    })?;
    let mut children = Vec::new();

    for (index, entry) in entries.enumerate() {
        if index >= MAX_DIRECTORY_ENTRIES {
            return Err(PluginLoadError::TooManyDirectoryEntries {
                root: root.to_path_buf(),
                maximum: MAX_DIRECTORY_ENTRIES,
            });
        }
        let entry = entry.map_err(|source| PluginLoadError::ReadDirectoryEntry {
            root: root.to_path_buf(),
            source,
        })?;
        let file_type =
            entry
                .file_type()
                .map_err(|source| PluginLoadError::ReadDirectoryEntry {
                    root: root.to_path_buf(),
                    source,
                })?;
        if file_type.is_dir() {
            children.push(entry.path());
        }
    }
    children.sort();

    let mut candidates = Vec::new();
    for bundle in children {
        let manifest_path = bundle.join("plugin.toml");
        let metadata = match fs::symlink_metadata(&manifest_path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(PluginLoadError::InspectManifest {
                    path: manifest_path,
                    source,
                });
            }
        };
        if !metadata.file_type().is_file() {
            return Err(PluginLoadError::InspectManifest {
                path: manifest_path,
                source: io::Error::new(
                    io::ErrorKind::InvalidData,
                    "plugin.toml is not a regular file",
                ),
            });
        }
        if candidates.len() >= MAX_PLUGINS {
            return Err(PluginLoadError::TooManyPlugins {
                root: root.to_path_buf(),
                maximum: MAX_PLUGINS,
            });
        }

        let bytes = read_manifest(&manifest_path)?;
        let manifest =
            parse_manifest(&bytes).map_err(|source| PluginLoadError::InvalidManifest {
                path: manifest_path.clone(),
                source,
            })?;
        candidates.push(Candidate {
            bundle,
            manifest_path,
            manifest,
        });
    }

    candidates.sort_by(|left, right| {
        left.manifest
            .id()
            .cmp(right.manifest.id())
            .then_with(|| left.manifest_path.cmp(&right.manifest_path))
    });
    for pair in candidates.windows(2) {
        let [first, duplicate] = pair else {
            continue;
        };
        if first.manifest.id() == duplicate.manifest.id() {
            return Err(PluginLoadError::DuplicatePluginId {
                id: first.manifest.id().to_owned(),
                first: first.manifest_path.clone(),
                duplicate: duplicate.manifest_path.clone(),
            });
        }
    }

    Ok(candidates)
}

fn read_manifest(path: &Path) -> Result<Vec<u8>, PluginLoadError> {
    let file = File::open(path).map_err(|source| PluginLoadError::ReadManifest {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::with_capacity(MAX_MANIFEST_BYTES + 1);
    file.take(MANIFEST_READ_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(|source| PluginLoadError::ReadManifest {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(PluginLoadError::ManifestTooLarge {
            path: path.to_path_buf(),
            maximum: MAX_MANIFEST_BYTES,
        });
    }
    Ok(bytes)
}

fn validate_manifest_policy(
    config: &LoaderConfig,
    manifest: &PluginManifest,
) -> Result<(), PluginLoadError> {
    let plugin = manifest.abi_version();
    if plugin.major() != CURRENT_ABI.major() {
        return Err(PluginLoadError::WrongAbiMajor {
            id: manifest.id().to_owned(),
            host: CURRENT_ABI,
            plugin,
        });
    }
    if plugin.minor() > CURRENT_ABI.minor() {
        return Err(PluginLoadError::AbiMinorTooNew {
            id: manifest.id().to_owned(),
            host: CURRENT_ABI,
            plugin,
        });
    }
    if !manifest.server_api().matches(config.server_api()) {
        return Err(PluginLoadError::UnsupportedServerApi {
            id: manifest.id().to_owned(),
            server: config.server_api().clone(),
            requirement: manifest.server_api().clone(),
        });
    }
    if manifest.version().major > u64::from(u32::MAX)
        || manifest.version().minor > u64::from(u32::MAX)
        || manifest.version().patch > u64::from(u32::MAX)
    {
        return Err(PluginLoadError::ManifestVersionOutOfRange {
            id: manifest.id().to_owned(),
            version: manifest.version().clone(),
        });
    }

    let requested = manifest.capabilities();
    let available = config.available_capabilities();
    let missing = requested.bits() & !available.bits();
    if missing != 0 {
        return Err(PluginLoadError::UnavailableCapabilities {
            id: manifest.id().to_owned(),
            requested,
            available,
            missing,
        });
    }
    Ok(())
}

fn resolve_library(candidate: &Candidate) -> Result<PathBuf, PluginLoadError> {
    let id = candidate.manifest.id();
    let unresolved = candidate.bundle.join(candidate.manifest.library());
    let bundle =
        fs::canonicalize(&candidate.bundle).map_err(|source| PluginLoadError::ResolveLibrary {
            id: id.to_owned(),
            path: candidate.bundle.clone(),
            source,
        })?;
    let library =
        fs::canonicalize(&unresolved).map_err(|source| PluginLoadError::ResolveLibrary {
            id: id.to_owned(),
            path: unresolved,
            source,
        })?;
    if !library.starts_with(&bundle) {
        return Err(PluginLoadError::LibraryEscapesBundle {
            id: id.to_owned(),
            bundle,
            library,
        });
    }
    let metadata =
        fs::symlink_metadata(&library).map_err(|source| PluginLoadError::ResolveLibrary {
            id: id.to_owned(),
            path: library.clone(),
            source,
        })?;
    if !metadata.file_type().is_file() {
        return Err(PluginLoadError::LibraryNotRegular {
            id: id.to_owned(),
            path: library,
        });
    }
    Ok(library)
}

#[derive(Debug)]
struct ValidatedPlugin<P> {
    manifest: PluginManifest,
    boundary: P,
}

trait BoundaryPlugin {
    fn metadata(&self) -> BoundaryMetadata<'_>;
}

impl BoundaryPlugin for LoadedAbiPlugin {
    fn metadata(&self) -> BoundaryMetadata<'_> {
        BoundaryMetadata::from_owned(LoadedAbiPlugin::metadata(self))
    }
}

#[derive(Clone, Copy)]
struct BoundaryMetadata<'metadata> {
    abi_version: AbiVersion,
    version: PluginSemanticVersion,
    requested_capabilities: u64,
    id: &'metadata str,
    name: &'metadata str,
    target: &'metadata str,
}

impl<'metadata> BoundaryMetadata<'metadata> {
    fn from_owned(metadata: &'metadata OwnedPluginMetadata) -> Self {
        Self {
            abi_version: metadata.abi_version(),
            version: metadata.version(),
            requested_capabilities: metadata.requested_capabilities(),
            id: metadata.id(),
            name: metadata.name(),
            target: metadata.target(),
        }
    }
}

fn validate_boundary_metadata(
    config: &LoaderConfig,
    manifest: &PluginManifest,
    metadata: BoundaryMetadata<'_>,
) -> Result<(), PluginLoadError> {
    let id = manifest.id();
    if metadata.abi_version != manifest.abi_version() {
        return Err(PluginLoadError::ManifestAbiMismatch {
            id: id.to_owned(),
            manifest: manifest.abi_version(),
            binary: metadata.abi_version,
        });
    }
    if metadata.id != id {
        return Err(PluginLoadError::ManifestMetadataMismatch {
            id: id.to_owned(),
            field: "id",
            manifest: id.to_owned(),
            binary: metadata.id.to_owned(),
        });
    }
    if metadata.name != manifest.name() {
        return Err(PluginLoadError::ManifestMetadataMismatch {
            id: id.to_owned(),
            field: "name",
            manifest: manifest.name().to_owned(),
            binary: metadata.name.to_owned(),
        });
    }
    if u64::from(metadata.version.major()) != manifest.version().major
        || u64::from(metadata.version.minor()) != manifest.version().minor
        || u64::from(metadata.version.patch()) != manifest.version().patch
    {
        return Err(PluginLoadError::ManifestVersionMismatch {
            id: id.to_owned(),
            manifest: manifest.version().clone(),
            binary: metadata.version,
        });
    }

    let unknown = metadata.requested_capabilities & !FC_CAPABILITIES_V1;
    if unknown != 0 {
        return Err(PluginLoadError::UnsupportedCapabilityBits {
            id: id.to_owned(),
            requested: metadata.requested_capabilities,
            unknown,
        });
    }
    if metadata.requested_capabilities != manifest.capabilities().bits() {
        return Err(PluginLoadError::CapabilityMismatch {
            id: id.to_owned(),
            manifest: manifest.capabilities().bits(),
            binary: metadata.requested_capabilities,
        });
    }
    if metadata.target != config.target() {
        return Err(PluginLoadError::WrongTarget {
            id: id.to_owned(),
            host: config.target().to_owned(),
            plugin: metadata.target.to_owned(),
        });
    }
    Ok(())
}

fn map_boundary_error(
    manifest: &PluginManifest,
    path: &Path,
    source: BoundaryLoadError,
) -> PluginLoadError {
    let id = manifest.id().to_owned();
    match source.validation_error() {
        Some(ValidationError::RecordTooShort {
            record,
            declared,
            required,
        }) => PluginLoadError::ShortAbiRecord {
            id,
            record: *record,
            declared: *declared,
            required: *required,
        },
        Some(ValidationError::NullDescriptor) => PluginLoadError::NullRequiredPointer {
            id,
            record: ferrumc_plugin_abi_sys::AbiRecord::Descriptor,
            slot: "bootstrap result",
        },
        Some(ValidationError::NullRequiredSlot { record, slot }) => {
            PluginLoadError::NullRequiredPointer {
                id,
                record: *record,
                slot,
            }
        }
        Some(ValidationError::NullFunctionTable) => PluginLoadError::NullRequiredPointer {
            id,
            record: ferrumc_plugin_abi_sys::AbiRecord::Descriptor,
            slot: "functions result",
        },
        Some(ValidationError::IncompatibleAbi {
            source: AbiVersionError::MajorMismatch { host, plugin },
            ..
        }) => PluginLoadError::WrongAbiMajor {
            id,
            host: *host,
            plugin: *plugin,
        },
        Some(ValidationError::IncompatibleAbi {
            source: AbiVersionError::MinorTooNew { host, plugin },
            ..
        }) => PluginLoadError::AbiMinorTooNew {
            id,
            host: *host,
            plugin: *plugin,
        },
        _ => PluginLoadError::NativeBoundary {
            id,
            path: path.to_path_buf(),
            source,
        },
    }
}

fn hex_digest(digest: [u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use ferrumc_plugin_abi::{AbiVersion, AbiVersionError, CURRENT_ABI, FC_CAPABILITY_READ_WORLD};
    use ferrumc_plugin_abi_sys::{
        AbiRecord, LoadError as BoundaryLoadError, PluginSemanticVersion, ValidationError,
    };
    use semver::Version;

    use super::{
        digest_file, hex_digest, read_manifest, BoundaryMetadata, BoundaryPlugin, LoaderConfig,
        PluginLoader, MAX_MANIFEST_BYTES,
    };
    use crate::{
        PluginCapabilities, PluginCapability, PluginLoadError, HOST_TARGET, SERVER_API_VERSION,
    };

    struct ScratchDir {
        path: PathBuf,
    }

    impl ScratchDir {
        fn new(name: &str) -> Self {
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
            let repository = manifest
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .expect("loader crate is nested under the repository server workspace");
            let path = repository
                .join(".codex-tmp")
                .join(format!("plugin-loader-{}-{name}", std::process::id()));
            match fs::remove_dir_all(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove stale loader scratch {}: {error}", path.display()),
            }
            fs::create_dir_all(&path).expect("create loader scratch directory");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _result = fs::remove_dir_all(&self.path);
        }
    }

    #[derive(Clone, Debug)]
    struct FakeBoundary {
        abi_version: AbiVersion,
        version: PluginSemanticVersion,
        requested_capabilities: u64,
        id: String,
        name: String,
        target: String,
    }

    impl BoundaryPlugin for FakeBoundary {
        fn metadata(&self) -> BoundaryMetadata<'_> {
            BoundaryMetadata {
                abi_version: self.abi_version,
                version: self.version,
                requested_capabilities: self.requested_capabilities,
                id: &self.id,
                name: &self.name,
                target: &self.target,
            }
        }
    }

    fn test_config() -> LoaderConfig {
        LoaderConfig::new(
            Version::parse("0.2.0-dev").expect("test server version"),
            "test-target",
            PluginCapabilities::all(),
        )
        .expect("valid test policy")
    }

    fn manifest_text(
        id: &str,
        name: &str,
        abi: AbiVersion,
        server_api: &str,
        hash: &str,
        capabilities: &[&str],
    ) -> String {
        let capabilities = capabilities
            .iter()
            .map(|capability| format!("\"{capability}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "id = \"{id}\"\n\
             name = \"{name}\"\n\
             version = \"1.2.3\"\n\
             abi_major = {}\n\
             abi_minor = {}\n\
             server_api = \"{server_api}\"\n\
             library = \"plugin.bin\"\n\
             library_sha256 = \"{hash}\"\n\
             capabilities = [{capabilities}]\n",
            abi.major(),
            abi.minor()
        )
    }

    #[derive(Clone, Copy)]
    struct BundleSpec<'spec> {
        directory: &'spec str,
        id: &'spec str,
        name: &'spec str,
        abi: AbiVersion,
        server_api: &'spec str,
        capabilities: &'spec [&'spec str],
        bytes: &'spec [u8],
    }

    fn write_bundle(root: &Path, spec: BundleSpec<'_>) -> (PathBuf, FakeBoundary) {
        let bundle = root.join(spec.directory);
        fs::create_dir_all(&bundle).expect("create test plugin bundle");
        let library = bundle.join("plugin.bin");
        fs::write(&library, spec.bytes).expect("write synthetic library bytes");
        let hash = hex_digest(digest_file(&library).expect("hash synthetic library"));
        fs::write(
            bundle.join("plugin.toml"),
            manifest_text(
                spec.id,
                spec.name,
                spec.abi,
                spec.server_api,
                &hash,
                spec.capabilities,
            ),
        )
        .expect("write plugin manifest");
        (
            fs::canonicalize(library).expect("canonical synthetic library"),
            FakeBoundary {
                abi_version: spec.abi,
                version: PluginSemanticVersion::new(1, 2, 3),
                requested_capabilities: if spec.capabilities.contains(&"read-world") {
                    FC_CAPABILITY_READ_WORLD
                } else {
                    0
                },
                id: spec.id.to_owned(),
                name: spec.name.to_owned(),
                target: "test-target".to_owned(),
            },
        )
    }

    #[test]
    fn valid_bundles_load_in_plugin_id_order_without_initialization() {
        let scratch = ScratchDir::new("ordered-valid");
        let (beta_path, beta) = write_bundle(
            scratch.path(),
            BundleSpec {
                directory: "a-directory",
                id: "beta",
                name: "Beta",
                abi: CURRENT_ABI,
                server_api: "=0.2.0-dev",
                capabilities: &["read-world"],
                bytes: b"beta-library",
            },
        );
        let (alpha_path, alpha) = write_bundle(
            scratch.path(),
            BundleSpec {
                directory: "z-directory",
                id: "alpha",
                name: "Alpha",
                abi: CURRENT_ABI,
                server_api: "=0.2.0-dev",
                capabilities: &[],
                bytes: b"alpha-library",
            },
        );
        let boundaries = BTreeMap::from([(alpha_path, alpha), (beta_path, beta)]);
        let load_count = Cell::new(0);

        let loaded = PluginLoader::new(test_config())
            .load_directory_with(scratch.path(), |path| {
                load_count.set(load_count.get() + 1);
                Ok(boundaries
                    .get(path)
                    .cloned()
                    .expect("every valid path has a synthetic boundary"))
            })
            .expect("valid synthetic bundles load");

        assert_eq!(load_count.get(), 2);
        assert_eq!(
            loaded
                .iter()
                .map(|plugin| plugin.manifest.id())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
    }

    #[test]
    fn duplicate_ids_are_rejected_before_any_native_load() {
        let scratch = ScratchDir::new("duplicates");
        write_bundle(
            scratch.path(),
            BundleSpec {
                directory: "first",
                id: "same",
                name: "First",
                abi: CURRENT_ABI,
                server_api: "=0.2.0-dev",
                capabilities: &[],
                bytes: b"first",
            },
        );
        write_bundle(
            scratch.path(),
            BundleSpec {
                directory: "second",
                id: "same",
                name: "Second",
                abi: CURRENT_ABI,
                server_api: "=0.2.0-dev",
                capabilities: &[],
                bytes: b"second",
            },
        );
        let calls = Cell::new(0);

        let error = PluginLoader::new(test_config())
            .load_directory_with(
                scratch.path(),
                |_path| -> Result<FakeBoundary, BoundaryLoadError> {
                    calls.set(calls.get() + 1);
                    unreachable!("duplicates must fail before native loading")
                },
            )
            .expect_err("duplicate id must fail");
        assert!(matches!(error, PluginLoadError::DuplicatePluginId { .. }));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn manifest_policy_and_hash_failures_precede_native_loading() {
        let cases = [
            (
                "wrong-major",
                AbiVersion::new(CURRENT_ABI.major() + 1, 0),
                "=0.2.0-dev",
                false,
            ),
            (
                "future-minor",
                AbiVersion::new(CURRENT_ABI.major(), CURRENT_ABI.minor() + 1),
                "=0.2.0-dev",
                false,
            ),
            ("server-api", CURRENT_ABI, ">=9.0.0", false),
            ("hash", CURRENT_ABI, "=0.2.0-dev", true),
        ];

        for (name, abi, server_api, corrupt_hash) in cases {
            let scratch = ScratchDir::new(name);
            write_bundle(
                scratch.path(),
                BundleSpec {
                    directory: "bundle",
                    id: name,
                    name: "Policy",
                    abi,
                    server_api,
                    capabilities: &[],
                    bytes: b"policy",
                },
            );
            if corrupt_hash {
                let manifest = scratch.path().join("bundle/plugin.toml");
                let text = fs::read_to_string(&manifest).expect("read manifest to corrupt hash");
                fs::write(
                    manifest,
                    text.replace(
                        "library_sha256 = \"",
                        "library_sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n# ",
                    ),
                )
                .expect("write mismatched hash");
            }
            let calls = Cell::new(0);
            let error = PluginLoader::new(test_config())
                .load_directory_with(
                    scratch.path(),
                    |_path| -> Result<FakeBoundary, BoundaryLoadError> {
                        calls.set(calls.get() + 1);
                        unreachable!("static rejection must precede native loading")
                    },
                )
                .expect_err("case must be rejected");
            match name {
                "wrong-major" => {
                    assert!(matches!(error, PluginLoadError::WrongAbiMajor { .. }));
                }
                "future-minor" => {
                    assert!(matches!(error, PluginLoadError::AbiMinorTooNew { .. }));
                }
                "server-api" => {
                    assert!(matches!(
                        error,
                        PluginLoadError::UnsupportedServerApi { .. }
                    ));
                }
                "hash" => {
                    assert!(matches!(error, PluginLoadError::LibraryHashMismatch { .. }));
                }
                _ => unreachable!("all cases are named above"),
            }
            assert_eq!(calls.get(), 0);
        }
    }

    #[test]
    fn raw_boundary_failures_keep_distinct_loader_errors() {
        let cases = ["wrong-major", "short", "null"];
        for case in cases {
            let scratch = ScratchDir::new(&format!("raw-{case}"));
            let (library, _) = write_bundle(
                scratch.path(),
                BundleSpec {
                    directory: "bundle",
                    id: case,
                    name: "Raw",
                    abi: CURRENT_ABI,
                    server_api: "=0.2.0-dev",
                    capabilities: &[],
                    bytes: b"raw",
                },
            );
            let error = PluginLoader::new(test_config())
                .load_directory_with(
                    scratch.path(),
                    |path| -> Result<FakeBoundary, BoundaryLoadError> {
                        let source = match case {
                            "wrong-major" => ValidationError::IncompatibleAbi {
                                record: AbiRecord::Descriptor,
                                source: AbiVersionError::MajorMismatch {
                                    host: CURRENT_ABI,
                                    plugin: AbiVersion::new(CURRENT_ABI.major() + 1, 0),
                                },
                            },
                            "short" => ValidationError::RecordTooShort {
                                record: AbiRecord::FunctionTable,
                                declared: 8,
                                required: 32,
                            },
                            "null" => ValidationError::NullRequiredSlot {
                                record: AbiRecord::Descriptor,
                                slot: "functions",
                            },
                            _ => unreachable!("all cases are named above"),
                        };
                        Err(BoundaryLoadError::Validation {
                            path: path.to_path_buf(),
                            source,
                        })
                    },
                )
                .expect_err("raw boundary case must fail");
            match case {
                "wrong-major" => {
                    assert!(matches!(error, PluginLoadError::WrongAbiMajor { .. }));
                }
                "short" => {
                    assert!(matches!(
                        error,
                        PluginLoadError::ShortAbiRecord {
                            record: AbiRecord::FunctionTable,
                            declared: 8,
                            required: 32,
                            ..
                        }
                    ));
                }
                "null" => {
                    assert!(matches!(
                        error,
                        PluginLoadError::NullRequiredPointer {
                            record: AbiRecord::Descriptor,
                            slot: "functions",
                            ..
                        }
                    ));
                }
                _ => unreachable!("all cases are named above"),
            }
            assert_eq!(
                library.parent(),
                fs::canonicalize(scratch.path().join("bundle"))
                    .ok()
                    .as_deref()
            );
        }
    }

    #[test]
    fn target_and_capability_mismatches_are_typed() {
        let cases = ["target", "unknown-capability", "capability-mask"];
        for case in cases {
            let scratch = ScratchDir::new(case);
            let (library, mut boundary) = write_bundle(
                scratch.path(),
                BundleSpec {
                    directory: "bundle",
                    id: case,
                    name: "Metadata",
                    abi: CURRENT_ABI,
                    server_api: "=0.2.0-dev",
                    capabilities: &["read-world"],
                    bytes: b"metadata",
                },
            );
            match case {
                "target" => boundary.target = "other-target".to_owned(),
                "unknown-capability" => boundary.requested_capabilities |= 1_u64 << 63,
                "capability-mask" => boundary.requested_capabilities = 0,
                _ => unreachable!("all cases are named above"),
            }
            let error = PluginLoader::new(test_config())
                .load_directory_with(scratch.path(), |path| {
                    assert_eq!(path, library);
                    Ok(boundary.clone())
                })
                .expect_err("metadata mismatch must fail");
            match case {
                "target" => assert!(matches!(error, PluginLoadError::WrongTarget { .. })),
                "unknown-capability" => assert!(matches!(
                    error,
                    PluginLoadError::UnsupportedCapabilityBits { .. }
                )),
                "capability-mask" => {
                    assert!(matches!(error, PluginLoadError::CapabilityMismatch { .. }));
                }
                _ => unreachable!("all cases are named above"),
            }
        }
    }

    #[test]
    fn unavailable_manifest_capability_is_rejected_before_loading() {
        let scratch = ScratchDir::new("unavailable-capability");
        write_bundle(
            scratch.path(),
            BundleSpec {
                directory: "bundle",
                id: "capability",
                name: "Capability",
                abi: CURRENT_ABI,
                server_api: "=0.2.0-dev",
                capabilities: &["read-world"],
                bytes: b"capability",
            },
        );
        let config = LoaderConfig::new(
            Version::parse("0.2.0-dev").expect("test version"),
            "test-target",
            PluginCapabilities::empty(),
        )
        .expect("test policy");
        let calls = Cell::new(0);
        let error = PluginLoader::new(config)
            .load_directory_with(
                scratch.path(),
                |_path| -> Result<FakeBoundary, BoundaryLoadError> {
                    calls.set(calls.get() + 1);
                    unreachable!("unavailable capability must precede native load")
                },
            )
            .expect_err("unavailable capability must fail");
        assert!(matches!(
            error,
            PluginLoadError::UnavailableCapabilities { missing, .. }
                if missing == PluginCapability::ReadWorld.bit()
        ));
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn library_change_during_native_load_is_detected() {
        let scratch = ScratchDir::new("changed-during-load");
        let (library, boundary) = write_bundle(
            scratch.path(),
            BundleSpec {
                directory: "bundle",
                id: "changing",
                name: "Changing",
                abi: CURRENT_ABI,
                server_api: "=0.2.0-dev",
                capabilities: &[],
                bytes: b"before",
            },
        );
        let error = PluginLoader::new(test_config())
            .load_directory_with(scratch.path(), |path| {
                assert_eq!(path, library);
                fs::write(path, b"after").expect("mutate synthetic library during load");
                Ok(boundary.clone())
            })
            .expect_err("changed library must fail");
        assert!(matches!(
            error,
            PluginLoadError::LibraryChangedDuringLoad { .. }
        ));
    }

    #[test]
    fn manifest_reader_accepts_exact_limit_and_rejects_one_more_byte() {
        let scratch = ScratchDir::new("manifest-bound");
        let exact = scratch.path().join("exact.toml");
        let oversized = scratch.path().join("oversized.toml");
        fs::write(&exact, vec![b'#'; MAX_MANIFEST_BYTES]).expect("write exact manifest bound");
        fs::write(&oversized, vec![b'#'; MAX_MANIFEST_BYTES + 1])
            .expect("write oversized manifest");

        assert_eq!(
            read_manifest(&exact).expect("exact manifest bound").len(),
            MAX_MANIFEST_BYTES
        );
        assert!(matches!(
            read_manifest(&oversized),
            Err(PluginLoadError::ManifestTooLarge { maximum, .. })
                if maximum == MAX_MANIFEST_BYTES
        ));
    }

    #[cfg(unix)]
    #[test]
    fn canonical_library_must_remain_inside_bundle() {
        use std::os::unix::fs::symlink;

        let scratch = ScratchDir::new("library-escape");
        let outside = scratch.path().join("outside.bin");
        fs::write(&outside, b"outside").expect("write outside library");
        let bundle = scratch.path().join("bundle");
        fs::create_dir_all(&bundle).expect("create bundle");
        symlink(&outside, bundle.join("plugin.bin")).expect("link outside library");
        let hash = hex_digest(digest_file(&outside).expect("hash outside file"));
        fs::write(
            bundle.join("plugin.toml"),
            manifest_text("escape", "Escape", CURRENT_ABI, "=0.2.0-dev", &hash, &[]),
        )
        .expect("write escape manifest");

        let error = PluginLoader::new(test_config())
            .load_directory_with(
                scratch.path(),
                |_path| -> Result<FakeBoundary, BoundaryLoadError> {
                    unreachable!("escaping library must not load")
                },
            )
            .expect_err("escaping library must fail");
        assert!(matches!(
            error,
            PluginLoadError::LibraryEscapesBundle { .. }
        ));
    }

    #[test]
    fn compiled_loader_policy_uses_exact_target_and_prerelease_version() {
        let current =
            LoaderConfig::current(PluginCapabilities::all()).expect("compiled policy is valid");
        assert_eq!(current.target(), HOST_TARGET);
        assert_eq!(
            current.server_api(),
            &Version::parse(SERVER_API_VERSION).expect("package version is semver")
        );
    }
}
