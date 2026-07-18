//! Bounded pure-data command registration and invocation values.

use core::fmt;
use core::num::NonZeroU64;

use crate::{CommandError, PermissionNode, PlayerId};

/// Maximum nodes in one registered command tree.
pub const MAX_COMMAND_NODES: usize = 256;

/// Maximum arguments in one command invocation.
pub const MAX_COMMAND_ARGUMENTS: usize = 64;

/// Maximum byte length of a command node or invocation-argument name.
pub const MAX_COMMAND_NAME_BYTES: usize = 64;

/// Maximum byte length of one text command-argument value.
pub const MAX_COMMAND_TEXT_BYTES: usize = 4_096;

/// Maximum aggregate encoded size of one command invocation.
pub const MAX_COMMAND_INVOCATION_BYTES: usize = 64 * 1024;

/// Stable nonzero identifier for a plugin command handler.
///
/// An adapter records registrations by this value and routes a later command
/// invocation back to [`Plugin::on_command`](crate::Plugin::on_command).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HandlerId(NonZeroU64);

impl HandlerId {
    /// Creates a handler identifier, returning `None` for the reserved zero.
    pub const fn new(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the nonzero numeric identifier.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for HandlerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Inclusive bounds for an integer command argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegerBounds {
    min: i64,
    max: i64,
}

impl IntegerBounds {
    /// Creates inclusive integer bounds.
    pub const fn new(min: i64, max: i64) -> Result<Self, CommandError> {
        if min > max {
            return Err(CommandError::ReversedIntegerBounds { min, max });
        }
        Ok(Self { min, max })
    }

    /// Returns the inclusive minimum.
    pub const fn min(self) -> i64 {
        self.min
    }

    /// Returns the inclusive maximum.
    pub const fn max(self) -> i64 {
        self.max
    }
}

/// The parser shape of one command node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommandNodeKind {
    /// Match one literal word.
    Literal,
    /// Parse one non-whitespace word.
    Word,
    /// Parse the remaining input as text.
    GreedyText,
    /// Parse one bounded signed integer.
    Integer(IntegerBounds),
}

/// One node in a preorder command-tree definition.
///
/// Parent indices always refer to an earlier node. The first node is the only
/// root. Nodes contain stable handler identifiers rather than closures or
/// function pointers, so the same definition crosses either packaging adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandNode {
    parent: Option<usize>,
    kind: CommandNodeKind,
    name: String,
    handler: Option<HandlerId>,
    required_level: Option<u8>,
    required_permission: Option<PermissionNode>,
}

impl CommandNode {
    /// Creates a non-executable command node.
    pub fn new(
        parent: Option<usize>,
        kind: CommandNodeKind,
        name: impl Into<String>,
    ) -> Result<Self, CommandError> {
        let name = name.into();
        validate_name("command node", &name)?;
        Ok(Self {
            parent,
            kind,
            name,
            handler: None,
            required_level: None,
            required_permission: None,
        })
    }

    /// Marks this node executable through a stable handler identifier.
    #[must_use]
    pub const fn with_handler(mut self, handler: HandlerId) -> Self {
        self.handler = Some(handler);
        self
    }

    /// Requires a `Minecraft` operator level from zero through four.
    pub fn with_required_level(mut self, level: u8) -> Result<Self, CommandError> {
        if level > 4 {
            return Err(CommandError::InvalidOperatorLevel { level });
        }
        self.required_level = Some(level);
        Ok(self)
    }

    /// Requires a validated permission node.
    #[must_use]
    pub fn with_required_permission(mut self, permission: PermissionNode) -> Self {
        self.required_permission = Some(permission);
        self
    }

    /// Returns the preorder parent index, or `None` for the root.
    pub const fn parent(&self) -> Option<usize> {
        self.parent
    }

    /// Returns the node parser shape.
    pub const fn kind(&self) -> CommandNodeKind {
        self.kind
    }

    /// Returns the node name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the handler if this node is executable.
    pub const fn handler(&self) -> Option<HandlerId> {
        self.handler
    }

    /// Returns the required operator level.
    pub const fn required_level(&self) -> Option<u8> {
        self.required_level
    }

    /// Returns the required permission.
    pub const fn required_permission(&self) -> Option<&PermissionNode> {
        self.required_permission.as_ref()
    }
}

/// One validated, bounded command tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDefinition {
    nodes: Vec<CommandNode>,
}

