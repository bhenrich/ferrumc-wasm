//! Per-connection chunk-streaming state and the bounded, center-out pump that
//! keeps a client's loaded chunk square in sync with its position.

use std::collections::BTreeSet;

use tokio::sync::oneshot;

use ferrumc_math::{BlockPos, ChunkPos, Vec3};
use ferrumc_net::{CompressionState, OutboundPriority, PlayWriter};
use ferrumc_observability::SessionDebug;
use ferrumc_proto::generated::play::{
    ClientboundPlayPacket, SetCenterChunk, SynchronizePlayerPosition, UnloadChunk,
};

use crate::driver::SimCommand;
use crate::player_data::is_valid_player_position;

use super::context::ConnContext;
use super::outbound::{enqueue_traced, send_mandatory};

/// Maximum number of `ChunkDataAndLight` packets a single streaming evaluation
/// will request and send for one player.
///
/// Each chunk packet carries the full section + light payload (tens of KiB), so
/// an uncapped burst — a teleport that makes the whole view square new, or the
/// initial gap between the small spawn batch and a large view distance — could
/// dump megabytes onto one socket at once. Capping the per-update load count
/// paces the stream: the leftover chunks are picked up on the next stream
/// cadence (the desired-vs-loaded diff is recomputed every pump), nearest-first,
/// so the player always gets the chunks closest to them first. `16` keeps a
/// normal single-chunk step fully served in one update while bounding a teleport
/// flood.
const MAX_CHUNK_LOADS_PER_UPDATE: usize = 16;

/// Maximum number of departed chunk columns released by one stream pump.
///
/// Releasing a player ticket crosses the driver's durability barrier and can
/// evict a chunk, so the tiny `UnloadChunk` wire size is irrelevant to the true
/// cost. Matching the load bound limits one pump to at most 32 chunk mutations.
/// The remainder is recomputed from the latest centre on the next cadence tick.
const MAX_CHUNK_UNLOADS_PER_UPDATE: usize = 16;

/// Upper bound applied to the configured view distance when streaming chunks.
///
/// The streamed view is a `(2 * r + 1)` square, so the per-player loaded-chunk
/// set is bounded by `(2 * r + 1)^2`. Clamping `r` here (to the vanilla view-
/// distance ceiling) keeps that set — and the work a single boundary crossing can
/// request — bounded even if the config carries an absurd view distance.
pub(super) const STREAM_VIEW_DISTANCE_MAX: i32 = 32;

/// Per-connection chunk-streaming state: which chunk the client is centred on and
/// which chunk columns it currently holds.
///
/// This is connection/session bookkeeping, not world state: it records what has
/// been *sent to this client* so the stream never re-sends a chunk and knows what
/// to unload. The authoritative chunk data still lives in the simulation shard;
/// the connection only ever asks the driver (via [`SimCommand::StreamChunks`]) to
/// load-or-generate and never touches the chunk map itself.
pub(super) struct ChunkStream {
    /// The chunk the client is currently centred on (the last `Set Center Chunk`).
    center: ChunkPos,
    /// The square radius, in chunks, of the streamed view (already clamped).
    view_distance: i32,
    /// The chunk columns the client currently holds (spawn batch + streamed in),
    /// bounded by `(2 * view_distance + 1)^2`.
    pub(super) loaded: BTreeSet<ChunkPos>,
    /// The latest accepted client position or authoritative server teleport since
    /// the last fixed-cadence evaluation. Replacing this slot coalesces every
    /// superseded target into one streaming pass.
    pending_position: Option<Vec3>,
    /// The latest accepted client position or authoritative server teleport
    /// (never cleared), used to persist where the player was standing when they
    /// leave. If neither occurs, it remains `None` and the join position is
    /// persisted instead.
    last_position: Option<Vec3>,
    /// The integer block of the latest accepted client position or authoritative
    /// teleport, used to throttle the `PlayerMove` plugin event to block
    /// granularity. `None` until the first accepted position establishes a baseline
    /// (so the very first packet only seeds it and does not fire), see
    /// [`block_crossing`](ChunkStream::block_crossing).
    last_move_block: Option<BlockPos>,
}

