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
use ferrumc_core::{GameMode, PlayerId, TextColor, TextComponent};
use ferrumc_math::{BlockPos, Cuboid, Vec3};
use ferrumc_proto::generated::play::ClientboundPlayPacket;
use ferrumc_registry::block_state::block_default_state;
use ferrumc_session::{
    action_bar, boss_bar_add, boss_bar_remove, boss_bar_update_health, boss_bar_update_style,
    boss_bar_update_title, display_objective, objective_create, objective_remove, play_sound,
    score_set, spawn_dust, spawn_simple_particle, subtitle, team_add_entities, team_create,
    team_remove, team_update, title, title_animation_times, BossBarColor, BossBarDivision,
    ObjectiveRender, SoundCategory, SoundId, DISPLAY_SLOT_BELOW_NAME, DISPLAY_SLOT_LIST,
    DISPLAY_SLOT_SIDEBAR, PARTICLE_CLOUD, PARTICLE_CRIT, PARTICLE_EXPLOSION, PARTICLE_FLAME,
    PARTICLE_HAPPY_VILLAGER, PARTICLE_HEART, PARTICLE_SMOKE, SOUND_ANVIL_LAND,
    SOUND_EXPERIENCE_ORB_PICKUP, SOUND_NOTE_BLOCK_HARP, SOUND_PLAYER_LEVELUP,
    SOUND_UI_BUTTON_CLICK,
};
use ferrumc_sim::{BlockStateId, RegionOp, TIME_DAY, TIME_MIDNIGHT, TIME_NIGHT, TIME_NOON};
use uuid::Uuid;

use crate::driver::SimCommand;
use crate::player_data::is_valid_player_position;

/// The literal name of the teleport-to-spawn command.
pub const SPAWN_COMMAND: &str = "spawn";

/// The literal name of the set-game-mode command.
pub const GAMEMODE_COMMAND: &str = "gamemode";

/// Permission *level* required to run `/gamemode` (operator-tier, mirroring
/// vanilla). `/spawn` requires no special level.
pub const GAMEMODE_LEVEL: u8 = 2;

/// The literal name of the teleport command.
pub const TP_COMMAND: &str = "tp";
/// Permission *level* required to run `/tp` (operator-tier, matching
/// [`GAMEMODE_LEVEL`]).
pub const TP_LEVEL: u8 = 2;

/// The literal name of the weather command.
pub const WEATHER_COMMAND: &str = "weather";
/// Permission *level* required to run `/weather` (operator-tier, matching
/// [`GAMEMODE_LEVEL`]).
pub const WEATHER_LEVEL: u8 = 2;

/// The literal name of the world-time command.
pub const TIME_COMMAND: &str = "time";
/// Permission *level* required to run `/time` (operator-tier, matching
/// [`GAMEMODE_LEVEL`]).
pub const TIME_LEVEL: u8 = 2;
/// Usage shown when `/time` arguments do not parse.
const TIME_USAGE: &str =
    "usage: /time set <day|noon|night|midnight|ticks> | add <ticks> | query daytime";

/// Usage shown when `/tp` arguments do not parse.
const TP_USAGE: &str = "usage: /tp <x> <y> <z> | /tp <player>";
/// Usage shown when `/gamemode` arguments do not parse.
const GAMEMODE_USAGE: &str =
    "usage: /gamemode <survival|creative|adventure|spectator|0-3> [player]";
/// Usage shown when `/weather` arguments do not parse.
const WEATHER_USAGE: &str = "usage: /weather <clear|rain> [duration]";

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

/// The literal name of the `/scoreboard` command.
pub const SCOREBOARD_COMMAND: &str = "scoreboard";
/// The literal name of the `/team` command.
pub const TEAM_COMMAND: &str = "team";
/// The literal name of the `/bossbar` command.
pub const BOSSBAR_COMMAND: &str = "bossbar";

/// Permission *level* required to run the scoreboard/team/boss-bar commands
/// (operator-tier, matching [`GAMEMODE_LEVEL`]).
pub const SCOREBOARD_LEVEL: u8 = 2;

/// The literal name of the region-fill command.
pub const FILL_COMMAND: &str = "fill";
/// The literal name of the region-replace command.
pub const REPLACE_COMMAND: &str = "replace";
/// The literal name of the undo-last-region-edit command.
pub const UNDO_COMMAND: &str = "undo";

/// Permission *level* required to run the region build commands (operator-tier,
/// matching [`GAMEMODE_LEVEL`]). These mutate the world wholesale, so they are
/// gated like every other powerful command.
pub const REGION_EDIT_LEVEL: u8 = 2;

/// Default ceiling on the number of blocks a single `/fill` or `/replace` may
/// affect, used by the arg-less [`build_command_tree`]. Mirrors the configuration
/// default `max_region_fill_volume` and `ferrumc_sim`'s built-in region cap; the
/// running server threads its configured value through
/// [`build_command_tree_with_limits`].
pub const DEFAULT_REGION_VOLUME_CAP: u64 = 32_768;

/// Usage shown when `/fill` arguments do not parse.
const FILL_USAGE: &str = "usage: /fill <x1> <y1> <z1> <x2> <y2> <z2> <block>";
/// Usage shown when `/replace` arguments do not parse.
const REPLACE_USAGE: &str = "usage: /replace <x1> <y1> <z1> <x2> <y2> <z2> <from_block> <to_block>";

/// Usage shown when `/scoreboard` arguments do not parse.
const SCOREBOARD_USAGE: &str = "usage: /scoreboard objective add|remove <name> | \
     objective setdisplay <list|sidebar|below_name> <name> | score set <player> <objective> <value>";
/// Usage shown when `/team` arguments do not parse.
const TEAM_USAGE: &str = "usage: /team add <name> [display] | remove <name> | \
     join <team> [player] | color <team> <color>";
/// Usage shown when `/bossbar` arguments do not parse.
const BOSSBAR_USAGE: &str = "usage: /bossbar add <id> [title] | remove <id> | \
     title <id> <title> | progress <id> <0.0..=1.0> | color <id> <color>";

/// The sidebar render type every `/scoreboard objective add` uses in this slice.
const OBJECTIVE_DEFAULT_RENDER: ObjectiveRender = ObjectiveRender::Integer;
/// Full health (`1.0`) every `/bossbar add` starts at.
const BOSSBAR_FULL_HEALTH: f32 = 1.0;
/// The color every `/bossbar add` starts with (until `/bossbar color`).
const BOSSBAR_DEFAULT_COLOR: BossBarColor = BossBarColor::Pink;
/// The division every `/bossbar` style uses in this slice (a solid bar).
const BOSSBAR_DEFAULT_DIVISION: BossBarDivision = BossBarDivision::NoDivision;
/// No boss-bar flags are set by `/bossbar add` in this slice.
const BOSSBAR_DEFAULT_FLAGS: u8 = 0;

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

/// Builds the application's [`CommandTree`] with the default region-edit volume
/// cap ([`DEFAULT_REGION_VOLUME_CAP`]).
///
/// A convenience wrapper over [`build_command_tree_with_limits`] for tests and
/// any caller that does not thread configuration; the running server passes its
/// configured cap. See that function for the full command set.
pub fn build_command_tree() -> CommandTree {
    build_command_tree_with_limits(DEFAULT_REGION_VOLUME_CAP)
}

