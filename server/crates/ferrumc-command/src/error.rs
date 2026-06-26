//! The classifying error type for command parsing, routing, and dispatch.

use ferrumc_core::ServerError;

/// A classified failure produced while parsing or dispatching a command.
///
/// Every variant names *why* a command could not be executed so callers can
/// react programmatically (for example, render a different message for an
/// unknown command than for a permission failure) instead of matching on
/// strings.
///
/// This error reports failures that prevent the command's handler from running.
/// A handler that *did* run and chose to report failure does so through
/// [`crate::CommandResult`], not through this type.
///
/// The enum is `#[non_exhaustive]`, so new variants may be added later and
/// downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CommandError {
    /// The input was empty or contained only whitespace.
    #[error("empty command input")]
    EmptyInput,

    /// The input exceeded the maximum accepted length and was rejected before
    /// parsing, to bound work done on untrusted input.
    #[error("command input too long: {len} bytes (maximum {max})")]
    InputTooLong {
        /// Length of the rejected input, in bytes.
        len: usize,
        /// Maximum accepted length, in bytes.
        max: usize,
    },

    /// No registered command matched the given token.
    #[error("unknown command: {0}")]
    UnknownCommand(String),

    /// The matched command path expected another argument that was not supplied.
    ///
    /// The payload describes what was expected (an argument name, or the set of
    /// possible next tokens).
    #[error("missing argument: expected {0}")]
    MissingArgument(String),

    /// An argument was present but could not be parsed into the expected type.
    #[error("invalid argument '{name}': {reason}")]
    InvalidArgument {
        /// Name of the argument node that failed to parse.
        name: String,
        /// Human-readable reason the value was rejected.
        reason: String,
    },

    /// An integer argument parsed correctly but fell outside its allowed range.
    #[error("argument '{name}' value {value} is out of range [{min}, {max}]")]
    IntegerOutOfRange {
        /// Name of the integer argument node.
        name: String,
        /// The value that was supplied.
        value: i64,
        /// Inclusive lower bound of the allowed range.
        min: i64,
        /// Inclusive upper bound of the allowed range.
        max: i64,
    },

    /// The command path was fully matched but trailing input remained that no
    /// node could consume.
    #[error("too many arguments: unexpected '{0}'")]
    TooManyArguments(String),

    /// The source did not meet a node's required permission level or permission
    /// node. The payload describes the unmet requirement.
    ///
    /// The handler is *not* run when this is returned.
    #[error("permission denied: requires {0}")]
    PermissionDenied(String),
}

impl From<CommandError> for ServerError {
    fn from(err: CommandError) -> Self {
        let message = err.to_string();
        match err {
            CommandError::UnknownCommand(_) => Self::not_found(message),
            CommandError::InputTooLong { .. } => Self::capacity(message),
            _ => Self::invalid_state(message),
        }
    }
}
