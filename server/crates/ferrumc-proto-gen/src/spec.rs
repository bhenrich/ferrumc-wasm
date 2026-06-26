//! The pinned protocol spec: what release the generator emits code for.
//!
//! The single source of truth is the vendored
//! `fixtures/protocol/1_21_8/manifest.toml`, which records the upstream
//! `minecraft-data` commit alongside the protocol and data versions. Loading it
//! here (rather than hardcoding the values) keeps the generator and the vendored
//! data from silently disagreeing.

use std::path::Path;

use serde::Deserialize;

use crate::error::GenError;

/// Protocol (network) version this generator is built to emit code for.
pub(crate) const EXPECTED_PROTOCOL_VERSION: i32 = 772;

/// World data version this generator is built to emit code for.
///
/// Disambiguates 1.21.8 from 1.21.7, which share protocol `772`.
pub(crate) const EXPECTED_DATA_VERSION: i32 = 4440;

/// Human-readable Minecraft release the generated code targets.
pub(crate) const MINECRAFT_VERSION: &str = "1.21.8";

/// The validated, pinned spec the generator emits from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spec {
    /// Upstream `minecraft-data` commit the vendored data was pulled from.
    pub(crate) source_commit: String,
    /// Protocol (network) version, guaranteed to equal [`EXPECTED_PROTOCOL_VERSION`].
    pub(crate) protocol_version: i32,
    /// World data version, guaranteed to equal [`EXPECTED_DATA_VERSION`].
    pub(crate) data_version: i32,
}

impl Spec {
    /// Loads and validates the pinned spec from a `manifest.toml` at `path`.
    ///
    /// Fails with [`GenError::SpecRead`]/[`GenError::SpecParse`] if the file is
    /// missing or malformed, and [`GenError::SpecMismatch`] if it does not pin
    /// the protocol/data versions this generator supports.
    pub(crate) fn load_pinned(path: &Path) -> Result<Self, GenError> {
        let raw = std::fs::read_to_string(path).map_err(|source| GenError::SpecRead {
            path: path.to_path_buf(),
            source,
        })?;

        let manifest: Manifest = toml::from_str(&raw).map_err(|source| GenError::SpecParse {
            path: path.to_path_buf(),
            source,
        })?;

        if manifest.minecraft.protocol_version != EXPECTED_PROTOCOL_VERSION
            || manifest.minecraft.data_version != EXPECTED_DATA_VERSION
        {
            return Err(GenError::SpecMismatch {
                expected_protocol: EXPECTED_PROTOCOL_VERSION,
                expected_data: EXPECTED_DATA_VERSION,
                found_protocol: manifest.minecraft.protocol_version,
                found_data: manifest.minecraft.data_version,
            });
        }

        Ok(Self {
            source_commit: manifest.source.commit,
            protocol_version: manifest.minecraft.protocol_version,
            data_version: manifest.minecraft.data_version,
        })
    }
}

/// Subset of `manifest.toml` the generator reads. Unknown keys are ignored by
/// serde, so unrelated provenance/checksum fields do not need modeling here.
#[derive(Debug, Deserialize)]
struct Manifest {
    source: SourceSection,
    minecraft: MinecraftSection,
}

/// The `[source]` table: upstream provenance.
#[derive(Debug, Deserialize)]
struct SourceSection {
    commit: String,
}

/// The `[minecraft]` table: the version pin we validate against.
#[derive(Debug, Deserialize)]
struct MinecraftSection {
    protocol_version: i32,
    data_version: i32,
}
