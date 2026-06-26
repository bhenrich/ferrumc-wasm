# ADR-0007: redb as the Storage Backend Implementation

**Status:** Accepted (benchmark deferred)
**Date:** 2026-06-27
**Supersedes:** none (refines ADR-0004)

## Context

ADR-0004 established that storage sits behind trait boundaries
(`WorldStore`, `PlayerStore`, `PluginStore`) and named redb as the starting
point pending a benchmark against LMDB. The traits and an in-memory backend
(M16) already exist. This milestone (M18) needs a real durable backend behind
those same traits.

## Decision

Implement the durable backend with **redb**, the project default per `CLAUDE.md`
("Current backend: redb"). The implementation is `ferrumc_storage::RedbStore`.

The formal redb-vs-LMDB benchmark from ADR-0004 (chunk load throughput under
concurrent player movement plus periodic dirty-chunk flushes) is **deferred**.
It can revisit this choice later without touching any caller, because everything
above storage depends only on the traits, not on `RedbStore`.

## Why redb (for now)

- Pure Rust, embedded, ACID, crash-safe. No native/C dependency, no build
  toolchain surprises.
- Simple `BTreeMap`-style API over copy-on-write B-trees; MVCC readers do not
  block the single writer.
- Matches the storage model's single-writer + batched-write design directly.

LMDB (heed) remains the main alternative if the benchmark later favours
memory-mapped zero-copy reads; the trait boundary keeps that swap cheap.

## Implementation notes

- One table per category (`ferrumc:chunk`, `ferrumc:entity`, `ferrumc:player`,
  `ferrumc:plugin`) plus a `ferrumc:meta` table holding an on-disk **format
  version**, checked on open so an incompatible file is rejected rather than
  misread. This is separate from the per-record `SchemaVersion`.
- redb stores raw `&[u8]` keys/values; a private `codec` module is the only
  place that encodes typed keys and versioned records to bytes and back, and
  validates every length on read (no panics on corrupt bytes).
- Plugin keys are namespaced as `plugin_len ++ plugin_id ++ key`, so plugins are
  isolated and one plugin's keys enumerate via a single prefix range scan.
- redb transactions are synchronous and blocking, so every async trait method
  runs the transaction inside `tokio::task::spawn_blocking` — DB work never runs
  on an async executor worker. Batched saves commit in one transaction.
- The raw `redb::Database` handle is private (`Arc<Database>`) and never escapes
  the crate; no caller or plugin can obtain a handle or transaction.

## Consequences

- A durable backend exists behind the unchanged M16 traits; `InMemoryStore`
  stays for tests and the test harness.
- The LMDB benchmark and any backend swap are still open and cheap, as designed.
- The byte `codec` is an internal format; changing it bumps `STORE_FORMAT_VERSION`
  and requires a migration path before shipping persisted worlds.
