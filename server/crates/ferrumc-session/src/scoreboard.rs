//! Builders for the clientbound *scoreboard* family: objectives, the display
//! slot, scores, teams, and boss bars.
//!
//! Like [`crate::presentation`], every function here returns a ready-to-enqueue
//! [`ClientboundPlayPacket`]; the connection writer owns the socket and this
//! module only shapes bytes. Several of these packets carry a body the generated
//! grammar leaves opaque (a typed *head* plus a hand-encoded *tail*), so this
//! module hand-encodes the tail for protocol 772, cross-checked field-for-field
//! against the 1.21.8 protocol ground truth:
//!
//! - [`UpdateObjectives`] tail (create/update): display-name NBT, render `type`
//!   (`VarInt`), and an absent optional number-format. See [`objective_create`].
//! - [`UpdateScore`] tail: an absent optional display-name and an absent optional
//!   number-format. See [`score_set`].
//! - [`SetPlayerTeam`] tail (create/update): display-name NBT, friendly flags,
//!   name-tag visibility (`VarInt`), collision rule (`VarInt`), formatting color
//!   (`VarInt`), prefix/suffix NBT, then for create an entity list. See
//!   [`team_create`].
//! - [`BossBar`] tail: per-action title NBT, health (`f32`), color/division
//!   (`VarInt`), and flags (`u8`). See [`boss_bar_add`].
//!
//! Text fields are encoded through the same [`crate::text::text_component_to_nbt`]
//! anonymous-root path `SystemChat` uses — the single source of truth for the
//! `TextComponent` -> network-NBT mapping — so a display name renders identically
//! whether it arrives in chat or on the scoreboard.
//!
//! Because constructing the bounded name strings and encoding the NBT tails can
//! fail (a name past the protocol cap, an NBT body past its depth/size limits),
//! the builders return a [`Result`] classified as a [`SessionError`]. For the
//! short, server-authored inputs these builders see, the failure paths are
//! unreachable in practice, but the crate does not `unwrap`.

use ferrumc_codec::{write_var_int, BoundedString};
use ferrumc_core::{TextColor, TextComponent};
use ferrumc_nbt::{write_network_root, NbtLimits};
use ferrumc_proto::generated::play::{
    BossBar, ClientboundPlayPacket, DisplayObjective, SetPlayerTeam, UpdateObjectives, UpdateScore,
};
use uuid::Uuid;

use crate::error::SessionError;
use crate::text::text_component_to_nbt;

// ---------------------------------------------------------------------------
// Wire constants — named so no magic byte ever reaches the encoders.
// ---------------------------------------------------------------------------

/// `UpdateObjectives` mode: create a new objective (carries a display tail).
pub const OBJECTIVE_MODE_CREATE: i8 = 0;
/// `UpdateObjectives` mode: remove an objective (empty tail).
pub const OBJECTIVE_MODE_REMOVE: i8 = 1;
/// `UpdateObjectives` mode: update an existing objective's display tail.
pub const OBJECTIVE_MODE_UPDATE: i8 = 2;

/// `DisplayObjective` slot: the tab-list column.
pub const DISPLAY_SLOT_LIST: i32 = 0;
/// `DisplayObjective` slot: the right-hand sidebar (the minigame default).
pub const DISPLAY_SLOT_SIDEBAR: i32 = 1;
/// `DisplayObjective` slot: below a player's name tag.
pub const DISPLAY_SLOT_BELOW_NAME: i32 = 2;

/// `SetPlayerTeam` method: create a team (metadata tail + entity list).
pub const TEAM_METHOD_CREATE: i8 = 0;
/// `SetPlayerTeam` method: remove a team (empty tail).
pub const TEAM_METHOD_REMOVE: i8 = 1;
/// `SetPlayerTeam` method: update a team's metadata (no entity list).
pub const TEAM_METHOD_UPDATE: i8 = 2;
/// `SetPlayerTeam` method: add entities to a team (entity list only).
pub const TEAM_METHOD_ADD_ENTITIES: i8 = 3;
/// `SetPlayerTeam` method: remove entities from a team (entity list only).
pub const TEAM_METHOD_REMOVE_ENTITIES: i8 = 4;

/// Team friendly flag: members can damage one another.
pub const TEAM_FLAG_FRIENDLY_FIRE: u8 = 0x01;
/// Team friendly flag: members can see one another while invisible.
pub const TEAM_FLAG_SEE_FRIENDLY_INVISIBLE: u8 = 0x02;

