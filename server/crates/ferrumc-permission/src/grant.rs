//! Grants, resolution, and the most-specific-wins [`PermissionSet`].

use crate::node::{MatchSpecificity, PermissionNode};

/// The effect of a single [`Grant`]: whether it allows or denies a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GrantEffect {
    /// The node is explicitly granted.
    Allow,
    /// The node is explicitly forbidden.
    Deny,
}

/// An explicit decision (`Allow` or `Deny`) attached to a [`PermissionNode`].
///
/// The node may be concrete (`ferrumc.command.gamemode`) or a wildcard
/// (`ferrumc.command.*`). Build one with [`Grant::allow`] or [`Grant::deny`];
/// the fields are read through [`Grant::node`] and [`Grant::effect`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Grant {
    node: PermissionNode,
    effect: GrantEffect,
}

impl Grant {
    /// Creates a grant with the given `node` and `effect`.
    pub fn new(node: PermissionNode, effect: GrantEffect) -> Self {
        Self { node, effect }
    }

    /// Creates an [`GrantEffect::Allow`] grant for `node`.
    pub fn allow(node: PermissionNode) -> Self {
        Self::new(node, GrantEffect::Allow)
    }

    /// Creates a [`GrantEffect::Deny`] grant for `node`.
    pub fn deny(node: PermissionNode) -> Self {
        Self::new(node, GrantEffect::Deny)
    }

    /// Returns the node this grant applies to.
    pub fn node(&self) -> &PermissionNode {
        &self.node
    }

    /// Returns this grant's effect.
    pub const fn effect(&self) -> GrantEffect {
        self.effect
    }
}

/// The tri-state outcome of resolving a concrete node against a
/// [`PermissionSet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resolution {
    /// A matching grant allows the node.
    Allowed,
    /// A matching grant denies the node.
    Denied,
    /// No grant matched the node; the decision is left to the caller's default.
    Unset,
}

impl Resolution {
    /// Returns `true` only for [`Resolution::Allowed`].
    ///
    /// Both [`Resolution::Denied`] and [`Resolution::Unset`] map to `false`,
    /// matching the closed-by-default convention used by
    /// [`Subject::has_permission`](crate::Subject::has_permission).
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Returns `true` if no grant matched.
    pub const fn is_unset(self) -> bool {
        matches!(self, Self::Unset)
    }
}

/// An ordered collection of [`Grant`]s resolved most-specific-wins.
///
/// # Precedence
///
/// Resolving a concrete node considers every grant whose node *matches* it
/// (see [`PermissionNode::matches`]) and applies the single most specific one:
///
/// 1. An exact match beats any wildcard match.
/// 2. Among wildcards, a longer literal prefix beats a shorter one, and the
///    root `*` is the weakest.
/// 3. At equal specificity (the same node granted twice), an explicit
///    [`GrantEffect::Deny`] beats [`GrantEffect::Allow`].
///
/// Because matching wildcards for any given node are strictly nested, only
/// grants for the *same* node can ever tie, so the outcome does not depend on
/// insertion order. Insertion order is nonetheless preserved for stable
/// iteration via [`PermissionSet::grants`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionSet {
    grants: Vec<Grant>,
}

impl PermissionSet {
    /// Creates an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `grant`, ignoring it if an identical grant is already present.
    ///
    /// Adding the same node with the opposite effect is allowed and is resolved
    /// by the deny-beats-allow tie-break documented on [`PermissionSet`].
    /// Returns `true` if the grant was newly inserted.
    pub fn add(&mut self, grant: Grant) -> bool {
        if self.grants.contains(&grant) {
            return false;
        }
        self.grants.push(grant);
        true
    }

    /// Removes every grant for `node` (regardless of effect).
    ///
    /// Returns the number of grants removed.
    pub fn remove(&mut self, node: &PermissionNode) -> usize {
        let before = self.grants.len();
        self.grants.retain(|grant| grant.node() != node);
        before - self.grants.len()
    }

    /// Removes all grants.
    pub fn clear(&mut self) {
        self.grants.clear();
    }

    /// Returns the number of grants in the set.
    pub fn len(&self) -> usize {
        self.grants.len()
    }

    /// Returns whether the set contains no grants.
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// Returns an iterator over the grants in insertion order.
    pub fn grants(&self) -> impl Iterator<Item = &Grant> {
        self.grants.iter()
    }

