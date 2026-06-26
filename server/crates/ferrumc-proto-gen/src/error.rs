//! Error types for the protocol generator.

use std::path::PathBuf;

use thiserror::Error;

/// Everything that can go wrong while generating or checking protocol code.
///
/// Each variant classifies a distinct failure mode so callers (and CI) can tell
/// a missing/garbled spec apart from a `rustfmt` failure, an I/O error, or a
/// genuine drift between the committed files and what the generator would emit.
#[derive(Debug, Error)]
pub enum GenError {
    /// The pinned spec file could not be read from disk.
    #[error("failed to read pinned spec at {path}")]
    SpecRead {
        /// Path the generator tried to read.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The pinned spec file was read but is not valid TOML / has the wrong shape.
    #[error("failed to parse pinned spec at {path}")]
    SpecParse {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying deserialization failure.
        #[source]
        source: toml::de::Error,
    },

    /// The pinned spec does not target the protocol/data version this generator
    /// supports, so emitting would silently produce code for the wrong release.
    #[error(
        "pinned spec mismatch: expected protocol {expected_protocol} / data version \
         {expected_data}, found protocol {found_protocol} / data version {found_data}"
    )]
    SpecMismatch {
        /// Protocol version this generator is built for.
        expected_protocol: i32,
        /// Data version this generator is built for.
        expected_data: i32,
        /// Protocol version found in the spec.
        found_protocol: i32,
        /// Data version found in the spec.
        found_data: i32,
    },

    /// The declarative packet spec (`packets.toml`) could not be read from disk.
    #[error("failed to read packet spec at {path}")]
    PacketsRead {
        /// Path the generator tried to read.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// The packet spec was read but is not valid TOML / has the wrong shape.
    #[error("failed to parse packet spec at {path}")]
    PacketsParse {
        /// Path that failed to parse.
        path: PathBuf,
        /// Underlying deserialization failure.
        #[source]
        source: toml::de::Error,
    },

    /// The packet spec parsed but is semantically invalid: an unknown wire type,
    /// an unknown state/direction, a duplicate packet id, or a dangling struct
    /// reference. The payload describes the offending entry.
    #[error("invalid packet spec: {0}")]
    PacketsInvalid(String),

    /// Invoking `rustfmt` failed, or it exited non-zero / emitted non-UTF-8.
    #[error("rustfmt normalization failed: {0}")]
    Rustfmt(String),

    /// A filesystem operation against a generated file or its directory failed.
    #[error("i/o error at {path}")]
    Io {
        /// Path the operation targeted.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// In `--check` mode, the committed files differ from freshly generated
    /// output. The payload is a human-readable diff naming the stale files.
    #[error("generated protocol code is out of date:\n{0}")]
    Drift(String),
}