impl ChunkStream {
    /// Seeds the stream from the join kit: centred on the spawn chunk and already
    /// holding the spawn-batch columns the client received at join.
    pub(super) fn new(ctx: &ConnContext) -> Self {
        Self {
            center: ctx.join_kit.spawn_chunk(),
            view_distance: ctx.view_distance.clamp(0, STREAM_VIEW_DISTANCE_MAX),
            loaded: ctx.join_kit.chunk_positions().collect(),
            pending_position: None,
            last_position: None,
            last_move_block: None,
        }
    }

    /// Records the client's latest validated and admitted position.
    pub(super) fn observe(&mut self, position: Vec3) {
        self.pending_position = Some(position);
        self.last_position = Some(position);
    }

    /// The latest accepted client position or authoritative server teleport, or
    /// `None` if neither occurred (in which case the caller persists the join
    /// position).
    pub(super) fn last_position(&self) -> Option<Vec3> {
        self.last_position
    }

    /// Returns the coalesced target for handler-boundary regression tests.
    #[cfg(test)]
    pub(super) fn pending_position_for_test(&self) -> Option<Vec3> {
        self.pending_position
    }

    /// Returns the delivered stream centre for queue-overflow regression tests.
    #[cfg(test)]
    pub(super) fn center_for_test(&self) -> ChunkPos {
        self.center
    }

    /// Records `position` and returns the `(from, to)` block crossing if the player
    /// entered a *new* integer block since the last reported crossing, else `None`.
    ///
    /// This throttles the `PlayerMove` plugin event to block granularity: many
    /// sub-block movement packets within one block collapse to no event, and the
    /// event fires once per block the player enters. The first call after join only
    /// seeds the baseline (there is no prior block to report a crossing *from*), so
    /// it returns `None`. Coordinates are floored to block space (matching vanilla:
    /// block `x = -0.5` is block `-1`).
    pub(super) fn block_crossing(&mut self, position: Vec3) -> Option<(BlockPos, BlockPos)> {
        let to = BlockPos::new(
            position.x.floor() as i32,
            position.y.floor() as i32,
            position.z.floor() as i32,
        );
        let crossing = match self.last_move_block {
            Some(from) if from == to => None,
            Some(from) => Some((from, to)),
            None => None,
        };
        self.last_move_block = Some(to);
        crossing
    }

    /// Records one authoritative server teleport for persistence and streaming.
    ///
    /// A server-driven teleport is already authoritative, so waiting for a client
    /// echo would leave simulation/router/save at the destination while tickets
    /// remain at the old centre. The fixed-cadence pump coalesces this pending
    /// destination with any later accepted observation.
    pub(super) fn mirror_teleport(&mut self, position: Vec3) {
        self.pending_position = Some(position);
        self.last_position = Some(position);
        // A server teleport establishes a new plugin movement baseline. Retaining
        // the pre-teleport block would turn the first client echo or tiny step at
        // the destination into one false world-spanning PlayerMove notification.
        self.last_move_block = Some(block_of(position));
    }
}

/// Mirrors a server-driven absolute teleport into the persistence state so a later
/// leave-save captures the teleported position and look, not the stale
/// client-reported one.
///
/// The server snaps a player with an authoritative `SynchronizePlayerPosition` for
/// `/spawn`, a routed plugin `Teleport`, or an anti-cheat correction. The mirror is
/// otherwise advanced only by validated client movement packets. Only safe,
/// fully-absolute syncs (`flags == 0`, the only kind the server emits) are
/// mirrored; a partially-relative or non-finite/out-of-range sync is left
/// untouched because it is not a trustworthy absolute destination.
pub(super) fn mirror_server_teleport(
    chunk_stream: &mut ChunkStream,
    player_yaw: &mut f32,
    player_pitch: &mut f32,
    sync: &SynchronizePlayerPosition,
) {
    let position = Vec3::new(sync.x(), sync.y(), sync.z());
    if sync.flags() != 0
        || !is_valid_player_position(position)
        || !sync.yaw().is_finite()
        || !sync.pitch().is_finite()
    {
        return;
    }
    chunk_stream.mirror_teleport(position);
    *player_yaw = sync.yaw();
    *player_pitch = sync.pitch();
}