/// Builds the application's [`CommandTree`], wiring `/spawn`, `/gamemode`, the
/// presentation and scoreboard commands, and the region build commands
/// (`/fill`, `/replace`, `/undo`).
///
/// `/spawn` always succeeds (its teleport side effect is applied by the
/// connection on a successful dispatch). `/gamemode <mode>` takes an integer in
/// `0..=3`, range-checked by the argument type and mapped to a [`GameMode`];
/// it requires permission level [`GAMEMODE_LEVEL`].
///
/// `region_volume_cap` bounds `/fill` and `/replace`: a cuboid larger than it is
/// rejected with a clear command error (the user-facing gate). The simulation
/// re-checks the cap defensively. The block mutations are routed to the
/// simulation by [`region_commands`] on a successful dispatch.
pub fn build_command_tree_with_limits(region_volume_cap: u64) -> CommandTree {
    let mut tree = CommandTree::new();

    tree.register(literal(SPAWN_COMMAND).executes(|ctx| {
        CommandResult::success(TextComponent::text(format!(
            "{} teleported to spawn",
            ctx.source().name()
        )))
    }));

    // `/gamemode <mode> [player]` (self or targeted), `/tp <x y z>|<player>`, and
    // `/weather <clear|rain> [duration]` — built by dedicated helpers to keep this
    // registration function readable.
    tree.register(gamemode_command());
    tree.register(tp_command());
    tree.register(weather_command());
    tree.register(time_command());

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

    // `/scoreboard`, `/team`, `/bossbar`: each takes a greedy argument tail parsed
    // by its own validator; the clientbound packets are built on success by
    // `scoreboard_packets`. The handler here only validates and reports feedback.
    tree.register(greedy_command(
        SCOREBOARD_COMMAND,
        SCOREBOARD_USAGE,
        |args| parse_scoreboard(args).map(|a| a.feedback()),
    ));
    tree.register(greedy_command(TEAM_COMMAND, TEAM_USAGE, |args| {
        // The issuer name is irrelevant to validation feedback; `join` defaults to
        // it only when the packets are built.
        parse_team(args, "").map(|a| a.feedback())
    }));
    tree.register(greedy_command(BOSSBAR_COMMAND, BOSSBAR_USAGE, |args| {
        parse_bossbar(args).map(|a| a.feedback())
    }));

    // Region build commands. `/fill` and `/replace` validate their cuboid against
    // `region_volume_cap` (captured by the executor) and report a clear error when
    // it is exceeded; the block mutations themselves are routed to the simulation
    // by `region_commands` on a successful dispatch. `/undo` takes no arguments —
    // the simulation no-ops when the issuer has no recorded edit.
    tree.register(region_fill_command(region_volume_cap));
    tree.register(region_replace_command(region_volume_cap));
    tree.register(
        literal(UNDO_COMMAND)
            .requires_level(REGION_EDIT_LEVEL)
            .executes(|_| {
                CommandResult::success(TextComponent::text("Undoing your last region edit"))
            }),
    );

    tree
}

/// Builds a level-gated literal command taking one greedy argument tail. The
/// `validate` closure parses that tail, returning `Some(feedback)` for a valid
/// invocation (reported as success) or `None` for a malformed one (reported as
/// `usage`).
fn greedy_command(
    name: &'static str,
    usage: &'static str,
    validate: fn(&str) -> Option<String>,
) -> CommandBuilder {
    literal(name).requires_level(SCOREBOARD_LEVEL).then(
        argument("args", ArgumentType::GreedyString).executes(move |ctx| {
            let args = ctx.string("args").unwrap_or_default();
            match validate(args) {
                Some(feedback) => CommandResult::success(TextComponent::text(feedback)),
                None => CommandResult::failure(TextComponent::text(usage)),
            }
        }),
    )
}

/// Builds the `/gamemode <mode> [player]` command.
///
/// `mode` is a [`ArgumentType::Word`] (a name like `creative` or a `0..=3` id,
/// both resolved by [`gamemode_by_name`]), so it accepts more than a bare integer.
/// The bare `/gamemode <mode>` form retargets the issuer (its `GameEvent` plus
/// authoritative `SetGameMode` are applied by the connection via
/// [`parse_gamemode`]); the `/gamemode <mode> <player>` form retargets the named
/// online player and is routed to the simulation by [`region_commands`].
fn gamemode_command() -> CommandBuilder {
    literal(GAMEMODE_COMMAND)
        .requires_level(GAMEMODE_LEVEL)
        .then(
            argument("mode", ArgumentType::Word)
                .executes(|ctx| gamemode_feedback(ctx.string("mode").unwrap_or_default(), None))
                .then(argument("target", ArgumentType::Word).executes(|ctx| {
                    gamemode_feedback(
                        ctx.string("mode").unwrap_or_default(),
                        Some(ctx.string("target").unwrap_or_default()),
                    )
                })),
        )
}

/// Builds the `/tp <x> <y> <z>` (teleport to coordinates) and `/tp <player>`
/// (teleport to an online player) command.
///
/// Both forms parse from a greedy tail and are routed to the simulation by
/// [`region_commands`]; the issuer's client is snapped via the existing teleport
/// path the driver owns.
fn tp_command() -> CommandBuilder {
    literal(TP_COMMAND).requires_level(TP_LEVEL).then(
        argument("args", ArgumentType::GreedyString).executes(|ctx| {
            let args = ctx.string("args").unwrap_or_default();
            match parse_tp(args) {
                Some(TpTarget::Coords(pos)) => CommandResult::success(TextComponent::text(
                    format!("Teleported to {} {} {}", pos.x, pos.y, pos.z),
                )),
                Some(TpTarget::Player(name)) => {
                    CommandResult::success(TextComponent::text(format!("Teleporting to {name}")))
                }
                None => CommandResult::failure(TextComponent::text(TP_USAGE)),
            }
        }),
    )
}

/// Builds the `/weather <clear|rain> [duration]` command: a server-wide weather
/// toggle broadcast to every player as a `GameEvent` by the driver (no rain
/// simulation in this slice). The optional duration is accepted but unused beyond
/// the feedback.
fn weather_command() -> CommandBuilder {
    literal(WEATHER_COMMAND).requires_level(WEATHER_LEVEL).then(
        argument("args", ArgumentType::GreedyString).executes(|ctx| {
            let args = ctx.string("args").unwrap_or_default();
            match parse_weather(args) {
                Some((kind, duration)) => {
                    CommandResult::success(TextComponent::text(weather_feedback(kind, duration)))
                }
                None => CommandResult::failure(TextComponent::text(WEATHER_USAGE)),
            }
        }),
    )
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

/// Parses the [`GameMode`] a *self-targeted* `/gamemode <mode>` command selects,
/// or `None` if `command` is not a `/gamemode` invocation, its mode argument is
/// missing or invalid, **or** it names another player (the `/gamemode <mode>
/// <player>` form).
///
/// The connection uses this after a successful dispatch to apply the issuer's own
/// game-mode change side effect (a clientbound `GameEvent` plus the authoritative
/// `SetGameMode`), parsing the same argument the handler validated so the two
/// agree on the selected mode. The targeted form returns `None` here so that side
/// effect is suppressed for the issuer; it is routed to the named player by
/// [`region_commands`] instead.
pub fn parse_gamemode(command: &str) -> Option<GameMode> {
    let mut tokens = command.split_whitespace();
    if tokens.next() != Some(GAMEMODE_COMMAND) {
        return None;
    }
    let mode = gamemode_by_name(tokens.next()?)?;
    // A trailing token names another player: that case is not self-targeted.
    only(mode, tokens)
}

/// Parses the targeted `/gamemode <mode> <player>` form into its mode and target
/// player name, or `None` if `command` is not a `/gamemode` invocation, omits the
/// player, has an invalid mode, or carries trailing tokens.
///
/// [`region_commands`] uses this to route the change to the named online player
/// (resolved against the live roster by the driver); the self-targeted form
/// (no player) returns `None` here and is handled via [`parse_gamemode`].
fn parse_gamemode_target(args: &str) -> Option<(GameMode, String)> {
    let mut tokens = args.split_whitespace();
    let mode = gamemode_by_name(tokens.next()?)?;
    let target = tokens.next()?.to_owned();
    only((), tokens)?;
    Some((mode, target))
}

/// Resolves a `/gamemode` mode token — a name (`survival`, `creative`,
/// `adventure`, `spectator`) or its `0..=3` id — to a [`GameMode`], or `None` for
/// any other token.
fn gamemode_by_name(token: &str) -> Option<GameMode> {
    let mode = match token {
        "survival" | "0" => GameMode::Survival,
        "creative" | "1" => GameMode::Creative,
        "adventure" | "2" => GameMode::Adventure,
        "spectator" | "3" => GameMode::Spectator,
        _ => return None,
    };
    Some(mode)
}

/// Builds the `/gamemode` command feedback for a validated `mode` token, naming
/// `target` when the change retargets another player, or the usage error when the
/// mode token is invalid.
fn gamemode_feedback(mode: &str, target: Option<&str>) -> CommandResult {
    match gamemode_by_name(mode) {
        Some(mode) => CommandResult::success(TextComponent::text(match target {
            Some(player) => format!("Set {player}'s game mode to {mode:?}"),
            None => format!("Game mode set to {mode:?}"),
        })),
        None => CommandResult::failure(TextComponent::text(GAMEMODE_USAGE)),
    }
}

/// A parsed `/tp` destination: explicit coordinates or another player to teleport
/// to.
enum TpTarget {
    /// `/tp <x> <y> <z>` — absolute coordinates.
    Coords(Vec3),
    /// `/tp <player>` — the named online player's current position.
    Player(String),
}

/// Parses a `/tp` argument tail: a single token is a player name, exactly three
/// tokens are safe finite/in-range `f64` coordinates, and anything else (zero,
/// two, or 4+ tokens, a non-numeric coordinate, or an unsafe destination) is
/// `None`.
fn parse_tp(args: &str) -> Option<TpTarget> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    match tokens.as_slice() {
        [name] => Some(TpTarget::Player((*name).to_owned())),
        [x, y, z] => {
            let position = Vec3::new(x.parse().ok()?, y.parse().ok()?, z.parse().ok()?);
            is_valid_player_position(position).then_some(TpTarget::Coords(position))
        }
        _ => None,
    }
}

