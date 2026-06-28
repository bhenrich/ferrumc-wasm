//! The application command set: `/spawn`, `/gamemode`, and the *presentation*
//! commands `/title`, `/subtitle`, `/actionbar`, `/playsound`, and `/particle`.
//!
//! Commands are built on [`ferrumc_command`] and dispatched per connection (see
//! [`crate::connection`]). [`build_command_tree`] is also public so a test can
//! assert command behaviour directly — the closest server-side observable for
//! `/gamemode`, which has no clientbound carrier in the pinned 1.21.8 packet set
//! this slice generates.
//!
//! Like `/gamemode`, the presentation commands carry a clientbound side effect
//! the dispatch handler applies on success. The command framework's executor only
//! yields a [`CommandResult`] (feedback), so [`presentation_packets`] re-parses a
//! successfully dispatched presentation command into the actual clientbound
//! packets, built by the [`ferrumc_session`] presentation builders. The
//! connection enqueues them; this keeps the parse rules in one place (shared with
//! the executor's validation) and the contended dispatch handler edit a single
//! additive call.

use ferrumc_command::{
    argument, literal, ArgumentType, CommandBuilder, CommandResult, CommandTree,
};
use ferrumc_core::{GameMode, TextComponent};
use ferrumc_math::Vec3;
use ferrumc_proto::generated::play::ClientboundPlayPacket;
use ferrumc_session::{
    action_bar, play_sound, spawn_dust, spawn_simple_particle, subtitle, title,
    title_animation_times, SoundCategory, SoundId, PARTICLE_CLOUD, PARTICLE_CRIT,
    PARTICLE_EXPLOSION, PARTICLE_FLAME, PARTICLE_HAPPY_VILLAGER, PARTICLE_HEART, PARTICLE_SMOKE,
    SOUND_ANVIL_LAND, SOUND_EXPERIENCE_ORB_PICKUP, SOUND_NOTE_BLOCK_HARP, SOUND_PLAYER_LEVELUP,
    SOUND_UI_BUTTON_CLICK,
};

/// The literal name of the teleport-to-spawn command.
pub const SPAWN_COMMAND: &str = "spawn";

/// The literal name of the set-game-mode command.
pub const GAMEMODE_COMMAND: &str = "gamemode";

/// Permission *level* required to run `/gamemode` (operator-tier, mirroring
/// vanilla). `/spawn` requires no special level.
pub const GAMEMODE_LEVEL: u8 = 2;

/// The literal name of the `/title` command.
pub const TITLE_COMMAND: &str = "title";
/// The literal name of the `/subtitle` command.
pub const SUBTITLE_COMMAND: &str = "subtitle";
/// The literal name of the `/actionbar` command.
pub const ACTIONBAR_COMMAND: &str = "actionbar";
/// The literal name of the `/playsound` command.
pub const PLAYSOUND_COMMAND: &str = "playsound";
/// The literal name of the `/particle` command.
pub const PARTICLE_COMMAND: &str = "particle";

/// Permission *level* required to run the presentation commands (operator-tier,
/// matching [`GAMEMODE_LEVEL`]).
pub const PRESENTATION_LEVEL: u8 = 2;

