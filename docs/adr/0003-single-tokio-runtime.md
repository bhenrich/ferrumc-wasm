# ADR-0003: Single Tokio Multi-Thread Runtime

**Status:** Accepted
**Date:** 2026-02-16

## Context

Options: single Tokio multi-thread runtime, per-core Tokio runtimes, or io_uring-based custom runtime.

## Decision

One `tokio::runtime::Builder::new_multi_thread()` runtime for all network I/O and async orchestration. Explicit worker thread count from config.

## Rationale

- Per-core runtimes add complexity without proven benefit at our scale
- Tokio's work-stealing already distributes load across cores
- Single runtime is simpler to reason about, debug, and profile
- If profiling later shows network I/O is the bottleneck (unlikely before 1000+ players), per-core runtimes can be added without changing sim/storage

## Consequences

- Tokio handles ONLY network I/O and async control flow
- Simulation runs on dedicated thread pool (NOT Tokio tasks)
- Storage runs on dedicated worker thread (NOT Tokio tasks)
- No `spawn_blocking` for game logic — that goes to sim workers
