//! [`Subject`]: an owned holder of permissions.

use crate::grant::{Grant, PermissionSet, Resolution};
use crate::level::OperatorLevel;
use crate::node::PermissionNode;

/// A holder of permissions, such as a player or a console.
///
/// A `Subject` owns a [`PermissionSet`] and an optional [`OperatorLevel`]. It is
/// a plain owned value with no shared or global state: clone it, store it, and
/// pass it by reference or value as needed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subject {
    permissions: PermissionSet,
    operator_level: Option<OperatorLevel>,
}

impl Subject {
    /// Creates a subject with no grants and no operator level.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `grant` to this subject's permission set.
    ///
    /// Returns `true` if the grant was newly inserted (see
    /// [`PermissionSet::add`]).
    pub fn add_grant(&mut self, grant: Grant) -> bool {
        self.permissions.add(grant)
    }

    /// Removes every grant for `node`, returning how many were removed.
    pub fn remove_grant(&mut self, node: &PermissionNode) -> usize {
        self.permissions.remove(node)
    }

    /// Resolves `node` to a tri-state [`Resolution`] against this subject's
    /// permission set.
    pub fn resolve(&self, node: &PermissionNode) -> Resolution {
        self.permissions.resolve(node)
    }

    /// Returns whether `node` is allowed, treating
    /// [`Resolution::Unset`] as `false` (closed by default).
    pub fn has_permission(&self, node: &PermissionNode) -> bool {
        self.permissions.resolve(node).is_allowed()
    }

    /// Returns this subject's operator level, if any.
    pub const fn operator_level(&self) -> Option<OperatorLevel> {
        self.operator_level
    }

    /// Sets (or clears, with `None`) this subject's operator level.
    pub fn set_operator_level(&mut self, level: Option<OperatorLevel>) {
        self.operator_level = level;
    }

    /// Returns a shared reference to this subject's permission set.
    pub const fn permissions(&self) -> &PermissionSet {
        &self.permissions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grant::GrantEffect;

    fn node(s: &str) -> PermissionNode {
        PermissionNode::parse(s).expect("valid node")
    }

    #[test]
    fn has_permission_treats_unset_as_false() {
        let subject = Subject::new();
        assert!(!subject.has_permission(&node("ferrumc.command.gamemode")));
        assert_eq!(
            subject.resolve(&node("ferrumc.command.gamemode")),
            Resolution::Unset
        );
    }

    #[test]
    fn allow_then_query() {
        let mut subject = Subject::new();
        subject.add_grant(Grant::allow(node("ferrumc.command.*")));
        assert!(subject.has_permission(&node("ferrumc.command.gamemode")));
        assert!(!subject.has_permission(&node("ferrumc.world.time")));
    }

    #[test]
    fn deny_makes_has_permission_false() {
        let mut subject = Subject::new();
        subject.add_grant(Grant::allow(node("ferrumc.command.*")));
        subject.add_grant(Grant::deny(node("ferrumc.command.stop")));
        assert!(subject.has_permission(&node("ferrumc.command.gamemode")));
        assert!(!subject.has_permission(&node("ferrumc.command.stop")));
        assert_eq!(
            subject.resolve(&node("ferrumc.command.stop")),
            Resolution::Denied
        );
    }

    #[test]
    fn remove_grant_reverts_to_unset() {
        let mut subject = Subject::new();
        subject.add_grant(Grant::new(node("a.b"), GrantEffect::Allow));
        assert!(subject.has_permission(&node("a.b")));
        assert_eq!(subject.remove_grant(&node("a.b")), 1);
        assert!(!subject.has_permission(&node("a.b")));
    }

    #[test]
    fn operator_level_round_trips() {
        let mut subject = Subject::new();
        assert_eq!(subject.operator_level(), None);
        subject.set_operator_level(Some(OperatorLevel::OWNER));
        assert_eq!(subject.operator_level(), Some(OperatorLevel::OWNER));
        subject.set_operator_level(None);
        assert_eq!(subject.operator_level(), None);
    }

    #[test]
    fn permissions_accessor_exposes_set() {
        let mut subject = Subject::new();
        subject.add_grant(Grant::allow(node("a.b")));
        assert_eq!(subject.permissions().len(), 1);
    }
}
