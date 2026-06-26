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
  `try_send`, so the router never blocks the tick loop. A full channel is a
  classified `SessionError`, never a silent drop.
- The player<->shard mapping is the single source of truth for player location.
  `disconnect_player` removes the mapping before anything else, so cleanup holds
  even if the despawn notice cannot be delivered.
- Translation (`net_event_to_input`, `output_to_clientbound`,
  `shard_for_position`) is pure: no channels, maps, or I/O.
