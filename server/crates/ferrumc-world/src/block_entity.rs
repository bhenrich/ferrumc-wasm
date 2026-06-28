//! Block entities: the extra per-block state some blocks carry beyond their
//! block-state id (signs, and — in later milestones — chests, spawners, ...).
//!
//! This crate models the *data* only. Serializing a block entity to the network
//! NBT a client expects lives in the session layer (it owns the
//! `ferrumc-proto`/`ferrumc-nbt` dependencies); this crate stays free of the
//! protocol so the world model can be unit-tested in isolation. A [`Chunk`]
//! owns its block entities keyed by [`BlockPos`](ferrumc_math::BlockPos); they
//! are dropped with the chunk on unload, so the map is bounded by the chunk's
//! block count (and an explicit [`Chunk::set_block_entity`] cap) and needs no
//! separate cleanup.
//!
//! [`Chunk`]: crate::Chunk
//! [`Chunk::set_block_entity`]: crate::Chunk::set_block_entity

use ferrumc_registry::block_state::state_id_to_block_name;

/// The number of text lines on one face of a sign (the vanilla fixed count).
pub const SIGN_LINES: usize = 4;

/// Which sign block-entity kind a sign block is. Selects the protocol
/// block-entity-type id the client expects in a `BlockEntityData` packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignKind {
    /// A standing or wall sign (`minecraft:sign`).
    Sign,
    /// A hanging sign (`minecraft:hanging_sign`).
    Hanging,
}

impl SignKind {
    /// The protocol block-entity-type id for this kind (1.21.8 / protocol 772):
    /// `minecraft:sign` is `7`, `minecraft:hanging_sign` is `8`.
    ///
    /// Verified against the pinned `minecraft:block_entity_type` registry
    /// (`sign -> 7`, `hanging_sign -> 8`).
    #[must_use]
    pub const fn block_entity_type(self) -> i32 {
        match self {
            SignKind::Sign => 7,
            SignKind::Hanging => 8,
        }
    }
}

/// The text and styling of one face (front or back) of a sign.
///
/// Holds the four text lines plus the minimal styling a sign carries: a dye
/// `color` applied to the whole face (default `"black"`) and a `has_glowing_text`
/// flag. Each line is plain text; the network encoder wraps each as a text
/// component (a bare string is the shorthand form a 1.21.8 client accepts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignFace {
    lines: [String; SIGN_LINES],
    color: String,
    has_glowing_text: bool,
}

impl SignFace {
    /// Returns the four text lines of this face, top to bottom.
    #[must_use]
    pub fn lines(&self) -> &[String; SIGN_LINES] {
        &self.lines
    }

    /// Returns the dye color applied to the whole face (e.g. `"black"`).
    #[must_use]
    pub fn color(&self) -> &str {
        &self.color
    }

    /// Returns whether the face's text glows in the dark.
    #[must_use]
    pub fn has_glowing_text(&self) -> bool {
        self.has_glowing_text
    }

    /// Replaces the four text lines, leaving `color` and `has_glowing_text`
    /// untouched (a sign edit only carries the line text this milestone).
    pub fn set_lines(&mut self, lines: [String; SIGN_LINES]) {
        self.lines = lines;
    }
}

impl Default for SignFace {
    /// An empty face: four blank lines, default `"black"` color, no glow.
    fn default() -> Self {
        Self {
            lines: std::array::from_fn(|_| String::new()),
            color: "black".to_owned(),
            has_glowing_text: false,
        }
    }
}

/// A sign block entity: a [`SignKind`], a waxed flag, and front/back [`SignFace`]
/// text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sign {
    kind: SignKind,
    is_waxed: bool,
    front: SignFace,
    back: SignFace,
}

impl Sign {
    /// Creates a blank sign of `kind`: both faces empty, not waxed.
    #[must_use]
    pub fn new(kind: SignKind) -> Self {
        Self {
            kind,
            is_waxed: false,
            front: SignFace::default(),
            back: SignFace::default(),
        }
    }

    /// Returns the sign's kind (standing/wall vs hanging).
    #[must_use]
    pub fn kind(&self) -> SignKind {
        self.kind
    }

    /// Returns whether the sign is waxed (its text can no longer be edited).
    #[must_use]
    pub fn is_waxed(&self) -> bool {
        self.is_waxed
    }

    /// Returns the front face.
    #[must_use]
    pub fn front(&self) -> &SignFace {
        &self.front
    }

