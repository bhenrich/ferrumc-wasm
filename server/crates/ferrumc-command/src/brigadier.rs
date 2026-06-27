//! Lowering a [`CommandTree`](crate::CommandTree) into the Brigadier command-node
//! graph the clientbound `Commands` (`declare_commands`) packet carries.
//!
//! The vanilla client only renders a command name as valid (non-red) and offers
//! autocomplete for it once the server has declared the command graph. This module
//! flattens the tree's parent/child node structure into the flat, index-addressed
//! node array Brigadier expects ([`BrigadierGraph`]), filtered to the commands a
//! player at a given permission level may use, and serializes that array to the
//! exact wire bytes ([`BrigadierGraph::encode`]).
//!
//! # Wire format (protocol 772)
//!
//! The body is `varint nodeCount`, then each node, then `varint rootIndex`. A node
//! is a 1-byte `flags` bitfield, a `varint`-prefixed array of child indices, an
//! optional redirect index, and per-node-type extra data:
//!
//! ```text
//! flags: bit[1:0] node type (0=root, 1=literal, 2=argument)
//!        bit[2]   has_command         (executable here)
//!        bit[3]   has_redirect_node   (unused here)
//!        bit[4]   has_custom_suggestions (unused here)
//!        bit[5]   allows_restricted   (unused here)
//! literal  extra: name: string
//! argument extra: name: string, parser: varint, properties, [suggestionType]
//! ```

use ferrumc_codec::write_var_int;

use crate::argument::ArgumentType;
use crate::builder::{CommandNode, NodeKind};

/// Mask selecting the 2-bit node-type field (bits `[1:0]`) of a node's flags byte.
const NODE_TYPE_MASK: u8 = 0x03;
/// Node-type value for the synthetic root node.
const NODE_TYPE_ROOT: u8 = 0;
/// Node-type value for a literal (fixed-keyword) node.
const NODE_TYPE_LITERAL: u8 = 1;
/// Node-type value for an argument (typed-value) node.
const NODE_TYPE_ARGUMENT: u8 = 2;
/// Flags bit `[2]`: the command may be executed when parsing ends at this node.
const FLAG_HAS_COMMAND: u8 = 0x04;

/// Brigadier parser id for `brigadier:integer`.
const PARSER_INTEGER: i32 = 3;
/// Brigadier parser id for `brigadier:string`.
const PARSER_STRING: i32 = 5;
/// `brigadier:string` mode reading a single whitespace-delimited word.
const STRING_MODE_SINGLE_WORD: i32 = 0;
/// `brigadier:string` mode reading the entire rest of the input greedily.
const STRING_MODE_GREEDY_PHRASE: i32 = 2;

/// `brigadier:integer` properties flag bit `[0]`: a minimum bound follows.
const INT_FLAG_MIN_PRESENT: u8 = 0x01;
/// `brigadier:integer` properties flag bit `[1]`: a maximum bound follows.
const INT_FLAG_MAX_PRESENT: u8 = 0x02;

/// A flattened Brigadier command graph: an index-addressed node array plus the
/// index of the root node the client starts parsing from.
///
/// Produced by [`CommandTree::to_brigadier`](crate::CommandTree::to_brigadier) and
/// serialized to the `Commands` packet body by [`BrigadierGraph::encode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrigadierGraph {
    nodes: Vec<BrigadierNode>,
    root_index: u32,
}

impl BrigadierGraph {
    /// The nodes of the graph, in wire (flatten) order; child indices reference
    /// positions in this slice.
    pub fn nodes(&self) -> &[BrigadierNode] {
        &self.nodes
    }

    /// The index, into [`nodes`](Self::nodes), of the root node.
    pub fn root_index(&self) -> u32 {
        self.root_index
    }

    /// Serializes the graph into the `Commands` packet body: `varint nodeCount`,
    /// each node's bytes in order, then the trailing `varint rootIndex`.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        write_var_int(&mut buf, len_as_var(self.nodes.len()));
        for node in &self.nodes {
            node.encode(&mut buf);
        }
        write_var_int(&mut buf, index_as_var(self.root_index));
        buf
    }
}