/// Team name-tag visibility (`VarInt` mapper): always shown. The non-default
/// values are `never` (1), `hide_for_other_teams` (2), `hide_for_own_team` (3).
const TEAM_VISIBILITY_ALWAYS: i32 = 0;
/// Team collision rule (`VarInt` mapper): always collide. The non-default values
/// are `never` (1), `push_other_teams` (2), `push_own_team` (3).
const TEAM_COLLISION_ALWAYS: i32 = 0;
/// Team formatting `VarInt` for "no color": the `reset` chat-formatting ordinal.
const TEAM_COLOR_RESET: i32 = 21;

/// `BossBar` action: add a new bar (full tail: title, health, color, division,
/// flags).
pub const BOSS_BAR_ACTION_ADD: i32 = 0;
/// `BossBar` action: remove a bar (empty tail).
pub const BOSS_BAR_ACTION_REMOVE: i32 = 1;
/// `BossBar` action: update health only (`f32` tail).
pub const BOSS_BAR_ACTION_UPDATE_HEALTH: i32 = 2;
/// `BossBar` action: update title only (NBT tail).
pub const BOSS_BAR_ACTION_UPDATE_TITLE: i32 = 3;
/// `BossBar` action: update color + division (two `VarInt`s).
pub const BOSS_BAR_ACTION_UPDATE_STYLE: i32 = 4;
/// `BossBar` action: update flags only (`u8` tail).
pub const BOSS_BAR_ACTION_UPDATE_FLAGS: i32 = 5;

/// Boss-bar flag: darken the player's sky while the bar is shown.
pub const BOSS_BAR_FLAG_DARKEN_SKY: u8 = 0x01;
/// Boss-bar flag: play the (ender dragon) boss music while the bar is shown.
pub const BOSS_BAR_FLAG_PLAY_MUSIC: u8 = 0x02;
/// Boss-bar flag: thicken world fog while the bar is shown.
pub const BOSS_BAR_FLAG_CREATE_FOG: u8 = 0x04;

/// The single byte an *absent* optional (`option<T>`) field encodes to: a `false`
/// presence boolean with no following payload.
const OPTIONAL_ABSENT: u8 = 0x00;

// ---------------------------------------------------------------------------
// Objectives
// ---------------------------------------------------------------------------

/// How a numeric score renders next to its entry on the scoreboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectiveRender {
    /// Render the raw integer value.
    Integer,
    /// Render the value as a row of hearts (the vanilla health style).
    Hearts,
}

impl ObjectiveRender {
    /// Returns the `VarInt` wire id for this render type.
    pub const fn id(self) -> i32 {
        match self {
            Self::Integer => 0,
            Self::Hearts => 1,
        }
    }
}

/// Builds an `UpdateObjectives` packet that *creates* an objective named `name`
/// with the given `display` name and render `type`.
///
/// The hand-encoded tail is the display-name NBT, the render-type `VarInt`, and a
/// single `false` byte for the absent optional number-format (so no styling
/// follows) — the protocol-772 create body.
///
/// # Errors
///
/// Returns [`SessionError`] if `name` exceeds the protocol string cap or the
/// display component cannot be encoded as NBT.
pub fn objective_create(
    name: &str,
    display: &TextComponent,
    render: ObjectiveRender,
) -> Result<ClientboundPlayPacket, SessionError> {
    let mut tail = encode_text_nbt(display)?;
    write_var_int(&mut tail, render.id());
    tail.push(OPTIONAL_ABSENT);
    Ok(ClientboundPlayPacket::UpdateObjectives(
        UpdateObjectives::new(bounded(name)?, OBJECTIVE_MODE_CREATE, tail),
    ))
}

/// Builds an `UpdateObjectives` packet that *removes* the objective named `name`.
/// The remove body has no tail.
///
/// # Errors
///
/// Returns [`SessionError`] if `name` exceeds the protocol string cap.
pub fn objective_remove(name: &str) -> Result<ClientboundPlayPacket, SessionError> {
    Ok(ClientboundPlayPacket::UpdateObjectives(
        UpdateObjectives::new(bounded(name)?, OBJECTIVE_MODE_REMOVE, Vec::new()),
    ))
}