impl CommandDefinition {
    /// Validates nodes in preorder and builds a command definition.
    pub fn new(nodes: Vec<CommandNode>) -> Result<Self, CommandError> {
        if nodes.is_empty() {
            return Err(CommandError::EmptyTree);
        }
        if nodes.len() > MAX_COMMAND_NODES {
            return Err(CommandError::TooMany {
                resource: "command node",
                len: nodes.len(),
                max: MAX_COMMAND_NODES,
            });
        }
        let mut ancestry = Vec::new();
        for (index, node) in nodes.iter().enumerate() {
            match (index, node.parent) {
                (0, None) => ancestry.push(0),
                (0, parent) => {
                    return Err(CommandError::InvalidParent {
                        node: index,
                        parent,
                    });
                }
                (_, None) => return Err(CommandError::MultipleRoots { node: index }),
                (_, Some(parent)) if parent < index => {
                    let Some(depth) = ancestry.iter().rposition(|ancestor| *ancestor == parent)
                    else {
                        return Err(CommandError::InvalidParent {
                            node: index,
                            parent: Some(parent),
                        });
                    };
                    ancestry.truncate(depth + 1);
                    ancestry.push(index);
                }
                (_, parent) => {
                    return Err(CommandError::InvalidParent {
                        node: index,
                        parent,
                    });
                }
            }
        }
        Ok(Self { nodes })
    }

    /// Returns nodes in their validated preorder.
    pub fn nodes(&self) -> &[CommandNode] {
        &self.nodes
    }
}

/// One typed command argument value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommandArgumentValue {
    /// Parsed text.
    Text(String),
    /// Parsed signed integer.
    Integer(i64),
}

/// One named argument in a command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandArgument {
    name: String,
    value: CommandArgumentValue,
}

impl CommandArgument {
    /// Creates a named text argument.
    pub fn text(name: impl Into<String>, value: impl Into<String>) -> Result<Self, CommandError> {
        let value = value.into();
        if value.len() > MAX_COMMAND_TEXT_BYTES {
            return Err(CommandError::TextTooLong {
                len: value.len(),
                max: MAX_COMMAND_TEXT_BYTES,
            });
        }
        Self::new(name.into(), CommandArgumentValue::Text(value))
    }

    /// Creates a named integer argument.
    pub fn integer(name: impl Into<String>, value: i64) -> Result<Self, CommandError> {
        Self::new(name.into(), CommandArgumentValue::Integer(value))
    }

    fn new(name: String, value: CommandArgumentValue) -> Result<Self, CommandError> {
        validate_name("command argument", &name)?;
        Ok(Self { name, value })
    }

    /// Returns the argument name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the typed argument value.
    pub const fn value(&self) -> &CommandArgumentValue {
        &self.value
    }
}

/// A validated invocation routed to one registered handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInvocation {
    handler: HandlerId,
    player: PlayerId,
    arguments: Vec<CommandArgument>,
}

impl CommandInvocation {
    /// Creates a bounded command invocation.
    pub fn new(
        handler: HandlerId,
        player: PlayerId,
        arguments: Vec<CommandArgument>,
    ) -> Result<Self, CommandError> {
        if arguments.len() > MAX_COMMAND_ARGUMENTS {
            return Err(CommandError::TooMany {
                resource: "command argument",
                len: arguments.len(),
                max: MAX_COMMAND_ARGUMENTS,
            });
        }
        let encoded_len = arguments.iter().try_fold(28usize, |total, argument| {
            let argument_len = match &argument.value {
                CommandArgumentValue::Text(value) => 12usize
                    .checked_add(argument.name.len())
                    .and_then(|length| length.checked_add(value.len())),
                CommandArgumentValue::Integer(_) => 16usize.checked_add(argument.name.len()),
            };
            argument_len.and_then(|length| total.checked_add(length))
        });
        let Some(encoded_len) = encoded_len else {
            return Err(CommandError::InvocationTooLarge {
                len: usize::MAX,
                max: MAX_COMMAND_INVOCATION_BYTES,
            });
        };
        if encoded_len > MAX_COMMAND_INVOCATION_BYTES {
            return Err(CommandError::InvocationTooLarge {
                len: encoded_len,
                max: MAX_COMMAND_INVOCATION_BYTES,
            });
        }
        Ok(Self {
            handler,
            player,
            arguments,
        })
    }

    /// Returns the registered handler identifier.
    pub const fn handler(&self) -> HandlerId {
        self.handler
    }

    /// Returns the player who invoked the command.
    pub const fn player(&self) -> PlayerId {
        self.player
    }

    /// Returns arguments in parser order.
    pub fn arguments(&self) -> &[CommandArgument] {
        &self.arguments
    }
}

