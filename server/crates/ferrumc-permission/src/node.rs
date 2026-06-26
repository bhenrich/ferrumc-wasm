//! [`PermissionNode`]: validated, dotted-path permission identifiers.

use core::fmt;
use core::str::FromStr;

use crate::error::{NodeParseError, MAX_NODE_LEN};

/// The character used to separate node segments.
const SEPARATOR: char = '.';

/// The wildcard segment marker.
const WILDCARD: &str = "*";

/// Returns whether `ch` is permitted inside a literal (non-wildcard) segment.
///
/// The allowed set mirrors Minecraft namespaced-id conventions: lowercase ASCII
/// letters, ASCII digits, underscore, and hyphen.
const fn is_segment_char(ch: char) -> bool {
    matches!(ch, 'a'..='z' | '0'..='9' | '_' | '-')
}

/// How specifically a node matches a concrete target.
///
/// Ordering is the heart of [`PermissionSet`](crate::PermissionSet)
/// precedence: an exact match always outranks any wildcard match (the `Exact`
/// variant is declared last, so it compares greater regardless of the inner
/// count), and among wildcards a longer literal prefix outranks a shorter one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MatchSpecificity {
    /// A wildcard match, ranked by the number of literal segments before `*`
    /// (the root `*` has zero).
    Wildcard(usize),
    /// An exact, segment-for-segment match, ranked by segment count.
    Exact(usize),
}

/// A validated, dotted-path permission identifier such as
/// `ferrumc.command.gamemode`.
///
/// A node is a non-empty sequence of `.`-separated segments. Each literal
/// segment is non-empty and contains only `a`-`z`, `0`-`9`, `_`, or `-`. A node
/// may be a *wildcard*: either the bare root `*` (matches everything) or a
/// trailing `.*` (for example `ferrumc.command.*`, which matches every concrete
/// node strictly beneath `ferrumc.command`).
///
/// Construct one with [`PermissionNode::parse`] (or the [`FromStr`] /
/// [`TryFrom`] impls), which reject malformed input with a [`NodeParseError`].
/// The internal representation is hidden; use [`PermissionNode::as_str`],
/// [`PermissionNode::is_wildcard`], and [`PermissionNode::matches`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PermissionNode {
    /// The validated, canonical string (segments joined by `.`).
    value: String,
    /// Whether the final segment is the `*` wildcard.
    wildcard: bool,
}

impl PermissionNode {
    /// Parses and validates a permission node from `input`.
    ///
    /// # Errors
    ///
    /// Returns a [`NodeParseError`] if `input` is empty, exceeds
    /// [`MAX_NODE_LEN`], has an empty segment, contains a disallowed character,
    /// or places a `*` anywhere but as a whole trailing segment.
    pub fn parse(input: &str) -> Result<Self, NodeParseError> {
        if input.is_empty() {
            return Err(NodeParseError::Empty);
        }
        if input.len() > MAX_NODE_LEN {
            return Err(NodeParseError::TooLong { len: input.len() });
        }

        let mut wildcard = false;
        let mut segments = input.split(SEPARATOR).peekable();
        while let Some(segment) = segments.next() {
            if segment.is_empty() {
                return Err(NodeParseError::EmptySegment);
            }

            let is_last = segments.peek().is_none();
            if segment == WILDCARD {
                // A `*` is only legal as the whole final segment.
                if !is_last {
                    return Err(NodeParseError::MisplacedWildcard);
                }
                wildcard = true;
                continue;
            }

            for ch in segment.chars() {
                // A `*` reaching this point is embedded in a longer segment.
                if ch == '*' {
                    return Err(NodeParseError::MisplacedWildcard);
                }
                if !is_segment_char(ch) {
                    return Err(NodeParseError::InvalidCharacter { character: ch });
                }
            }
        }

        Ok(Self {
            value: input.to_owned(),
            wildcard,
        })
    }

    /// Returns the canonical string form of this node.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Returns whether this node is a wildcard (the root `*` or a trailing
    /// `.*`).
    pub const fn is_wildcard(&self) -> bool {
        self.wildcard
    }

