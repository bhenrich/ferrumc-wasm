//! The builder API for constructing command nodes and the underlying node type.

use crate::argument::ArgumentType;
use crate::context::CommandContext;
use crate::result::CommandResult;

/// A boxed command handler: runs against a [`CommandContext`] and returns a
/// [`CommandResult`].
pub(crate) type HandlerFn = Box<dyn Fn(&CommandContext<'_>) -> CommandResult + Send + Sync>;

/// What a node matches against during parsing.
pub(crate) enum NodeKind {
    /// The synthetic root of a tree; matches nothing itself.
    Root,
    /// A fixed keyword that must be matched verbatim.
    Literal(String),
    /// A named, typed argument parsed from the input.
    Argument {
        name: String,
        arg_type: ArgumentType,
    },
}

/// A single node in a command tree.
///
/// Internal representation; the public surface is [`CommandBuilder`].
pub(crate) struct CommandNode {
    pub(crate) kind: NodeKind,
    pub(crate) children: Vec<CommandNode>,
    pub(crate) handler: Option<HandlerFn>,
    pub(crate) required_level: Option<u8>,
    pub(crate) required_permission: Option<String>,
}

impl CommandNode {
    /// Creates a childless, non-executable node of the given `kind`.
    pub(crate) fn new(kind: NodeKind) -> Self {
        Self {
            kind,
            children: Vec::new(),
            handler: None,
            required_level: None,
            required_permission: None,
        }
    }

    /// Creates the synthetic root node of a tree.
    pub(crate) fn root() -> Self {
        Self::new(NodeKind::Root)
    }
}

/// A fluent builder for a command (sub)tree.
///
/// Start with [`literal`] or [`argument`], attach children with
/// [`CommandBuilder::then`], make a node executable with
/// [`CommandBuilder::executes`], and constrain access with
/// [`CommandBuilder::requires_level`] / [`CommandBuilder::requires_permission`].
/// Register the finished builder with [`crate::CommandTree::register`].
pub struct CommandBuilder {
    node: CommandNode,
}

impl CommandBuilder {
    /// Attaches `child` as a sub-node, returning the builder for chaining.
    #[must_use]
    pub fn then(mut self, child: CommandBuilder) -> Self {
        self.node.children.push(child.node);
        self
    }

    /// Makes this node executable, running `handler` when the parse ends here.
    #[must_use]
    pub fn executes<F>(mut self, handler: F) -> Self
    where
        F: Fn(&CommandContext<'_>) -> CommandResult + Send + Sync + 'static,
    {
        self.node.handler = Some(Box::new(handler));
        self
    }

    /// Requires the source's permission level to be at least `level` to traverse
    /// this node.
    #[must_use]
    pub fn requires_level(mut self, level: u8) -> Self {
        self.node.required_level = Some(level);
        self
    }

    /// Requires the opaque permission `node` string to be granted to traverse
    /// this node.
    ///
    /// The string is meaningful only to the permission backend; this crate never
    /// interprets it. It is enforced only when a checker is supplied via
    /// [`crate::CommandTree::dispatch_with`].
    #[must_use]
    pub fn requires_permission(mut self, node: impl Into<String>) -> Self {
        self.node.required_permission = Some(node.into());
        self
    }

    /// Consumes the builder, yielding the built node.
    pub(crate) fn into_node(self) -> CommandNode {
        self.node
    }
}

/// Starts a literal node that matches the keyword `name` verbatim.
pub fn literal(name: impl Into<String>) -> CommandBuilder {
    CommandBuilder {
        node: CommandNode::new(NodeKind::Literal(name.into())),
    }
}

/// Starts an argument node named `name` that parses values of `arg_type`.
pub fn argument(name: impl Into<String>, arg_type: ArgumentType) -> CommandBuilder {
    CommandBuilder {
        node: CommandNode::new(NodeKind::Argument {
            name: name.into(),
            arg_type,
        }),
    }
}