/// A single node in a [`BrigadierGraph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrigadierNode {
    flags: u8,
    children: Vec<u32>,
    redirect: Option<u32>,
    extra: BrigadierExtra,
}

impl BrigadierNode {
    /// The node's flags byte (node type in bits `[1:0]`, `has_command` in bit
    /// `[2]`, and the unused redirect/suggestion/restricted bits above it).
    pub fn flags(&self) -> u8 {
        self.flags
    }

    /// The 2-bit node-type value (0 = root, 1 = literal, 2 = argument).
    pub fn node_type(&self) -> u8 {
        self.flags & NODE_TYPE_MASK
    }

    /// `true` if the command may be executed when a parse ends at this node.
    pub fn has_command(&self) -> bool {
        self.flags & FLAG_HAS_COMMAND != 0
    }

    /// The indices, into [`BrigadierGraph::nodes`], of this node's children.
    pub fn children(&self) -> &[u32] {
        &self.children
    }

    /// The redirect-target index, if any. Always `None` in the current lowering
    /// (redirects are unused).
    pub fn redirect(&self) -> Option<u32> {
        self.redirect
    }

    /// The per-node-type extra data (root marker, literal name, or argument spec).
    pub fn extra(&self) -> &BrigadierExtra {
        &self.extra
    }

    /// The literal or argument name of this node, or `None` for the root.
    pub fn name(&self) -> Option<&str> {
        match &self.extra {
            BrigadierExtra::Root => None,
            BrigadierExtra::Literal { name } | BrigadierExtra::Argument { name, .. } => Some(name),
        }
    }

    /// Serializes this node: flags, the child-index array, an optional redirect,
    /// then the type-specific extra data.
    fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(self.flags);
        write_var_int(buf, len_as_var(self.children.len()));
        for &child in &self.children {
            write_var_int(buf, index_as_var(child));
        }
        if let Some(redirect) = self.redirect {
            write_var_int(buf, index_as_var(redirect));
        }
        match &self.extra {
            BrigadierExtra::Root => {}
            BrigadierExtra::Literal { name } => write_string(buf, name),
            BrigadierExtra::Argument {
                name,
                parser_id,
                props,
            } => {
                write_string(buf, name);
                write_var_int(buf, *parser_id);
                props.encode(buf);
            }
        }
    }
}

/// The per-node-type extra data of a [`BrigadierNode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrigadierExtra {
    /// The synthetic root node; carries no extra data.
    Root,
    /// A literal (fixed-keyword) node.
    Literal {
        /// The keyword the client must type to match this node.
        name: String,
    },
    /// An argument (typed-value) node.
    Argument {
        /// The argument's display name (shown in the client's autocomplete hint).
        name: String,
        /// The Brigadier parser id (e.g. 3 = `brigadier:integer`, 5 =
        /// `brigadier:string`).
        parser_id: i32,
        /// The parser-specific properties (bounds, string mode, ...).
        props: BrigadierProps,
    },
}

/// Parser-specific properties for an [`BrigadierExtra::Argument`] node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrigadierProps {
    /// `brigadier:integer` bounds. Each bound is `Some` only when it constrains
    /// the value (a sentinel-wide bound is omitted, matching vanilla's
    /// min/max-present flags).
    Integer {
        /// Inclusive minimum bound, if constrained.
        min: Option<i32>,
        /// Inclusive maximum bound, if constrained.
        max: Option<i32>,
    },
    /// `brigadier:string` read mode (0 = single word, 2 = greedy phrase).
    String {
        /// The string-reading mode varint.
        mode: i32,
    },
}