/// Streams chunks toward the latest accepted movement or authoritative teleport,
/// then advances the view toward the full view distance.
///
/// If a new target arrived since the last call, this recenters on it (sending
/// `Set Center Chunk` on a chunk-boundary crossing). Either way it then runs
/// [`pump_chunk_stream`] against the current center, so the view advances even
/// when no target arrived (a freshly-joined or standing player). A chunk already
/// in the loaded set is never re-requested or re-sent.
///
/// The connection never generates chunks itself: it only decides the desired set
/// and renders the packets the driver returns.
pub(super) async fn apply_chunk_stream(
    ctx: &ConnContext,
    writer: &mut PlayWriter,
    chunk_stream: &mut ChunkStream,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    // Position path: recenter on the latest accepted target, if any.
    if let Some(position) = chunk_stream.pending_position.take() {
        let new_center = chunk_of(position);
        if new_center != chunk_stream.center {
            // The centre is mandatory client state. Do not advance internal
            // ownership/tickets if a full State queue drops it; fail the session
            // before the load/unload command instead.
            send_mandatory(
                writer,
                debug,
                compression,
                &ctx.clock,
                ClientboundPlayPacket::SetCenterChunk(SetCenterChunk::new(
                    new_center.x(),
                    new_center.z(),
                )),
            )?;
            chunk_stream.center = new_center;
        }
    }

    // Always advance the view toward full view distance from the current center,
    // whether or not a position packet moved it.
    pump_chunk_stream(ctx, writer, chunk_stream, debug, compression).await
}

/// Advances the chunk view one bounded batch toward the full view distance around
/// the stream's current center, independent of any position packet.
///
/// Diffs the `(2 * view_distance + 1)` square against the per-player loaded set,
/// sends `Unload Chunk` for any column that left the radius, and asks the driver
/// (via [`SimCommand::StreamChunks`]) to load-or-generate the columns newly in
/// range — nearest-first and capped at [`MAX_CHUNK_LOADS_PER_UPDATE`] per call (see
/// [`next_chunk_transition`]), with the remainder caught on a later pump. Driven
/// at the fixed stream cadence, so a non-moving joiner still fills out to the
/// advertised view distance while a packet flood cannot run this work once per
/// socket read.
pub(super) async fn pump_chunk_stream(
    ctx: &ConnContext,
    writer: &mut PlayWriter,
    chunk_stream: &mut ChunkStream,
    debug: &mut SessionDebug,
    compression: &CompressionState,
) -> anyhow::Result<()> {
    let clock = &ctx.clock;
    let center = chunk_stream.center;

    let desired = desired_chunks(center, chunk_stream.view_distance);
    // Both sides of the transition are bounded. The pure helper recomputes from
    // the latest centre each pump, so a superseding move discards stale planned
    // work rather than draining an old teleport's queue.
    let transition = next_chunk_transition(
        center,
        &desired,
        &chunk_stream.loaded,
        MAX_CHUNK_LOADS_PER_UPDATE,
        MAX_CHUNK_UNLOADS_PER_UPDATE,
    );
    let to_load = transition.load;
    let to_unload = transition.unload;
    if to_load.is_empty() && to_unload.is_empty() {
        return Ok(());
    }

    // Drop the departed columns from the client now (tiny packets) and from the
    // tracked set; the driver releases their tickets via the command below.
    for pos in &to_unload {
        let outcome = enqueue_traced(
            writer,
            debug,
            compression,
            clock,
            OutboundPriority::World,
            ClientboundPlayPacket::UnloadChunk(UnloadChunk::new(pos.z(), pos.x())),
        );
        // Count only unloads that actually reach the queue. The column always
        // leaves the tracked set, though: the driver releases its ticket via the
        // `StreamChunks` command below regardless of whether the packet queued.
        if outcome.is_enqueued() {
            ctx.metrics.incr_chunk_unloaded(1);
        }
        chunk_stream.loaded.remove(pos);
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    ctx.commands
        .send(SimCommand::StreamChunks {
            load: to_load,
            unload: to_unload,
            reply: reply_tx,
        })
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver is gone"))?;
    let packets = reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("simulation driver dropped the chunk-stream reply"))?;

    // Only the chunks the driver actually built come back; record exactly those so
    // a skipped chunk is retried on a later update rather than treated as sent.
    for streamed in packets {
        let pos = ChunkPos::new(streamed.chunk.x(), streamed.chunk.z());
        // Track the column unconditionally: the driver already took a player ticket
        // for it, and `loaded` is what the disconnect/unload path uses to release
        // that ticket. Gating this on the enqueue would leak the ticket on a
        // tail-drop. Only the send *counter* is gated on a real enqueue.
        chunk_stream.loaded.insert(pos);
        let outcome = enqueue_traced(
            writer,
            debug,
            compression,
            clock,
            OutboundPriority::World,
            ClientboundPlayPacket::ChunkDataAndLight(streamed.chunk),
        );
        if outcome.is_enqueued() {
            ctx.metrics.incr_chunk_sent(1);
        }
        // Render any signs in the column right after it (chunk-enter half of the
        // sign loop), at the same World priority.
        for block_entity in streamed.block_entities {
            enqueue_traced(
                writer,
                debug,
                compression,
                clock,
                OutboundPriority::World,
                block_entity,
            );
        }
    }
    Ok(())
}