/// Builds a `DisplayObjective` packet binding the objective named `name` to a
/// display `slot` (use a `DISPLAY_SLOT_*` constant).
///
/// This packet is fully typed (position `VarInt` + name string), so it carries no
/// hand-encoded tail.
///
/// # Errors
///
/// Returns [`SessionError`] if `name` exceeds the protocol string cap.
pub fn display_objective(slot: i32, name: &str) -> Result<ClientboundPlayPacket, SessionError> {
    Ok(ClientboundPlayPacket::DisplayObjective(
        DisplayObjective::new(slot, bounded(name)?),
    ))
}

/// Builds an `UpdateScore` packet setting `entity`'s score under `objective` to
/// `value`.
///
/// The tail is two `false` bytes: an absent optional display-name override and an
/// absent optional number-format (so no styling follows) — the protocol-772 body
/// for a plain numeric score.
///
/// # Errors
///
/// Returns [`SessionError`] if `entity` or `objective` exceeds the protocol
/// string cap.
pub fn score_set(
    entity: &str,
    objective: &str,
    value: i32,
) -> Result<ClientboundPlayPacket, SessionError> {
    let tail = vec![OPTIONAL_ABSENT, OPTIONAL_ABSENT];
    Ok(ClientboundPlayPacket::UpdateScore(UpdateScore::new(
        bounded(entity)?,
        bounded(objective)?,
        value,
        tail,
    )))
}

// ---------------------------------------------------------------------------
// Teams
// ---------------------------------------------------------------------------

/// Builds a `SetPlayerTeam` packet that *creates* the team `name` with the given
/// `display` name and optional `color`, with no initial members.
///
/// The metadata tail is: display NBT, friendly flags (none), name-tag visibility
/// (`always`), collision rule (`always`), the formatting color `VarInt`, empty
/// prefix/suffix NBT, then the entity list (count `0`). Pass [`None`] for `color`
/// to use the `reset` (no color) formatting.
///
/// # Errors
///
/// Returns [`SessionError`] if `name` exceeds the protocol string cap or a text
/// component cannot be encoded as NBT.
pub fn team_create(
    name: &str,
    display: &TextComponent,
    color: Option<TextColor>,
) -> Result<ClientboundPlayPacket, SessionError> {
    let mut tail = team_metadata_tail(display, color)?;
    // The create body ends with the (empty) member list: a VarInt count of 0.
    write_var_int(&mut tail, 0);
    Ok(ClientboundPlayPacket::SetPlayerTeam(SetPlayerTeam::new(
        bounded(name)?,
        TEAM_METHOD_CREATE,
        tail,
    )))
}

/// Builds a `SetPlayerTeam` packet that *updates* the metadata of team `name`
/// (display name + color), leaving membership untouched (the update body carries
/// no entity list).
///
/// # Errors
///
/// Returns [`SessionError`] if `name` exceeds the protocol string cap or a text
/// component cannot be encoded as NBT.
pub fn team_update(
    name: &str,
    display: &TextComponent,
    color: Option<TextColor>,
) -> Result<ClientboundPlayPacket, SessionError> {
    Ok(ClientboundPlayPacket::SetPlayerTeam(SetPlayerTeam::new(
        bounded(name)?,
        TEAM_METHOD_UPDATE,
        team_metadata_tail(display, color)?,
    )))
}

/// Builds a `SetPlayerTeam` packet that *removes* the team `name`. The remove
/// body has no tail.
///
/// # Errors
///
/// Returns [`SessionError`] if `name` exceeds the protocol string cap.
pub fn team_remove(name: &str) -> Result<ClientboundPlayPacket, SessionError> {
    Ok(ClientboundPlayPacket::SetPlayerTeam(SetPlayerTeam::new(
        bounded(name)?,
        TEAM_METHOD_REMOVE,
        Vec::new(),
    )))
}

/// Builds a `SetPlayerTeam` packet that *adds* `entities` to the team `name`. The
/// tail is the entity list (a `VarInt` count followed by each name string).
///
/// # Errors
///
/// Returns [`SessionError`] if `name` or any entity exceeds the protocol string
/// cap.
pub fn team_add_entities(
    name: &str,
    entities: &[&str],
) -> Result<ClientboundPlayPacket, SessionError> {
    Ok(ClientboundPlayPacket::SetPlayerTeam(SetPlayerTeam::new(
        bounded(name)?,
        TEAM_METHOD_ADD_ENTITIES,
        entity_list_tail(entities)?,
    )))
}

