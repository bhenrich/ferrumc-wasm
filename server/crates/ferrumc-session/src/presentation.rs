//! Builders for the clientbound *presentation* packets: titles, the action bar,
//! sound effects, and particles.
//!
//! Like [`crate::system_chat`], every function here returns a ready-to-enqueue
//! [`ClientboundPlayPacket`] — the connection writer owns the socket; this module
//! only shapes bytes. Two of these packets carry a body the generated grammar
//! leaves opaque, so this module *hand-encodes* them for protocol 772:
//!
//! - [`SoundEffect`]'s whole body (`payload`) — see [`encode_sound_effect_payload`].
//! - [`Particle`]'s per-type `data` tail (the typed head is generated) — see
//!   [`encode_dust_data`]; common particles carry no `data` at all.
//!
//! ## Sound ids — a known limitation
//!
//! The 1.21.8 sound holder identifies its sound by a numeric `sound_event`
//! registry id (the "by id" form, encoded as `id + 1`). `ferrumc-registry` does
//! not yet carry a `sound_event` table, so [`SoundId`] is an opaque numeric id and
//! the [curated constants](#constants) below are a small, hand-maintained subset.
//! Their *integer values* are best-effort for protocol 772 and should be verified
//! against the target client's registry report; the *encoding* (the `id + 1`
//! holder form, fixed-point position, volume/pitch/seed layout) is what the golden
//! tests pin and is version-stable.

use ferrumc_codec::write_var_int;
use ferrumc_core::TextComponent;
use ferrumc_math::Vec3;
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, Particle, SetActionBarText, SetSubtitleText, SetTitleAnimationTimes,
    SetTitleText, SoundEffect,
};

use crate::text::text_component_to_nbt;

// ---------------------------------------------------------------------------
// Titles / subtitle / action bar
// ---------------------------------------------------------------------------

/// Builds a `SetTitleText` packet rendering `component` as the large centred
/// title. The client only shows it once a `SetTitleAnimationTimes` (or a default)
/// is in effect; pair it with [`title_animation_times`] for explicit timing.
pub fn title(component: &TextComponent) -> ClientboundPlayPacket {
    ClientboundPlayPacket::SetTitleText(SetTitleText::new(text_component_to_nbt(component)))
}

/// Builds a `SetSubtitleText` packet. A subtitle is only drawn alongside a title,
/// so send this before (or with) the [`title`] it annotates.
pub fn subtitle(component: &TextComponent) -> ClientboundPlayPacket {
    ClientboundPlayPacket::SetSubtitleText(SetSubtitleText::new(text_component_to_nbt(component)))
}

/// Builds a `SetActionBarText` packet rendering `component` above the hotbar.
///
/// This is the dedicated action-bar carrier; unlike [`crate::system_chat`] with
/// `overlay = true`, it needs no chat apparatus and is the right tool for a
/// transient status line.
pub fn action_bar(component: &TextComponent) -> ClientboundPlayPacket {
    ClientboundPlayPacket::SetActionBarText(SetActionBarText::new(text_component_to_nbt(component)))
}

/// Builds a `SetTitleAnimationTimes` packet. Each value is in **ticks** (20/s):
/// `fade_in` ramps the title up, `stay` holds it, `fade_out` ramps it down.
pub fn title_animation_times(fade_in: i32, stay: i32, fade_out: i32) -> ClientboundPlayPacket {
    ClientboundPlayPacket::SetTitleAnimationTimes(SetTitleAnimationTimes::new(
        fade_in, stay, fade_out,
    ))
}

// ---------------------------------------------------------------------------
// Sound effects
// ---------------------------------------------------------------------------

/// A sound's numeric `sound_event` registry id (protocol 772).
///
/// Held opaquely because `ferrumc-registry` has no sound table yet; see the
/// [module docs](self) for the limitation. The wire holder encodes it as
/// `id + 1` (a leading `0` means an *inline* sound, which this slice does not
/// emit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoundId(i32);

impl SoundId {
    /// Wraps a raw `sound_event` registry id.
    pub const fn new(registry_id: i32) -> Self {
        Self(registry_id)
    }

    /// Returns the raw registry id.
    pub const fn get(self) -> i32 {
        self.0
    }
}

/// The sound category (vanilla "sound source"), used by clients to apply the
/// matching volume slider. Wire ids match the 1.21.8 `soundSource` mapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundCategory {
    /// `master` — the global slider.
    Master,
    /// `music` — background music.
    Music,
    /// `record` — jukeboxes / note blocks records.
    Record,
    /// `weather` — rain, thunder.
    Weather,
    /// `block` — block sounds.
    Block,
    /// `hostile` — hostile mobs.
    Hostile,
    /// `neutral` — friendly/neutral mobs.
    Neutral,
    /// `player` — player-emitted sounds.
    Player,
    /// `ambient` — environment / ambience.
    Ambient,
    /// `voice` — voice / speech.
    Voice,
    /// `ui` — interface sounds (button clicks).
    Ui,
}

