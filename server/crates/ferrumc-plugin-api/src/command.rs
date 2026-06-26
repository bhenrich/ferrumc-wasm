//! The registrar a plugin uses to contribute commands.

use ferrumc_command::CommandBuilder;

/// Collects the commands a plugin registers during setup.
///
/// The host hands a plugin a registrar (subject to the
/// [`RegisterCommands`](crate::Capability::RegisterCommands) capability) in
/// [`Plugin::on_enable`](crate::Plugin::on_enable). The plugin builds commands
/// with [`ferrumc_command::literal`] / [`ferrumc_command::argument`] and adds
/// them here; the host drains them into its own
/// [`CommandTree`](ferrumc_command::CommandTree) afterward.
#[derive(Default)]
pub struct CommandRegistrar {
    commands: Vec<CommandBuilder>,
}

impl CommandRegistrar {
    /// Creates an empty registrar.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `command` to the set to be registered, returning the registrar for
    /// chaining.
    pub fn register(&mut self, command: CommandBuilder) -> &mut Self {
        self.commands.push(command);
        self
    }

    /// Returns how many commands have been registered.
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Returns whether no commands have been registered.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Consumes the registrar, yielding the registered command builders in the
    /// order they were added.
    pub fn into_commands(self) -> Vec<CommandBuilder> {
        self.commands
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_command::literal;

    #[test]
    fn registrar_collects_commands_in_order() {
        let mut registrar = CommandRegistrar::new();
        assert!(registrar.is_empty());
        registrar
            .register(literal("alpha"))
            .register(literal("beta"));
        assert_eq!(registrar.len(), 2);
        assert_eq!(registrar.into_commands().len(), 2);
    }
}
