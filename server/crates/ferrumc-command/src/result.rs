//! The success/failure value a command handler returns.

use ferrumc_core::TextComponent;

/// The outcome of a command handler that actually ran.
///
/// A handler reports whether it logically succeeded and attaches a
/// [`TextComponent`] feedback message to show the source. This is distinct from
/// [`crate::CommandError`], which reports failures that stop a handler from
/// running at all (unknown command, bad argument, permission denied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    success: bool,
    feedback: TextComponent,
}

impl CommandResult {
    /// Builds a successful result carrying `feedback`.
    pub fn success(feedback: TextComponent) -> Self {
        Self {
            success: true,
            feedback,
        }
    }

    /// Builds a failed result carrying `feedback`.
    ///
    /// Use this when the command ran but could not complete its intent (for
    /// example, a target player was offline). Failures that prevent the handler
    /// from running are reported via [`crate::CommandError`] instead.
    pub fn failure(feedback: TextComponent) -> Self {
        Self {
            success: false,
            feedback,
        }
    }

    /// Returns `true` if the handler reported success.
    pub const fn is_success(&self) -> bool {
        self.success
    }

    /// Returns the feedback message.
    pub const fn feedback(&self) -> &TextComponent {
        &self.feedback
    }

    /// Consumes the result, returning its feedback message.
    pub fn into_feedback(self) -> TextComponent {
        self.feedback
    }
}