/// A parsed `/weather` selection: the two client-visible weather states this
/// slice toggles.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WeatherKind {
    /// `clear` — stop raining.
    Clear,
    /// `rain` — start raining.
    Rain,
}

/// Parses a `/weather` argument tail (`<clear|rain> [duration]`) into its weather
/// kind and optional duration (seconds), or `None` on an unknown kind, a
/// non-integer duration, or trailing tokens. The duration is cosmetic in this
/// slice (no rain simulation), surfaced only in the command feedback.
fn parse_weather(args: &str) -> Option<(WeatherKind, Option<u32>)> {
    let mut tokens = args.split_whitespace();
    let kind = match tokens.next()? {
        "clear" => WeatherKind::Clear,
        "rain" => WeatherKind::Rain,
        _ => return None,
    };
    let duration = match tokens.next() {
        Some(token) => Some(token.parse().ok()?),
        None => None,
    };
    only((), tokens)?;
    Some((kind, duration))
}

/// Builds the `/weather` command feedback for a validated selection.
fn weather_feedback(kind: WeatherKind, duration: Option<u32>) -> String {
    let label = match kind {
        WeatherKind::Clear => "clear",
        WeatherKind::Rain => "rain",
    };
    match duration {
        Some(secs) => format!("Set the weather to {label} for {secs}s"),
        None => format!("Set the weather to {label}"),
    }
}

/// Builds the `/time set <day|noon|night|midnight|ticks>`, `/time add <ticks>`, and
/// `/time query daytime` command (operator-gated).
///
/// The day-night phase lives in the simulation/driver, not the command layer, so
/// this only validates and reports feedback; [`region_commands`] routes the
/// matching [`SimCommand::SetTime`] / [`SimCommand::AddTime`] / [`SimCommand::QueryTime`]
/// to the driver on a successful dispatch. The query carries no command-layer
/// feedback (the driver replies with the live value), so its handler returns empty
/// feedback the connection suppresses.
fn time_command() -> CommandBuilder {
    literal(TIME_COMMAND).requires_level(TIME_LEVEL).then(
        argument("args", ArgumentType::GreedyString).executes(|ctx| {
            let args = ctx.string("args").unwrap_or_default();
            match parse_time(args) {
                Some(action) => CommandResult::success(TextComponent::text(action.feedback())),
                None => CommandResult::failure(TextComponent::text(TIME_USAGE)),
            }
        }),
    )
}

/// A parsed `/time` invocation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TimeAction {
    /// `set <phase|ticks>` — set the absolute day-night phase.
    Set { time_of_day: i64 },
    /// `add <ticks>` — add (signed) to the day-night phase.
    Add { ticks: i64 },
    /// `query daytime` — report the current day-night phase.
    QueryDaytime,
}

impl TimeAction {
    /// The success message shown to the issuer.
    ///
    /// `query daytime` returns an empty string: the driver answers with the live
    /// value, so the connection suppresses the command-layer line to avoid a
    /// duplicate (blank-feedback handlers are skipped at the dispatch site).
    fn feedback(self) -> String {
        match self {
            Self::Set { time_of_day } => format!("Set the time to {time_of_day}"),
            Self::Add { ticks } => format!("Added {ticks} to the time"),
            Self::QueryDaytime => String::new(),
        }
    }
}

/// Parses a `/time` argument tail, or `None` if it is not a supported form (an
/// unknown subcommand, a non-integer tick count, an unknown phase name, or
/// trailing tokens).
fn parse_time(args: &str) -> Option<TimeAction> {
    let mut tokens = args.split_whitespace();
    match tokens.next()? {
        "set" => {
            let time_of_day = time_phase_by_name(tokens.next()?)?;
            only((), tokens)?;
            Some(TimeAction::Set { time_of_day })
        }
        "add" => {
            let ticks: i64 = tokens.next()?.parse().ok()?;
            only((), tokens)?;
            Some(TimeAction::Add { ticks })
        }
        "query" => match tokens.next()? {
            "daytime" => only(TimeAction::QueryDaytime, tokens),
            _ => None,
        },
        _ => None,
    }
}

