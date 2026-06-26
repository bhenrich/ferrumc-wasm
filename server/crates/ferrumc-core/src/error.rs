//! The shared error root for the whole server.

/// The shared, top-level error type for the server.
///
/// Every fallible operation in the server ultimately surfaces a `ServerError`.
/// Variants *classify* the kind of failure so callers can react programmatically
/// instead of parsing message strings. Lower layers convert their own specific
/// errors into one of these classifying variants.
///
/// This type deliberately depends on no other crate in the project: it is a leaf
/// that everything else can build on.
///
/// The enum is `#[non_exhaustive]`, so new variants may be added in future
/// releases and downstream `match`es must include a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ServerError {
    /// A requested resource (player, world, entity, ...) does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// An operation was requested while its target was in a state that does not
    /// permit it (for example, sending play packets before login completes).
    #[error("invalid state: {0}")]
    InvalidState(String),

    /// A bounded resource is exhausted (connection slots, queue capacity, ...).
    #[error("capacity exceeded: {0}")]
    Capacity(String),

    /// Configuration was missing, malformed, or contained an invalid value.
    #[error("configuration error: {0}")]
    Config(String),

    /// A requested operation, protocol feature, or value is not supported.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// An internal invariant was violated or an otherwise unexpected condition
    /// occurred. The `context` should describe where and what, not merely that
    /// "an error" happened.
    #[error("internal error: {context}")]
    Internal {
        /// Human-readable description of the failed invariant or operation.
        context: String,
    },
}

impl ServerError {
    /// Builds a [`ServerError::NotFound`] describing the missing resource.
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound(what.into())
    }

    /// Builds a [`ServerError::InvalidState`] describing the disallowed operation.
    pub fn invalid_state(detail: impl Into<String>) -> Self {
        Self::InvalidState(detail.into())
    }

    /// Builds a [`ServerError::Capacity`] describing the exhausted resource.
    pub fn capacity(detail: impl Into<String>) -> Self {
        Self::Capacity(detail.into())
    }

    /// Builds a [`ServerError::Config`] describing the configuration problem.
    pub fn config(detail: impl Into<String>) -> Self {
        Self::Config(detail.into())
    }

    /// Builds a [`ServerError::Unsupported`] describing the unsupported request.
    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::Unsupported(detail.into())
    }

    /// Builds a [`ServerError::Internal`] with the given `context`.
    pub fn internal(context: impl Into<String>) -> Self {
        Self::Internal {
            context: context.into(),
        }
    }
}

/// Convenience result alias whose error type is [`ServerError`].
///
/// Prefer this over writing `core::result::Result<T, ServerError>` by hand.
pub type Result<T> = core::result::Result<T, ServerError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_strings_are_classified() {
        assert_eq!(
            ServerError::not_found("player abc").to_string(),
            "not found: player abc"
        );
        assert_eq!(
            ServerError::invalid_state("not logged in").to_string(),
            "invalid state: not logged in"
        );
        assert_eq!(
            ServerError::capacity("connection slots").to_string(),
            "capacity exceeded: connection slots"
        );
        assert_eq!(
            ServerError::config("missing port").to_string(),
            "configuration error: missing port"
        );
        assert_eq!(
            ServerError::unsupported("protocol 47").to_string(),
            "unsupported: protocol 47"
        );
        assert_eq!(
            ServerError::internal("shard 3 desynced").to_string(),
            "internal error: shard 3 desynced"
        );
    }

    #[test]
    fn constructors_accept_str_and_string() {
        let from_str = ServerError::config("x");
        let from_string = ServerError::config(String::from("x"));
        assert_eq!(from_str, from_string);
    }

    #[test]
    fn result_alias_round_trips() {
        let ok: Result<u8> = Ok(7);
        let err: Result<u8> = Err(ServerError::not_found("thing"));
        assert_eq!(ok, Ok(7));
        assert!(matches!(err, Err(ServerError::NotFound(_))));
    }
}
