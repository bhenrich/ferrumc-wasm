# Public Alpha — "CreativeCore v0" Finish Line

> **North star:** a deterministic, Rust-native, observable creative/minigame server core for vanilla 1.21.8 clients.
> **Public alpha = every checkbox below is green.** Each future workstream closes one or more of these.

Status legend: `[x]` verified done (cross-checked against [FEATURES.md](FEATURES.md)) · `[ ]` not done / in flight.

---

## User-visible

- [ ] Fresh clone builds
- [x] Running with no config starts safely (no panic)
- [x] Vanilla 1.21.8 joins in offline mode
- [x] Two clients see each other
- [x] Two clients see movement/head rotation
- [x] Two clients see block changes
- [x] Other clients see held main-hand item
- [x] Creative hotbar placement works
- [x] Basic block states work: logs, slabs, stairs, torches, fences
- [x] Leave/rejoin preserves placed blocks
- [x] Restart preserves placed blocks
- [x] Plugin Replace visibly works
- [x] Dashboard opens locally

## Correctness

- [ ] Strict raw-wire trace for critical packets
- [ ] Real-client smoke checklist documented
- [ ] Persistence tests cover pinned chunks
- [ ] Dirty state/journal path has metrics
- [ ] Reject/resync behavior tested for invalid placement
- [x] No unbounded channels
- [x] No unwrap outside tests
- [x] xtask generate --check green

## Performance

- [ ] Startup benchmark
- [ ] Join storm benchmark
- [ ] Movement benchmark
- [ ] Active builder benchmark
- [ ] Plugin overhead benchmark
- [ ] Results include commit SHA + hardware

## Honesty (README must say)

- [x] Flat-only
- [x] Offline-only
- [x] No survival yet
- [x] No full vanilla parity
- [x] Roadmap: Anvil import/export, online mode, lighting next
