# ADR-0001: Clean Rewrite

**Status:** Accepted
**Date:** 2026-02-16

## Context

FerrumC v1 accumulated ~2 years of tech debt: mixed architectural patterns, Bevy ECS used as a global game state holder, LMDB/heed storage tightly coupled to simulation, DashMap as a design substitute for ownership, inconsistent crate boundaries, and code patterns that make AI-agent development unreliable.

## Decision

Clean rewrite in the same repo on a new branch (`rework/ferrumc-v2`). Delete all source files and rebuild from scratch with proper architecture. The old codebase remains on `master` as reference.

## What We Keep From v1

- Packet/data generation concepts and fixtures
- Codec edge-case tests
- NBT parsing logic (after adding bounds/fuzzing)
- Anvil chunk format knowledge (as reference for import/export)
- Generated registry data (as a seed — regenerate for v2)

## What We Drop

- Bevy ECS as core architecture → actor-sharded simulation
- LMDB/heed storage → trait-based storage with redb (pending benchmarks)
- DashMap chunk cache → shard-owned chunk maps
- Global shared state patterns → message passing
- Basic scheduler → tick coordinator with backpressure

## Consequences

- Months of work before feature parity with v1
- Risk of "v2 never ships" → mitigate with vertical slice milestones
- Can't accept v1 PRs during transition → communicate to community
- Clean architecture enables AI-agent parallel development
- Proper crate boundaries prevent cross-cutting merge conflicts