impl SoundCategory {
    /// Returns the `VarInt` wire id for this category.
    pub const fn id(self) -> i32 {
        match self {
            Self::Master => 0,
            Self::Music => 1,
            Self::Record => 2,
            Self::Weather => 3,
            Self::Block => 4,
            Self::Hostile => 5,
            Self::Neutral => 6,
            Self::Player => 7,
            Self::Ambient => 8,
            Self::Voice => 9,
            Self::Ui => 10,
        }
    }
}

/// Sound positions are sent as fixed-point integers: the world coordinate scaled
/// by 8 and truncated. Mirrors vanilla's `(int)(coord * 8.0)`.
const SOUND_POSITION_FIXED_POINT_SCALE: f64 = 8.0;

/// Converts a world coordinate to the fixed-point integer the sound packet uses.
fn fixed_point(coord: f64) -> i32 {
    // `as i32` truncates toward zero and saturates, matching Java's `(int)` cast
    // for the in-range coordinates a sound is ever played at.
    (coord * SOUND_POSITION_FIXED_POINT_SCALE) as i32
}

/// Hand-encodes the opaque `SoundEffect` body for protocol 772.
///
/// Layout: sound holder (`VarInt` = `sound.get() + 1`, the registry-id form),
/// then the category (`VarInt`), the fixed-point position (`x`/`y`/`z` as big-
/// endian `i32`, each the block coordinate times 8), `volume` and `pitch` as
/// big-endian `f32`, and a `seed` as a big-endian `i64`.
pub fn encode_sound_effect_payload(
    sound: SoundId,
    category: SoundCategory,
    pos: Vec3,
    volume: f32,
    pitch: f32,
    seed: i64,
) -> Vec<u8> {
    let mut buf = Vec::new();
    // Registry-id holder: 0 would select the inline form; a non-zero leading
    // VarInt is `registry id + 1`.
    write_var_int(&mut buf, sound.get() + 1);
    write_var_int(&mut buf, category.id());
    buf.extend_from_slice(&fixed_point(pos.x).to_be_bytes());
    buf.extend_from_slice(&fixed_point(pos.y).to_be_bytes());
    buf.extend_from_slice(&fixed_point(pos.z).to_be_bytes());
    buf.extend_from_slice(&volume.to_be_bytes());
    buf.extend_from_slice(&pitch.to_be_bytes());
    buf.extend_from_slice(&seed.to_be_bytes());
    buf
}

/// Builds a `SoundEffect` packet playing `sound` at a fixed world position.
///
/// `volume` above `1.0` increases audible range rather than loudness; `pitch` is
/// a playback-rate multiplier (`0.5..=2.0` is the client's usable range). `seed`
/// selects among a sound event's variants — pass `0` for deterministic output.
pub fn play_sound(
    sound: SoundId,
    category: SoundCategory,
    pos: Vec3,
    volume: f32,
    pitch: f32,
    seed: i64,
) -> ClientboundPlayPacket {
    ClientboundPlayPacket::SoundEffect(SoundEffect::new(encode_sound_effect_payload(
        sound, category, pos, volume, pitch, seed,
    )))
}

/// `minecraft:block.note_block.harp`. Curated id — see the [module docs](self).
pub const SOUND_NOTE_BLOCK_HARP: SoundId = SoundId::new(1088);
/// `minecraft:entity.player.levelup`. Curated id — see the [module docs](self).
pub const SOUND_PLAYER_LEVELUP: SoundId = SoundId::new(489);
/// `minecraft:ui.button.click`. Curated id — see the [module docs](self).
pub const SOUND_UI_BUTTON_CLICK: SoundId = SoundId::new(1235);
/// `minecraft:entity.experience_orb.pickup`. Curated id — see the [module docs](self).
pub const SOUND_EXPERIENCE_ORB_PICKUP: SoundId = SoundId::new(327);
/// `minecraft:block.anvil.land`. Curated id — see the [module docs](self).
pub const SOUND_ANVIL_LAND: SoundId = SoundId::new(150);

// ---------------------------------------------------------------------------
// Particles
// ---------------------------------------------------------------------------

