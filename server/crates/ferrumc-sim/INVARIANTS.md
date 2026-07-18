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
  records must use exactly the current schema; old and future schemas are
  refused as `IncompatiblePreAlphaData`. Overlay sections replace only the
  sections they carry; an empty overlay is a no-op. A non-empty current overlay
  carries the complete block-entity snapshot, so removals persist while imported
  entities survive empty overlays.
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
