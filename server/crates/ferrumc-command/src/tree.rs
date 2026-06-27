//! The command tree: registration, dispatch, and suggestions.

use crate::argument::{ArgumentType, ArgumentValue, ParsedArgs};
use crate::brigadier::{self, BrigadierGraph};
use crate::builder::{CommandBuilder, CommandNode, NodeKind};
use crate::context::CommandContext;
use crate::error::CommandError;
use crate::result::CommandResult;
use crate::source::CommandSource;

/// Maximum accepted input length, in bytes.
///
/// Command input arrives from clients and is therefore untrusted. Anything
/// longer than this is rejected up front so parsing never works on an
/// unbounded string. The limit is generous relative to the vanilla chat cap.
const MAX_INPUT_LEN: usize = 32_768;

/// A registry of commands that can parse and dispatch input.
///
/// Build commands with [`crate::literal`] / [`crate::argument`], register them
/// with [`CommandTree::register`], then run input through [`CommandTree::dispatch`]
/// (level-only permission checks) or [`CommandTree::dispatch_with`] (level checks
/// plus an injected permission-node checker). Tab-completion candidates come from
/// [`CommandTree::suggest`].
pub struct CommandTree {
    root: CommandNode,
}

impl CommandTree {
    /// Creates an empty tree with no registered commands.
    pub fn new() -> Self {
        Self {
            root: CommandNode::root(),
        }
    }

    /// Registers a top-level command (typically a [`crate::literal`]).
    ///
    /// Commands are matched in registration order; the first match wins.
    pub fn register(&mut self, command: CommandBuilder) {
        self.root.children.push(command.into_node());
    }

    /// Parses and runs `input` on behalf of `source`, checking only permission
    /// *levels*.
    ///
    /// Nodes that declare a required permission *node string* (via
    /// [`crate::CommandBuilder::requires_permission`]) are not enforced here,
    /// since no checker is supplied; use [`CommandTree::dispatch_with`] when a
    /// permission backend is available.
    ///
    /// Returns the handler's [`CommandResult`] on success, or a [`CommandError`]
    /// classifying why the command could not run.
    pub fn dispatch(
        &self,
        input: &str,
        source: &CommandSource,
    ) -> Result<CommandResult, CommandError> {
        self.dispatch_inner(input, source, None)
    }

    /// Parses and runs `input` on behalf of `source`, checking permission levels
    /// *and* permission-node strings via `is_allowed`.
    ///
    /// For every traversed node that declares a required permission node string,
    /// `is_allowed(node)` is called; if it returns `false`, dispatch fails with
    /// [`CommandError::PermissionDenied`] and the handler never runs. Permission
    /// *level* checks still apply as in [`CommandTree::dispatch`].
    ///
    /// The checker is borrowed, so callers pass a reference to a closure, e.g.
    /// `tree.dispatch_with(input, &src, &|node| backend.has(node))`.
    pub fn dispatch_with(
        &self,
        input: &str,
        source: &CommandSource,
        is_allowed: &dyn Fn(&str) -> bool,
    ) -> Result<CommandResult, CommandError> {
        self.dispatch_inner(input, source, Some(is_allowed))
    }

    fn dispatch_inner(
        &self,
        input: &str,
        source: &CommandSource,
        perm: Option<&dyn Fn(&str) -> bool>,
    ) -> Result<CommandResult, CommandError> {
        if input.len() > MAX_INPUT_LEN {
            return Err(CommandError::InputTooLong {
                len: input.len(),
                max: MAX_INPUT_LEN,
            });
        }

        let mut reader = InputReader::new(input);
        reader.skip_whitespace();
        if reader.at_end() {
            return Err(CommandError::EmptyInput);
        }

        let mut args = ParsedArgs::new();
        walk(&self.root, &mut reader, &mut args, source, perm)
    }