/// The chunk column the world `position` falls in (flooring so negatives land on
/// the correct column).
fn chunk_of(position: Vec3) -> ChunkPos {
    block_of(position).to_chunk_pos()
}

/// The integer block containing a validated world position.
fn block_of(position: Vec3) -> BlockPos {
    BlockPos::new(
        position.x.floor() as i32,
        position.y.floor() as i32,
        position.z.floor() as i32,
    )
}

/// The `(2 * radius + 1)` square of chunk columns centred on `center`.
///
/// Coordinates are added saturating so a centre at the edge of the coordinate
/// range can never overflow; any realistic position is exact.
fn desired_chunks(center: ChunkPos, radius: i32) -> BTreeSet<ChunkPos> {
    let mut set = BTreeSet::new();
    for dz in -radius..=radius {
        for dx in -radius..=radius {
            set.insert(ChunkPos::new(
                center.x().saturating_add(dx),
                center.z().saturating_add(dz),
            ));
        }
    }
    set
}

/// The Chebyshev (square/king-move) chunk distance between `a` and `b`.
///
/// Computed in `i64` so the difference cannot overflow at the coordinate
/// extremes.
fn chebyshev_distance(a: ChunkPos, b: ChunkPos) -> i64 {
    let dx = (i64::from(a.x()) - i64::from(b.x())).abs();
    let dz = (i64::from(a.z()) - i64::from(b.z())).abs();
    dx.max(dz)
}

/// One bounded transition toward the latest desired chunk square.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChunkTransition {
    /// Missing desired columns to acquire, nearest-first.
    load: Vec<ChunkPos>,
    /// Stale held columns to release, in deterministic coordinate order.
    unload: Vec<ChunkPos>,
}