/// Builds a `SetPlayerTeam` packet that *removes* `entities` from the team
/// `name`. The tail is the entity list (a `VarInt` count followed by each name
/// string).
///
/// # Errors
///
/// Returns [`SessionError`] if `name` or any entity exceeds the protocol string
/// cap.
pub fn team_remove_entities(
    name: &str,
    entities: &[&str],
) -> Result<ClientboundPlayPacket, SessionError> {
    Ok(ClientboundPlayPacket::SetPlayerTeam(SetPlayerTeam::new(
        bounded(name)?,
        TEAM_METHOD_REMOVE_ENTITIES,
        entity_list_tail(entities)?,
    )))
}

/// Hand-encodes the shared create/update team-metadata block (everything up to
/// but excluding the optional member list).
fn team_metadata_tail(
    display: &TextComponent,
    color: Option<TextColor>,
) -> Result<Vec<u8>, SessionError> {
    let mut tail = encode_text_nbt(display)?;
    // Friendly flags: neither friendly-fire nor see-invisible by default.
    tail.push(0);
    write_var_int(&mut tail, TEAM_VISIBILITY_ALWAYS);
    write_var_int(&mut tail, TEAM_COLLISION_ALWAYS);
    write_var_int(&mut tail, team_color_id(color));
    // Prefix and suffix are required NBT fields; empty components are valid.
    let empty = TextComponent::text(String::new());
    tail.extend_from_slice(&encode_text_nbt(&empty)?);
    tail.extend_from_slice(&encode_text_nbt(&empty)?);
    Ok(tail)
}

/// Hand-encodes a team entity list: a `VarInt` count followed by each name as a
/// length-prefixed string.
fn entity_list_tail(entities: &[&str]) -> Result<Vec<u8>, SessionError> {
    let count = i32::try_from(entities.len()).map_err(|_| {
        // A list this large cannot occur from a command, but stay panic-free.
        SessionError::from(ferrumc_codec::CodecError::StringTooLong {
            length: entities.len(),
            max: i32::MAX as usize,
        })
    })?;
    let mut tail = Vec::new();
    write_var_int(&mut tail, count);
    for entity in entities {
        bounded(entity)?.write(&mut tail);
    }
    Ok(tail)
}

/// Maps an optional team color to its formatting `VarInt`: a named color uses its
/// chat-formatting ordinal (which [`TextColor`] already orders `0..=15`), and
/// [`None`] uses the `reset` ordinal for "no color".
fn team_color_id(color: Option<TextColor>) -> i32 {
    color.map_or(TEAM_COLOR_RESET, formatting_id)
}

/// Returns the chat-formatting ordinal for a named color. The 16 [`TextColor`]
/// variants are declared in the same order as Minecraft's chat-formatting color
/// ids, so the ordinal is the wire value.
const fn formatting_id(color: TextColor) -> i32 {
    match color {
        TextColor::Black => 0,
        TextColor::DarkBlue => 1,
        TextColor::DarkGreen => 2,
        TextColor::DarkAqua => 3,
        TextColor::DarkRed => 4,
        TextColor::DarkPurple => 5,
        TextColor::Gold => 6,
        TextColor::Gray => 7,
        TextColor::DarkGray => 8,
        TextColor::Blue => 9,
        TextColor::Green => 10,
        TextColor::Aqua => 11,
        TextColor::Red => 12,
        TextColor::LightPurple => 13,
        TextColor::Yellow => 14,
        TextColor::White => 15,
    }
}

// ---------------------------------------------------------------------------
// Boss bars
// ---------------------------------------------------------------------------

/// The base color of a boss bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BossBarColor {
    /// `pink`.
    Pink,
    /// `blue`.
    Blue,
    /// `red`.
    Red,
    /// `green`.
    Green,
    /// `yellow`.
    Yellow,
    /// `purple`.
    Purple,
    /// `white`.
    White,
}

impl BossBarColor {
    /// Returns the `VarInt` wire id for this color.
    pub const fn id(self) -> i32 {
        match self {
            Self::Pink => 0,
            Self::Blue => 1,
            Self::Red => 2,
            Self::Green => 3,
            Self::Yellow => 4,
            Self::Purple => 5,
            Self::White => 6,
        }
    }
}

/// The notch divisions drawn over a boss bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BossBarDivision {
    /// A solid bar with no notches.
    NoDivision,
    /// Six notches.
    SixNotches,
    /// Ten notches.
    TenNotches,
    /// Twelve notches.
    TwelveNotches,
    /// Twenty notches.
    TwentyNotches,
}