    /// Returns completion candidates for the next token of `input`.
    ///
    /// Complete (whitespace-terminated) tokens are followed down the tree; the
    /// final, still-being-typed token is treated as a prefix to match against the
    /// reached node's children. Literal children are returned by name (filtered
    /// by the prefix); argument children contribute a hint such as `<count: 1..9>`
    /// when a fresh token is expected. An unrecognized path yields no candidates.
    ///
    /// This does not validate argument values or enforce permissions; it is a
    /// best-effort aid for tab completion. Input longer than the accepted maximum
    /// yields no candidates.
    pub fn suggest(&self, input: &str) -> Vec<String> {
        if input.len() > MAX_INPUT_LEN {
            return Vec::new();
        }

        // A trailing space (or empty input) means a fresh token is expected, so
        // every complete token is consumed and the prefix is empty.
        let expecting_fresh_token = input.chars().next_back().is_none_or(char::is_whitespace);
        let tokens: Vec<&str> = input.split_whitespace().collect();
        let (complete, prefix) = if expecting_fresh_token {
            (tokens.as_slice(), "")
        } else {
            match tokens.split_last() {
                Some((last, rest)) => (rest, *last),
                None => (tokens.as_slice(), ""),
            }
        };

        let mut node = &self.root;
        for token in complete {
            match navigable_child(node, token) {
                Some(child) => node = child,
                None => return Vec::new(),
            }
        }

        let mut suggestions = Vec::new();
        for child in &node.children {
            match &child.kind {
                NodeKind::Literal(literal) if literal.starts_with(prefix) => {
                    suggestions.push(literal.clone());
                }
                NodeKind::Argument { name, arg_type } if prefix.is_empty() => {
                    suggestions.push(argument_hint(name, *arg_type));
                }
                _ => {}
            }
        }
        suggestions
    }

    /// Lowers this tree into the Brigadier command-node graph the clientbound
    /// `Commands` packet carries, filtered to the commands a player at
    /// `player_level` may use.
    ///
    /// A node whose required permission *level* exceeds `player_level` is dropped
    /// along with its whole subtree, and the surviving child indices are
    /// renumbered. Permission *node strings* are not consulted here: the level is
    /// the only gate the client graph can express. The synthetic root is always
    /// present at index 0. See [`crate::BrigadierGraph`] for the resulting shape
    /// and [`CommandTree::encode_commands_body`] for the wire bytes.
    pub fn to_brigadier(&self, player_level: u8) -> BrigadierGraph {
        brigadier::lower(&self.root, player_level)
    }

    /// Encodes the `Commands` packet body for a player at `player_level`: the
    /// `VarInt` node count, each Brigadier node's bytes, then the trailing
    /// `VarInt` root index.
    ///
    /// This is the opaque payload the generated `Commands` packet wraps (the
    /// Brigadier node graph is not expressible in the declarative packet grammar).
    /// Equivalent to `self.to_brigadier(player_level).encode()`.
    pub fn encode_commands_body(&self, player_level: u8) -> Vec<u8> {
        self.to_brigadier(player_level).encode()
    }
}