/// Default title fade-in time in ticks, sent with every `/title`.
const TITLE_FADE_IN_TICKS: i32 = 10;
/// Default title hold time in ticks, sent with every `/title`.
const TITLE_STAY_TICKS: i32 = 70;
/// Default title fade-out time in ticks, sent with every `/title`.
const TITLE_FADE_OUT_TICKS: i32 = 20;
/// Volume `/playsound` plays at (full range, no attenuation).
const PLAYSOUND_VOLUME: f32 = 1.0;
/// Pitch `/playsound` plays at (unchanged playback rate).
const PLAYSOUND_PITCH: f32 = 1.0;
/// Variant seed `/playsound` uses (`0` is deterministic).
const PLAYSOUND_SEED: i64 = 0;
/// Particle count `/particle` spawns per invocation.
const PARTICLE_COUNT: i32 = 1;
/// The default `minecraft:dust` colour/scale `/particle dust` uses, as
/// `(red, green, blue, scale)` — a unit-scale red, since the command takes no
/// colour argument in this slice.
const DUST_DEFAULT_COLOR: (f32, f32, f32, f32) = (1.0, 0.0, 0.0, 1.0);

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

    // `/title`, `/subtitle`, `/actionbar` take greedy free text; the clientbound
    // title/action-bar packets are applied on success by `presentation_packets`.
    tree.register(text_command(TITLE_COMMAND, "Title shown"));
    tree.register(text_command(SUBTITLE_COMMAND, "Subtitle shown"));
    tree.register(text_command(ACTIONBAR_COMMAND, "Action bar shown"));

    // `/playsound <sound> [x y z]`: the greedy tail is the sound name plus the
    // optional absolute coordinates, validated the same way `presentation_packets`
    // parses it for the side effect.
    tree.register(
        literal(PLAYSOUND_COMMAND)
            .requires_level(PRESENTATION_LEVEL)
            .then(
                argument("args", ArgumentType::GreedyString).executes(|ctx| {
                    let args = ctx.string("args").unwrap_or_default();
                    match parse_playsound(args, Vec3::new(0.0, 0.0, 0.0)) {
                        Some(_) => CommandResult::success(TextComponent::text(format!(
                            "Playing sound {}",
                            first_token(args)
                        ))),
                        None => CommandResult::failure(TextComponent::text(
                            "usage: /playsound <sound> [x y z] — unknown sound or bad coordinates",
                        )),
                    }
                }),
            ),
    );

    // `/particle <type> [x y z]`.
    tree.register(
        literal(PARTICLE_COMMAND)
            .requires_level(PRESENTATION_LEVEL)
            .then(
                argument("args", ArgumentType::GreedyString).executes(|ctx| {
                    let args = ctx.string("args").unwrap_or_default();
                    match parse_particle(args, Vec3::new(0.0, 0.0, 0.0)) {
                        Some(_) => CommandResult::success(TextComponent::text(format!(
                            "Spawning particle {}",
                            first_token(args)
                        ))),
                        None => CommandResult::failure(TextComponent::text(
                            "usage: /particle <type> [x y z] — unknown particle or bad coordinates",
                        )),
                    }
                }),
            ),
    );

    tree
}

