# Invariants: ferrumc-session

> Rules that must hold for all code in this crate. Violating these is a bug.

## General

- No `unwrap()` or `expect()` outside `#[cfg(test)]`.
- No unbounded channels or allocations from untrusted input.
- All public items have rustdoc.
- Error types classify the failure mode.

## Crate-Specific

- The router exchanges only `GameInput`/`GameOutput`/`ClientboundPlayPacket`
  messages. It never holds a `SimShard`, a `Chunk`, a socket, or a DB handle.
- All transport is bounded `tokio::sync::mpsc`; routing uses non-blocking
  `try_send`, so the router never blocks the tick loop. Ordinary data stops at a
  fixed reserved tail for join/leave/reject control traffic. A rejected input is
  returned with its typed delivery policy and classified `SessionError`, never
  silently dropped.
- A lifecycle leave rejected during a slow-client cascade remains represented in
  a player-bounded pending-disconnect set until an explicit retry accepts it;
  cascade errors are never reduced to a log line or discarded result.
- The player<->shard mapping is the single source of truth for player location.
  `disconnect_player` must enqueue `PlayerLeave` before removing the mapping; if
  the control lane rejects the leave, the mapping remains available for retry or
  explicit overload termination.
- Logical `ShardId` targets and runtime endpoints are distinct. Directory
  resolution is scoped by `WorldId` + `DimensionId`, prefers exact coverage over
  a world-covering fallback, and never falls through after a selected endpoint
  reports full or closed.
- Directory registration is atomic and lease-validated. Duplicate insertion
  cannot overwrite an endpoint; replace/remove requires the current generation;
  generations and registration-lineage ids are never reused after removal
  within a directory instance.
- A player binding retains one coverage slot, endpoint home, and registration
  lineage. An authorized sender rotation preserves that lineage and immediately
  serves existing sessions. Unregister/re-register creates a new lineage and
  cannot silently retarget them.
- Translation (`net_event_to_input`, `output_to_clientbound`,
  `shard_for_position`) is pure: no channels, maps, or I/O.