/// `minecraft:cloud` — no `data`.
pub const PARTICLE_CLOUD: i32 = 4;
/// `minecraft:crit` — no `data`.
pub const PARTICLE_CRIT: i32 = 5;
/// `minecraft:explosion` — no `data`.
pub const PARTICLE_EXPLOSION: i32 = 22;
/// `minecraft:flame` — no `data`.
pub const PARTICLE_FLAME: i32 = 31;
/// `minecraft:happy_villager` — no `data`.
pub const PARTICLE_HAPPY_VILLAGER: i32 = 42;
/// `minecraft:heart` — no `data`.
pub const PARTICLE_HEART: i32 = 44;
/// `minecraft:smoke` — no `data`.
pub const PARTICLE_SMOKE: i32 = 59;
/// `minecraft:dust` — carries a four-`f32` `data` tail; see [`encode_dust_data`].
pub const PARTICLE_DUST: i32 = 13;

/// Hand-encodes the `data` tail of a `minecraft:dust` particle for protocol 772:
/// the `red`, `green`, `blue` colour channels (each `0.0..=1.0`) and a `scale`,
/// all big-endian `f32`.
pub fn encode_dust_data(red: f32, green: f32, blue: f32, scale: f32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&red.to_be_bytes());
    buf.extend_from_slice(&green.to_be_bytes());
    buf.extend_from_slice(&blue.to_be_bytes());
    buf.extend_from_slice(&scale.to_be_bytes());
    buf
}

/// Builds a `Particle` packet from every wire field, with `data` already encoded
/// for the chosen `kind`.
///
/// Most callers want [`spawn_simple_particle`] (no-`data` kinds) or [`spawn_dust`]
/// (the one with-`data` example); this is the escape hatch when you need explicit
/// offsets, speed, the `long_distance` flag, or a `data` tail this module does not
/// yet model. `offset` is the per-axis spawn jitter; `max_speed` scales velocity;
/// `count` is the particle count (`0` makes `max_speed` a directional speed).
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors the Particle packet's wire fields"
)]
pub fn spawn_particle(
    kind: i32,
    pos: Vec3,
    offset: Vec3,
    max_speed: f32,
    count: i32,
    long_distance: bool,
    always_show: bool,
    data: Vec<u8>,
) -> ClientboundPlayPacket {
    ClientboundPlayPacket::Particle(Particle::new(
        long_distance,
        always_show,
        pos.x,
        pos.y,
        pos.z,
        offset.x as f32,
        offset.y as f32,
        offset.z as f32,
        max_speed,
        count,
        kind,
        data,
    ))
}

/// Builds a `Particle` packet for a no-`data` `kind` (flame, heart, crit, …) with
/// zero offset and speed. Use a `PARTICLE_*` constant for `kind`.
pub fn spawn_simple_particle(kind: i32, pos: Vec3, count: i32) -> ClientboundPlayPacket {
    spawn_particle(
        kind,
        pos,
        Vec3::new(0.0, 0.0, 0.0),
        0.0,
        count,
        false,
        false,
        Vec::new(),
    )
}

/// Builds a `minecraft:dust` `Particle` packet of the given colour and scale —
/// the one particle with a non-empty `data` tail this module models.
pub fn spawn_dust(
    pos: Vec3,
    red: f32,
    green: f32,
    blue: f32,
    scale: f32,
    count: i32,
) -> ClientboundPlayPacket {
    spawn_particle(
        PARTICLE_DUST,
        pos,
        Vec3::new(0.0, 0.0, 0.0),
        0.0,
        count,
        false,
        false,
        encode_dust_data(red, green, blue, scale),
    )
}

#[cfg(test)]
mod tests {
    use ferrumc_core::TextColor;
    use ferrumc_proto::generated::play::{
        Particle, SetTitleAnimationTimes, SetTitleText, SoundEffect,
    };
    use ferrumc_testkit::assert_wire_frame;

    use super::*;

    /// Strips the frame-length and packet-id prefixes the oracle prepends,
    /// returning just the packet body for byte-exact assertions.
    fn body_of(framed: &[u8]) -> Vec<u8> {
        // For all packets here the id is a single-byte VarInt, so the body starts
        // after the length VarInt (1 byte for these short frames) and the id byte.
        // Decode the length VarInt properly to stay correct if a frame grows.
        let mut idx = 0;
        // length VarInt
        loop {
            let byte = framed[idx];
            idx += 1;
            if byte & 0x80 == 0 {
                break;
            }
        }
        // id VarInt
        loop {
            let byte = framed[idx];
            idx += 1;
            if byte & 0x80 == 0 {
                break;
            }
        }
        framed[idx..].to_vec()
    }

    #[test]
    fn title_builders_round_trip_through_the_oracle() {
        let component = TextComponent::text("Victory").with_color(TextColor::Gold);
        let ClientboundPlayPacket::SetTitleText(packet) = title(&component) else {
            panic!("title must build a SetTitleText");
        };
        assert_eq!(packet.text(), &text_component_to_nbt(&component));
        assert_wire_frame(
            &packet,
            SetTitleText::encode,
            SetTitleText::decode,
            SetTitleText::PACKET_ID,
            None,
        )
        .expect("SetTitleText forms a valid, round-tripping frame");
    }