impl BossBarDivision {
    /// Returns the `VarInt` wire id for this division style.
    pub const fn id(self) -> i32 {
        match self {
            Self::NoDivision => 0,
            Self::SixNotches => 1,
            Self::TenNotches => 2,
            Self::TwelveNotches => 3,
            Self::TwentyNotches => 4,
        }
    }
}

/// Builds a `BossBar` packet that *adds* a bar keyed by `uuid`.
///
/// The hand-encoded tail is: the title NBT, `health` (a `0.0..=1.0` `f32`), the
/// color `VarInt`, the division `VarInt`, and the `flags` byte (a bitmask of the
/// `BOSS_BAR_FLAG_*` constants).
///
/// # Errors
///
/// Returns [`SessionError`] if the title cannot be encoded as NBT.
pub fn boss_bar_add(
    uuid: Uuid,
    title: &TextComponent,
    health: f32,
    color: BossBarColor,
    division: BossBarDivision,
    flags: u8,
) -> Result<ClientboundPlayPacket, SessionError> {
    let mut tail = encode_text_nbt(title)?;
    tail.extend_from_slice(&health.to_be_bytes());
    write_var_int(&mut tail, color.id());
    write_var_int(&mut tail, division.id());
    tail.push(flags);
    Ok(boss_bar(uuid, BOSS_BAR_ACTION_ADD, tail))
}

/// Builds a `BossBar` packet that *removes* the bar keyed by `uuid`. The remove
/// body has no tail.
pub fn boss_bar_remove(uuid: Uuid) -> ClientboundPlayPacket {
    boss_bar(uuid, BOSS_BAR_ACTION_REMOVE, Vec::new())
}

/// Builds a `BossBar` packet updating the `health` (`0.0..=1.0`) of the bar keyed
/// by `uuid`. The tail is a single big-endian `f32`.
pub fn boss_bar_update_health(uuid: Uuid, health: f32) -> ClientboundPlayPacket {
    boss_bar(
        uuid,
        BOSS_BAR_ACTION_UPDATE_HEALTH,
        health.to_be_bytes().to_vec(),
    )
}

/// Builds a `BossBar` packet updating the title of the bar keyed by `uuid`. The
/// tail is the title NBT.
///
/// # Errors
///
/// Returns [`SessionError`] if the title cannot be encoded as NBT.
pub fn boss_bar_update_title(
    uuid: Uuid,
    title: &TextComponent,
) -> Result<ClientboundPlayPacket, SessionError> {
    Ok(boss_bar(
        uuid,
        BOSS_BAR_ACTION_UPDATE_TITLE,
        encode_text_nbt(title)?,
    ))
}

/// Builds a `BossBar` packet updating the color and division of the bar keyed by
/// `uuid`. The tail is the color `VarInt` then the division `VarInt`.
pub fn boss_bar_update_style(
    uuid: Uuid,
    color: BossBarColor,
    division: BossBarDivision,
) -> ClientboundPlayPacket {
    let mut tail = Vec::new();
    write_var_int(&mut tail, color.id());
    write_var_int(&mut tail, division.id());
    boss_bar(uuid, BOSS_BAR_ACTION_UPDATE_STYLE, tail)
}

/// Builds a `BossBar` packet updating the `flags` (a bitmask of the
/// `BOSS_BAR_FLAG_*` constants) of the bar keyed by `uuid`. The tail is one byte.
pub fn boss_bar_update_flags(uuid: Uuid, flags: u8) -> ClientboundPlayPacket {
    boss_bar(uuid, BOSS_BAR_ACTION_UPDATE_FLAGS, vec![flags])
}

