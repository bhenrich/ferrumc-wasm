//! Classified errors on the shared author-facing surface.

use crate::CapabilityError;

/// A static plugin declaration is invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeclarationError {
    /// The stable plugin identifier was empty.
    #[error("plugin id must not be empty")]
    EmptyId,
    /// The stable plugin identifier exceeded its byte limit.
    #[error("plugin id length {len} exceeds the maximum of {max} bytes")]
    IdTooLong {
        /// Rejected byte length.
        len: usize,
        /// Maximum accepted byte length.
        max: usize,
    },
    /// The display name was empty.
    #[error("plugin display name must not be empty")]
    EmptyName,
    /// The display name exceeded its byte limit.
    #[error("plugin display-name length {len} exceeds the maximum of {max} bytes")]
    NameTooLong {
        /// Rejected byte length.
        len: usize,
        /// Maximum accepted byte length.
        max: usize,
    },
}

/// A pure-data command definition or invocation is invalid.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CommandError {
    /// A command tree contained no nodes.
    #[error("command tree must contain a root node")]
    EmptyTree,
    /// A bounded command collection exceeded its limit.
    #[error("{resource} count {len} exceeds the maximum of {max}")]
    TooMany {
        /// Name of the bounded collection.
        resource: &'static str,
        /// Rejected element count.
        len: usize,
        /// Maximum accepted element count.
        max: usize,
    },
    /// A command node or argument name was empty.
    #[error("{resource} name must not be empty")]
    EmptyName {
        /// Kind of value whose name was empty.
        resource: &'static str,
    },
    /// A command node or argument name exceeded its byte limit.
    #[error("{resource} name length {len} exceeds the maximum of {max} bytes")]
    NameTooLong {
        /// Kind of value whose name was rejected.
        resource: &'static str,
        /// Rejected byte length.
        len: usize,
        /// Maximum accepted byte length.
        max: usize,
    },
    /// A command text argument exceeded its byte limit.
    #[error("command text length {len} exceeds the maximum of {max} bytes")]
    TextTooLong {
        /// Rejected byte length.
        len: usize,
        /// Maximum accepted byte length.
        max: usize,
    },
    /// The encoded command invocation exceeded its aggregate byte limit.
    #[error("command invocation size {len} exceeds the maximum of {max} bytes")]
    InvocationTooLarge {
        /// Rejected aggregate byte estimate.
        len: usize,
        /// Maximum accepted aggregate byte estimate.
        max: usize,
    },
    /// A root or child parent index violates preorder.
    #[error("command node {node} has invalid parent {parent:?}")]
    InvalidParent {
        /// Node index.
        node: usize,
        /// Declared parent, or no parent for a root.
        parent: Option<usize>,
    },
    /// A command tree contained a second root.
    #[error("command node {node} declares an additional root")]
    MultipleRoots {
        /// Index of the additional root.
        node: usize,
    },
    /// Integer argument bounds were reversed.
    #[error("integer argument minimum {min} exceeds maximum {max}")]
    ReversedIntegerBounds {
        /// Inclusive minimum.
        min: i64,
        /// Inclusive maximum.
        max: i64,
    },
    /// A required operator level exceeded `Minecraft`'s supported range.
    #[error("required operator level {level} exceeds the maximum of 4")]
    InvalidOperatorLevel {
        /// Rejected level.
        level: u8,
    },
}

/// A capability facade operation could not be completed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FacadeError {
    /// The plugin was not granted the required capability.
    #[error(transparent)]
    Capability(#[from] CapabilityError),
    /// An author-supplied value violated a declared bound or precondition.
    #[error("{resource} value is invalid: {reason}")]
    InvalidInput {
        /// Value class.
        resource: &'static str,
        /// Stable reason.
        reason: &'static str,
    },
    /// A bounded value or collection exceeded its limit.
    #[error("{resource} size {len} exceeds the maximum of {max}")]
    LimitExceeded {
        /// Bounded resource.
        resource: &'static str,
        /// Rejected size.
        len: usize,
        /// Maximum size.
        max: usize,
    },
    /// The bounded host command buffer rejected the newest operation.
    #[error("host command buffer is full")]
    BufferFull,
    /// The current host phase or world does not provide this operation.
    #[error("host operation is unavailable: {operation}")]
    Unavailable {
        /// Operation that was unavailable.
        operation: &'static str,
    },
    /// The host rejected the operation by policy.
    #[error("host rejected operation: {0}")]
    Rejected(String),
    /// A host implementation returned a malformed or out-of-contract result.
    #[error("host returned an invalid {resource}: {reason}")]
    InvalidHostResponse {
        /// Result class.
        resource: &'static str,
        /// Stable reason.
        reason: &'static str,
    },
    /// The adapter reported an implementation-specific host failure.
    #[error("host facade failed: {0}")]
    Host(String),
}

/// A plugin callback failed cooperatively.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PluginError {
    /// A capability-facade operation failed.
    #[error(transparent)]
    Facade(#[from] FacadeError),
    /// A command definition was invalid.
    #[error(transparent)]
    Command(#[from] CommandError),
    /// The plugin rejected its own callback for a described reason.
    #[error("plugin callback failed: {0}")]
    Failed(String),
}

impl PluginError {
    /// Creates a cooperative plugin callback error.
    pub fn failed(reason: impl Into<String>) -> Self {
        Self::Failed(reason.into())
    }
}