/// Computes one bounded load/unload transition toward `desired`.
///
/// The desired set is built once by the caller. Missing columns are sorted
/// center-out for useful visual progress; stale columns already come from a
/// `BTreeSet` difference, so taking its prefix is deterministic without sorting
/// all 4,225 maximum-radius candidates. No queue is retained: every call
/// recomputes against the newest centre, which coalesces superseded teleports.
fn next_chunk_transition(
    center: ChunkPos,
    desired: &BTreeSet<ChunkPos>,
    loaded: &BTreeSet<ChunkPos>,
    load_bound: usize,
    unload_bound: usize,
) -> ChunkTransition {
    let mut to_load: Vec<ChunkPos> = desired.difference(loaded).copied().collect();
    to_load.sort_by_key(|pos| (chebyshev_distance(center, *pos), pos.x(), pos.z()));
    to_load.truncate(load_bound);
    let to_unload = loaded
        .difference(desired)
        .take(unload_bound)
        .copied()
        .collect();
    ChunkTransition {
        load: to_load,
        unload: to_unload,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn chunk_pump_makes_center_out_progress_without_a_position_packet() {
        use std::collections::BTreeSet;

        use ferrumc_math::ChunkPos;

        use super::{
            chebyshev_distance, desired_chunks, next_chunk_transition, MAX_CHUNK_LOADS_PER_UPDATE,
            MAX_CHUNK_UNLOADS_PER_UPDATE,
        };

        let center = ChunkPos::new(0, 0);
        let view_distance = 10;
        // A fresh joiner holds only the spawn batch (radius-2 square, 25 columns)
        // before sending any movement packet.
        let mut loaded: BTreeSet<ChunkPos> = desired_chunks(center, 2);

        // The first batch is the nearest ring (center-out): every column sits at the
        // closest missing Chebyshev distance — 3, just outside the spawn batch.
        let desired = desired_chunks(center, view_distance);
        let first = next_chunk_transition(
            center,
            &desired,
            &loaded,
            MAX_CHUNK_LOADS_PER_UPDATE,
            MAX_CHUNK_UNLOADS_PER_UPDATE,
        );
        assert_eq!(first.load.len(), MAX_CHUNK_LOADS_PER_UPDATE);
        assert!(first.unload.is_empty());
        assert!(first
            .load
            .iter()
            .all(|pos| chebyshev_distance(center, *pos) == 3));

        // Driving the pump repeatedly — with NO position packet ever — fills the
        // whole advertised view square. Each call simulates the driver loading the
        // returned batch.
        let mut iterations = 0;
        loop {
            let transition = next_chunk_transition(
                center,
                &desired,
                &loaded,
                MAX_CHUNK_LOADS_PER_UPDATE,
                MAX_CHUNK_UNLOADS_PER_UPDATE,
            );
            if transition.load.is_empty() {
                break;
            }
            assert!(transition.unload.is_empty());
            for pos in transition.load {
                loaded.insert(pos);
            }
            iterations += 1;
            assert!(iterations < 1000, "the pump must terminate");
        }
        assert!(
            desired.is_subset(&loaded),
            "the pump fills out to the full advertised view distance"
        );
        // 441 desired - 25 seeded = 416 columns, in bounded batches of 16 -> 26 pumps.
        assert_eq!(iterations, 26);
    }

    #[test]
    fn maximum_loaded_square_has_a_bounded_unload_batch() {
        use ferrumc_math::ChunkPos;

        use super::{
            desired_chunks, next_chunk_transition, MAX_CHUNK_LOADS_PER_UPDATE,
            MAX_CHUNK_UNLOADS_PER_UPDATE, STREAM_VIEW_DISTANCE_MAX,
        };

        let mut loaded = desired_chunks(ChunkPos::ORIGIN, STREAM_VIEW_DISTANCE_MAX);
        let superseding_center = ChunkPos::new(10_000, -10_000);
        let desired = desired_chunks(superseding_center, STREAM_VIEW_DISTANCE_MAX);

        assert_eq!(
            loaded.len(),
            65 * 65,
            "the fixture exercises the maximum streamed square"
        );
        let first = next_chunk_transition(
            superseding_center,
            &desired,
            &loaded,
            MAX_CHUNK_LOADS_PER_UPDATE,
            MAX_CHUNK_UNLOADS_PER_UPDATE,
        );
        assert_eq!(first.load.len(), MAX_CHUNK_LOADS_PER_UPDATE);
        assert_eq!(first.unload.len(), MAX_CHUNK_UNLOADS_PER_UPDATE);
        assert_eq!(
            first,
            next_chunk_transition(
                superseding_center,
                &desired,
                &loaded,
                MAX_CHUNK_LOADS_PER_UPDATE,
                MAX_CHUNK_UNLOADS_PER_UPDATE,
            ),
            "the same state always plans the same bounded transition"
        );

        let mut pumps = 0usize;
        while loaded != desired {
            let transition = next_chunk_transition(
                superseding_center,
                &desired,
                &loaded,
                MAX_CHUNK_LOADS_PER_UPDATE,
                MAX_CHUNK_UNLOADS_PER_UPDATE,
            );
            assert!(transition.load.len() <= MAX_CHUNK_LOADS_PER_UPDATE);
            assert!(transition.unload.len() <= MAX_CHUNK_UNLOADS_PER_UPDATE);
            assert!(
                transition.load.len() + transition.unload.len()
                    <= MAX_CHUNK_LOADS_PER_UPDATE + MAX_CHUNK_UNLOADS_PER_UPDATE
            );
            assert!(
                !transition.load.is_empty() || !transition.unload.is_empty(),
                "a non-converged stream must make progress"
            );
            for pos in transition.unload {
                assert!(loaded.remove(&pos), "only held columns may be released");
            }
            for pos in transition.load {
                assert!(desired.contains(&pos), "only desired columns may load");
                assert!(loaded.insert(pos), "a load cannot duplicate a held column");
            }
            assert!(
                loaded.len() <= 65 * 65,
                "equal load/unload caps prevent ticket-set growth"
            );
            pumps += 1;
            assert!(pumps <= 265, "the bounded transition must converge");
        }
        assert_eq!(pumps, 265, "4,225 disjoint columns drain in ceil(n/16)");
    }

    #[test]
    fn superseding_center_discards_the_stale_transition() {
        use ferrumc_math::ChunkPos;

        use super::{
            desired_chunks, next_chunk_transition, MAX_CHUNK_LOADS_PER_UPDATE,
            MAX_CHUNK_UNLOADS_PER_UPDATE, STREAM_VIEW_DISTANCE_MAX,
        };

        let loaded = desired_chunks(ChunkPos::ORIGIN, STREAM_VIEW_DISTANCE_MAX);
        let stale_center = ChunkPos::new(10_000, 10_000);
        let stale_desired = desired_chunks(stale_center, STREAM_VIEW_DISTANCE_MAX);
        let stale = next_chunk_transition(
            stale_center,
            &stale_desired,
            &loaded,
            MAX_CHUNK_LOADS_PER_UPDATE,
            MAX_CHUNK_UNLOADS_PER_UPDATE,
        );
        assert!(!stale.load.is_empty() && !stale.unload.is_empty());

        // Before any stale plan is applied, a latest-wins observation returns to
        // the already-loaded origin. Recomputing has no old backlog to drain.
        let latest_desired = desired_chunks(ChunkPos::ORIGIN, STREAM_VIEW_DISTANCE_MAX);
        let latest = next_chunk_transition(
            ChunkPos::ORIGIN,
            &latest_desired,
            &loaded,
            MAX_CHUNK_LOADS_PER_UPDATE,
            MAX_CHUNK_UNLOADS_PER_UPDATE,
        );
        assert!(latest.load.is_empty());
        assert!(latest.unload.is_empty());
    }

    /// Builds a bare [`ChunkStream`] for the block-crossing test without a
    /// [`ConnContext`](super::super::context::ConnContext): only `last_move_block`
    /// matters here, so the rest is seeded empty.
    fn bare_stream() -> super::ChunkStream {
        use std::collections::BTreeSet;

        use ferrumc_math::ChunkPos;

        super::ChunkStream {
            center: ChunkPos::ORIGIN,
            view_distance: 0,
            loaded: BTreeSet::new(),
            pending_position: None,
            last_position: None,
            last_move_block: None,
        }
    }

    #[test]
    fn block_crossing_fires_only_on_a_new_block() {
        use ferrumc_math::{BlockPos, Vec3};

        let mut stream = bare_stream();

        // The first reported position only seeds the baseline; there is no prior
        // block to report a crossing from.
        assert_eq!(stream.block_crossing(Vec3::new(0.5, 64.0, 0.5)), None);

        // Sub-block jitter within the same block does not refire.
        assert_eq!(stream.block_crossing(Vec3::new(0.9, 64.2, 0.1)), None);

        // Stepping into the next block fires exactly one crossing.
        assert_eq!(
            stream.block_crossing(Vec3::new(1.1, 64.0, 0.5)),
            Some((BlockPos::new(0, 64, 0), BlockPos::new(1, 64, 0)))
        );
        // Staying put after the crossing does not refire.
        assert_eq!(stream.block_crossing(Vec3::new(1.4, 64.0, 0.6)), None);

        // Negative coordinates floor toward -inf (vanilla block space).
        assert_eq!(
            stream.block_crossing(Vec3::new(-0.1, 64.0, 0.5)),
            Some((BlockPos::new(1, 64, 0), BlockPos::new(-1, 64, 0)))
        );
    }

    #[test]
    fn pending_stream_position_is_latest_wins() {
        use ferrumc_math::Vec3;

        let mut stream = bare_stream();
        let first = Vec3::new(128.5, 64.0, 8.5);
        let second = Vec3::new(-128.5, 64.0, 8.5);
        let latest = Vec3::new(8.5, 64.0, 256.5);

        stream.observe(first);
        stream.observe(second);
        stream.observe(latest);

        assert_eq!(stream.pending_position, Some(latest));
        assert_eq!(stream.last_position(), Some(latest));
    }

    #[test]
    fn authoritative_teleport_recenters_only_when_fully_safe() {
        use ferrumc_math::Vec3;
        use ferrumc_proto::generated::play::SynchronizePlayerPosition;

        use super::mirror_server_teleport;

        let mut stream = bare_stream();
        let mut yaw = 5.0;
        let mut pitch = -5.0;
        let target = Vec3::new(128.5, 70.0, -64.5);
        let safe = SynchronizePlayerPosition::new(
            7, target.x, target.y, target.z, 0.0, 0.0, 0.0, 90.0, -30.0, 0,
        );
        mirror_server_teleport(&mut stream, &mut yaw, &mut pitch, &safe);
        assert_eq!(stream.pending_position, Some(target));
        assert_eq!(stream.last_position(), Some(target));
        assert_eq!((yaw, pitch), (90.0, -30.0));
        assert_eq!(
            stream.block_crossing(Vec3::new(128.9, 70.1, -64.1)),
            None,
            "a client echo in the destination block is not a giant move event",
        );
        assert_eq!(
            stream.block_crossing(Vec3::new(129.1, 70.1, -64.1)),
            Some((
                ferrumc_math::BlockPos::new(128, 70, -65),
                ferrumc_math::BlockPos::new(129, 70, -65),
            )),
            "the next real block crossing starts at the teleport destination",
        );

        let invalid = [
            SynchronizePlayerPosition::new(8, f64::NAN, 64.0, 8.0, 0.0, 0.0, 0.0, 10.0, 20.0, 0),
            SynchronizePlayerPosition::new(
                9,
                8.0,
                64.0,
                8.0,
                0.0,
                0.0,
                0.0,
                f32::INFINITY,
                20.0,
                0,
            ),
            SynchronizePlayerPosition::new(10, 8.0, 64.0, 8.0, 0.0, 0.0, 0.0, 10.0, 20.0, 1),
        ];
        for sync in invalid {
            mirror_server_teleport(&mut stream, &mut yaw, &mut pitch, &sync);
            assert_eq!(stream.pending_position, Some(target));
            assert_eq!(stream.last_position(), Some(target));
            assert_eq!((yaw, pitch), (90.0, -30.0));
        }
    }
}
