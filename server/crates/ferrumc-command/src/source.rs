//! The identity and permission level of whoever runs a command.

use ferrumc_core::PlayerId;

/// Who is executing a command, plus the data needed to authorize it.
///
/// A source carries a display `name`, an optional [`PlayerId`] (absent for
/// non-player sources such as the server console), and a numeric permission
/// `level`. The level is compared against a node's required level during
/// dispatch; richer, node-string-based permission checks are injected by the
/// caller (see [`crate::CommandTree::dispatch_with`]).
///
/// This type deliberately does not depend on `ferrumc-permission`: it only
/// models *what the source has*, not how permissions are stored or evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSource {
    name: String,
    player: Option<PlayerId>,
    permission_level: u8,
}

impl CommandSource {
    /// Builds a source from its parts: a display `name`, an optional `player`
    /// identity, and a `permission_level`.
    pub fn new(name: impl Into<String>, player: Option<PlayerId>, permission_level: u8) -> Self {
        Self {
            name: name.into(),
            player,
            permission_level,
        }
    }

    /// Builds a source for a player, given their `player` id, display `name`,
    /// and `permission_level`.
    pub fn for_player(player: PlayerId, name: impl Into<String>, permission_level: u8) -> Self {
        Self::new(name, Some(player), permission_level)
    }

    /// Builds a console source (no [`PlayerId`]) with the given `permission_level`.
    ///
    /// The console is named `"Console"` and typically runs at the highest level.
    pub fn console(permission_level: u8) -> Self {
        Self::new("Console", None, permission_level)
    }

    /// Returns the source's display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the player identity, or `None` for non-player sources.
    pub const fn player_id(&self) -> Option<PlayerId> {
        self.player
    }

    /// Returns the source's permission level.
    pub const fn permission_level(&self) -> u8 {
        self.permission_level
    }
}