impl Default for CommandTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Recursively matches `reader` against `node`'s subtree and runs the handler at
/// the end of the matched path.
fn walk(
    node: &CommandNode,
    reader: &mut InputReader<'_>,
    args: &mut ParsedArgs,
    source: &CommandSource,
    perm: Option<&dyn Fn(&str) -> bool>,
) -> Result<CommandResult, CommandError> {
    check_permission(node, source, perm)?;

    reader.skip_whitespace();
    if reader.at_end() {
        return match &node.handler {
            Some(handler) => {
                let ctx = CommandContext::new(source, args);
                Ok(handler(&ctx))
            }
            None => Err(CommandError::MissingArgument(expected_next(node))),
        };
    }

    // Input remains, so a child must consume it.
    if node.children.is_empty() {
        return Err(CommandError::TooManyArguments(
            reader.peek_word().to_string(),
        ));
    }

    let word = reader.peek_word();

    // Literal children take priority and match by exact keyword.
    for child in &node.children {
        if let NodeKind::Literal(literal) = &child.kind {
            if literal == word {
                let _ = reader.read_word();
                return walk(child, reader, args, source, perm);
            }
        }
    }

    // Otherwise try argument children in declaration order; the first that parses
    // wins. A failed attempt does not consume input (the reader is copied).
    let mut first_error: Option<CommandError> = None;
    let mut had_argument_child = false;
    for child in &node.children {
        if let NodeKind::Argument { name, arg_type } = &child.kind {
            had_argument_child = true;
            let mut trial = *reader;
            match parse_argument(name, *arg_type, &mut trial) {
                Ok(value) => {
                    *reader = trial;
                    args.insert(name.clone(), value);
                    return walk(child, reader, args, source, perm);
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
    }

    if had_argument_child {
        Err(first_error.unwrap_or_else(|| CommandError::UnknownCommand(word.to_string())))
    } else {
        Err(CommandError::UnknownCommand(word.to_string()))
    }
}

/// Verifies `source` satisfies `node`'s permission level and permission node.
fn check_permission(
    node: &CommandNode,
    source: &CommandSource,
    perm: Option<&dyn Fn(&str) -> bool>,
) -> Result<(), CommandError> {
    if let Some(level) = node.required_level {
        if source.permission_level() < level {
            return Err(CommandError::PermissionDenied(format!(
                "permission level {level}"
            )));
        }
    }

    if let Some(node_permission) = &node.required_permission {
        // With no checker (level-only dispatch), node-string permissions cannot
        // be evaluated and are treated as satisfied; the caller opted out.
        let allowed = perm.is_none_or(|check| check(node_permission));
        if !allowed {
            return Err(CommandError::PermissionDenied(format!(
                "permission node '{node_permission}'"
            )));
        }
    }

    Ok(())
}

/// Parses one argument of the given type from `reader`.
fn parse_argument(
    name: &str,
    arg_type: ArgumentType,
    reader: &mut InputReader<'_>,
) -> Result<ArgumentValue, CommandError> {
    match arg_type {
        ArgumentType::Word => {
            let word = reader.read_word();
            if word.is_empty() {
                return Err(CommandError::MissingArgument(name.to_string()));
            }
            Ok(ArgumentValue::String(word.to_string()))
        }
        ArgumentType::GreedyString => {
            let rest = reader.read_remaining();
            if rest.is_empty() {
                return Err(CommandError::MissingArgument(name.to_string()));
            }
            Ok(ArgumentValue::String(rest.to_string()))
        }
        ArgumentType::Integer { min, max } => {
            let word = reader.read_word();
            if word.is_empty() {
                return Err(CommandError::MissingArgument(name.to_string()));
            }
            let value: i64 = word.parse().map_err(|_| CommandError::InvalidArgument {
                name: name.to_string(),
                reason: format!("'{word}' is not a valid integer"),
            })?;
            if value < min || value > max {
                return Err(CommandError::IntegerOutOfRange {
                    name: name.to_string(),
                    value,
                    min,
                    max,
                });
            }
            Ok(ArgumentValue::Integer(value))
        }
    }
}

/// Describes the tokens a node expects next, for error messages.
fn expected_next(node: &CommandNode) -> String {
    if node.children.is_empty() {
        return "more input".to_string();
    }
    node.children
        .iter()
        .map(|child| match &child.kind {
            NodeKind::Literal(literal) => literal.clone(),
            NodeKind::Argument { name, .. } => format!("<{name}>"),
            NodeKind::Root => String::new(),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Picks the child a complete suggestion token would descend into: an exact
/// literal match, otherwise the first argument child.
fn navigable_child<'a>(node: &'a CommandNode, token: &str) -> Option<&'a CommandNode> {
    for child in &node.children {
        if let NodeKind::Literal(literal) = &child.kind {
            if literal == token {
                return Some(child);
            }
        }
    }
    node.children
        .iter()
        .find(|child| matches!(child.kind, NodeKind::Argument { .. }))
}

/// Renders a human-readable hint for an argument node.
fn argument_hint(name: &str, arg_type: ArgumentType) -> String {
    match arg_type {
        ArgumentType::Word => format!("<{name}>"),
        ArgumentType::GreedyString => format!("<{name}...>"),
        ArgumentType::Integer { min, max } => format!("<{name}: {min}..{max}>"),
    }
}

/// A cheap, copyable cursor over the remaining input.
///
/// Tokens are split on Unicode whitespace; greedy reads take the rest verbatim.
#[derive(Debug, Clone, Copy)]
struct InputReader<'a> {
    rest: &'a str,
}

impl<'a> InputReader<'a> {
    const fn new(input: &'a str) -> Self {
        Self { rest: input }
    }

    /// Advances past any leading whitespace.
    fn skip_whitespace(&mut self) {
        self.rest = self.rest.trim_start();
    }

    /// Returns `true` if only whitespace (or nothing) remains.
    fn at_end(&self) -> bool {
        self.rest.trim_start().is_empty()
    }

    /// Returns the next word without consuming it (empty if at end).
    fn peek_word(&self) -> &'a str {
        let trimmed = self.rest.trim_start();
        match trimmed.find(char::is_whitespace) {
            Some(idx) => &trimmed[..idx],
            None => trimmed,
        }
    }

    /// Consumes and returns the next word (empty if at end).
    fn read_word(&mut self) -> &'a str {
        self.skip_whitespace();
        let (word, rest) = match self.rest.find(char::is_whitespace) {
            Some(idx) => (&self.rest[..idx], &self.rest[idx..]),
            None => (self.rest, ""),
        };
        self.rest = rest;
        word
    }

    /// Consumes and returns all remaining input, trimmed of surrounding whitespace.
    fn read_remaining(&mut self) -> &'a str {
        let remaining = self.rest.trim();
        self.rest = "";
        remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{argument, literal};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use ferrumc_core::TextComponent;

    fn op() -> CommandSource {
        CommandSource::console(4)
    }

    fn member() -> CommandSource {
        CommandSource::for_player(ferrumc_core::PlayerId::offline("Steve"), "Steve", 0)
    }

    fn sample_tree() -> CommandTree {
        let mut tree = CommandTree::new();
        tree.register(literal("spawn").executes(|ctx| {
            CommandResult::success(TextComponent::text(format!(
                "{} teleported to spawn",
                ctx.source().name()
            )))
        }));
        tree.register(literal("gamemode").requires_level(2).then(
            argument("mode", ArgumentType::integer(0, 3)).executes(|ctx| {
                let mode = ctx.integer("mode").unwrap_or_default();
                CommandResult::success(TextComponent::text(format!("gamemode set to {mode}")))
            }),
        ));
        tree.register(
            literal("say")
                .requires_permission("ferrumc.command.say")
                .then(
                    argument("message", ArgumentType::GreedyString).executes(|ctx| {
                        let message = ctx.string("message").unwrap_or_default();
                        CommandResult::success(TextComponent::text(format!(
                            "{}: {message}",
                            ctx.source().name()
                        )))
                    }),
                ),
        );
        tree.register(
            literal("seed").executes(|_| CommandResult::success(TextComponent::text("seed: 42"))),
        );
        tree
    }

    #[test]
    fn dispatch_happy_path_literal() {
        let tree = sample_tree();
        let result = tree.dispatch("spawn", &op()).expect("spawn dispatches");
        assert!(result.is_success());
        assert_eq!(
            result.feedback().to_plain_string(),
            "Console teleported to spawn"
        );
    }

    #[test]
    fn dispatch_happy_path_integer_argument() {
        let tree = sample_tree();
        let result = tree
            .dispatch("gamemode 1", &op())
            .expect("gamemode dispatches");
        assert!(result.is_success());
        assert_eq!(result.feedback().to_plain_string(), "gamemode set to 1");
    }

    #[test]
    fn dispatch_trims_surrounding_whitespace() {
        let tree = sample_tree();
        let result = tree
            .dispatch("   gamemode   3  ", &op())
            .expect("dispatches");
        assert_eq!(result.feedback().to_plain_string(), "gamemode set to 3");
    }

    #[test]
    fn unknown_command_is_rejected() {
        let tree = sample_tree();
        let err = tree
            .dispatch("fly", &op())
            .expect_err("fly is not registered");
        assert_eq!(err, CommandError::UnknownCommand("fly".to_string()));
    }

    #[test]
    fn empty_input_is_rejected() {
        let tree = sample_tree();
        assert_eq!(tree.dispatch("", &op()), Err(CommandError::EmptyInput));
        assert_eq!(tree.dispatch("    ", &op()), Err(CommandError::EmptyInput));
    }

    #[test]
    fn missing_argument_is_rejected() {
        let tree = sample_tree();
        let err = tree
            .dispatch("gamemode", &op())
            .expect_err("gamemode needs a mode");
        assert!(matches!(err, CommandError::MissingArgument(_)));
    }

    #[test]
    fn invalid_argument_is_rejected() {
        let tree = sample_tree();
        let err = tree
            .dispatch("gamemode creative", &op())
            .expect_err("creative is not an integer");
        match err {
            CommandError::InvalidArgument { name, .. } => assert_eq!(name, "mode"),
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn integer_out_of_range_is_rejected() {
        let tree = sample_tree();
        let err = tree
            .dispatch("gamemode 9", &op())
            .expect_err("9 is out of range");
        assert_eq!(
            err,
            CommandError::IntegerOutOfRange {
                name: "mode".to_string(),
                value: 9,
                min: 0,
                max: 3,
            }
        );
    }

    #[test]
    fn too_many_arguments_is_rejected() {
        let tree = sample_tree();
        let err = tree
            .dispatch("spawn extra", &op())
            .expect_err("spawn takes no arguments");
        assert_eq!(err, CommandError::TooManyArguments("extra".to_string()));
    }

    #[test]
    fn greedy_string_consumes_the_rest() {
        let tree = sample_tree();
        let result = tree
            .dispatch_with("say hello there world", &op(), &|node| {
                node == "ferrumc.command.say"
            })
            .expect("say dispatches with permission");
        assert_eq!(
            result.feedback().to_plain_string(),
            "Console: hello there world"
        );
    }

    #[test]
    fn permission_level_denied_does_not_run_handler() {
        let ran = Arc::new(AtomicUsize::new(0));
        let ran_in_handler = Arc::clone(&ran);

        let mut tree = CommandTree::new();
        tree.register(literal("gamemode").requires_level(2).then(
            argument("mode", ArgumentType::integer(0, 3)).executes(move |_| {
                ran_in_handler.fetch_add(1, Ordering::SeqCst);
                CommandResult::success(TextComponent::text("ran"))
            }),
        ));

        let err = tree
            .dispatch("gamemode 1", &member())
            .expect_err("member lacks level 2");
        assert!(matches!(err, CommandError::PermissionDenied(_)));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "handler must not run");

        // The same source at a sufficient level succeeds.
        let allowed = tree
            .dispatch(
                "gamemode 1",
                &CommandSource::for_player(ferrumc_core::PlayerId::offline("Steve"), "Steve", 2),
            )
            .expect("level 2 may run gamemode");
        assert!(allowed.is_success());
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn permission_node_checker_gates_dispatch() {
        let tree = sample_tree();

        // Checker grants the node: command runs.
        let ok = tree
            .dispatch_with("say hi", &op(), &|node| node == "ferrumc.command.say")
            .expect("granted node runs");
        assert!(ok.is_success());

        // Checker denies the node: dispatch fails before the handler.
        let denied = tree
            .dispatch_with("say hi", &op(), &|_| false)
            .expect_err("denied node is rejected");
        assert!(matches!(denied, CommandError::PermissionDenied(_)));
    }

    #[test]
    fn level_only_dispatch_ignores_node_permissions() {
        // `dispatch` cannot evaluate node-string permissions, so it treats them
        // as satisfied. `say` has level 0 requirements, so it runs.
        let tree = sample_tree();
        let result = tree
            .dispatch("say hi", &op())
            .expect("level-only allows node perms");
        assert!(result.is_success());
    }

    #[test]
    fn input_too_long_is_rejected() {
        let tree = sample_tree();
        let long = "a".repeat(MAX_INPUT_LEN + 1);
        let err = tree
            .dispatch(&long, &op())
            .expect_err("input exceeds the cap");
        assert!(matches!(err, CommandError::InputTooLong { .. }));
        assert!(tree.suggest(&long).is_empty());
    }

    #[test]
    fn suggestions_for_empty_input_list_all_commands() {
        let tree = sample_tree();
        let suggestions = tree.suggest("");
        assert_eq!(suggestions, vec!["spawn", "gamemode", "say", "seed"]);
    }

    #[test]
    fn suggestions_filter_by_prefix() {
        let tree = sample_tree();
        assert_eq!(tree.suggest("s"), vec!["spawn", "say", "seed"]);
        assert_eq!(tree.suggest("ga"), vec!["gamemode"]);
        assert!(tree.suggest("xyz").is_empty());
    }

    #[test]
    fn suggestions_offer_argument_hint() {
        let tree = sample_tree();
        assert_eq!(tree.suggest("gamemode "), vec!["<mode: 0..3>"]);
        // A node with no children offers nothing further.
        assert!(tree.suggest("spawn ").is_empty());
    }

    #[test]
    fn suggestions_for_unknown_path_are_empty() {
        let tree = sample_tree();
        assert!(tree.suggest("nope ").is_empty());
    }
}
