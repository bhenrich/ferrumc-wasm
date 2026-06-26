# Experiments

> **Rule: No architectural commitment without benchmarks.**
>
> Each experiment below is a question that must be answered with real numbers
> before we commit to an implementation. Run each on the VPS (1 vCPU, 1GB RAM)
> and the dev machine (i7-13700K, 32GB, RTX 4070 Super) to see both constrained
> and unconstrained performance.

## Status

| # | Experiment | Status | Winner | Notes |
|---|-----------|--------|--------|-------|
| 1 | Storage: redb vs LMDB | ⬜ Not started | — | |
| 2 | Chunk compression: zstd vs lz4 | ⬜ Not started | — | |
| 3 | Protocol compression: zlib levels | ⬜ Not started | — | |
| 4 | Memory allocator: system vs jemalloc vs mimalloc | ⬜ Not started | — | |
| 5 | Spatial index: chunk-bucket vs grid vs BVH | ⬜ Not started | — | |
| 6 | Entity storage: SlotMap vs Vec+freelist vs Arena | ⬜ Not started | — | |
| 7 | Packet encoding: pre-encode vs per-player | ⬜ Not started | — | |
| 8 | Shard size: 4×4 vs 8×8 vs 16×16 | ⬜ Not started | — | |
| 9 | VarInt decoding: branchless vs loop vs LUT | ⬜ Not started | — | |
| 10 | Channel impl: tokio mpsc vs crossbeam vs flume | ⬜ Not started | — | |
| 11 | NBT parsing: streaming vs tree-build | ⬜ Not started | — | |
| 12 | Chunk palette: threshold for single→indirect→direct | ⬜ Not started | — | |

## Methodology

Each experiment follows the same pattern:

1. **Isolate the variable.** Test ONE thing. Same data, same hardware, same Rust version.
2. **Use Criterion.** `cargo bench -p ferrumc-bench --bench <name>`. HTML reports in `target/criterion/`.
3. **Realistic workloads.** Use actual Minecraft data (chunk records, NBT blobs, protocol packets) from `fixtures/`.
4. **Both machines.** VPS shows constrained behavior. Dev machine shows peak throughput.
5. **Document everything.** Results, methodology, and decision go in `docs/experiments/<name>.md`.
6. **Commit the benchmark code.** Benchmarks live in `crates/ferrumc-bench/benches/`.

## Experiment Details

### 1. Storage: redb vs LMDB

**Question:** Which embedded DB gives better chunk load/save throughput for our access pattern?

**Test workload:**
- Load 10,000 chunks sequentially (cold cache)
- Load 10,000 chunks randomly (simulating player movement)
- Save 1,000 dirty chunks in batches of 50
- Concurrent reads during batch writes
- Measure: throughput, p50/p99 latency, memory usage, file size

**Key difference:** redb is pure Rust, simpler API. LMDB has zero-copy reads via memory mapping.

### 2. Chunk compression: zstd vs lz4

**Question:** zstd (better ratio) or lz4 (faster) for on-disk chunk records?

**Test workload:**
- Compress/decompress 10,000 real chunk records
- Vary zstd levels (1, 3, 6)
- Measure: compression ratio, compress throughput, decompress throughput
- Consider: chunks are read-heavy (decompress matters more than compress)

### 3. Protocol compression: zlib levels

**Question:** What zlib compression level balances CPU vs bandwidth for protocol packets?

**Test workload:**
- Compress 10,000 outbound packets of varying sizes
- Vary levels (1, 3, 6)
- Measure: compression ratio, CPU time per packet, bandwidth savings
- Also test: what threshold (64, 128, 256, 512 bytes) minimizes total CPU+bandwidth cost?

### 4. Memory allocator

**Question:** Does jemalloc or mimalloc meaningfully improve throughput or reduce fragmentation?

**Test workload:**
- Simulate 200 players joining/moving/leaving over 5 minutes
- Measure: peak RSS, allocation rate, p99 latency per tick
- This experiment requires the simulation to be somewhat functional

### 5-12: Similar structure

Each needs a specific question, realistic workload, and clear metrics. Fill in details when the relevant crate is ready to benchmark.

## Template for Results

```markdown
# Experiment N: <Title>

## Question
<One sentence>

## Setup
- Hardware: <machine>
- Rust version: <version>
- Date: <date>
- Commit: <hash>

## Results

| Variant | Throughput | p50 | p99 | Memory | Notes |
|---------|-----------|-----|-----|--------|-------|
| A       |           |     |     |        |       |
| B       |           |     |     |        |       |

## Analysis
<What do the numbers mean?>

## Decision
<Which one and why?>

## Benchmark Code
`crates/ferrumc-bench/benches/<name>.rs`
```