    /// Resolves `node` to [`Resolution::Allowed`], [`Resolution::Denied`], or
    /// [`Resolution::Unset`] using the precedence documented on
    /// [`PermissionSet`].
    ///
    /// `node` is expected to be concrete; a wildcard `node` never matches any
    /// grant and therefore resolves to [`Resolution::Unset`].
    pub fn resolve(&self, node: &PermissionNode) -> Resolution {
        let mut best: Option<(MatchSpecificity, GrantEffect)> = None;

        for grant in &self.grants {
            let Some(specificity) = grant.node().match_specificity(node) else {
                continue;
            };

            best = Some(match best {
                // Strictly more specific match wins outright.
                Some((current, _)) if specificity > current => (specificity, grant.effect()),
                // Equal specificity (same node): Deny beats Allow.
                Some((current, _))
                    if specificity == current && grant.effect() == GrantEffect::Deny =>
                {
                    (current, GrantEffect::Deny)
                }
                Some(existing) => existing,
                None => (specificity, grant.effect()),
            });
        }

        match best {
            Some((_, GrantEffect::Allow)) => Resolution::Allowed,
            Some((_, GrantEffect::Deny)) => Resolution::Denied,
            None => Resolution::Unset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(s: &str) -> PermissionNode {
        PermissionNode::parse(s).expect("valid node")
    }

    #[test]
    fn unset_when_no_grant_matches() {
        let set = PermissionSet::new();
        assert_eq!(set.resolve(&node("a.b.c")), Resolution::Unset);
        assert!(set.is_empty());
    }

    #[test]
    fn wildcard_grant_allows_descendant() {
        let mut set = PermissionSet::new();
        assert!(set.add(Grant::allow(node("ferrumc.command.*"))));
        assert_eq!(
            set.resolve(&node("ferrumc.command.gamemode")),
            Resolution::Allowed
        );
        // The wildcard does not reach its own prefix.
        assert_eq!(set.resolve(&node("ferrumc.command")), Resolution::Unset);
    }

    #[test]
    fn exact_grant_overrides_broader_wildcard() {
        let mut set = PermissionSet::new();
        set.add(Grant::allow(node("ferrumc.command.*")));
        set.add(Grant::deny(node("ferrumc.command.gamemode")));

        // Exact deny beats the broader allow.
        assert_eq!(
            set.resolve(&node("ferrumc.command.gamemode")),
            Resolution::Denied
        );
        // A sibling still rides the wildcard allow.
        assert_eq!(
            set.resolve(&node("ferrumc.command.give")),
            Resolution::Allowed
        );
    }

    #[test]
    fn longer_wildcard_overrides_shorter() {
        let mut set = PermissionSet::new();
        set.add(Grant::deny(node("ferrumc.*")));
        set.add(Grant::allow(node("ferrumc.command.*")));

        assert_eq!(
            set.resolve(&node("ferrumc.command.gamemode")),
            Resolution::Allowed
        );
        // Outside the narrower allow, the broad deny applies.
        assert_eq!(set.resolve(&node("ferrumc.world.time")), Resolution::Denied);
    }

    #[test]
    fn root_wildcard_is_weakest() {
        let mut set = PermissionSet::new();
        set.add(Grant::allow(node("*")));
        set.add(Grant::deny(node("ferrumc.*")));

        assert_eq!(set.resolve(&node("other.thing")), Resolution::Allowed);
        assert_eq!(set.resolve(&node("ferrumc.command")), Resolution::Denied);
    }

    #[test]
    fn deny_beats_allow_at_equal_specificity() {
        // Order-independent: insert allow first, then deny.
        let mut allow_first = PermissionSet::new();
        allow_first.add(Grant::allow(node("a.b.c")));
        allow_first.add(Grant::deny(node("a.b.c")));
        assert_eq!(allow_first.resolve(&node("a.b.c")), Resolution::Denied);

        // And deny first, then allow.
        let mut deny_first = PermissionSet::new();
        deny_first.add(Grant::deny(node("a.b.c")));
        deny_first.add(Grant::allow(node("a.b.c")));
        assert_eq!(deny_first.resolve(&node("a.b.c")), Resolution::Denied);
    }

    #[test]
    fn add_dedupes_identical_grants() {
        let mut set = PermissionSet::new();
        assert!(set.add(Grant::allow(node("a.b"))));
        assert!(!set.add(Grant::allow(node("a.b"))));
        assert_eq!(set.len(), 1);
        // A different effect for the same node is a distinct grant.
        assert!(set.add(Grant::deny(node("a.b"))));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn remove_drops_all_effects_for_node() {
        let mut set = PermissionSet::new();
        set.add(Grant::allow(node("a.b")));
        set.add(Grant::deny(node("a.b")));
        set.add(Grant::allow(node("a.c")));

        assert_eq!(set.remove(&node("a.b")), 2);
        assert_eq!(set.remove(&node("a.b")), 0);
        assert_eq!(set.len(), 1);
        assert_eq!(set.resolve(&node("a.c")), Resolution::Allowed);

        set.clear();
        assert!(set.is_empty());
    }

    #[test]
    fn grants_iterates_in_insertion_order() {
        let mut set = PermissionSet::new();
        set.add(Grant::allow(node("a")));
        set.add(Grant::deny(node("b")));
        let collected: Vec<_> = set.grants().map(|g| g.node().as_str().to_owned()).collect();
        assert_eq!(collected, vec!["a", "b"]);
    }

    #[test]
    fn resolution_helpers() {
        assert!(Resolution::Allowed.is_allowed());
        assert!(!Resolution::Denied.is_allowed());
        assert!(!Resolution::Unset.is_allowed());
        assert!(Resolution::Unset.is_unset());
        assert!(!Resolution::Allowed.is_unset());
    }

    #[test]
    fn grant_accessors() {
        let g = Grant::deny(node("a.b"));
        assert_eq!(g.node(), &node("a.b"));
        assert_eq!(g.effect(), GrantEffect::Deny);
        assert_eq!(
            Grant::new(node("a.b"), GrantEffect::Allow).effect(),
            GrantEffect::Allow
        );
    }
}