fn validate_name(resource: &'static str, name: &str) -> Result<(), CommandError> {
    if name.is_empty() {
        return Err(CommandError::EmptyName { resource });
    }
    if name.len() > MAX_COMMAND_NAME_BYTES {
        return Err(CommandError::NameTooLong {
            resource,
            len: name.len(),
            max: MAX_COMMAND_NAME_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(raw: u64) -> HandlerId {
        HandlerId::new(raw).expect("test handler is nonzero")
    }

    #[test]
    fn pure_data_tree_preserves_preorder_and_handler_ids() {
        let root = CommandNode::new(None, CommandNodeKind::Literal, "region")
            .expect("root")
            .with_handler(handler(7));
        let bounds = IntegerBounds::new(-8, 8).expect("ordered bounds");
        let child = CommandNode::new(Some(0), CommandNodeKind::Integer(bounds), "radius")
            .expect("child")
            .with_handler(handler(9));
        let tree = CommandDefinition::new(vec![root, child]).expect("valid preorder");

        assert_eq!(tree.nodes()[1].parent(), Some(0));
        assert_eq!(tree.nodes()[1].handler(), Some(handler(9)));
        assert_eq!(
            tree.nodes()[1].kind(),
            CommandNodeKind::Integer(IntegerBounds::new(-8, 8).expect("bounds"))
        );
    }

    #[test]
    fn tree_rejects_empty_oversized_and_forward_parent_shapes() {
        assert_eq!(
            CommandDefinition::new(Vec::new()),
            Err(CommandError::EmptyTree)
        );
        let root = CommandNode::new(Some(1), CommandNodeKind::Literal, "bad").expect("node shape");
        assert!(matches!(
            CommandDefinition::new(vec![root]),
            Err(CommandError::InvalidParent { .. })
        ));

        let nodes = (0..=MAX_COMMAND_NODES)
            .map(|index| {
                CommandNode::new(
                    if index > 0 { Some(index - 1) } else { None },
                    CommandNodeKind::Literal,
                    format!("n{index}"),
                )
                .expect("bounded node name")
            })
            .collect();
        assert!(matches!(
            CommandDefinition::new(nodes),
            Err(CommandError::TooMany { .. })
        ));

        let non_preorder = vec![
            CommandNode::new(None, CommandNodeKind::Literal, "root").expect("root"),
            CommandNode::new(Some(0), CommandNodeKind::Literal, "first").expect("first child"),
            CommandNode::new(Some(0), CommandNodeKind::Literal, "second").expect("second child"),
            CommandNode::new(Some(1), CommandNodeKind::Literal, "late").expect("late grandchild"),
        ];
        assert!(matches!(
            CommandDefinition::new(non_preorder),
            Err(CommandError::InvalidParent {
                node: 3,
                parent: Some(1)
            })
        ));
    }

    #[test]
    fn zero_handler_and_reversed_bounds_are_rejected() {
        assert_eq!(HandlerId::new(0), None);
        assert_eq!(
            IntegerBounds::new(3, 2),
            Err(CommandError::ReversedIntegerBounds { min: 3, max: 2 })
        );
    }

    #[test]
    fn invocation_preserves_order_and_typed_arguments() {
        let player = PlayerId::offline("CommandUser");
        let invocation = CommandInvocation::new(
            handler(11),
            player,
            vec![
                CommandArgument::text("target", "spawn").expect("text argument"),
                CommandArgument::integer("radius", 12).expect("integer argument"),
            ],
        )
        .expect("invocation");

        assert_eq!(invocation.handler(), handler(11));
        assert_eq!(invocation.player(), player);
        assert_eq!(invocation.arguments()[0].name(), "target");
        assert_eq!(
            invocation.arguments()[1].value(),
            &CommandArgumentValue::Integer(12)
        );
    }

    #[test]
    fn invocation_text_and_aggregate_payloads_are_bounded() {
        assert!(matches!(
            CommandArgument::text("value", "x".repeat(MAX_COMMAND_TEXT_BYTES + 1)),
            Err(CommandError::TextTooLong { .. })
        ));

        let arguments = (0..MAX_COMMAND_ARGUMENTS)
            .map(|index| {
                CommandArgument::text(format!("a{index}"), "x".repeat(MAX_COMMAND_TEXT_BYTES))
                    .expect("individual value is bounded")
            })
            .collect();
        assert!(matches!(
            CommandInvocation::new(handler(12), PlayerId::offline("PayloadUser"), arguments),
            Err(CommandError::InvocationTooLarge { .. })
        ));

        let mut exact = (0..15)
            .map(|_| {
                CommandArgument::text("a", "x".repeat(4_081)).expect("individual value is bounded")
            })
            .collect::<Vec<_>>();
        exact.push(
            CommandArgument::text("a", "x".repeat(4_085)).expect("exact-boundary value is bounded"),
        );
        assert!(CommandInvocation::new(
            handler(13),
            PlayerId::offline("ExactPayloadUser"),
            exact.clone(),
        )
        .is_ok());

        exact.pop();
        exact.push(
            CommandArgument::text("a", "x".repeat(4_086))
                .expect("over-boundary value is individually bounded"),
        );
        assert!(matches!(
            CommandInvocation::new(handler(14), PlayerId::offline("OverPayloadUser"), exact),
            Err(CommandError::InvocationTooLarge {
                len: 65_537,
                max: MAX_COMMAND_INVOCATION_BYTES
            })
        ));
    }
}