/// Resolves a `/time set` phase argument: a named phase (`day`, `noon`, `night`,
/// `midnight`) or a raw tick count. Returns `None` for an unknown name that is also
/// not a valid integer.
fn time_phase_by_name(token: &str) -> Option<i64> {
    let phase = match token {
        "day" => TIME_DAY,
        "noon" => TIME_NOON,
        "night" => TIME_NIGHT,
        "midnight" => TIME_MIDNIGHT,
        other => other.parse().ok()?,
    };
    Some(phase)
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

/// Builds the clientbound packets a successfully dispatched `/scoreboard`,
/// `/team`, or `/bossbar` command produces, or an empty vector for any other
/// command.
///
/// Mirrors [`presentation_packets`]: the connection calls this after a successful
/// dispatch and enqueues each packet on the player's writer, re-parsing the same
/// argument the executor validated. `issuer` is the running player's name, used as
/// the default target of `/team join`. Unlike the presentation builders these
/// hand-encode NBT/string bodies, so building is fallible; a parse miss yields an
/// empty vector (the command already reported its own failure feedback).
///
/// # Errors
///
/// Returns an error only if a packet body cannot be encoded (a name past the
/// protocol cap or an NBT body past its limits) — unreachable for the bounded,
/// already-validated inputs a dispatched command carries.
pub fn scoreboard_packets(
    command: &str,
    issuer: &str,
) -> anyhow::Result<Vec<ClientboundPlayPacket>> {
    let Some(name) = command.split_whitespace().next() else {
        return Ok(Vec::new());
    };
    let args = command_args(command);
    let packets = match name {
        SCOREBOARD_COMMAND => match parse_scoreboard(args) {
            Some(action) => action.into_packets()?,
            None => Vec::new(),
        },
        TEAM_COMMAND => match parse_team(args, issuer) {
            Some(action) => action.into_packets()?,
            None => Vec::new(),
        },
        BOSSBAR_COMMAND => match parse_bossbar(args) {
            Some(action) => action.into_packets()?,
            None => Vec::new(),
        },
        _ => Vec::new(),
    };
    Ok(packets)
}

// ---------------------------------------------------------------------------
// /scoreboard
// ---------------------------------------------------------------------------

/// A parsed `/scoreboard` invocation.
enum ScoreboardAction {
    /// `objective add <name> [display]`.
    ObjectiveAdd { name: String, display: String },
    /// `objective remove <name>`.
    ObjectiveRemove { name: String },
    /// `objective setdisplay <slot> <name>`.
    SetDisplay { slot: i32, name: String },
    /// `score set <player> <objective> <value>`.
    ScoreSet {
        entity: String,
        objective: String,
        value: i32,
    },
}

impl ScoreboardAction {
    /// The success message shown to the issuer.
    fn feedback(&self) -> String {
        match self {
            Self::ObjectiveAdd { name, .. } => format!("Created objective {name}"),
            Self::ObjectiveRemove { name } => format!("Removed objective {name}"),
            Self::SetDisplay { name, .. } => format!("Now displaying objective {name}"),
            Self::ScoreSet {
                entity,
                objective,
                value,
            } => format!("Set {entity}'s {objective} score to {value}"),
        }
    }

    /// Builds the clientbound packets for this action.
    fn into_packets(self) -> anyhow::Result<Vec<ClientboundPlayPacket>> {
        let packet = match self {
            Self::ObjectiveAdd { name, display } => objective_create(
                &name,
                &TextComponent::text(display),
                OBJECTIVE_DEFAULT_RENDER,
            )?,
            Self::ObjectiveRemove { name } => objective_remove(&name)?,
            Self::SetDisplay { slot, name } => display_objective(slot, &name)?,
            Self::ScoreSet {
                entity,
                objective,
                value,
            } => score_set(&entity, &objective, value)?,
        };
        Ok(vec![packet])
    }
}

/// Parses a `/scoreboard` argument tail, or `None` if it is not a supported form.
fn parse_scoreboard(args: &str) -> Option<ScoreboardAction> {
    let mut tokens = args.split_whitespace();
    match tokens.next()? {
        "objective" => match tokens.next()? {
            "add" => {
                let name = tokens.next()?.to_owned();
                let display = join_rest(tokens);
                let display = if display.is_empty() {
                    name.clone()
                } else {
                    display
                };
                Some(ScoreboardAction::ObjectiveAdd { name, display })
            }
            "remove" => Some(ScoreboardAction::ObjectiveRemove {
                name: only(tokens.next()?, tokens)?.to_owned(),
            }),
            "setdisplay" => {
                let slot = display_slot_by_name(tokens.next()?)?;
                let name = only(tokens.next()?, tokens)?.to_owned();
                Some(ScoreboardAction::SetDisplay { slot, name })
            }
            _ => None,
        },
        "score" => match tokens.next()? {
            "set" => {
                let entity = tokens.next()?.to_owned();
                let objective = tokens.next()?.to_owned();
                let value: i32 = tokens.next()?.parse().ok()?;
                only((), tokens)?; // reject trailing tokens
                Some(ScoreboardAction::ScoreSet {
                    entity,
                    objective,
                    value,
                })
            }
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// /team
// ---------------------------------------------------------------------------

/// A parsed `/team` invocation.
enum TeamAction {
    /// `add <name> [display]`.
    Add { name: String, display: String },
    /// `remove <name>`.
    Remove { name: String },
    /// `join <team> [player]` (player defaults to the issuer).
    Join { team: String, player: String },
    /// `color <team> <color>`.
    Color { team: String, color: TextColor },
}

impl TeamAction {
    /// The success message shown to the issuer.
    fn feedback(&self) -> String {
        match self {
            Self::Add { name, .. } => format!("Created team {name}"),
            Self::Remove { name } => format!("Removed team {name}"),
            Self::Join { team, player } => format!("Added {player} to team {team}"),
            Self::Color { team, color } => format!("Set team {team} color to {color}"),
        }
    }

    /// Builds the clientbound packets for this action.
    fn into_packets(self) -> anyhow::Result<Vec<ClientboundPlayPacket>> {
        let packet = match self {
            Self::Add { name, display } => team_create(&name, &TextComponent::text(display), None)?,
            Self::Remove { name } => team_remove(&name)?,
            Self::Join { team, player } => team_add_entities(&team, &[player.as_str()])?,
            // We persist no team metadata in this slice, so a color change re-sends
            // an update carrying the team name as its display and the new color.
            Self::Color { team, color } => {
                team_update(&team, &TextComponent::text(team.clone()), Some(color))?
            }
        };
        Ok(vec![packet])
    }
}

/// Parses a `/team` argument tail, defaulting `join`'s target to `issuer`, or
/// `None` if it is not a supported form.
fn parse_team(args: &str, issuer: &str) -> Option<TeamAction> {
    let mut tokens = args.split_whitespace();
    match tokens.next()? {
        "add" => {
            let name = tokens.next()?.to_owned();
            let display = join_rest(tokens);
            let display = if display.is_empty() {
                name.clone()
            } else {
                display
            };
            Some(TeamAction::Add { name, display })
        }
        "remove" => Some(TeamAction::Remove {
            name: only(tokens.next()?, tokens)?.to_owned(),
        }),
        "join" => {
            let team = tokens.next()?.to_owned();
            let player = match tokens.next() {
                Some(name) => only(name, tokens)?.to_owned(),
                None => issuer.to_owned(),
            };
            Some(TeamAction::Join { team, player })
        }
        "color" => {
            let team = tokens.next()?.to_owned();
            let color = text_color_by_name(tokens.next()?)?;
            only((), tokens)?;
            Some(TeamAction::Color { team, color })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// /bossbar
// ---------------------------------------------------------------------------

/// A parsed `/bossbar` invocation. Each bar is keyed by a stable UUID derived
/// from its friendly `id` (see [`bossbar_uuid`]).
enum BossAction {
    /// `add <id> [title]`.
    Add { id: String, title: String },
    /// `remove <id>`.
    Remove { id: String },
    /// `title <id> <title>`.
    Title { id: String, title: String },
    /// `progress <id> <0.0..=1.0>`.
    Progress { id: String, health: f32 },
    /// `color <id> <color>`.
    Color { id: String, color: BossBarColor },
}

impl BossAction {
    /// The success message shown to the issuer.
    fn feedback(&self) -> String {
        match self {
            Self::Add { id, .. } => format!("Created boss bar {id}"),
            Self::Remove { id } => format!("Removed boss bar {id}"),
            Self::Title { id, .. } => format!("Set boss bar {id} title"),
            Self::Progress { id, health } => format!("Set boss bar {id} progress to {health}"),
            Self::Color { id, color } => format!("Set boss bar {id} color to {color:?}"),
        }
    }

    /// Builds the clientbound packets for this action.
    fn into_packets(self) -> anyhow::Result<Vec<ClientboundPlayPacket>> {
        let packet = match self {
            Self::Add { id, title } => boss_bar_add(
                bossbar_uuid(&id),
                &TextComponent::text(title),
                BOSSBAR_FULL_HEALTH,
                BOSSBAR_DEFAULT_COLOR,
                BOSSBAR_DEFAULT_DIVISION,
                BOSSBAR_DEFAULT_FLAGS,
            )?,
            Self::Remove { id } => boss_bar_remove(bossbar_uuid(&id)),
            Self::Title { id, title } => {
                boss_bar_update_title(bossbar_uuid(&id), &TextComponent::text(title))?
            }
            Self::Progress { id, health } => boss_bar_update_health(bossbar_uuid(&id), health),
            Self::Color { id, color } => {
                boss_bar_update_style(bossbar_uuid(&id), color, BOSSBAR_DEFAULT_DIVISION)
            }
        };
        Ok(vec![packet])
    }
}

/// Parses a `/bossbar` argument tail, or `None` if it is not a supported form.
fn parse_bossbar(args: &str) -> Option<BossAction> {
    let mut tokens = args.split_whitespace();
    match tokens.next()? {
        "add" => {
            let id = tokens.next()?.to_owned();
            let title = join_rest(tokens);
            let title = if title.is_empty() { id.clone() } else { title };
            Some(BossAction::Add { id, title })
        }
        "remove" => Some(BossAction::Remove {
            id: only(tokens.next()?, tokens)?.to_owned(),
        }),
        "title" => {
            let id = tokens.next()?.to_owned();
            let title = join_rest(tokens);
            if title.is_empty() {
                return None;
            }
            Some(BossAction::Title { id, title })
        }
        "progress" => {
            let id = tokens.next()?.to_owned();
            let health: f32 = tokens.next()?.parse().ok()?;
            only((), tokens)?;
            // Health is a 0.0..=1.0 fraction; reject anything outside it (and NaN).
            if !(0.0..=1.0).contains(&health) {
                return None;
            }
            Some(BossAction::Progress { id, health })
        }
        "color" => {
            let id = tokens.next()?.to_owned();
            let color = boss_color_by_name(tokens.next()?)?;
            only((), tokens)?;
            Some(BossAction::Color { id, color })
        }
        _ => None,
    }
}

/// Derives a stable boss-bar UUID from its friendly `id` so that `/bossbar title`
/// and friends target the same bar `/bossbar add` created, with no need to store
/// the mapping. Two salted FNV-1a passes fill the 128 bits deterministically.
fn bossbar_uuid(id: &str) -> Uuid {
    Uuid::from_u64_pair(fnv1a64(0x01, id.as_bytes()), fnv1a64(0x02, id.as_bytes()))
}

/// A salted 64-bit FNV-1a hash. Deterministic and dependency-free; the salt lets
/// two passes over the same input produce two independent halves.
fn fnv1a64(salt: u8, data: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET ^ u64::from(salt);
    for &byte in data {
        hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
    }
    hash
}

/// Resolves a `/scoreboard objective setdisplay` slot name to its wire position.
fn display_slot_by_name(name: &str) -> Option<i32> {
    match name {
        "list" | "tab" => Some(DISPLAY_SLOT_LIST),
        "sidebar" => Some(DISPLAY_SLOT_SIDEBAR),
        "below_name" | "belowname" | "below" => Some(DISPLAY_SLOT_BELOW_NAME),
        _ => None,
    }
}

/// Maps a color name to its [`TextColor`] (the 16 named Minecraft colors).
fn text_color_by_name(name: &str) -> Option<TextColor> {
    let color = match name {
        "black" => TextColor::Black,
        "dark_blue" => TextColor::DarkBlue,
        "dark_green" => TextColor::DarkGreen,
        "dark_aqua" => TextColor::DarkAqua,
        "dark_red" => TextColor::DarkRed,
        "dark_purple" => TextColor::DarkPurple,
        "gold" => TextColor::Gold,
        "gray" => TextColor::Gray,
        "dark_gray" => TextColor::DarkGray,
        "blue" => TextColor::Blue,
        "green" => TextColor::Green,
        "aqua" => TextColor::Aqua,
        "red" => TextColor::Red,
        "light_purple" => TextColor::LightPurple,
        "yellow" => TextColor::Yellow,
        "white" => TextColor::White,
        _ => return None,
    };
    Some(color)
}

/// Maps a color name to its [`BossBarColor`] (the seven boss-bar colors).
fn boss_color_by_name(name: &str) -> Option<BossBarColor> {
    let color = match name {
        "pink" => BossBarColor::Pink,
        "blue" => BossBarColor::Blue,
        "red" => BossBarColor::Red,
        "green" => BossBarColor::Green,
        "yellow" => BossBarColor::Yellow,
        "purple" => BossBarColor::Purple,
        "white" => BossBarColor::White,
        _ => return None,
    };
    Some(color)
}

/// Joins the remaining tokens with single spaces (used for free-text tails like a
/// display name or boss-bar title).
fn join_rest<'a>(tokens: impl Iterator<Item = &'a str>) -> String {
    tokens.collect::<Vec<_>>().join(" ")
}

/// Returns `value` only if `tokens` is exhausted, enforcing "no trailing tokens".
/// `value` is passed through so callers can chain it inline.
fn only<'a, T>(value: T, mut tokens: impl Iterator<Item = &'a str>) -> Option<T> {
    match tokens.next() {
        None => Some(value),
        Some(_) => None,
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

// ---------------------------------------------------------------------------
// /fill, /replace, /undo — region build commands
// ---------------------------------------------------------------------------

/// Builds the `/fill <x1> <y1> <z1> <x2> <y2> <z2> <block>` command.
///
/// The executor parses the cuboid and block name, then reports success or a
/// clear over-cap error; `cap` (the configured volume limit) is captured so the
/// feedback is generated where the user sees it. The block mutations are applied
/// by [`region_commands`] on success.
fn region_fill_command(cap: u64) -> CommandBuilder {
    literal(FILL_COMMAND)
        .requires_level(REGION_EDIT_LEVEL)
        .then(
            argument("args", ArgumentType::GreedyString).executes(move |ctx| {
                let args = ctx.string("args").unwrap_or_default();
                match parse_fill_args(args) {
                    Some((region, _)) => {
                        bounded_feedback(region, cap, format!("Filled {} blocks", region.volume()))
                    }
                    None => CommandResult::failure(TextComponent::text(FILL_USAGE)),
                }
            }),
        )
}

/// Builds the `/replace <x1> <y1> <z1> <x2> <y2> <z2> <from> <to>` command. As
/// with [`region_fill_command`], the executor validates and reports feedback and
/// [`region_commands`] applies the change on success.
fn region_replace_command(cap: u64) -> CommandBuilder {
    literal(REPLACE_COMMAND)
        .requires_level(REGION_EDIT_LEVEL)
        .then(
            argument("args", ArgumentType::GreedyString).executes(move |ctx| {
                let args = ctx.string("args").unwrap_or_default();
                match parse_replace_args(args) {
                    Some((region, _, _)) => bounded_feedback(
                        region,
                        cap,
                        format!("Replacing blocks across a {}-block region", region.volume()),
                    ),
                    None => CommandResult::failure(TextComponent::text(REPLACE_USAGE)),
                }
            }),
        )
}

/// Reports a region command's success, or a clear failure when the cuboid's
/// volume exceeds `cap` — the denial-of-service gate that stops one command from
/// addressing an unbounded number of blocks.
fn bounded_feedback(region: Cuboid, cap: u64, success: String) -> CommandResult {
    let volume = region.volume();
    if volume > cap {
        CommandResult::failure(TextComponent::text(format!(
            "region too large: {volume} blocks exceeds the {cap}-block limit"
        )))
    } else {
        CommandResult::success(TextComponent::text(success))
    }
}

/// Builds the [`SimCommand`]s a successfully dispatched command produces that the
/// simulation must apply, or an empty vector for any command with no such effect.
///
/// This is the generic command→[`SimCommand`] hook: the connection calls it after
/// a successful dispatch and sends each returned command to the simulation driver
/// (the only side that may reach the shard and other players' channels). `player`
/// is the issuer. It re-parses the same arguments the executor validated. Covered
/// commands:
///
/// - `/fill`, `/replace`, `/undo` — region edits applied through the shard-owned
///   block funnel (an over-cap region never reaches here; the executor rejected it
///   and the simulation re-checks the cap defensively regardless).
/// - `/tp <x> <y> <z>` — a [`SimCommand::TeleportPlayer`] snapping the issuer.
/// - `/tp <player>` — a [`SimCommand::TeleportToPlayer`]; the driver resolves the
///   named player against the live roster and snaps the issuer to their position.
/// - `/gamemode <mode> <player>` — a [`SimCommand::SetGameModeFor`] retargeting the
///   named online player (the self-targeted form carries no command here; the
///   connection applies it via [`parse_gamemode`]).
/// - `/weather <clear|rain>` — a [`SimCommand::SetWeather`] the driver broadcasts
///   to every player as a `GameEvent`.
/// - `/time set|add|query` — a [`SimCommand::SetTime`] / [`SimCommand::AddTime`] /
///   [`SimCommand::QueryTime`]; the driver owns the world clock, broadcasting the
///   new time on set/add and replying to the issuer on query.
pub(crate) fn region_commands(command: &str, player: PlayerId) -> Vec<SimCommand> {
    let Some(name) = command.split_whitespace().next() else {
        return Vec::new();
    };
    let args = command_args(command);
    match name {
        FILL_COMMAND => match parse_fill_args(args) {
            Some((region, state)) => vec![SimCommand::RegionEdit {
                player,
                region,
                op: RegionOp::Fill { state },
                acceptance: None,
            }],
            None => Vec::new(),
        },
        REPLACE_COMMAND => match parse_replace_args(args) {
            Some((region, from, to)) => vec![SimCommand::RegionEdit {
                player,
                region,
                op: RegionOp::Replace { from, to },
                acceptance: None,
            }],
            None => Vec::new(),
        },
        // `/undo` takes no arguments; a trailing token makes it a parse miss.
        UNDO_COMMAND if args.is_empty() => vec![SimCommand::RegionUndo {
            player,
            acceptance: None,
        }],
        TP_COMMAND => match parse_tp(args) {
            Some(TpTarget::Coords(position)) => vec![SimCommand::TeleportPlayer {
                player,
                position,
                acceptance: None,
            }],
            Some(TpTarget::Player(target)) => vec![SimCommand::TeleportToPlayer {
                player,
                target,
                acceptance: None,
            }],
            None => Vec::new(),
        },
        // Only the targeted form routes here; the self form is applied by the
        // connection via `parse_gamemode`.
        GAMEMODE_COMMAND => match parse_gamemode_target(args) {
            Some((mode, target)) => vec![SimCommand::SetGameModeFor {
                target,
                mode,
                acceptance: None,
            }],
            None => Vec::new(),
        },
        WEATHER_COMMAND => match parse_weather(args) {
            Some((kind, _duration)) => vec![SimCommand::SetWeather {
                raining: kind == WeatherKind::Rain,
            }],
            None => Vec::new(),
        },
        TIME_COMMAND => match parse_time(args) {
            Some(TimeAction::Set { time_of_day }) => vec![SimCommand::SetTime { time_of_day }],
            Some(TimeAction::Add { ticks }) => vec![SimCommand::AddTime { ticks }],
            Some(TimeAction::QueryDaytime) => vec![SimCommand::QueryTime { player }],
            None => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Parses a `/fill` argument tail (`<x1> <y1> <z1> <x2> <y2> <z2> <block>`) into
/// its cuboid and fill block-state, or `None` if it is malformed (bad
/// coordinate, unknown block, or trailing tokens).
fn parse_fill_args(args: &str) -> Option<(Cuboid, BlockStateId)> {
    let mut tokens = args.split_whitespace();
    let region = parse_cuboid(&mut tokens)?;
    let state = block_state_by_name(tokens.next()?)?;
    only((), tokens)?;
    Some((region, state))
}

/// Parses a `/replace` argument tail
/// (`<x1> <y1> <z1> <x2> <y2> <z2> <from> <to>`) into its cuboid and the from/to
/// block-states, or `None` if it is malformed.
fn parse_replace_args(args: &str) -> Option<(Cuboid, BlockStateId, BlockStateId)> {
    let mut tokens = args.split_whitespace();
    let region = parse_cuboid(&mut tokens)?;
    let from = block_state_by_name(tokens.next()?)?;
    let to = block_state_by_name(tokens.next()?)?;
    only((), tokens)?;
    Some((region, from, to))
}

/// Reads six whitespace-separated `i32` coordinates (`x1 y1 z1 x2 y2 z2`) from
/// `tokens` and builds the inclusive [`Cuboid`] they bound, or `None` if a
/// coordinate is missing or not an integer.
fn parse_cuboid<'a>(tokens: &mut impl Iterator<Item = &'a str>) -> Option<Cuboid> {
    let x1: i32 = tokens.next()?.parse().ok()?;
    let y1: i32 = tokens.next()?.parse().ok()?;
    let z1: i32 = tokens.next()?.parse().ok()?;
    let x2: i32 = tokens.next()?.parse().ok()?;
    let y2: i32 = tokens.next()?.parse().ok()?;
    let z2: i32 = tokens.next()?.parse().ok()?;
    Some(Cuboid::new(
        BlockPos::new(x1, y1, z1),
        BlockPos::new(x2, y2, z2),
    ))
}

/// Resolves a block resource location (`"minecraft:stone"` or the bare
/// `"stone"`) to its default block-state id via the registry, or `None` for an
/// unknown block.
fn block_state_by_name(name: &str) -> Option<BlockStateId> {
    block_default_state(name).map(BlockStateId::new)
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
    fn gamemode_accepts_named_mode_for_an_operator() {
        let tree = build_command_tree();
        let result = tree
            .dispatch("gamemode creative", &op())
            .expect("dispatches");
        assert!(result.is_success());
        assert_eq!(
            result.feedback().to_plain_string(),
            "Game mode set to Creative"
        );
    }

    #[test]
    fn gamemode_rejects_invalid_mode() {
        // A `Word` mode arg always dispatches; an unrecognised mode (out-of-range
        // id or a typo'd name) is reported by the handler as a usage failure.
        let tree = build_command_tree();
        for cmd in ["gamemode 9", "gamemode creativ"] {
            let result = tree.dispatch(cmd, &op()).expect("reaches the handler");
            assert!(!result.is_success(), "{cmd}");
        }
    }

    #[test]
    fn gamemode_targets_another_player() {
        let tree = build_command_tree();
        let result = tree
            .dispatch("gamemode creative Joe", &op())
            .expect("dispatches");
        assert!(result.is_success());
        assert_eq!(
            result.feedback().to_plain_string(),
            "Set Joe's game mode to Creative"
        );
    }

    #[test]
    fn parse_gamemode_extracts_a_valid_self_mode() {
        // Both id and name forms resolve for the self-targeted command.
        assert_eq!(parse_gamemode("gamemode 0"), Some(GameMode::Survival));
        assert_eq!(parse_gamemode("gamemode 1"), Some(GameMode::Creative));
        assert_eq!(parse_gamemode("gamemode 3"), Some(GameMode::Spectator));
        assert_eq!(
            parse_gamemode("gamemode survival"),
            Some(GameMode::Survival)
        );
        assert_eq!(
            parse_gamemode("gamemode spectator"),
            Some(GameMode::Spectator)
        );
    }

    #[test]
    fn parse_gamemode_rejects_non_gamemode_or_bad_arg() {
        assert_eq!(parse_gamemode("spawn"), None);
        assert_eq!(parse_gamemode("gamemode"), None);
        assert_eq!(parse_gamemode("gamemode 9"), None);
        assert_eq!(parse_gamemode("gamemode x"), None);
        // The targeted form is NOT self-targeted, so the connection's self side
        // effect is suppressed (it routes through `region_commands` instead).
        assert_eq!(parse_gamemode("gamemode 1 Joe"), None);
        assert_eq!(parse_gamemode("gamemode creative Joe"), None);
    }

    #[test]
    fn gamemode_target_routes_a_set_game_mode_for() {
        // The targeted form builds a SetGameModeFor; the self form builds nothing
        // here (the connection applies it via parse_gamemode).
        let player = PlayerId::offline("Op");
        let cmds = region_commands("gamemode creative Joe", player);
        let [SimCommand::SetGameModeFor { target, mode, .. }] = cmds.as_slice() else {
            panic!("/gamemode <mode> <player> builds one SetGameModeFor");
        };
        assert_eq!(target, "Joe");
        assert_eq!(*mode, GameMode::Creative);
        assert!(region_commands("gamemode creative", player).is_empty());
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
        // `mode` is a `Word` argument, which lowers to brigadier:string (parser 5).
        assert!(matches!(
            mode.extra(),
            BrigadierExtra::Argument { parser_id: 5, .. }
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

    /// The issuer name passed to `scoreboard_packets` in tests.
    const ISSUER: &str = "Saad";

    #[test]
    fn scoreboard_team_bossbar_require_operator_level() {
        let tree = build_command_tree();
        let member = CommandSource::for_player(PlayerId::offline("Joe"), "Joe", 0);
        for cmd in [
            "scoreboard objective add kills Kills",
            "team add red Red",
            "bossbar add boss Boss",
        ] {
            let err = tree.dispatch(cmd, &member).expect_err("member lacks level");
            assert!(matches!(err, CommandError::PermissionDenied(_)), "{cmd}");
        }
    }

    #[test]
    fn scoreboard_objective_lifecycle_dispatches_and_builds_packets() {
        let tree = build_command_tree();
        for cmd in [
            "scoreboard objective add kills Kills",
            "scoreboard objective setdisplay sidebar kills",
            "scoreboard score set Saad kills 7",
            "scoreboard objective remove kills",
        ] {
            assert!(
                tree.dispatch(cmd, &op()).expect("dispatches").is_success(),
                "{cmd}"
            );
        }

        assert!(matches!(
            scoreboard_packets("scoreboard objective add kills Kills", ISSUER)
                .expect("build")
                .as_slice(),
            [ClientboundPlayPacket::UpdateObjectives(_)]
        ));
        assert!(matches!(
            scoreboard_packets("scoreboard objective setdisplay sidebar kills", ISSUER)
                .expect("build")
                .as_slice(),
            [ClientboundPlayPacket::DisplayObjective(_)]
        ));
        assert!(matches!(
            scoreboard_packets("scoreboard score set Saad kills 7", ISSUER)
                .expect("build")
                .as_slice(),
            [ClientboundPlayPacket::UpdateScore(_)]
        ));
        assert!(matches!(
            scoreboard_packets("scoreboard objective remove kills", ISSUER)
                .expect("build")
                .as_slice(),
            [ClientboundPlayPacket::UpdateObjectives(_)]
        ));
    }

    #[test]
    fn scoreboard_rejects_malformed_forms() {
        let tree = build_command_tree();
        for cmd in [
            "scoreboard objective add",                   // missing name
            "scoreboard objective setdisplay nope kills", // bad slot
            "scoreboard score set Saad kills",            // missing value
            "scoreboard score set Saad kills x",          // non-integer value
            "scoreboard score set Saad kills 7 extra",    // trailing token
            "scoreboard nonsense",
        ] {
            assert!(
                !tree
                    .dispatch(cmd, &op())
                    .expect("reaches handler")
                    .is_success(),
                "{cmd}"
            );
            assert!(
                scoreboard_packets(cmd, ISSUER)
                    .expect("no error")
                    .is_empty(),
                "{cmd}"
            );
        }
    }

    #[test]
    fn team_commands_dispatch_and_build_set_player_team() {
        let tree = build_command_tree();
        for cmd in [
            "team add red Red Team",
            "team join red Saad",
            "team color red yellow",
            "team remove red",
        ] {
            assert!(
                tree.dispatch(cmd, &op()).expect("dispatches").is_success(),
                "{cmd}"
            );
            assert!(
                matches!(
                    scoreboard_packets(cmd, ISSUER).expect("build").as_slice(),
                    [ClientboundPlayPacket::SetPlayerTeam(_)]
                ),
                "{cmd}"
            );
        }
    }

    #[test]
    fn team_join_defaults_to_issuer() {
        // No explicit player → the entity list carries the issuer's name.
        let TeamAction::Join { team, player } = parse_team("join red", "Saad").expect("parses")
        else {
            panic!("expected a join action");
        };
        assert_eq!(team, "red");
        assert_eq!(player, "Saad");
        // Explicit player overrides the issuer.
        let TeamAction::Join { player, .. } = parse_team("join red Op", "Saad").expect("parses")
        else {
            panic!("expected a join action");
        };
        assert_eq!(player, "Op");
    }

    #[test]
    fn team_rejects_unknown_color() {
        let tree = build_command_tree();
        assert!(!tree
            .dispatch("team color red mauve", &op())
            .expect("reaches handler")
            .is_success());
        assert!(scoreboard_packets("team color red mauve", ISSUER)
            .expect("no error")
            .is_empty());
    }

    #[test]
    fn bossbar_commands_dispatch_and_build_boss_bar() {
        let tree = build_command_tree();
        for cmd in [
            "bossbar add boss The Boss",
            "bossbar title boss A New Title",
            "bossbar progress boss 0.5",
            "bossbar color boss purple",
            "bossbar remove boss",
        ] {
            assert!(
                tree.dispatch(cmd, &op()).expect("dispatches").is_success(),
                "{cmd}"
            );
            assert!(
                matches!(
                    scoreboard_packets(cmd, ISSUER).expect("build").as_slice(),
                    [ClientboundPlayPacket::BossBar(_)]
                ),
                "{cmd}"
            );
        }
    }

    #[test]
    fn bossbar_rejects_bad_progress_and_color() {
        let tree = build_command_tree();
        for cmd in [
            "bossbar progress boss 2.0", // out of 0.0..=1.0
            "bossbar progress boss low", // non-numeric
            "bossbar color boss chartreuse",
            "bossbar title boss", // missing title text
        ] {
            assert!(
                !tree
                    .dispatch(cmd, &op())
                    .expect("reaches handler")
                    .is_success(),
                "{cmd}"
            );
            assert!(
                scoreboard_packets(cmd, ISSUER)
                    .expect("no error")
                    .is_empty(),
                "{cmd}"
            );
        }
    }

    #[test]
    fn bossbar_uuid_is_stable_per_id() {
        // The same id always maps to the same bar so updates target the add.
        assert_eq!(bossbar_uuid("boss"), bossbar_uuid("boss"));
        // Different ids map to different bars.
        assert_ne!(bossbar_uuid("boss"), bossbar_uuid("other"));
    }

    #[test]
    fn scoreboard_packets_ignores_other_commands() {
        assert!(scoreboard_packets("spawn", ISSUER)
            .expect("no error")
            .is_empty());
        assert!(scoreboard_packets("title hi", ISSUER)
            .expect("no error")
            .is_empty());
        assert!(scoreboard_packets("", ISSUER).expect("no error").is_empty());
    }

    /// The operator-level player used as the issuer in region-command tests.
    fn region_player() -> PlayerId {
        PlayerId::offline("Builder")
    }

    #[test]
    fn region_commands_require_operator_level() {
        let tree = build_command_tree();
        let member = CommandSource::for_player(PlayerId::offline("Joe"), "Joe", 0);
        for cmd in [
            "fill 0 64 0 2 65 2 stone",
            "replace 0 64 0 2 65 2 stone dirt",
            "undo",
        ] {
            let err = tree.dispatch(cmd, &member).expect_err("member lacks level");
            assert!(matches!(err, CommandError::PermissionDenied(_)), "{cmd}");
        }
    }

    #[test]
    fn fill_dispatches_and_builds_a_region_fill() {
        let tree = build_command_tree();
        let cmd = "fill 0 64 0 2 65 2 stone";
        assert!(tree.dispatch(cmd, &op()).expect("dispatches").is_success());

        let player = region_player();
        let cmds = region_commands(cmd, player);
        let [SimCommand::RegionEdit {
            player: p,
            region,
            op,
            ..
        }] = cmds.as_slice()
        else {
            panic!("/fill builds one RegionEdit");
        };
        assert_eq!(*p, player);
        assert_eq!(
            *region,
            Cuboid::new(BlockPos::new(0, 64, 0), BlockPos::new(2, 65, 2))
        );
        assert!(matches!(op, RegionOp::Fill { state } if *state == BlockStateId::new(1)));
    }

    #[test]
    fn fill_accepts_a_namespaced_block_name() {
        let cmds = region_commands("fill 0 0 0 0 0 0 minecraft:stone", region_player());
        let [SimCommand::RegionEdit { op, .. }] = cmds.as_slice() else {
            panic!("expected a RegionEdit");
        };
        assert!(matches!(op, RegionOp::Fill { state } if *state == BlockStateId::new(1)));
    }

    #[test]
    fn replace_dispatches_and_builds_a_region_replace() {
        let tree = build_command_tree();
        let cmd = "replace 0 64 0 2 65 2 stone dirt";
        assert!(tree.dispatch(cmd, &op()).expect("dispatches").is_success());

        let cmds = region_commands(cmd, region_player());
        let [SimCommand::RegionEdit { region, op, .. }] = cmds.as_slice() else {
            panic!("/replace builds one RegionEdit");
        };
        assert_eq!(
            *region,
            Cuboid::new(BlockPos::new(0, 64, 0), BlockPos::new(2, 65, 2))
        );
        assert!(matches!(
            op,
            RegionOp::Replace { from, to }
                if *from == BlockStateId::new(1) && *to == BlockStateId::new(10)
        ));
    }

    #[test]
    fn undo_dispatches_and_builds_a_region_undo() {
        let tree = build_command_tree();
        assert!(tree
            .dispatch("undo", &op())
            .expect("dispatches")
            .is_success());
        let player = region_player();
        assert!(matches!(
            region_commands("undo", player).as_slice(),
            [SimCommand::RegionUndo { player: p, .. }] if *p == player
        ));
        // A trailing token makes /undo a parse miss for the side effect.
        assert!(region_commands("undo now", player).is_empty());
    }

    #[test]
    fn fill_over_the_volume_cap_is_rejected_with_a_clear_error() {
        // Cap of 8 blocks; a 5x1x5 = 25-block region is rejected.
        let tree = build_command_tree_with_limits(8);
        let result = tree
            .dispatch("fill 0 64 0 4 64 4 stone", &op())
            .expect("reaches the handler");
        assert!(!result.is_success());
        assert!(result
            .feedback()
            .to_plain_string()
            .contains("region too large"));
    }

    #[test]
    fn fill_rejects_malformed_arguments() {
        let tree = build_command_tree();
        for cmd in [
            "fill 0 64 0 2 65 stone",         // missing a coordinate
            "fill 0 64 0 2 65 2 notablock",   // unknown block
            "fill 0 64 0 2 65 2 stone extra", // trailing token
            "fill a b c d e f stone",         // non-integer coordinates
        ] {
            assert!(
                !tree
                    .dispatch(cmd, &op())
                    .expect("reaches handler")
                    .is_success(),
                "{cmd}"
            );
            assert!(region_commands(cmd, region_player()).is_empty(), "{cmd}");
        }
    }

    #[test]
    fn region_commands_ignore_other_commands() {
        let player = region_player();
        assert!(region_commands("spawn", player).is_empty());
        assert!(region_commands("gamemode 1", player).is_empty());
        assert!(region_commands("", player).is_empty());
    }

    // -----------------------------------------------------------------------
    // /tp, /weather
    // -----------------------------------------------------------------------

    #[test]
    fn tp_and_weather_require_operator_level() {
        let tree = build_command_tree();
        let member = CommandSource::for_player(PlayerId::offline("Joe"), "Joe", 0);
        for cmd in ["tp 1 2 3", "tp Joe", "weather rain"] {
            let err = tree.dispatch(cmd, &member).expect_err("member lacks level");
            assert!(matches!(err, CommandError::PermissionDenied(_)), "{cmd}");
        }
    }

    #[test]
    fn tp_to_coords_dispatches_and_teleports_the_issuer() {
        let tree = build_command_tree();
        assert!(tree
            .dispatch("tp 1 64 -3", &op())
            .expect("dispatches")
            .is_success());

        let player = PlayerId::offline("Op");
        let cmds = region_commands("tp 1 64 -3", player);
        let [SimCommand::TeleportPlayer {
            player: p,
            position,
            ..
        }] = cmds.as_slice()
        else {
            panic!("/tp <x y z> builds one TeleportPlayer");
        };
        assert_eq!(*p, player);
        assert_eq!(*position, Vec3::new(1.0, 64.0, -3.0));
    }

    #[test]
    fn tp_to_player_dispatches_and_builds_a_teleport_to_player() {
        let tree = build_command_tree();
        assert!(tree
            .dispatch("tp Joe", &op())
            .expect("dispatches")
            .is_success());

        let player = PlayerId::offline("Op");
        let cmds = region_commands("tp Joe", player);
        let [SimCommand::TeleportToPlayer {
            player: p, target, ..
        }] = cmds.as_slice()
        else {
            panic!("/tp <player> builds one TeleportToPlayer");
        };
        assert_eq!(*p, player);
        assert_eq!(target, "Joe");
    }

    #[test]
    fn tp_rejects_malformed_arguments() {
        let tree = build_command_tree();
        for cmd in [
            "tp 1 2",     // partial coordinate triple
            "tp 1 2 x",   // non-numeric coordinate
            "tp 1 2 3 4", // too many tokens
            "tp NaN 64 0",
            "tp inf 64 0",
            "tp -inf 64 0",
            "tp 30000001 64 0",
            "tp -30000001 64 0",
        ] {
            assert!(
                !tree
                    .dispatch(cmd, &op())
                    .expect("reaches the handler")
                    .is_success(),
                "{cmd}"
            );
            assert!(region_commands(cmd, region_player()).is_empty(), "{cmd}");
        }
    }

    #[test]
    fn weather_dispatches_and_builds_a_set_weather() {
        let tree = build_command_tree();
        for (cmd, raining) in [
            ("weather rain", true),
            ("weather clear", false),
            ("weather rain 600", true),
        ] {
            assert!(
                tree.dispatch(cmd, &op()).expect("dispatches").is_success(),
                "{cmd}"
            );
            let cmds = region_commands(cmd, region_player());
            let [SimCommand::SetWeather { raining: r }] = cmds.as_slice() else {
                panic!("{cmd} builds one SetWeather");
            };
            assert_eq!(*r, raining, "{cmd}");
        }
    }

    #[test]
    fn weather_rejects_malformed_arguments() {
        let tree = build_command_tree();
        // A bare `/weather` is missing its required greedy argument, so it fails at
        // dispatch rather than reaching the handler.
        assert!(matches!(
            tree.dispatch("weather", &op()),
            Err(CommandError::MissingArgument(_))
        ));
        // These reach the handler with an unparsable tail and report a usage failure.
        for cmd in [
            "weather snow",      // unknown kind
            "weather rain fast", // non-integer duration
            "weather rain 1 2",  // trailing token
        ] {
            assert!(
                !tree
                    .dispatch(cmd, &op())
                    .expect("reaches the handler")
                    .is_success(),
                "{cmd}"
            );
            assert!(region_commands(cmd, region_player()).is_empty(), "{cmd}");
        }
    }

    #[test]
    fn time_set_dispatches_and_builds_set_time() {
        let tree = build_command_tree();
        // Named phases resolve to their canonical ticks; a raw count passes through.
        for (cmd, ticks) in [
            ("time set day", 1_000),
            ("time set noon", 6_000),
            ("time set night", 13_000),
            ("time set midnight", 18_000),
            ("time set 1234", 1_234),
        ] {
            let result = tree.dispatch(cmd, &op()).expect("dispatches");
            assert!(result.is_success(), "{cmd}");
            assert_eq!(
                result.feedback().to_plain_string(),
                format!("Set the time to {ticks}"),
                "{cmd}"
            );
            let cmds = region_commands(cmd, region_player());
            let [SimCommand::SetTime { time_of_day }] = cmds.as_slice() else {
                panic!("{cmd} builds one SetTime");
            };
            assert_eq!(*time_of_day, ticks, "{cmd}");
        }
    }

    #[test]
    fn time_add_builds_add_time_including_negative() {
        let tree = build_command_tree();
        for (cmd, ticks) in [("time add 100", 100), ("time add -50", -50)] {
            assert!(
                tree.dispatch(cmd, &op()).expect("dispatches").is_success(),
                "{cmd}"
            );
            let cmds = region_commands(cmd, region_player());
            let [SimCommand::AddTime { ticks: t }] = cmds.as_slice() else {
                panic!("{cmd} builds one AddTime");
            };
            assert_eq!(*t, ticks, "{cmd}");
        }
    }

    #[test]
    fn time_query_builds_query_time_with_empty_feedback() {
        let tree = build_command_tree();
        // The query dispatches successfully but carries no command-layer feedback:
        // the driver replies with the live value, so the connection suppresses the
        // empty line.
        let result = tree
            .dispatch("time query daytime", &op())
            .expect("dispatches");
        assert!(result.is_success());
        assert!(result.feedback().to_plain_string().is_empty());

        let player = region_player();
        let cmds = region_commands("time query daytime", player);
        let [SimCommand::QueryTime { player: p }] = cmds.as_slice() else {
            panic!("/time query daytime builds one QueryTime");
        };
        assert_eq!(*p, player);
    }

    #[test]
    fn time_requires_operator_level() {
        let tree = build_command_tree();
        let member = CommandSource::for_player(PlayerId::offline("Joe"), "Joe", 0);
        for cmd in ["time set day", "time add 1", "time query daytime"] {
            let err = tree.dispatch(cmd, &member).expect_err("member lacks level");
            assert!(matches!(err, CommandError::PermissionDenied(_)), "{cmd}");
        }
    }

    #[test]
    fn time_rejects_malformed_arguments() {
        let tree = build_command_tree();
        // A bare `/time` is missing its required greedy argument.
        assert!(matches!(
            tree.dispatch("time", &op()),
            Err(CommandError::MissingArgument(_))
        ));
        // These reach the handler with an unparsable tail and report a usage failure.
        for cmd in [
            "time set",         // missing phase/ticks
            "time set dusk",    // unknown phase that is not an integer
            "time set day 1",   // trailing token
            "time add",         // missing ticks
            "time add x",       // non-integer ticks
            "time query",       // missing subject
            "time query night", // unknown query subject
            "time wibble",      // unknown subcommand
        ] {
            assert!(
                !tree
                    .dispatch(cmd, &op())
                    .expect("reaches the handler")
                    .is_success(),
                "{cmd}"
            );
            assert!(region_commands(cmd, region_player()).is_empty(), "{cmd}");
        }
    }
}