    /// Returns the back face.
    #[must_use]
    pub fn back(&self) -> &SignFace {
        &self.back
    }

    /// Returns the requested face: the front when `is_front`, otherwise the back.
    #[must_use]
    pub fn face(&self, is_front: bool) -> &SignFace {
        if is_front {
            &self.front
        } else {
            &self.back
        }
    }

    /// Replaces the text lines of the requested face (front when `is_front`,
    /// otherwise back), keeping that face's color and glow.
    pub fn set_face_lines(&mut self, is_front: bool, lines: [String; SIGN_LINES]) {
        if is_front {
            self.front.set_lines(lines);
        } else {
            self.back.set_lines(lines);
        }
    }
}

/// A block entity stored in a [`Chunk`](crate::Chunk), keyed by its
/// [`BlockPos`](ferrumc_math::BlockPos).
///
/// The enum is `#[non_exhaustive]`: later milestones add chests, spawners, and
/// the like, so downstream `match`es must carry a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BlockEntity {
    /// A sign with front/back text.
    Sign(Sign),
}

/// Classifies a block-state id as the [`SignKind`] of the sign block it belongs
/// to, or `None` if it is not a sign block.
///
/// Resolves the state id to its block name via the registry and matches the
/// vanilla sign families by suffix: any `*_hanging_sign` (wall or ceiling) is a
/// [`SignKind::Hanging`]; any other `*_sign` (standing or wall) is a
/// [`SignKind::Sign`]. Hanging is checked first because its name also ends with
/// `_sign`.
#[must_use]
pub fn sign_kind_for_state(state_id: u32) -> Option<SignKind> {
    let name = state_id_to_block_name(state_id)?;
    if name.ends_with("hanging_sign") {
        Some(SignKind::Hanging)
    } else if name.ends_with("_sign") {
        Some(SignKind::Sign)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrumc_registry::block_state::block_default_state;

    #[test]
    fn block_entity_type_ids_match_the_registry() {
        assert_eq!(SignKind::Sign.block_entity_type(), 7);
        assert_eq!(SignKind::Hanging.block_entity_type(), 8);
    }

    #[test]
    fn new_sign_is_blank_and_unwaxed() {
        let sign = Sign::new(SignKind::Sign);
        assert_eq!(sign.kind(), SignKind::Sign);
        assert!(!sign.is_waxed());
        for face in [sign.front(), sign.back()] {
            assert_eq!(face.color(), "black");
            assert!(!face.has_glowing_text());
            assert!(face.lines().iter().all(String::is_empty));
        }
    }

    #[test]
    fn set_face_lines_updates_only_the_named_face() {
        let mut sign = Sign::new(SignKind::Sign);
        let lines = [
            "hello".to_owned(),
            "world".to_owned(),
            String::new(),
            "!".to_owned(),
        ];
        sign.set_face_lines(true, lines.clone());
        assert_eq!(sign.front().lines(), &lines);
        // The back face is untouched.
        assert!(sign.back().lines().iter().all(String::is_empty));
        // `face(is_front)` selects the same face the setter wrote.
        assert_eq!(sign.face(true).lines(), &lines);
        assert_eq!(sign.face(false).lines(), sign.back().lines());
    }

    #[test]
    fn sign_kind_classifies_standing_wall_and_hanging() {
        // Standing and wall signs map to the regular sign block-entity.
        let oak = block_default_state("oak_sign").expect("oak_sign in registry");
        assert_eq!(sign_kind_for_state(oak), Some(SignKind::Sign));
        let oak_wall = block_default_state("oak_wall_sign").expect("oak_wall_sign in registry");
        assert_eq!(sign_kind_for_state(oak_wall), Some(SignKind::Sign));
        // Hanging signs map to the hanging block-entity (checked before `_sign`).
        let oak_hanging =
            block_default_state("oak_hanging_sign").expect("oak_hanging_sign in registry");
        assert_eq!(sign_kind_for_state(oak_hanging), Some(SignKind::Hanging));
    }

    #[test]
    fn non_sign_states_are_not_signs() {
        // Stone (1) and air (0) are not signs; an unknown id is rejected too.
        assert_eq!(sign_kind_for_state(0), None);
        assert_eq!(sign_kind_for_state(1), None);
        assert_eq!(sign_kind_for_state(u32::MAX), None);
    }
}
