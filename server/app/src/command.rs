//! The application command set: `/spawn` and `/gamemode`.
//!
//! Commands are built on [`ferrumc_command`] and dispatched per connection (see
//! [`crate::connection`]). [`build_command_tree`] is also public so a test can
//! assert command behaviour directly — the closest server-side observable for
//! `/gamemode`, which has no clientbound carrier in the pinned 1.21.8 packet set
//! this slice generates.

use ferrumc_command::{argument, literal, ArgumentType, CommandResult, CommandTree};
use ferrumc_core::{GameMode, TextComponent};

/// The literal name of the teleport-to-spawn command.
pub const SPAWN_COMMAND: &str = "spawn";

/// The literal name of the set-game-mode command.
pub const GAMEMODE_COMMAND: &str = "gamemode";

/// Permission *level* required to run `/gamemode` (operator-tier, mirroring
/// vanilla). `/spawn` requires no special level.
pub const GAMEMODE_LEVEL: u8 = 2;

/// Builds the application's [`CommandTree`], wiring `/spawn` and `/gamemode`.
///
/// `/spawn` always succeeds (its teleport side effect is applied by the
/// connection on a successful dispatch). `/gamemode <mode>` takes an integer in
/// `0..=3`, range-checked by the argument type and mapped to a [`GameMode`];
/// it requires permission level [`GAMEMODE_LEVEL`].
pub fn build_command_tree() -> CommandTree {
    let mut tree = CommandTree::new();

    tree.register(literal(SPAWN_COMMAND).executes(|ctx| {
        CommandResult::success(TextComponent::text(format!(
            "{} teleported to spawn",
            ctx.source().name()
        )))
    }));

    tree.register(
        literal(GAMEMODE_COMMAND)
            .requires_level(GAMEMODE_LEVEL)
            .then(
                argument("mode", ArgumentType::integer(0, 3)).executes(|ctx| {
                    let id = ctx.integer("mode").unwrap_or_default();
                    match u8::try_from(id).ok().and_then(GameMode::from_id) {
                        Some(mode) => CommandResult::success(TextComponent::text(format!(
                            "Game mode set to {mode:?}"
                        ))),
                        None => CommandResult::failure(TextComponent::text("invalid game mode")),
                    }
                }),
            ),
    );

    tree
}

/// Parses the [`GameMode`] a `/gamemode <id>` command selects, or `None` if
/// `command` is not a `/gamemode` invocation or its mode argument is missing or
/// out of range.
///
/// The connection uses this after a successful dispatch to apply the game-mode
/// change side effect (a clientbound `GameEvent`), parsing the same argument the
/// handler validated so the two agree on the selected mode.
pub fn parse_gamemode(command: &str) -> Option<GameMode> {
    let mut tokens = command.split_whitespace();
    if tokens.next() != Some(GAMEMODE_COMMAND) {
        return None;
    }
    let id: i64 = tokens.next()?.parse().ok()?;
    u8::try_from(id).ok().and_then(GameMode::from_id)
}

#[cfg(test)]
mod tests {
    use ferrumc_command::{CommandError, CommandSource};
    use ferrumc_core::PlayerId;

    use super::*;

    fn op() -> CommandSource {
        CommandSource::for_player(PlayerId::offline("Op"), "Op", 4)
    }

    #[test]
    fn spawn_dispatches_for_any_player() {
        let tree = build_command_tree();
        let src = CommandSource::for_player(PlayerId::offline("Joe"), "Joe", 0);
        let result = tree.dispatch("spawn", &src).expect("spawn dispatches");
        assert!(result.is_success());
    }

    #[test]
    fn gamemode_accepts_valid_mode_for_an_operator() {
        let tree = build_command_tree();
        let result = tree.dispatch("gamemode 1", &op()).expect("dispatches");
        assert!(result.is_success());
        assert_eq!(
            result.feedback().to_plain_string(),
            "Game mode set to Creative"
        );
    }

    #[test]
    fn gamemode_rejects_out_of_range_mode() {
        let tree = build_command_tree();
        let err = tree
            .dispatch("gamemode 9", &op())
            .expect_err("9 is out of range");
        assert!(matches!(err, CommandError::IntegerOutOfRange { .. }));
    }

    #[test]
    fn parse_gamemode_extracts_a_valid_mode() {
        assert_eq!(parse_gamemode("gamemode 0"), Some(GameMode::Survival));
        assert_eq!(parse_gamemode("gamemode 1"), Some(GameMode::Creative));
        assert_eq!(parse_gamemode("gamemode 3"), Some(GameMode::Spectator));
    }

    #[test]
    fn parse_gamemode_rejects_non_gamemode_or_bad_arg() {
        assert_eq!(parse_gamemode("spawn"), None);
        assert_eq!(parse_gamemode("gamemode"), None);
        assert_eq!(parse_gamemode("gamemode 9"), None);
        assert_eq!(parse_gamemode("gamemode x"), None);
    }

    #[test]
    fn gamemode_requires_operator_level() {
        let tree = build_command_tree();
        let member = CommandSource::for_player(PlayerId::offline("Joe"), "Joe", 0);
        let err = tree
            .dispatch("gamemode 1", &member)
            .expect_err("member lacks level 2");
        assert!(matches!(err, CommandError::PermissionDenied(_)));
    }
}
