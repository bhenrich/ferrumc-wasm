//! The read-only world facade exposed to plugins.

use ferrumc_core::{DimensionId, PlayerId};
use ferrumc_math::{BlockPos, ChunkPos, Vec3};

/// A read-only view of the world for the duration of a single event.
///
/// This is the *only* way a plugin observes world state: it never receives a
/// raw chunk, entity store, or simulation shard. The methods are reads with no
/// side effects, using typed coordinates throughout.
///
/// # Tick scoping
///
/// Implementations are valid only for the call in which they are handed out. A
/// plugin must not stash a `WorldView` for later: the real implementation
/// borrows live simulation state and is deliberately not `Send`, so it cannot
/// be held across an `.await` point or moved to another thread. This trait is a
/// shell; the simulation layer provides the concrete implementation.
pub trait WorldView {
    /// Returns the dimension this view observes.
    fn dimension(&self) -> DimensionId;

    /// Returns whether the chunk at `chunk` is currently loaded.
    fn is_chunk_loaded(&self, chunk: ChunkPos) -> bool;

    /// Returns the block-state id at `pos`, or `None` if its chunk is not
    /// loaded.
    ///
    /// The id is the opaque registry block-state value; this facade does not
    /// interpret it.
    fn block_state_id(&self, pos: BlockPos) -> Option<u32>;

    /// Returns the position of `player`, or `None` if the player is not present
    /// in this view.
    fn player_position(&self, player: PlayerId) -> Option<Vec3>;
}
