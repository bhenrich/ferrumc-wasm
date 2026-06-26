# ADR-0002: Actor-Sharded Simulation (Not Bevy ECS)

**Status:** Accepted
**Date:** 2026-02-16

## Context

v1 uses Bevy ECS as the core game state manager. While Bevy is excellent software, using it as the core of a networked Minecraft server creates specific problems.

## Decision

Use actor-style simulation shards (8×8 chunks each) with ECS-inspired internal storage (SlotMap + ComponentVec), not Bevy ECS.

## Why Not Bevy ECS

1. **Global world vs shard ownership.** Bevy has one `World`. We need each shard to exclusively own its chunks/entities for lock-free parallel ticking. Multiple Bevy worlds fights the framework.
2. **Implicit scheduling.** Bevy's system scheduler determines ordering by resource access. Minecraft needs deterministic tick ordering. Constraining Bevy's scheduler costs more than explicit function calls.
3. **Plugin boundary leakage.** Bevy ECS as core means plugins want `Query<>` access. That exposes internals through the API, making future changes impossible without breaking plugins.
4. **AI agent complexity.** `fn movement_system(ctx: &mut ShardTickCtx)` is trivially parseable. Bevy's system parameter extraction has implicit rules agents get subtly wrong.
5. **Wrong problem shape.** Minecraft's workload is spatial (chunks, block neighbors, entity proximity), not component-query-heavy. ECS columnar iteration isn't the bottleneck.

## What We Keep

ECS-inspired data layout inside each shard. `SlotMap<EntityId, EntityMeta>` + `ComponentVec<Position>` etc. Cache-friendly storage without the framework overhead.

## Consequences

- No Bevy ecosystem benefits (no bevy_rapier, bevy_ui, etc.) — acceptable since we're building a server, not a game engine
- Must implement our own tick scheduling — it's ~50 lines of explicit function calls
- Systems are plain `fn(ctx: &mut ShardTickCtx)` — boring, predictable, testable