/// Builds a presentation text command (`/title`, `/subtitle`, `/actionbar`):
/// a level-gated literal taking one greedy free-text argument whose handler
/// reports `feedback`. The clientbound packet is built by [`presentation_packets`]
/// on a successful dispatch.
fn text_command(name: &'static str, feedback: &'static str) -> CommandBuilder {
    literal(name).requires_level(PRESENTATION_LEVEL).then(
        argument("text", ArgumentType::GreedyString).executes(move |ctx| {
            let text = ctx.string("text").unwrap_or_default();
            CommandResult::success(TextComponent::text(format!("{feedback}: {text}")))
        }),
    )
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

/// Builds the clientbound packets a successfully dispatched presentation command
/// (`/title`, `/subtitle`, `/actionbar`, `/playsound`, `/particle`) produces, or
/// an empty vector for any other command.
///
/// The connection calls this after a successful dispatch and enqueues each packet
/// on the player's outbound writer — the same shape as `/gamemode`'s `GameEvent`
/// side effect, but data-driven so the contended dispatch handler only gains one
/// additive call. `default_pos` (the player's spawn) is used when a `/playsound`
/// or `/particle` omits its `[x y z]` coordinates. Parsing mirrors the executor's
/// own validation, so a command that dispatched successfully re-parses here.
pub fn presentation_packets(command: &str, default_pos: Vec3) -> Vec<ClientboundPlayPacket> {
    let Some(name) = command.split_whitespace().next() else {
        return Vec::new();
    };
    let args = command_args(command);
    match name {
        TITLE_COMMAND => vec![
            // A title only renders with animation times in effect; send the
            // defaults so the demo title is visible without a separate command.
            title_animation_times(TITLE_FADE_IN_TICKS, TITLE_STAY_TICKS, TITLE_FADE_OUT_TICKS),
            title(&TextComponent::text(args)),
        ],
        SUBTITLE_COMMAND => vec![subtitle(&TextComponent::text(args))],
        ACTIONBAR_COMMAND => vec![action_bar(&TextComponent::text(args))],
        PLAYSOUND_COMMAND => match parse_playsound(args, default_pos) {
            Some((sound, pos)) => vec![play_sound(
                sound,
                SoundCategory::Master,
                pos,
                PLAYSOUND_VOLUME,
                PLAYSOUND_PITCH,
                PLAYSOUND_SEED,
            )],
            None => Vec::new(),
        },
        PARTICLE_COMMAND => match parse_particle(args, default_pos) {
            Some((ParticleChoice::Simple(kind), pos)) => {
                vec![spawn_simple_particle(kind, pos, PARTICLE_COUNT)]
            }
            Some((ParticleChoice::Dust, pos)) => {
                let (r, g, b, scale) = DUST_DEFAULT_COLOR;
                vec![spawn_dust(pos, r, g, b, scale, PARTICLE_COUNT)]
            }
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Returns everything after the leading command literal (the argument tail),
/// trimmed of leading whitespace, or `""` when the command has no arguments.
fn command_args(command: &str) -> &str {
    command
        .split_once(char::is_whitespace)
        .map_or("", |(_, rest)| rest.trim_start())
}

/// Returns the first whitespace-delimited token of `args`, or `""` if empty.
fn first_token(args: &str) -> &str {
    args.split_whitespace().next().unwrap_or_default()
}

/// Parses a `/playsound` argument tail into its sound and target position.
///
/// The tail is `<sound> [x y z]`: a curated sound name (see [`sound_by_name`])
/// followed by either no coordinates (defaulting to `default_pos`) or exactly
/// three absolute `f64` coordinates. Returns `None` on an unknown sound, a partial
/// or unparsable coordinate triple, or trailing tokens.
fn parse_playsound(args: &str, default_pos: Vec3) -> Option<(SoundId, Vec3)> {
    let mut tokens = args.split_whitespace();
    let sound = sound_by_name(tokens.next()?)?;
    let rest: Vec<&str> = tokens.collect();
    Some((sound, coords_or_default(&rest, default_pos)?))
}

/// A `/particle` selection: a no-`data` particle kind, or `dust` (which carries a
/// colour/scale `data` tail this slice fills with [`DUST_DEFAULT_COLOR`]).
enum ParticleChoice {
    /// A no-`data` particle identified by its registry id.
    Simple(i32),
    /// The `minecraft:dust` particle.
    Dust,
}

/// Parses a `/particle` argument tail into its particle choice and target
/// position. Same coordinate rules as [`parse_playsound`]; returns `None` on an
/// unknown particle name, a bad coordinate triple, or trailing tokens.
fn parse_particle(args: &str, default_pos: Vec3) -> Option<(ParticleChoice, Vec3)> {
    let mut tokens = args.split_whitespace();
    let choice = particle_by_name(tokens.next()?)?;
    let rest: Vec<&str> = tokens.collect();
    Some((choice, coords_or_default(&rest, default_pos)?))
}

/// Resolves an optional coordinate triple: no tokens yields `default_pos`, exactly
/// three parsable `f64` tokens yield that position, and anything else is `None`
/// (a partial triple, unparsable number, or extra tokens).
fn coords_or_default(rest: &[&str], default_pos: Vec3) -> Option<Vec3> {
    match rest {
        [] => Some(default_pos),
        [x, y, z] => Some(Vec3::new(x.parse().ok()?, y.parse().ok()?, z.parse().ok()?)),
        _ => None,
    }
}

/// Maps a curated friendly sound name to its [`SoundId`]. The accepted names are a
/// small demo subset; see the [`ferrumc_session`] presentation module for the
/// sound-id limitation.
fn sound_by_name(name: &str) -> Option<SoundId> {
    let sound = match name {
        "note_block" | "harp" | "note_block.harp" => SOUND_NOTE_BLOCK_HARP,
        "levelup" | "level_up" => SOUND_PLAYER_LEVELUP,
        "click" | "button" | "ui.button.click" => SOUND_UI_BUTTON_CLICK,
        "xp" | "pickup" | "experience" => SOUND_EXPERIENCE_ORB_PICKUP,
        "anvil" => SOUND_ANVIL_LAND,
        _ => return None,
    };
    Some(sound)
}

/// Maps a particle name to its [`ParticleChoice`]. Covers the no-`data` demo set
/// plus `dust`.
fn particle_by_name(name: &str) -> Option<ParticleChoice> {
    let choice = match name {
        "flame" => ParticleChoice::Simple(PARTICLE_FLAME),
        "heart" => ParticleChoice::Simple(PARTICLE_HEART),
        "happy_villager" | "happy" => ParticleChoice::Simple(PARTICLE_HAPPY_VILLAGER),
        "crit" => ParticleChoice::Simple(PARTICLE_CRIT),
        "explosion" => ParticleChoice::Simple(PARTICLE_EXPLOSION),
        "cloud" => ParticleChoice::Simple(PARTICLE_CLOUD),
        "smoke" => ParticleChoice::Simple(PARTICLE_SMOKE),
        "dust" => ParticleChoice::Dust,
        _ => return None,
    };
    Some(choice)
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

    #[test]
    fn command_graph_is_permission_filtered() {
        use ferrumc_command::BrigadierExtra;

        let tree = build_command_tree();

        // An operator sees both commands and the typed `mode` argument.
        let op = tree.to_brigadier(4, &|_| true);
        let op_names: Vec<&str> = op.nodes().iter().filter_map(|n| n.name()).collect();
        assert!(op_names.contains(&SPAWN_COMMAND));
        assert!(op_names.contains(&GAMEMODE_COMMAND));
        assert!(op_names.contains(&"mode"));
        let mode = op
            .nodes()
            .iter()
            .find(|n| n.name() == Some("mode"))
            .expect("mode node");
        assert!(matches!(
            mode.extra(),
            BrigadierExtra::Argument { parser_id: 3, .. }
        ));

        // A level-0 player sees only `/spawn`; the gated `/gamemode` subtree is gone.
        let member = tree.to_brigadier(0, &|_| true);
        let member_names: Vec<&str> = member.nodes().iter().filter_map(|n| n.name()).collect();
        assert!(member_names.contains(&SPAWN_COMMAND));
        assert!(!member_names.contains(&GAMEMODE_COMMAND));
        assert!(!member_names.contains(&"mode"));

        // The encoded body the join kit sends therefore differs by level.
        assert!(
            tree.encode_commands_body(4, &|_| true).len()
                > tree.encode_commands_body(0, &|_| true).len()
        );
    }

    /// A fixed spawn used as the default position for `/playsound` and `/particle`
    /// when the command omits its coordinates.
    const SPAWN_POS: Vec3 = Vec3::new(0.5, 64.0, 0.5);

    #[test]
    fn presentation_commands_require_operator_level() {
        let tree = build_command_tree();
        let member = CommandSource::for_player(PlayerId::offline("Joe"), "Joe", 0);
        for cmd in [
            "title hi",
            "subtitle hi",
            "actionbar hi",
            "playsound click",
            "particle flame",
        ] {
            let err = tree.dispatch(cmd, &member).expect_err("member lacks level");
            assert!(matches!(err, CommandError::PermissionDenied(_)), "{cmd}");
        }
    }

    #[test]
    fn title_dispatches_and_emits_times_then_title() {
        let tree = build_command_tree();
        let result = tree
            .dispatch("title Hello World", &op())
            .expect("title dispatches");
        assert!(result.is_success());

        let packets = presentation_packets("title Hello World", SPAWN_POS);
        assert!(
            matches!(
                packets.as_slice(),
                [
                    ClientboundPlayPacket::SetTitleAnimationTimes(_),
                    ClientboundPlayPacket::SetTitleText(_),
                ]
            ),
            "/title sends the default animation times then the title text"
        );
    }

    #[test]
    fn subtitle_and_actionbar_build_their_packets() {
        assert!(matches!(
            presentation_packets("subtitle hi there", SPAWN_POS).as_slice(),
            [ClientboundPlayPacket::SetSubtitleText(_)]
        ));
        assert!(matches!(
            presentation_packets("actionbar hi there", SPAWN_POS).as_slice(),
            [ClientboundPlayPacket::SetActionBarText(_)]
        ));
    }

    #[test]
    fn playsound_known_sound_builds_a_sound_effect() {
        let tree = build_command_tree();
        assert!(tree
            .dispatch("playsound levelup", &op())
            .expect("dispatches")
            .is_success());
        assert!(matches!(
            presentation_packets("playsound levelup", SPAWN_POS).as_slice(),
            [ClientboundPlayPacket::SoundEffect(_)]
        ));
    }

    #[test]
    fn playsound_unknown_sound_fails_and_emits_nothing() {
        let tree = build_command_tree();
        let result = tree
            .dispatch("playsound not_a_sound", &op())
            .expect("dispatch reaches the handler");
        assert!(!result.is_success());
        assert!(presentation_packets("playsound not_a_sound", SPAWN_POS).is_empty());
    }

    #[test]
    fn playsound_parses_explicit_and_default_coordinates() {
        let (_, pos) = parse_playsound("click 1 2 3", SPAWN_POS).expect("explicit coords");
        assert_eq!(pos, Vec3::new(1.0, 2.0, 3.0));
        let (_, default) = parse_playsound("click", SPAWN_POS).expect("default coords");
        assert_eq!(default, SPAWN_POS);
        // A partial, unparsable, or over-long coordinate list is rejected.
        assert!(parse_playsound("click 1 2", SPAWN_POS).is_none());
        assert!(parse_playsound("click 1 2 x", SPAWN_POS).is_none());
        assert!(parse_playsound("click 1 2 3 4", SPAWN_POS).is_none());
    }

    #[test]
    fn particle_known_and_dust_build_particles() {
        assert!(matches!(
            presentation_packets("particle flame", SPAWN_POS).as_slice(),
            [ClientboundPlayPacket::Particle(_)]
        ));
        let dust = presentation_packets("particle dust 1 2 3", SPAWN_POS);
        let [ClientboundPlayPacket::Particle(packet)] = dust.as_slice() else {
            panic!("/particle dust builds one Particle packet");
        };
        assert_eq!(packet.data().len(), 16, "dust carries a four-f32 data tail");
    }

    #[test]
    fn particle_unknown_fails_and_emits_nothing() {
        let tree = build_command_tree();
        assert!(!tree
            .dispatch("particle nope", &op())
            .expect("reaches the handler")
            .is_success());
        assert!(presentation_packets("particle nope", SPAWN_POS).is_empty());
    }

    #[test]
    fn presentation_packets_ignores_other_commands() {
        assert!(presentation_packets("spawn", SPAWN_POS).is_empty());
        assert!(presentation_packets("gamemode 1", SPAWN_POS).is_empty());
        assert!(presentation_packets("", SPAWN_POS).is_empty());
    }
}