/// Assembles a [`BossBar`] packet from its typed head (`uuid`, `action`) and a
/// hand-encoded `tail`.
fn boss_bar(uuid: Uuid, action: i32, tail: Vec<u8>) -> ClientboundPlayPacket {
    ClientboundPlayPacket::BossBar(BossBar::new(uuid, action, tail))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Wraps a `&str` as the protocol's 32 767-char-bounded string, classifying an
/// over-long value as a [`SessionError`].
fn bounded(value: &str) -> Result<BoundedString<32_767>, SessionError> {
    Ok(BoundedString::<32_767>::new(value.to_owned())?)
}

/// Encodes a [`TextComponent`] to the network-form (anonymous-root) NBT bytes a
/// 1.21.8 client reads for a text field — the exact `anonymousNbt` the protocol
/// names for these tails.
fn encode_text_nbt(component: &TextComponent) -> Result<Vec<u8>, SessionError> {
    Ok(write_network_root(
        &text_component_to_nbt(component),
        &NbtLimits::default(),
    )?)
}

#[cfg(test)]
mod tests {
    use ferrumc_proto::generated::play::{
        BossBar, DisplayObjective, SetPlayerTeam, UpdateObjectives, UpdateScore,
    };
    use ferrumc_testkit::assert_wire_frame;

    use super::*;

    /// The exact anonymous-root NBT bytes a component encodes to — used to build
    /// the expected golden tail without re-deriving the (separately pinned) NBT
    /// encoding by hand.
    fn nbt(component: &TextComponent) -> Vec<u8> {
        write_network_root(&text_component_to_nbt(component), &NbtLimits::default())
            .expect("test component encodes to NBT")
    }

    fn unwrap_objective(packet: ClientboundPlayPacket) -> UpdateObjectives {
        let ClientboundPlayPacket::UpdateObjectives(p) = packet else {
            panic!("expected UpdateObjectives");
        };
        p
    }

    fn unwrap_team(packet: ClientboundPlayPacket) -> SetPlayerTeam {
        let ClientboundPlayPacket::SetPlayerTeam(p) = packet else {
            panic!("expected SetPlayerTeam");
        };
        p
    }

    fn unwrap_boss(packet: ClientboundPlayPacket) -> BossBar {
        let ClientboundPlayPacket::BossBar(p) = packet else {
            panic!("expected BossBar");
        };
        p
    }

    #[test]
    fn objective_create_tail_is_byte_exact() {
        let display = TextComponent::text("Kills");
        let packet = objective_create("kills", &display, ObjectiveRender::Integer)
            .expect("create objective");
        let p = unwrap_objective(packet);
        assert_eq!(p.name().as_str(), "kills");
        assert_eq!(p.mode(), OBJECTIVE_MODE_CREATE);

        // tail = display NBT, then render type VarInt (0 = integer), then a single
        // `false` byte for the absent optional number-format.
        let mut expected = nbt(&display);
        expected.push(0x00); // ObjectiveRender::Integer
        expected.push(OPTIONAL_ABSENT); // number_format option absent
        assert_eq!(p.rest(), expected.as_slice());
        assert_eq!(
            p.rest()[0],
            0x0a,
            "display text is an anonymous-root compound"
        );

        assert_wire_frame(
            &p,
            UpdateObjectives::encode,
            UpdateObjectives::decode,
            UpdateObjectives::PACKET_ID,
            None,
        )
        .expect("UpdateObjectives forms a valid frame");
    }

    #[test]
    fn objective_create_hearts_render_id() {
        let packet = objective_create("hp", &TextComponent::text("HP"), ObjectiveRender::Hearts)
            .expect("create");
        let p = unwrap_objective(packet);
        // The render-type VarInt sits immediately after the NBT body.
        let tail = p.rest();
        assert_eq!(tail[tail.len() - 2], 0x01, "Hearts render id is 1");
        assert_eq!(tail[tail.len() - 1], OPTIONAL_ABSENT);
    }

    #[test]
    fn objective_remove_has_empty_tail() {
        let p = unwrap_objective(objective_remove("kills").expect("remove"));
        assert_eq!(p.mode(), OBJECTIVE_MODE_REMOVE);
        assert!(p.rest().is_empty(), "remove carries no tail");
        assert_wire_frame(
            &p,
            UpdateObjectives::encode,
            UpdateObjectives::decode,
            UpdateObjectives::PACKET_ID,
            None,
        )
        .expect("valid frame");
    }

    #[test]
    fn display_objective_binds_slot_and_name() {
        let ClientboundPlayPacket::DisplayObjective(p) =
            display_objective(DISPLAY_SLOT_SIDEBAR, "kills").expect("display")
        else {
            panic!("expected DisplayObjective");
        };
        assert_eq!(p.position(), DISPLAY_SLOT_SIDEBAR);
        assert_eq!(p.name().as_str(), "kills");
        assert_wire_frame(
            &p,
            DisplayObjective::encode,
            DisplayObjective::decode,
            DisplayObjective::PACKET_ID,
            None,
        )
        .expect("valid frame");
    }

    #[test]
    fn score_set_tail_is_two_absent_options() {
        let ClientboundPlayPacket::UpdateScore(p) = score_set("Saad", "kills", 5).expect("score")
        else {
            panic!("expected UpdateScore");
        };
        assert_eq!(p.entity_name().as_str(), "Saad");
        assert_eq!(p.objective_name().as_str(), "kills");
        assert_eq!(p.value(), 5);
        // tail = absent display-name option, absent number-format option.
        assert_eq!(p.rest(), &[OPTIONAL_ABSENT, OPTIONAL_ABSENT]);
        assert_wire_frame(
            &p,
            UpdateScore::encode,
            UpdateScore::decode,
            UpdateScore::PACKET_ID,
            None,
        )
        .expect("valid frame");
    }

    #[test]
    fn score_set_encodes_negative_value() {
        let ClientboundPlayPacket::UpdateScore(p) = score_set("Saad", "kills", -3).expect("score")
        else {
            panic!("expected UpdateScore");
        };
        assert_eq!(p.value(), -3);
        assert_wire_frame(
            &p,
            UpdateScore::encode,
            UpdateScore::decode,
            UpdateScore::PACKET_ID,
            None,
        )
        .expect("a negative score still forms a valid frame");
    }

    #[test]
    fn team_create_tail_is_byte_exact() {
        let display = TextComponent::text("Red Team");
        let p = unwrap_team(team_create("red", &display, Some(TextColor::Red)).expect("create"));
        assert_eq!(p.name().as_str(), "red");
        assert_eq!(p.method(), TEAM_METHOD_CREATE);

        let empty = nbt(&TextComponent::text(String::new()));
        let mut expected = nbt(&display);
        expected.push(0x00); // friendly flags: none
        expected.push(0x00); // name-tag visibility VarInt: always (0)
        expected.push(0x00); // collision rule VarInt: always (0)
        expected.push(0x0c); // formatting VarInt: red (12)
        expected.extend_from_slice(&empty); // prefix
        expected.extend_from_slice(&empty); // suffix
        expected.push(0x00); // member list count VarInt: 0
        assert_eq!(p.rest(), expected.as_slice());

        assert_wire_frame(
            &p,
            SetPlayerTeam::encode,
            SetPlayerTeam::decode,
            SetPlayerTeam::PACKET_ID,
            None,
        )
        .expect("valid frame");
    }

    #[test]
    fn team_update_has_no_member_list() {
        let display = TextComponent::text("Red Team");
        let p = unwrap_team(team_update("red", &display, Some(TextColor::Yellow)).expect("update"));
        assert_eq!(p.method(), TEAM_METHOD_UPDATE);

        let empty = nbt(&TextComponent::text(String::new()));
        let mut expected = nbt(&display);
        expected.push(0x00); // flags
        expected.push(0x00); // visibility: always
        expected.push(0x00); // collision: always
        expected.push(0x0e); // formatting: yellow (14)
        expected.extend_from_slice(&empty); // prefix
        expected.extend_from_slice(&empty); // suffix
                                            // No member-list count byte for an update.
        assert_eq!(p.rest(), expected.as_slice());
    }

    #[test]
    fn team_create_no_color_uses_reset_ordinal() {
        let p = unwrap_team(
            team_create("n", &TextComponent::text("N"), None).expect("create with no color"),
        );
        // The formatting VarInt sits right after display NBT + 3 single-byte
        // VarInts (flags, visibility, collision). reset = 21 = 0x15.
        let after_display = nbt(&TextComponent::text("N")).len();
        assert_eq!(
            p.rest()[after_display + 3],
            0x15,
            "reset color ordinal is 21"
        );
    }

    #[test]
    fn team_remove_has_empty_tail() {
        let p = unwrap_team(team_remove("red").expect("remove"));
        assert_eq!(p.method(), TEAM_METHOD_REMOVE);
        assert!(p.rest().is_empty());
        assert_wire_frame(
            &p,
            SetPlayerTeam::encode,
            SetPlayerTeam::decode,
            SetPlayerTeam::PACKET_ID,
            None,
        )
        .expect("valid frame");
    }

    #[test]
    fn team_add_entities_tail_is_byte_exact() {
        let p = unwrap_team(team_add_entities("red", &["Saad"]).expect("add"));
        assert_eq!(p.method(), TEAM_METHOD_ADD_ENTITIES);
        // tail = count VarInt (1), then "Saad" as a length-prefixed string.
        assert_eq!(p.rest(), &[0x01, 0x04, b'S', b'a', b'a', b'd']);
        assert_wire_frame(
            &p,
            SetPlayerTeam::encode,
            SetPlayerTeam::decode,
            SetPlayerTeam::PACKET_ID,
            None,
        )
        .expect("valid frame");
    }

    #[test]
    fn team_remove_entities_uses_method_four() {
        let p = unwrap_team(team_remove_entities("red", &["Saad", "Op"]).expect("remove ents"));
        assert_eq!(p.method(), TEAM_METHOD_REMOVE_ENTITIES);
        assert_eq!(
            p.rest(),
            &[0x02, 0x04, b'S', b'a', b'a', b'd', 0x02, b'O', b'p']
        );
    }

    #[test]
    fn boss_bar_add_tail_is_byte_exact() {
        let title = TextComponent::text("Boss");
        let p = unwrap_boss(
            boss_bar_add(
                Uuid::nil(),
                &title,
                1.0,
                BossBarColor::Purple,
                BossBarDivision::TenNotches,
                BOSS_BAR_FLAG_DARKEN_SKY,
            )
            .expect("add"),
        );
        assert_eq!(p.action(), BOSS_BAR_ACTION_ADD);

        let mut expected = nbt(&title);
        expected.extend_from_slice(&1.0_f32.to_be_bytes()); // health = 1.0
        expected.push(0x05); // color VarInt: purple (5)
        expected.push(0x02); // division VarInt: ten notches (2)
        expected.push(0x01); // flags: darken sky
        assert_eq!(p.rest(), expected.as_slice());
        assert_eq!(p.rest()[0], 0x0a, "title is an anonymous-root compound");

        assert_wire_frame(
            &p,
            BossBar::encode,
            BossBar::decode,
            BossBar::PACKET_ID,
            None,
        )
        .expect("valid frame");
    }

    #[test]
    fn boss_bar_remove_has_empty_tail() {
        let p = unwrap_boss(boss_bar_remove(Uuid::nil()));
        assert_eq!(p.action(), BOSS_BAR_ACTION_REMOVE);
        assert!(p.rest().is_empty());
        assert_wire_frame(
            &p,
            BossBar::encode,
            BossBar::decode,
            BossBar::PACKET_ID,
            None,
        )
        .expect("valid frame");
    }

    #[test]
    fn boss_bar_health_tail_is_one_f32() {
        let p = unwrap_boss(boss_bar_update_health(Uuid::nil(), 0.5));
        assert_eq!(p.action(), BOSS_BAR_ACTION_UPDATE_HEALTH);
        assert_eq!(p.rest(), &0.5_f32.to_be_bytes());
        assert_eq!(p.rest(), &[0x3f, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn boss_bar_title_tail_is_nbt() {
        let title = TextComponent::text("Boss");
        let p = unwrap_boss(boss_bar_update_title(Uuid::nil(), &title).expect("title"));
        assert_eq!(p.action(), BOSS_BAR_ACTION_UPDATE_TITLE);
        assert_eq!(p.rest(), nbt(&title).as_slice());
        assert_eq!(p.rest()[0], 0x0a);
    }

    #[test]
    fn boss_bar_style_tail_is_two_varints() {
        let p = unwrap_boss(boss_bar_update_style(
            Uuid::nil(),
            BossBarColor::Red,
            BossBarDivision::NoDivision,
        ));
        assert_eq!(p.action(), BOSS_BAR_ACTION_UPDATE_STYLE);
        assert_eq!(p.rest(), &[0x02, 0x00], "red (2), no division (0)");
    }

    #[test]
    fn boss_bar_flags_tail_is_one_byte() {
        let flags = BOSS_BAR_FLAG_DARKEN_SKY | BOSS_BAR_FLAG_CREATE_FOG;
        let p = unwrap_boss(boss_bar_update_flags(Uuid::nil(), flags));
        assert_eq!(p.action(), BOSS_BAR_ACTION_UPDATE_FLAGS);
        assert_eq!(p.rest(), &[0x05], "darken sky (0x01) | create fog (0x04)");
    }

    #[test]
    fn color_ordinals_match_chat_formatting() {
        assert_eq!(formatting_id(TextColor::Black), 0);
        assert_eq!(formatting_id(TextColor::Red), 12);
        assert_eq!(formatting_id(TextColor::White), 15);
        assert_eq!(team_color_id(None), TEAM_COLOR_RESET);
        assert_eq!(team_color_id(Some(TextColor::Gold)), 6);
    }

    #[test]
    fn boss_bar_enum_ids_match_protocol() {
        assert_eq!(BossBarColor::Pink.id(), 0);
        assert_eq!(BossBarColor::White.id(), 6);
        assert_eq!(BossBarDivision::NoDivision.id(), 0);
        assert_eq!(BossBarDivision::TwentyNotches.id(), 4);
    }
}