    /// Returns the number of segments, counting a trailing `*` as one segment.
    ///
    /// `ferrumc.command.gamemode` has three segments; `ferrumc.command.*` has
    /// three; the root `*` has one.
    pub fn segment_count(&self) -> usize {
        self.value.split(SEPARATOR).count()
    }

    /// Returns the literal segments, excluding a trailing `*` wildcard.
    fn literal_segments(&self) -> impl Iterator<Item = &str> {
        let count = self.segment_count();
        let take = if self.wildcard { count - 1 } else { count };
        self.value.split(SEPARATOR).take(take)
    }

    /// Computes how specifically `self` matches the concrete node `target`, or
    /// `None` if it does not match.
    ///
    /// Returns `None` when `target` is itself a wildcard: matching is only
    /// defined against concrete targets.
    pub(crate) fn match_specificity(&self, target: &Self) -> Option<MatchSpecificity> {
        if target.wildcard {
            return None;
        }

        if !self.wildcard {
            // Concrete vs concrete: exact string equality.
            return (self.value == target.value)
                .then(|| MatchSpecificity::Exact(self.segment_count()));
        }

        // Wildcard vs concrete: every literal segment of `self` must equal the
        // corresponding leading segment of `target`, and `target` must have at
        // least one further segment (a wildcard never matches its own prefix).
        let mut literal_count = 0usize;
        let mut target_segments = target.value.split(SEPARATOR);
        for literal in self.literal_segments() {
            match target_segments.next() {
                Some(seg) if seg == literal => literal_count += 1,
                _ => return None,
            }
        }

        // Require a strictly deeper target so `a.b.*` matches `a.b.c` but not
        // `a.b`. The root `*` (no literals) matches every non-empty target.
        target_segments
            .next()
            .map(|_| MatchSpecificity::Wildcard(literal_count))
    }

    /// Returns whether this node matches the concrete node `target`.
    ///
    /// A concrete node matches only an identical node. A wildcard node matches
    /// every concrete node strictly beneath its literal prefix: `ferrumc.*`
    /// matches `ferrumc.command` and `ferrumc.command.gamemode`, but not
    /// `ferrumc` itself, and the root `*` matches every concrete node. Matching
    /// against a wildcard `target` always returns `false`.
    pub fn matches(&self, target: &Self) -> bool {
        self.match_specificity(target).is_some()
    }
}

impl fmt::Display for PermissionNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