    #[test]
    fn title_animation_times_pins_three_big_endian_i32() {
        let ClientboundPlayPacket::SetTitleAnimationTimes(packet) =
            title_animation_times(10, 70, 20)
        else {
            panic!("must build a SetTitleAnimationTimes");
        };
        let framed = assert_wire_frame(
            &packet,
            SetTitleAnimationTimes::encode,
            SetTitleAnimationTimes::decode,
            SetTitleAnimationTimes::PACKET_ID,
            None,
        )
        .expect("valid frame");
        // fade_in = 10, stay = 70, fade_out = 20, each big-endian i32.
        assert_eq!(
            body_of(&framed),
            vec![0x00, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x46, 0x00, 0x00, 0x00, 0x14,]
        );
    }

    #[test]
    fn sound_effect_payload_is_byte_exact() {
        // Deterministic inputs: id 123 (-> holder 124 = 0x7c), category Player (7),
        // position (8, 64, 16) -> fixed-point (64, 512, 128), volume/pitch 1.0,
        // seed 0.
        let payload = encode_sound_effect_payload(
            SoundId::new(123),
            SoundCategory::Player,
            Vec3::new(8.0, 64.0, 16.0),
            1.0,
            1.0,
            0,
        );
        assert_eq!(
            payload,
            vec![
                0x7c, // sound holder VarInt (123 + 1)
                0x07, // category VarInt (player)
                0x00, 0x00, 0x00, 0x40, // x fixed-point i32 = 64
                0x00, 0x00, 0x02, 0x00, // y fixed-point i32 = 512
                0x00, 0x00, 0x00, 0x80, // z fixed-point i32 = 128
                0x3f, 0x80, 0x00, 0x00, // volume f32 = 1.0
                0x3f, 0x80, 0x00, 0x00, // pitch f32 = 1.0
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // seed i64 = 0
            ]
        );

        // And the whole packet forms a valid, round-tripping frame.
        let packet = SoundEffect::new(payload);
        let framed = assert_wire_frame(
            &packet,
            SoundEffect::encode,
            SoundEffect::decode,
            SoundEffect::PACKET_ID,
            None,
        )
        .expect("SoundEffect frame is well-formed");
        assert_eq!(body_of(&framed), packet.payload());
    }

    #[test]
    fn fixed_point_truncates_toward_zero() {
        assert_eq!(fixed_point(0.0), 0);
        assert_eq!(fixed_point(1.0), 8);
        assert_eq!(fixed_point(0.5), 4);
        // -0.5 * 8 = -4.0 exactly; truncation keeps it at -4.
        assert_eq!(fixed_point(-0.5), -4);
    }

    #[test]
    fn simple_particle_has_no_data_tail() {
        let ClientboundPlayPacket::Particle(packet) =
            spawn_simple_particle(PARTICLE_FLAME, Vec3::new(1.0, 2.0, 3.0), 5)
        else {
            panic!("must build a Particle");
        };
        assert_eq!(packet.kind(), PARTICLE_FLAME);
        assert!(
            packet.data().is_empty(),
            "no-data particle has an empty tail"
        );
        assert_wire_frame(
            &packet,
            Particle::encode,
            Particle::decode,
            Particle::PACKET_ID,
            None,
        )
        .expect("Particle frame is well-formed");
    }

    #[test]
    fn dust_data_is_byte_exact() {
        // Pure red, scale 1.0.
        let data = encode_dust_data(1.0, 0.0, 0.0, 1.0);
        assert_eq!(
            data,
            vec![
                0x3f, 0x80, 0x00, 0x00, // red = 1.0
                0x00, 0x00, 0x00, 0x00, // green = 0.0
                0x00, 0x00, 0x00, 0x00, // blue = 0.0
                0x3f, 0x80, 0x00, 0x00, // scale = 1.0
            ]
        );

        let ClientboundPlayPacket::Particle(packet) =
            spawn_dust(Vec3::new(0.0, 0.0, 0.0), 1.0, 0.0, 0.0, 1.0, 1)
        else {
            panic!("must build a Particle");
        };
        assert_eq!(packet.kind(), PARTICLE_DUST);
        assert_eq!(packet.data(), data.as_slice());
        assert_wire_frame(
            &packet,
            Particle::encode,
            Particle::decode,
            Particle::PACKET_ID,
            None,
        )
        .expect("dust Particle frame is well-formed");
    }

    #[test]
    fn sound_category_ids_match_the_protocol_mapper() {
        assert_eq!(SoundCategory::Master.id(), 0);
        assert_eq!(SoundCategory::Player.id(), 7);
        assert_eq!(SoundCategory::Ui.id(), 10);
    }
}
