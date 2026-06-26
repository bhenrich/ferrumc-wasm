# FerrumC v2 — Architecture Overview

> This is the reference document. If something here conflicts with code, fix the code.

## Design Philosophy

**Boring, strict, layered, measured.**

- **Boring**: plain functions over framework magic. Explicit over implicit. If a junior dev can't understand the control flow, it's too clever.
- **Strict**: hostile-client-safe from day one. Every input is bounded. Every queue has backpressure. Every error is classified.
- **Layered**: five hard-separated layers. No shortcuts. Network doesn't touch world state. Simulation doesn't touch sockets.
- **Measured**: no architectural commitment without benchmarks. Experiments before implementation.

## System Diagram

```
┌──────────────────────────────────────────────────────────────┐
│ ferrumc-app                                                   │
│ Config · Service wiring · Startup/shutdown · Metrics export   │
└──────────────────┬───────────────────────────────────────────┘
                   │
┌──────────────────▼───────────────────────────────────────────┐
│ Network Edge (ferrumc-net)                                    │
│ Tokio TCP · Frame codec · Encryption · Compression · Decode  │
│ Hostile-client protection · Connection lifecycle              │
└──────────────────┬───────────────────────────────────────────┘
                   │ typed, bounded messages
┌──────────────────▼───────────────────────────────────────────┐
│ Session Router (ferrumc-session)                              │
│ Player↔Shard mapping · Packet budgets · State routing        │
│ NetEvent → GameInput · GameOutput → ClientboundPacket        │
└──────────────────┬───────────────────────────────────────────┘
                   │ tick-stamped game inputs
┌──────────────────▼───────────────────────────────────────────┐
│ Simulation (ferrumc-sim)                                      │
│ Tick coordinator · Shard workers (8×8 chunks each)           │
│ Chunks · Entities · Events · Plugin dispatch                 │
│ Deterministic ordering · Cross-shard messages at tick bounds  │
└──────────────────┬───────────────────────────────────────────┘
                   │ async load/save requests
┌──────────────────▼───────────────────────────────────────────┐
│ Storage (ferrumc-storage)                                     │
│ Trait-based (WorldStore, PlayerStore, PluginStore)            │
│ Dedicated worker thread · Batched transactions               │
│ Backend: redb (may switch to LMDB after benchmarks)          │
└──────────────────────────────────────────────────────────────┘
```

## Threading Model

Three executor domains. No mixing.

| Domain | Technology | Owns | Never does |
|--------|-----------|------|-----------|
| Network | Tokio multi-thread runtime | TCP sockets, codec state, connection lifecycle | Mutate world, hold DB handles |
| Simulation | Dedicated thread pool | Chunks, entities, spatial indexes, plugin dispatch | Touch sockets, run DB transactions |
| Storage | Dedicated worker thread(s) | Database handles, transactions | Run on sim or net threads |

## Data Flow

### Inbound (player action → world change)

```
TcpStream → ConnectionReader → RawFrame → ServerboundPacket
  → NetEvent → SessionRouter → GameInput → ShardInbox
  → SimShard processes during tick → world state changes
```

### Outbound (world change → player sees it)

```
ShardOutput → SessionRouter → ClientboundPacket
  → OutboundFrame (priority queued) → ConnectionWriter
  → batch → compress → encrypt → TcpStream
```

### Storage (async, off main paths)

```
SimShard (dirty chunk) → StorageRequest → bounded channel
  → StorageWorker → redb transaction → response oneshot
```

## Simulation Model

Actor-sharded. Each shard = 8×8 chunks with exclusive ownership.

**Why 8×8:**
- Smaller than storage regions (32×32) → better load distribution
- Large enough for local interactions (entity collisions, block updates)
- Simple chunk→shard mapping: `shard_x = chunk_x.div_euclid(8)`, same for z
- Clean adjacent-shard query window

**Tick flow (20 TPS):**
1. Drain network inputs → shard inboxes
2. Deliver storage completions
3. Queue scheduled plugin tasks
4. Run active shards in parallel
5. Collect cross-shard messages (applied next tick)
6. Route outputs → network
7. Queue dirty data → storage
8. Emit metrics

**Entity ownership:** one shard, always. Transfer at tick boundary only.

## Plugin Model

Capability-based. Plugins declare what they need, server config grants/denies.

**Plugins get:** `WorldView` (read-only), `PlayerApi`, `CommandSink` (intents), `PermissionApi`, `PluginStorageApi`

**Plugins never get:** raw sim internals, raw chunks, sockets, DB handles, Tokio runtime

**Phases:**
- A (dev): compiled-in for API iteration
- A (ship): dynamic libraries from /plugins/ folder via C ABI
- B (later): WASM for language-agnostic plugins

## Key Design Decisions

See `docs/adr/` for rationale on each:

| Decision | Chosen | Rejected | ADR |
|----------|--------|----------|-----|
| Clean rewrite vs refactor | Rewrite | Incremental refactor | 0001 |
| Simulation model | Actor-sharded | Global Bevy ECS | 0002 |
| Networking runtime | Single Tokio multi-thread | Per-core runtimes | 0003 |
| Storage backend | redb (pending benchmarks) | LMDB, RocksDB, custom | 0004 |
| Plugin phase 1 | Compiled Rust | WASM, scripting | 0005 |
| Plugin ABI | C ABI + opaque handles | Rust native (unstable ABI) | 0006 |

## Open Experiments

These must be resolved with benchmarks before committing:

See `docs/experiments/README.md` for the full list and methodology.

## Config Format

TOML. Parsed with `serde` + `toml` crate. Schema defined in `ferrumc-config`.

## Protocol Target

Minecraft Java Edition 1.21.8 (protocol version 772). Generated packet code. Pinned, not auto-following.