impl FromStr for PermissionNode {
    type Err = NodeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl TryFrom<&str> for PermissionNode {
    type Error = NodeParseError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<String> for PermissionNode {
    type Error = NodeParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(s: &str) -> PermissionNode {
        PermissionNode::parse(s).expect("valid node")
    }

    #[test]
    fn parses_valid_concrete_node() {
        let n = node("ferrumc.command.gamemode");
        assert_eq!(n.as_str(), "ferrumc.command.gamemode");
        assert!(!n.is_wildcard());
        assert_eq!(n.segment_count(), 3);
        assert_eq!(n.to_string(), "ferrumc.command.gamemode");
    }

    #[test]
    fn parses_allowed_characters() {
        let n = node("a-b_c.d0-9_e");
        assert!(!n.is_wildcard());
        assert_eq!(n.segment_count(), 2);
    }

    #[test]
    fn parses_trailing_wildcard() {
        let n = node("ferrumc.command.*");
        assert!(n.is_wildcard());
        assert_eq!(n.segment_count(), 3);
    }

    #[test]
    fn parses_root_wildcard() {
        let n = node("*");
        assert!(n.is_wildcard());
        assert_eq!(n.segment_count(), 1);
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(PermissionNode::parse(""), Err(NodeParseError::Empty));
    }

    #[test]
    fn rejects_too_long() {
        let long = "a".repeat(MAX_NODE_LEN + 1);
        assert_eq!(
            PermissionNode::parse(&long),
            Err(NodeParseError::TooLong {
                len: MAX_NODE_LEN + 1
            })
        );
        // A node exactly at the limit is accepted.
        assert!(PermissionNode::parse(&"a".repeat(MAX_NODE_LEN)).is_ok());
    }

    #[test]
    fn rejects_empty_segments() {
        for bad in [".a", "a.", "a..b", "."] {
            assert_eq!(
                PermissionNode::parse(bad),
                Err(NodeParseError::EmptySegment),
                "{bad:?} should be an empty segment"
            );
        }
    }

    #[test]
    fn rejects_invalid_characters() {
        assert_eq!(
            PermissionNode::parse("ferrumc.Command"),
            Err(NodeParseError::InvalidCharacter { character: 'C' })
        );
        assert_eq!(
            PermissionNode::parse("a.b c"),
            Err(NodeParseError::InvalidCharacter { character: ' ' })
        );
        assert_eq!(
            PermissionNode::parse("a.b!"),
            Err(NodeParseError::InvalidCharacter { character: '!' })
        );
        assert_eq!(
            PermissionNode::parse("a.é"),
            Err(NodeParseError::InvalidCharacter { character: 'é' })
        );
    }

    #[test]
    fn rejects_misplaced_wildcards() {
        for bad in ["a.*.b", "*.b", "ab*", "*a", "a*b", "a.b*"] {
            assert_eq!(
                PermissionNode::parse(bad),
                Err(NodeParseError::MisplacedWildcard),
                "{bad:?} should be a misplaced wildcard"
            );
        }
    }

    #[test]
    fn wildcard_matches_strict_descendants() {
        let w = node("ferrumc.command.*");
        assert!(w.matches(&node("ferrumc.command.gamemode")));
        assert!(w.matches(&node("ferrumc.command.gamemode.other")));
        // A wildcard does not match its own prefix.
        assert!(!w.matches(&node("ferrumc.command")));
        // Nor an unrelated branch.
        assert!(!w.matches(&node("ferrumc.world.time")));
        // Nor a segment that merely shares a textual prefix.
        assert!(!w.matches(&node("ferrumc.commandx.foo")));
    }

    #[test]
    fn root_wildcard_matches_everything_concrete() {
        let root = node("*");
        assert!(root.matches(&node("ferrumc")));
        assert!(root.matches(&node("ferrumc.command.gamemode")));
        // But not another wildcard.
        assert!(!root.matches(&node("ferrumc.*")));
    }

    #[test]
    fn concrete_matches_only_itself() {
        let exact = node("ferrumc.command.gamemode");
        assert!(exact.matches(&node("ferrumc.command.gamemode")));
        assert!(!exact.matches(&node("ferrumc.command")));
        assert!(!exact.matches(&node("ferrumc.command.gamemode.sub")));
    }

    #[test]
    fn specificity_orders_exact_above_wildcards() {
        let target = node("a.b.c");
        let exact = node("a.b.c").match_specificity(&target);
        let deep = node("a.b.*").match_specificity(&target);
        let shallow = node("a.*").match_specificity(&target);
        let root = node("*").match_specificity(&target);

        assert_eq!(exact, Some(MatchSpecificity::Exact(3)));
        assert_eq!(deep, Some(MatchSpecificity::Wildcard(2)));
        assert_eq!(shallow, Some(MatchSpecificity::Wildcard(1)));
        assert_eq!(root, Some(MatchSpecificity::Wildcard(0)));
        assert!(exact > deep);
        assert!(deep > shallow);
        assert!(shallow > root);
    }

    #[test]
    fn from_str_and_try_from_agree() {
        let from_str: PermissionNode = "ferrumc.x".parse().expect("valid");
        let from_ref = PermissionNode::try_from("ferrumc.x").expect("valid");
        let from_owned = PermissionNode::try_from(String::from("ferrumc.x")).expect("valid");
        assert_eq!(from_str, from_ref);
        assert_eq!(from_ref, from_owned);
        assert!("a.*.b".parse::<PermissionNode>().is_err());
    }
}