impl BrigadierProps {
    /// Serializes the properties body that follows the parser id.
    fn encode(&self, buf: &mut Vec<u8>) {
        match self {
            Self::Integer { min, max } => {
                let mut flags = 0u8;
                if min.is_some() {
                    flags |= INT_FLAG_MIN_PRESENT;
                }
                if max.is_some() {
                    flags |= INT_FLAG_MAX_PRESENT;
                }
                buf.push(flags);
                if let Some(min) = min {
                    buf.extend_from_slice(&min.to_be_bytes());
                }
                if let Some(max) = max {
                    buf.extend_from_slice(&max.to_be_bytes());
                }
            }
            Self::String { mode } => write_var_int(buf, *mode),
        }
    }
}

/// Flattens `root` and its (permission-permitted) descendants into a
/// [`BrigadierGraph`].
///
/// The synthetic root is always present at index 0. A descendant whose
/// `required_level` exceeds `player_level` is dropped along with its whole subtree,
/// and the surviving child indices are renumbered to match.
pub(crate) fn lower(root: &CommandNode, player_level: u8) -> BrigadierGraph {
    // The root is synthetic and never permission-gated, so it always occupies
    // index 0; only its descendants are subject to filtering.
    let mut nodes = vec![BrigadierNode {
        flags: NODE_TYPE_ROOT,
        children: Vec::new(),
        redirect: None,
        extra: BrigadierExtra::Root,
    }];
    let mut root_children = Vec::new();
    for child in &root.children {
        if let Some(index) = flatten(child, player_level, &mut nodes) {
            root_children.push(index);
        }
    }
    if let Some(root_node) = nodes.first_mut() {
        root_node.children = root_children;
    }
    BrigadierGraph {
        nodes,
        root_index: 0,
    }
}

/// Recursively appends `node` (if the player may use it) and its permitted
/// children to `nodes`, returning the index assigned to `node`, or `None` if it
/// was filtered out.
fn flatten(node: &CommandNode, player_level: u8, nodes: &mut Vec<BrigadierNode>) -> Option<u32> {
    if let Some(required) = node.required_level {
        if player_level < required {
            return None;
        }
    }

    // Reserve this node's slot before recursing so children get later indices.
    let my_index = u32::try_from(nodes.len()).ok()?;
    nodes.push(build_node(node));

    let mut child_indices = Vec::new();
    for child in &node.children {
        if let Some(index) = flatten(child, player_level, nodes) {
            child_indices.push(index);
        }
    }
    if let Some(slot) = nodes.get_mut(my_index as usize) {
        slot.children = child_indices;
    }
    Some(my_index)
}

/// Builds a single (childless) [`BrigadierNode`] from a command-tree node; the
/// caller fills in the child indices after recursing.
fn build_node(node: &CommandNode) -> BrigadierNode {
    let (type_bits, extra) = match &node.kind {
        NodeKind::Root => (NODE_TYPE_ROOT, BrigadierExtra::Root),
        NodeKind::Literal(name) => (
            NODE_TYPE_LITERAL,
            BrigadierExtra::Literal { name: name.clone() },
        ),
        NodeKind::Argument { name, arg_type } => {
            let (parser_id, props) = lower_argument(*arg_type);
            (
                NODE_TYPE_ARGUMENT,
                BrigadierExtra::Argument {
                    name: name.clone(),
                    parser_id,
                    props,
                },
            )
        }
    };
    let mut flags = type_bits;
    if node.handler.is_some() {
        flags |= FLAG_HAS_COMMAND;
    }
    BrigadierNode {
        flags,
        children: Vec::new(),
        redirect: None,
        extra,
    }
}

/// Maps an [`ArgumentType`] to its Brigadier parser id and properties.
fn lower_argument(arg_type: ArgumentType) -> (i32, BrigadierProps) {
    match arg_type {
        ArgumentType::Word => (
            PARSER_STRING,
            BrigadierProps::String {
                mode: STRING_MODE_SINGLE_WORD,
            },
        ),
        ArgumentType::GreedyString => (
            PARSER_STRING,
            BrigadierProps::String {
                mode: STRING_MODE_GREEDY_PHRASE,
            },
        ),
        ArgumentType::Integer { min, max } => (
            PARSER_INTEGER,
            BrigadierProps::Integer {
                min: present_bound(min, i32::MIN),
                max: present_bound(max, i32::MAX),
            },
        ),
    }
}

