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
