# ADR-0004: Storage Backend (Pending Benchmarks)

**Status:** Accepted (implementation choice pending experiment)
**Date:** 2026-02-16

## Context

Need an embedded database for chunk, entity, and player persistence. Candidates: redb, LMDB (heed), RocksDB, custom region files.

## Decision

Build storage behind trait boundaries (`WorldStore`, `PlayerStore`, `PluginStore`). Start with redb. Benchmark against LMDB before committing.

## Candidates

**redb:** Pure Rust, embedded, ACID, crash-safe. Single writer. Simple API. No native deps.

**LMDB (heed):** Battle-tested, memory-mapped zero-copy reads, MVCC concurrent reads. Single writer. C dependency. Requires upfront map_size config.

**RocksDB:** Overkill. Heavy C++ dependency. Complex tuning. Not justified at our scale.

**Custom region files:** Fun but premature. Build it after proving the access patterns with an existing DB.

## Key Experiment

See `docs/experiments/README.md` experiment #1. The benchmark that decides: chunk load throughput under concurrent player movement + periodic dirty chunk flushes.

## Consequences

- Storage trait layer is the API contract — backend swappable without touching sim/net
- No DB transaction on sim or net threads — dedicated storage worker only
- Both redb and LMDB have single-writer constraint — embrace with batched writes
