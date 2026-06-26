//! The read-only permission facade exposed to plugins.

use ferrumc_core::PlayerId;
use ferrumc_permission::{PermissionNode, Resolution};

/// A read-only facade for querying a subject's permissions.
///
/// A plugin uses this to decide whether a player may do something; it cannot
/// grant or revoke permissions, only ask. This trait is a shell; the host wires
/// it to the real permission store.
pub trait PermissionApi {
    /// Returns whether `player` is granted `node`, treating an unset node as
    /// denied (closed by default).
    fn has_permission(&self, player: PlayerId, node: &PermissionNode) -> bool;

    /// Resolves `node` for `player` to a tri-state [`Resolution`].
    ///
    /// Unlike [`PermissionApi::has_permission`], this distinguishes an explicit
    /// deny from an unset node.
    fn resolve(&self, player: PlayerId, node: &PermissionNode) -> Resolution;
}
