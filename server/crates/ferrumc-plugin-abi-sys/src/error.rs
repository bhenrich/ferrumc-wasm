use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use ferrumc_plugin_abi::{AbiVersion, AbiVersionError};

/// A raw ABI record validated at the native boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbiRecord {
    /// The plugin descriptor returned by the bootstrap symbol.
    Descriptor,
    /// The required plugin function table.
    FunctionTable,
}

impl fmt::Display for AbiRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Descriptor => formatter.write_str("plugin descriptor"),
            Self::FunctionTable => formatter.write_str("plugin function table"),
        }
    }
}

/// A plugin record or call-scoped metadata view failed boundary validation.
#[derive(Debug)]
pub enum ValidationError {
    /// The bootstrap symbol returned a null descriptor pointer.
    NullDescriptor,
    /// A record does not cover even the named required prefix.
    RecordTooShort {
        /// The record being validated.
        record: AbiRecord,
        /// The byte length declared in the record header.
        declared: u32,
        /// The minimum byte length required before the next validation step.
        required: u32,
    },
    /// A record pointer does not satisfy the typed record's alignment.
    MisalignedRecord {
        /// The record being validated.
        record: AbiRecord,
        /// The required byte alignment.
        required_alignment: usize,
    },
    /// A record declares an ABI version that this host cannot accept.
    IncompatibleAbi {
        /// The record being validated.
        record: AbiRecord,
        /// The version-policy failure.
        source: AbiVersionError,
    },
    /// A required raw function slot contains the null word.
    NullRequiredSlot {
        /// The record containing the slot.
        record: AbiRecord,
        /// The stable slot name.
        slot: &'static str,
    },
    /// The descriptor's function-table getter returned a null pointer.
    NullFunctionTable,
    /// The descriptor and its function table declare different ABI versions.
    FunctionTableVersionMismatch {
        /// The ABI version declared by the descriptor.
        descriptor: AbiVersion,
        /// The ABI version declared by the function table.
        function_table: AbiVersion,
    },
    /// The semantic-version reserved word is not zero.
    NonZeroSemanticVersionReserved {
        /// The rejected reserved value.
        value: u32,
    },
    /// A declared metadata length does not fit the host address space.
    MetadataLengthNotRepresentable {
        /// The metadata field being copied.
        field: &'static str,
        /// The rejected byte length.
        declared: u64,
    },
    /// A declared metadata length exceeds the boundary's fixed allocation cap.
    MetadataTooLong {
        /// The metadata field being copied.
        field: &'static str,
        /// The rejected byte length.
        declared: u64,
        /// The maximum accepted byte length.
        maximum: usize,
    },
    /// A nonempty metadata view has a null data pointer.
    NullMetadataPointer {
        /// The metadata field being copied.
        field: &'static str,
    },
    /// A metadata pointer extent wraps the host address space.
    MetadataAddressOverflow {
        /// The metadata field being copied.
        field: &'static str,
    },
    /// A copied metadata value is not valid UTF-8.
    InvalidMetadataUtf8 {
        /// The metadata field being copied.
        field: &'static str,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullDescriptor => {
                formatter.write_str("plugin bootstrap returned a null descriptor")
            }
            Self::RecordTooShort {
                record,
                declared,
                required,
            } => write!(
                formatter,
                "{record} declares {declared} bytes but {required} bytes are required"
            ),
            Self::MisalignedRecord {
                record,
                required_alignment,
            } => write!(
                formatter,
                "{record} pointer does not satisfy {required_alignment}-byte alignment"
            ),
            Self::IncompatibleAbi { record, source } => {
                write!(formatter, "{record} has an incompatible ABI version: {source}")
            }
            Self::NullRequiredSlot { record, slot } => {
                write!(formatter, "{record} has a null required `{slot}` slot")
            }
            Self::NullFunctionTable => {
                formatter.write_str("plugin descriptor returned a null function table")
            }
            Self::FunctionTableVersionMismatch {
                descriptor,
                function_table,
            } => write!(
                formatter,
                "plugin descriptor ABI {descriptor} does not match function-table ABI {function_table}"
            ),
            Self::NonZeroSemanticVersionReserved { value } => write!(
                formatter,
                "plugin semantic-version reserved word must be zero, got {value}"
            ),
            Self::MetadataLengthNotRepresentable { field, declared } => write!(
                formatter,
                "plugin {field} declares {declared} bytes, which the host cannot represent"
            ),
            Self::MetadataTooLong {
                field,
                declared,
                maximum,
            } => write!(
                formatter,
                "plugin {field} declares {declared} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::NullMetadataPointer { field } => {
                write!(formatter, "nonempty plugin {field} has a null data pointer")
            }
            Self::MetadataAddressOverflow { field } => {
                write!(formatter, "plugin {field} byte extent wraps the host address space")
            }
            Self::InvalidMetadataUtf8 { field } => {
                write!(formatter, "plugin {field} is not valid UTF-8")
            }
        }
    }
}

impl Error for ValidationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::IncompatibleAbi { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Opening, resolving, or validating a native plugin library failed.
#[derive(Debug)]
pub enum LoadError {
    /// The platform loader could not open the requested library.
    OpenLibrary {
        /// The requested library path.
        path: PathBuf,
        /// The platform-loader failure.
        source: libloading::Error,
    },
    /// The exact ABI v1 bootstrap symbol could not be resolved.
    MissingEntrypoint {
        /// The requested library path.
        path: PathBuf,
        /// The symbol-resolution failure.
        source: libloading::Error,
    },
    /// The opened library failed ABI boundary validation.
    Validation {
        /// The requested library path.
        path: PathBuf,
        /// The typed validation failure.
        source: ValidationError,
    },
}

impl LoadError {
    pub(crate) fn open_library(path: &Path, source: libloading::Error) -> Self {
        Self::OpenLibrary {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn missing_entrypoint(path: &Path, source: libloading::Error) -> Self {
        Self::MissingEntrypoint {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn validation(path: &Path, source: ValidationError) -> Self {
        Self::Validation {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Returns the library path associated with this failure.
    pub fn path(&self) -> &Path {
        match self {
            Self::OpenLibrary { path, .. }
            | Self::MissingEntrypoint { path, .. }
            | Self::Validation { path, .. } => path,
        }
    }

    /// Returns the validation failure when the library opened and resolved.
    pub const fn validation_error(&self) -> Option<&ValidationError> {
        match self {
            Self::Validation { source, .. } => Some(source),
            Self::OpenLibrary { .. } | Self::MissingEntrypoint { .. } => None,
        }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenLibrary { path, source } => {
                write!(
                    formatter,
                    "failed to open plugin library {}: {source}",
                    path.display()
                )
            }
            Self::MissingEntrypoint { path, source } => write!(
                formatter,
                "plugin library {} does not export the required ABI v1 entrypoint: {source}",
                path.display()
            ),
            Self::Validation { path, source } => write!(
                formatter,
                "plugin library {} failed validation: {source}",
                path.display()
            ),
        }
    }
}

impl Error for LoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OpenLibrary { source, .. } | Self::MissingEntrypoint { source, .. } => {
                Some(source)
            }
            Self::Validation { source, .. } => Some(source),
        }
    }
}
