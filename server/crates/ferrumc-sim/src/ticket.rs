//! Chunk tickets: requests to keep a chunk loaded.
//!
//! A [`ChunkTicket`] is a single *reason* to keep a chunk resident, paired with
//! a [`TicketLevel`] describing how strongly it should be kept. A chunk stays
//! loaded for exactly as long as it holds at least one ticket; when its last
//! ticket is released the [`crate::LoadedChunkMap`] unloads it. The level is, for
//! this milestone, informational — it records the intended strength (so later
//! milestones can drive load radius and tick eligibility from it) and is exposed
//! as a chunk's *effective* (strongest) level via
//! [`crate::LoadedChunkMap::effective_level`].

/// How strongly a [`ChunkTicket`] keeps its chunk loaded.
///
/// The scale mirrors vanilla's chunk-level convention: **lower numbers are
/// stronger** (keep the chunk "more loaded"). The strongest ticket on a chunk
/// wins, so a chunk's effective level is the *minimum* of its tickets' levels.
///
/// The named constants describe the eligibility tiers the simulation will grow
/// into; this milestone treats every level identically for load/unload purposes
/// (any ticket keeps the chunk loaded) and only records the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TicketLevel(u32);

impl TicketLevel {
    /// Fully active: blocks, entities, and scheduled ticks all run (strongest).
    pub const ENTITY_TICKING: Self = Self(31);

    /// Block ticking runs, but entities do not (a notch weaker than
    /// [`Self::ENTITY_TICKING`]).
    pub const TICKING: Self = Self(32);

    /// Loaded and kept in memory, but not ticked — the loaded "border"
    /// (weakest level that still keeps a chunk resident).
    pub const BORDER: Self = Self(33);

    /// Builds a ticket level from a raw value (lower is stronger).
    #[must_use]
    pub const fn new(level: u32) -> Self {
        Self(level)
    }

    /// Returns the raw level value (lower is stronger).
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Why a chunk is being kept loaded.
///
/// The reason classifies the source of a [`ChunkTicket`] so unrelated holders
/// (the world spawn, a player, an explicit force-load) keep distinct tickets and
/// releasing one never disturbs another. The enum is `#[non_exhaustive]`: more
/// reasons (portals, plugins, ...) will be added, so downstream `match`es must
/// include a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum TicketReason {
    /// Holds the world-spawn region resident for the lifetime of the world.
    Spawn,

    /// Holds the chunks around a player resident while they are nearby.
    Player,

    /// An explicit force-load (for example by an operator command).
    Forced,
}

impl TicketReason {
    /// Returns the level a ticket of this reason gets when none is specified.
    ///
    /// Spawn and forced chunks tick; the area immediately around a player ticks
    /// entities too. These defaults are starting points, not policy — callers
    /// that need a specific level use [`ChunkTicket::new`].
    #[must_use]
    pub const fn default_level(self) -> TicketLevel {
        match self {
            TicketReason::Spawn | TicketReason::Forced => TicketLevel::TICKING,
            TicketReason::Player => TicketLevel::ENTITY_TICKING,
        }
    }
}

/// A single request to keep a chunk loaded, classified by [`TicketReason`] and
/// graded by [`TicketLevel`].
///
/// Tickets are small, `Copy` values. They are compared and hashed by `(reason,
/// level)`, so two tickets are equal only when both match; the
/// [`crate::LoadedChunkMap`] counts identical tickets, so two independent holders
/// of the same `(reason, level)` each contribute one and the chunk unloads only
/// once both release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkTicket {
    reason: TicketReason,
    level: TicketLevel,
}

impl ChunkTicket {
    /// Builds a ticket with an explicit `reason` and `level`.
    #[must_use]
    pub const fn new(reason: TicketReason, level: TicketLevel) -> Self {
        Self { reason, level }
    }

    /// Builds a ticket with `reason` at that reason's
    /// [`default level`](TicketReason::default_level).
    #[must_use]
    pub const fn of(reason: TicketReason) -> Self {
        Self::new(reason, reason.default_level())
    }

    /// Returns the ticket's reason.
    #[must_use]
    pub const fn reason(self) -> TicketReason {
        self.reason
    }

    /// Returns the ticket's level.
    #[must_use]
    pub const fn level(self) -> TicketLevel {
        self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_order_lower_is_stronger() {
        assert!(TicketLevel::ENTITY_TICKING < TicketLevel::TICKING);
        assert!(TicketLevel::TICKING < TicketLevel::BORDER);
        assert_eq!(TicketLevel::new(31), TicketLevel::ENTITY_TICKING);
        assert_eq!(TicketLevel::TICKING.get(), 32);
    }

    #[test]
    fn default_levels_match_reason() {
        assert_eq!(TicketReason::Spawn.default_level(), TicketLevel::TICKING);
        assert_eq!(TicketReason::Forced.default_level(), TicketLevel::TICKING);
        assert_eq!(
            TicketReason::Player.default_level(),
            TicketLevel::ENTITY_TICKING
        );
    }

    #[test]
    fn of_uses_default_level() {
        let t = ChunkTicket::of(TicketReason::Player);
        assert_eq!(t.reason(), TicketReason::Player);
        assert_eq!(t.level(), TicketLevel::ENTITY_TICKING);
    }

    #[test]
    fn tickets_compare_by_reason_and_level() {
        let a = ChunkTicket::new(TicketReason::Spawn, TicketLevel::TICKING);
        let b = ChunkTicket::new(TicketReason::Spawn, TicketLevel::TICKING);
        let c = ChunkTicket::new(TicketReason::Spawn, TicketLevel::BORDER);
        let d = ChunkTicket::new(TicketReason::Player, TicketLevel::TICKING);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }
}
