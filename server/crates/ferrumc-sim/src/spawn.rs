//! The world-spawn chunk ticket set.
//!
//! The spawn region is a square of chunks centred on the world spawn that is
//! kept loaded for the lifetime of the world. [`SpawnChunkTickets`] describes
//! that square — its centre, its radius, and the [`ChunkTicket`] every chunk in
//! it receives — and enumerates the positions deterministically so loading them
//! (see [`crate::LoadedChunkMap::acquire_spawn`]) is reproducible.

use ferrumc_math::ChunkPos;

use crate::ticket::{ChunkTicket, TicketReason};

/// The set of chunks the world keeps loaded around its spawn point.
///
/// The set is the `(2 * radius + 1)` by `(2 * radius + 1)` square of chunks
/// centred on [`SpawnChunkTickets::center`]. Each chunk in it is held by a single
/// [`TicketReason::Spawn`] ticket (see [`SpawnChunkTickets::ticket`]).
///
/// The radius is a [`u8`], so the set is bounded to at most `511 * 511` chunks
/// regardless of input; a misconfigured radius can never request an unbounded
/// number of chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnChunkTickets {
    center: ChunkPos,
    radius: u8,
}

impl SpawnChunkTickets {
    /// Default spawn radius in chunks: a 5x5 square around the centre.
    pub const DEFAULT_RADIUS: u8 = 2;

    /// Builds a spawn set of the given `radius` (in chunks) around `center`.
    #[must_use]
    pub const fn new(center: ChunkPos, radius: u8) -> Self {
        Self { center, radius }
    }

    /// Builds a spawn set of [`DEFAULT_RADIUS`](Self::DEFAULT_RADIUS) chunks
    /// around `center`.
    #[must_use]
    pub const fn around(center: ChunkPos) -> Self {
        Self::new(center, Self::DEFAULT_RADIUS)
    }

    /// Returns the centre chunk of the spawn square.
    #[must_use]
    pub const fn center(self) -> ChunkPos {
        self.center
    }

    /// Returns the spawn radius, in chunks.
    #[must_use]
    pub const fn radius(self) -> u8 {
        self.radius
    }

    /// Returns the [`ChunkTicket`] applied to every chunk in the spawn set: a
    /// [`TicketReason::Spawn`] ticket at that reason's default level.
    #[must_use]
    pub fn ticket(self) -> ChunkTicket {
        ChunkTicket::of(TicketReason::Spawn)
    }

    /// Returns the number of chunks in the spawn set,
    /// `(2 * radius + 1).pow(2)`.
    #[must_use]
    pub fn chunk_count(self) -> usize {
        let side = 2 * usize::from(self.radius) + 1;
        side * side
    }

    /// Returns every chunk position in the spawn set, in a deterministic
    /// `z`-major, `x`-minor order from the lowest corner.
    ///
    /// Positions are computed with saturating arithmetic, so a centre at the
    /// edge of the [`i32`] coordinate range can never overflow into a panic; for
    /// any realistic spawn (near the origin) the square is exact.
    pub fn positions(self) -> impl Iterator<Item = ChunkPos> {
        let radius = i32::from(self.radius);
        let cx = self.center.x();
        let cz = self.center.z();
        (-radius..=radius).flat_map(move |dz| {
            (-radius..=radius)
                .map(move |dx| ChunkPos::new(cx.saturating_add(dx), cz.saturating_add(dz)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ticket::TicketLevel;

    #[test]
    fn default_radius_is_five_by_five() {
        let spawn = SpawnChunkTickets::around(ChunkPos::ORIGIN);
        assert_eq!(spawn.radius(), SpawnChunkTickets::DEFAULT_RADIUS);
        assert_eq!(spawn.chunk_count(), 25);
        assert_eq!(spawn.positions().count(), 25);
    }

    #[test]
    fn ticket_is_spawn_reason() {
        let t = SpawnChunkTickets::around(ChunkPos::ORIGIN).ticket();
        assert_eq!(t.reason(), TicketReason::Spawn);
        assert_eq!(t.level(), TicketLevel::TICKING);
    }

    #[test]
    fn positions_cover_the_square_deterministically() {
        let spawn = SpawnChunkTickets::new(ChunkPos::new(10, -4), 1);
        let positions: Vec<ChunkPos> = spawn.positions().collect();
        // 3x3 square, z-major from the lowest corner.
        assert_eq!(
            positions,
            vec![
                ChunkPos::new(9, -5),
                ChunkPos::new(10, -5),
                ChunkPos::new(11, -5),
                ChunkPos::new(9, -4),
                ChunkPos::new(10, -4),
                ChunkPos::new(11, -4),
                ChunkPos::new(9, -3),
                ChunkPos::new(10, -3),
                ChunkPos::new(11, -3),
            ]
        );
        // Same input, same order.
        let again: Vec<ChunkPos> = spawn.positions().collect();
        assert_eq!(positions, again);
    }

    #[test]
    fn radius_zero_is_a_single_chunk() {
        let spawn = SpawnChunkTickets::new(ChunkPos::new(3, 3), 0);
        assert_eq!(spawn.chunk_count(), 1);
        let positions: Vec<ChunkPos> = spawn.positions().collect();
        assert_eq!(positions, vec![ChunkPos::new(3, 3)]);
    }

    #[test]
    fn extreme_center_saturates_without_panicking() {
        let spawn = SpawnChunkTickets::new(ChunkPos::new(i32::MAX, i32::MIN), 1);
        // Must not panic; saturating arithmetic clamps at the bounds.
        let positions: Vec<ChunkPos> = spawn.positions().collect();
        assert_eq!(positions.len(), 9);
        assert!(positions.contains(&ChunkPos::new(i32::MAX, i32::MIN)));
    }
}
