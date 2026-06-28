# FerrumC — Independent Black-Box Protocol Smoke Test

A protocol gate for the FerrumC v2 Minecraft server (Java **1.21.8 / protocol
772**) driven by a client we did **not** write:
[PrismarineJS `node-minecraft-protocol`](https://github.com/PrismarineJS/node-minecraft-protocol),
whose bundled `minecraft-data` supports 1.21.8.

## Why an independent client matters (the hard lesson)

We previously gated protocol work with a **self-written fake client**. It passed.
Green across the board. Then a real vanilla Minecraft client connected and
**rejected our packets** — stuck on "Loading terrain", silent disconnects, the
works.

The reason is obvious in hindsight: when the same codebase produces both the
server's bytes *and* the test client's expectations, the test only proves the
code agrees with itself. Any shared bug — a wrong field order, a missing flag, an
off-by-one VarInt — is invisible, because both sides make the identical mistake.
That is not a test; it is a mirror. It manufactures **false confidence**.

`node-minecraft-protocol` is a third-party implementation maintained by a
different community, with its own independent decode/encode of the 1.21.8 wire
format (via `minecraft-data`). If FerrumC's bytes are wrong, this client
complains — exactly like a real player's client would. That is the entire point:
**the test oracle must be independent of the system under test.**

The same independence applies to **block state**: placed-block state ids are
decoded with `prismarine-block`, and streamed chunks are parsed with
`prismarine-chunk` — both third-party PrismarineJS libraries. So when the test
asserts "the log the server placed has `axis=x`" or "the edit is still there
after a restart", that fact is read back through a decoder we did not write.

## What it checks

The smoke (`smoke.mjs`) runs the full alpha flow end to end:

1. **Status ping** — asserts the server reports `protocol === 772` and a version
   string containing `1.21.8`.
2. **Offline-mode login** — completes handshake -> login -> configuration and
   reaches the **`play`** state (join-game received).
3. **First chunk(s)** — waits for the first chunk column(s) to stream in.
4. **Movement across a chunk boundary** — walks in small, server-acceptable
   steps and asserts it observes **both** new chunk **loads** (leading edge) and
   chunk **unloads** (trailing edge).
5. **Creative set-slot** — `set_creative_slot` to put `oak_log` / `oak_slab` /
   `oak_stairs` into the hotbar.
6. **Stateful block placement** — places blocks whose **block-state depends on
   how you place them**, and asserts the `block_change` + sequence
   `acknowledge_player_digging` the server returns carry the expected state id:
   - **log** — `axis` from the clicked face (east face -> `axis=x`, state 136),
   - **slab** — `type` from the cursor height on a side face (`cursorY=0.9` ->
     `type=top`, state 12052),
   - **stairs** — `facing` from the player's yaw (`yaw=0` -> `facing=south`),
     `half` from the cursor (`half=bottom`, state 2969).
7. **Block break** — `block_dig` (creative insta-mine) and asserts the
   `block_change` to **air** + the sequence ack.
8. **Cross-chunk edit** — walks into a non-spawn chunk, places a block, confirms.
9. **Disconnect + reconnect** — rejoins and verifies every placed/broken block
   streams back in (read out of the parsed chunks).
10. **Restart persistence (the kill-shot)** — **stops the server process**,
    starts it again on the **same world dir**, reconnects, and verifies the whole
    pattern survived — i.e. it was flushed to redb and read back, not just held
    in memory. *(Managed mode only — see below.)*

It exits `0` on success and non-zero with a clear diagnostic dump on failure.
Along the way it does the housekeeping a real client must do to stay connected:
replies to keep-alives and confirms server teleports.

## Two modes

- **Managed** (`MC_MANAGE_SERVER=1`, the `run-smoke.sh` default): the script owns
  the server lifecycle. It spawns the **built binary directly** (not `cargo run`,
  so a `SIGINT` reaches the server and triggers its graceful flush-on-shutdown),
  runs steps 1–9, then **stops and restarts** the process for step 10. Uses a
  fresh world under a temp dir, on port `25599` by default.
- **External** (`MC_MANAGE_SERVER=0`, the default when you run `node smoke.mjs`
  by hand): runs steps 1–9 against a server **you** started, on port `25565` by
  default. Step 10 is **skipped** — we can't restart a process we don't own.

> **Stop signal:** FerrumC handles `ctrl_c` / **SIGINT** only for graceful
> shutdown (it flushes pending edits on the way out). `SIGTERM`/`SIGKILL` skip
> that flush, so the managed restart uses SIGINT. A hard-kill (SIGKILL) crash-
> durability check is **not automated** — it depends on redb commit timing and
> would be flaky; verify it by hand if you need that guarantee.

## Running it locally

Prereqs: Node >= 18, `pnpm`, and (for managed mode) a Rust toolchain.

```bash
cd tests/blackbox

# Managed (default): builds the server, then spawns/stops/restarts it and runs
# the full scenario including restart-persistence.
./run-smoke.sh

# External: start a server yourself, then point the smoke at it.
cargo run                                   # from the server/ workspace
MC_MANAGE_SERVER=0 ./run-smoke.sh 127.0.0.1 25565
# or directly:
MC_MANAGE_SERVER=0 node smoke.mjs 127.0.0.1 25565
```

Useful env knobs: `MC_HOST`, `MC_PORT`, `MC_USERNAME`, `MC_MOVE_BLOCKS`,
`MC_STEP_BLOCKS`, `MC_STEP_MS`, `MC_SERVER_BIN` (managed: path to the server
binary), `MC_RUN_DIR` (managed: where the temp world/config/logs go),
`MC_KEEP_RUN=1` (managed: don't wipe the run dir at start), `RUST_LOG`.

Every step prints a `[PASS]` / `[FAIL]` / `[SKIP]` line; a managed run's server
logs land in `$MC_RUN_DIR/server-*.log` (default under the OS temp dir).

## Status

| Step                              | State        |
|-----------------------------------|--------------|
| status ping (proto/version)       | implemented  |
| offline login -> play             | implemented  |
| first chunk(s)                    | implemented  |
| move across chunk boundary        | implemented  |
| chunk load + unload observed      | implemented  |
| set creative-mode slot            | implemented  |
| select hotbar slot                | implemented  |
| place stateful block (log/slab/stairs) | implemented |
| assert placed block-state id      | implemented  |
| break block (-> air) + ack        | implemented  |
| cross-chunk edit                  | implemented  |
| disconnect / reconnect            | implemented  |
| verify persistence after restart  | implemented (managed mode) |

## Files

- `package.json` — pins `minecraft-protocol` + `minecraft-data` (the wire client)
  and `prismarine-registry` / `prismarine-chunk` / `prismarine-block` / `vec3`
  (the independent block-state + chunk oracle). `type: module`.
- `smoke.mjs` — the smoke test (full scenario + managed server lifecycle).
- `run-smoke.sh` — runner; managed by default (builds the server, runs the full
  scenario incl. restart); `MC_MANAGE_SERVER=0` for an external server.
- `README.md` — this file.