/// Narrows an `i64` command-tree bound to the `i32` Brigadier wire bound, treating
/// a value at (or beyond) the given `i32` extreme as "unbounded" (`None`), so the
/// matching min/max-present flag is cleared.
fn present_bound(value: i64, sentinel: i32) -> Option<i32> {
    let clamped = value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    if clamped == sentinel {
        None
    } else {
        Some(clamped)
    }
}

/// Encodes a collection length as the `i32` a `VarInt` count expects, saturating
/// rather than wrapping on the (unreachable) overflow.
fn len_as_var(len: usize) -> i32 {
    i32::try_from(len).unwrap_or(i32::MAX)
}

/// Encodes a node index as the `i32` a `VarInt` expects, saturating rather than
/// wrapping on the (unreachable) overflow.
fn index_as_var(index: u32) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

/// Writes a Brigadier `string`: a `VarInt` byte-length prefix then the UTF-8 bytes.
fn write_string(buf: &mut Vec<u8>, value: &str) {
    write_var_int(buf, len_as_var(value.len()));
    buf.extend_from_slice(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{argument, literal};
    use crate::result::CommandResult;
    use crate::CommandTree;

    use ferrumc_core::TextComponent;

    /// A tree mirroring the application's: an executable `/spawn`, and a
    /// level-2-gated `/gamemode <mode: 0..3>` whose argument node is executable.
    fn app_tree() -> CommandTree {
        let mut tree = CommandTree::new();
        tree.register(
            literal("spawn").executes(|_| CommandResult::success(TextComponent::text("spawn"))),
        );
        tree.register(
            literal("gamemode").requires_level(2).then(
                argument("mode", ArgumentType::integer(0, 3))
                    .executes(|_| CommandResult::success(TextComponent::text("gm"))),
            ),
        );
        tree
    }

    #[test]
    fn to_brigadier_op_has_both_commands_with_typed_argument() {
        let graph = app_tree().to_brigadier(4);

        // Root is present at the reported index and is genuinely a root node.
        assert_eq!(graph.root_index(), 0);
        let root = &graph.nodes()[graph.root_index() as usize];
        assert!(matches!(root.extra(), BrigadierExtra::Root));
        assert_eq!(root.node_type(), NODE_TYPE_ROOT);

        // Root points at exactly the two top-level literals.
        let child_names: Vec<&str> = root
            .children()
            .iter()
            .filter_map(|&i| graph.nodes()[i as usize].name())
            .collect();
        assert_eq!(child_names, vec!["spawn", "gamemode"]);

        // `/spawn` is an executable literal.
        let spawn = graph
            .nodes()
            .iter()
            .find(|n| n.name() == Some("spawn"))
            .expect("spawn node");
        assert_eq!(spawn.node_type(), NODE_TYPE_LITERAL);
        assert!(spawn.has_command());

        // `/gamemode` is a non-executable literal with one argument child.
        let gamemode = graph
            .nodes()
            .iter()
            .find(|n| n.name() == Some("gamemode"))
            .expect("gamemode node");
        assert_eq!(gamemode.node_type(), NODE_TYPE_LITERAL);
        assert!(!gamemode.has_command());
        assert_eq!(gamemode.children().len(), 1);

        // The `mode` argument lowers to brigadier:integer with 0..=3 bounds.
        let mode = graph
            .nodes()
            .iter()
            .find(|n| n.name() == Some("mode"))
            .expect("mode node");
        assert_eq!(mode.node_type(), NODE_TYPE_ARGUMENT);
        assert!(mode.has_command());
        match mode.extra() {
            BrigadierExtra::Argument {
                parser_id, props, ..
            } => {
                assert_eq!(*parser_id, PARSER_INTEGER);
                assert_eq!(
                    *props,
                    BrigadierProps::Integer {
                        min: Some(0),
                        max: Some(3)
                    }
                );
            }
            other => panic!("expected an argument node, got {other:?}"),
        }
    }

    #[test]
    fn to_brigadier_level_zero_hides_gated_gamemode() {
        let graph = app_tree().to_brigadier(0);
        assert!(graph.nodes().iter().any(|n| n.name() == Some("spawn")));
        assert!(graph.nodes().iter().all(|n| n.name() != Some("gamemode")));
        assert!(graph.nodes().iter().all(|n| n.name() != Some("mode")));
        // Root keeps only the surviving `/spawn` child.
        let root = &graph.nodes()[graph.root_index() as usize];
        assert_eq!(root.children().len(), 1);
    }

    #[test]
    fn encode_commands_body_op_matches_expected_bytes() {
        let body = app_tree().encode_commands_body(4);

        let mut expected = Vec::new();
        expected.push(0x04); // node count = 4
                             // node 0: root, children [1, 2]
        expected.extend_from_slice(&[0x00, 0x02, 0x01, 0x02]);
        // node 1: literal "spawn", executable (0x01 | 0x04), no children
        expected.extend_from_slice(&[0x05, 0x00, 0x05]);
        expected.extend_from_slice(b"spawn");
        // node 2: literal "gamemode", not executable, child [3]
        expected.extend_from_slice(&[0x01, 0x01, 0x03, 0x08]);
        expected.extend_from_slice(b"gamemode");
        // node 3: argument "mode", executable (0x02 | 0x04), parser 3, min+max present
        expected.extend_from_slice(&[0x06, 0x00, 0x04]);
        expected.extend_from_slice(b"mode");
        expected.extend_from_slice(&[PARSER_INTEGER as u8, 0x03]);
        expected.extend_from_slice(&0i32.to_be_bytes());
        expected.extend_from_slice(&3i32.to_be_bytes());
        expected.push(0x00); // root index = 0

        assert_eq!(body, expected);
    }

    #[test]
    fn encode_commands_body_level_zero_drops_gamemode_subtree() {
        let body = app_tree().encode_commands_body(0);

        let mut expected = Vec::new();
        expected.push(0x02); // node count = 2 (root + spawn)
        expected.extend_from_slice(&[0x00, 0x01, 0x01]); // root, child [1]
        expected.extend_from_slice(&[0x05, 0x00, 0x05]); // spawn flags/children/len
        expected.extend_from_slice(b"spawn");
        expected.push(0x00); // root index = 0

        assert_eq!(body, expected);
    }

    #[test]
    fn greedy_and_word_arguments_lower_to_string_parser() {
        let mut tree = CommandTree::new();
        tree.register(
            literal("say").then(
                argument("message", ArgumentType::GreedyString)
                    .executes(|_| CommandResult::success(TextComponent::text("said"))),
            ),
        );
        tree.register(
            literal("whisper").then(
                argument("word", ArgumentType::Word)
                    .executes(|_| CommandResult::success(TextComponent::text("whispered"))),
            ),
        );
        let graph = tree.to_brigadier(0);

        let message = graph
            .nodes()
            .iter()
            .find(|n| n.name() == Some("message"))
            .expect("message node");
        match message.extra() {
            BrigadierExtra::Argument {
                parser_id, props, ..
            } => {
                assert_eq!(*parser_id, PARSER_STRING);
                assert_eq!(
                    *props,
                    BrigadierProps::String {
                        mode: STRING_MODE_GREEDY_PHRASE
                    }
                );
            }
            other => panic!("expected argument, got {other:?}"),
        }

        let word = graph
            .nodes()
            .iter()
            .find(|n| n.name() == Some("word"))
            .expect("word node");
        match word.extra() {
            BrigadierExtra::Argument { props, .. } => assert_eq!(
                *props,
                BrigadierProps::String {
                    mode: STRING_MODE_SINGLE_WORD
                }
            ),
            other => panic!("expected argument, got {other:?}"),
        }
    }
}
