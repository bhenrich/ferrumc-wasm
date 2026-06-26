//! Error types for permission parsing and validation.

use ferrumc_core::ServerError;

/// The maximum byte length accepted for a permission node string.
///
/// Permission nodes routinely originate from configuration files and plugins,
/// which are not fully trusted to be well-behaved. Capping the length bounds the
/// work done by [`PermissionNode::parse`](crate::PermissionNode::parse) and the
/// memory a single node can occupy.
pub const MAX_NODE_LEN: usize = 255;

/// Why a string failed to parse into a
/// [`PermissionNode`](crate::PermissionNode).
///
/// Each variant *classifies* a distinct malformation so callers can react
/// programmatically (for example, surfacing a precise message to a server
/// operator) instead of matching on a formatted string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NodeParseError {
    /// The input string was empty.
    #[error("permission node is empty")]
    Empty,

    /// The input exceeded [`MAX_NODE_LEN`] bytes.
    #[error("permission node is too long: {len} bytes (max {MAX_NODE_LEN})")]
    TooLong {
        /// The rejected length, in bytes.
        len: usize,
    },

    /// A dot-separated segment was empty, caused by a leading dot, a trailing
    /// dot, or two consecutive dots (for example `a..b`).
    #[error("permission node has an empty segment")]
    EmptySegment,

    /// A segment contained a character outside the allowed set
    /// (`a`-`z`, `0`-`9`, `_`, `-`).
    #[error("permission node contains an invalid character: {character:?}")]
    InvalidCharacter {
        /// The first disallowed character encountered.
        character: char,
    },

    /// A `*` appeared anywhere other than as a whole trailing segment.
    ///
    /// Valid wildcards are the bare root `*` or a trailing `.*` (for example
    /// `ferrumc.command.*`). Embedded (`ab*`) or non-final (`a.*.b`) wildcards
    /// are rejected.
    #[error("permission node has a misplaced wildcard")]
    MisplacedWildcard,
}

impl From<NodeParseError> for ServerError {
    fn from(err: NodeParseError) -> Self {
        Self::invalid_state(err.to_string())
    }
}

/// Error returned when constructing an
/// [`OperatorLevel`](crate::OperatorLevel) from an out-of-range value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid operator level: {0} (valid range is 0..=4)")]
pub struct InvalidOperatorLevel(pub(crate) u8);

impl InvalidOperatorLevel {
    /// Returns the rejected level.
    pub const fn value(self) -> u8 {
        self.0
    }
}

impl From<InvalidOperatorLevel> for ServerError {
    fn from(err: InvalidOperatorLevel) -> Self {
        Self::invalid_state(err.to_string())
    }
}
