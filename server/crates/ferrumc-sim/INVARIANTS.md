# Invariants: ferrumc-sim

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- All public items have rustdoc.
- Error types classify the failure mode.

## Crate-Specific

- Inputs are applied **only** at tick boundaries (inside `SimShard::run_tick`),
  never on `enqueue`. Enqueue must not mutate shard state.
- The tick counter advances by exactly one per `TickCoordinator::advance`. There
  are **no catch-up ticks**: lag is reported, never replayed.
- The simulation holds no sockets and no database handles, and reads no wall
  clock in its step API. Stepping is fully deterministic.
- The shard inbox is bounded. When full it rejects with `SimError::InboxFull`;
  it never blocks and never silently drops.
- Player state is held in ordered containers so output ordering is deterministic
  for identical input sequences.
- A chunk is resident in a `LoadedChunkMap` **iff** it holds at least one
  ticket. Acquiring the first ticket runs the load-or-generate flow; releasing
  the last ticket unloads the chunk.
- The simulation owns chunk *data* but never a database handle: a `WorldStore`
  is borrowed per `acquire` call, never stored in the map or shard.
- Dirty chunks are *collected* for saving (`take_dirty`, and on unload) but
  never persisted here, and no flush policy is implemented — that is the
  caller's concern.
- Chunk loading selects a stored full `ChunkRecord` when present, otherwise a
  deterministic generated base, and then applies the stored overlay. Overlay
  sections replace only the sections they carry; an empty overlay is a no-op.
  A non-empty schema-v3 overlay carries the complete block-entity snapshot, so
  removals persist while imported entities survive legacy and empty overlays.
- Resident chunks and their tickets live in ordered containers (`BTreeMap`), so
  the resident set, its iteration order, and dirty batches are deterministic for
  identical acquire/release/mutate sequences.
- Logical ownership regions are fixed, world/dimension-scoped 8×8 chunk cells.
  Every typed block/chunk coordinate partitions to exactly one cell; arbitrary
  `ShardPos` values that cannot describe a complete cell are rejected.
- `ShardId` canonically includes world, dimension, and `ShardPos`; each id names
  exactly one logical region. Workers/endpoints are separate runtime concepts
  and may process or route multiple shard ids.
- A logical region may have at most one ownership claim.
- Shard lifecycle is strictly `Created -> Active -> Draining -> Stopped`.
  Rejected transitions leave the prior state unchanged.
- The scheduler defaults to the existing one-shard inline path. Its multi-shard
  worker mode is crate-internal, explicitly selected, and not wired into the
  application.
- A shadow tick visits each runnable shard exactly once in canonical `ShardId`
  order. Persistent workers receive owned, disjoint shard batches through
  capacity-one command/result channels; every shard is returned before the next
  tick, and worker completion order never changes output publication order.
- Only `Active` shards accept new scheduler input. `Draining` shards continue
  ticking admitted work, reject new input, and cannot become `Stopped` until
  their tick work is quiescent.
- A worker failure after dispatch permanently poisons that scheduler. A partial
  tick is fail-stop and must never be retried.
- Cross-shard workers emit destination-only intents; only the scheduler may
  stamp their source, source-local sequence, and completed tick. Workers never
  access or send directly to another shard.
- Accepted cross-shard envelopes produced in tick N apply exactly once as a
  separate prefix to the destination's tick N+1 execution. They never compete
  with the ordinary shard inbox. The prefix can enter `SimShard` only through
  an unforgeable scheduler-owned tick capability; sibling modules have no
  constructor or mid-tick mutation path.
- Central admission is bounded (1,024 envelopes by default), nonblocking, and
  reject-newest after sorting by destination `ShardId`, source `ShardId`, then
  source-local FIFO sequence. Rejections return the intact owned envelope.
- Boundary preparation is non-mutating. Tick overflow or pre-dispatch failure
  leaves ready envelopes queued; a successful tick commits exactly the
  prepared scheduler metadata identities, never floating-point payload
  equality. A draining destination cannot stop while an admitted envelope still
  targets it.
- Before a multi-shard tick advances, every resident full `ChunkKey`
  (world/dimension/typed chunk position) must occur in exactly one registered
  shard container. The scan includes clean chunks, so aliases cannot become
  competing persist records on a later tick; conflicts leave tick, inbox, and
  dirty state unchanged. Spatial region membership is not enforced by this
  packet; unique out-of-region residency remains valid until that separate
  architecture step lands.
- A successful scheduler tick transfers nonempty per-shard
  `ChunkOverlayRecord` batches in canonical `ShardId`/`ChunkPos` order only
  after every fallible worker and cross-shard phase succeeds. Each batch is
  capped at storage's `MAX_SAVE_BATCH` (4,096); a canonical overflow tail stays
  persist-dirty and is emitted by a later tick without blocking or loss. A
  deterministic per-map continuation cursor visits that tail before wrapping,
  so repeatedly re-dirtying lower chunk keys cannot starve it.
- Persist output is collected from every registered lifecycle state, including
  a stopped shard with a deferred tail. Clearing a dirty mask transfers
  ownership only into the move-owned scheduler outcome; the simulation still
  holds no database handle and mutation-journal output remains a separate
  bounded seam.
