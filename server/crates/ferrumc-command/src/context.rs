//! The context passed to a command handler when it runs.

use crate::argument::ParsedArgs;
use crate::source::CommandSource;

/// Everything a command handler needs to do its work: who ran the command and
/// the arguments parsed for it.
///
/// Borrowed for the duration of the handler call; handlers receive `&CommandContext`.
pub struct CommandContext<'a> {
    source: &'a CommandSource,
    args: &'a ParsedArgs,
}

impl<'a> CommandContext<'a> {
    /// Builds a context borrowing the given `source` and `args`.
    pub(crate) const fn new(source: &'a CommandSource, args: &'a ParsedArgs) -> Self {
        Self { source, args }
    }

    /// Returns the source that invoked the command.
    pub const fn source(&self) -> &CommandSource {
        self.source
    }

    /// Returns the full set of parsed arguments.
    pub const fn args(&self) -> &ParsedArgs {
        self.args
    }

    /// Convenience accessor for an integer argument named `name`.
    pub fn integer(&self, name: &str) -> Option<i64> {
        self.args.integer(name)
    }

    /// Convenience accessor for a string argument named `name`.
    pub fn string(&self, name: &str) -> Option<&str> {
        self.args.string(name)
    }
}
