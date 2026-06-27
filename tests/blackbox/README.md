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

## What it checks

The smoke (`smoke.mjs`) currently performs:

1. **Status ping** — asserts the server reports `protocol === 772` and a version
   string containing `1.21.8`.
2. **Offline-mode login** — completes handshake -> login -> configuration and
   reaches the **`play`** state (join-game received).
3. **First chunk(s)** — waits for the first chunk column(s) to stream in.
4. **Movement across a chunk boundary** — walks ~3 chunks in small, server-
   acceptable steps and asserts it observes **both** new chunk **loads** (leading
   edge) and chunk **unloads** (trailing edge).

It exits `0` on success and non-zero with a clear diagnostic dump on failure.
Along the way it does the housekeeping a real client must do to stay connected:
replies to keep-alives and confirms server teleports.

## Running it locally

Prereqs: Node >= 18 and `pnpm`.

```bash
cd core/tests/blackbox
pnpm install            # one-time; pulls node-minecraft-protocol + minecraft-data
```

Start a FerrumC server yourself on some port (this harness deliberately does NOT
start it — that keeps server lifecycle, and any cargo build locks, out of the
test path). From the repo root:

```bash
# example only — adjust to however FerrumC takes a port:
cargo run --release -- --port 25565
```

Then, in another shell, run the smoke against it:

```bash
# defaults to 127.0.0.1:25565
node smoke.mjs

# or explicit host/port (argv or env):
node smoke.mjs 127.0.0.1 25565
MC_HOST=127.0.0.1 MC_PORT=25565 node smoke.mjs

# or via the runner (installs deps if missing, then runs the smoke):
./run-smoke.sh 127.0.0.1 25565
```

Useful env knobs: `MC_USERNAME`, `MC_MOVE_BLOCKS`, `MC_STEP_BLOCKS`, `MC_STEP_MS`.

## Planned full end-to-end scenario

The script is structured so the remaining steps slot in cleanly. Stubs with the
verified 1.21.8 packet field layouts already exist in `smoke.mjs` (search for
`TODO`). The target scenario, end to end:

```
status -> login -> chunks -> move
       -> select hotbar slot
       -> set creative-mode slot (give a block/tool)
       -> place block
       -> break block
       -> (server restart, driven externally)
       -> reconnect
       -> verify the edits persisted
```

Status of each step:

| Step                              | State        |
|-----------------------------------|--------------|
| status ping (proto/version)       | implemented  |
| offline login -> play             | implemented  |
| first chunk(s)                    | implemented  |
| move across chunk boundary        | implemented  |
| chunk load + unload observed      | implemented  |
| select hotbar slot                | stub (TODO)  |
| set creative-mode slot            | stub (TODO)  |
| place block                       | stub (TODO)  |
| break block                       | stub (TODO)  |
| disconnect / reconnect            | stub (TODO)  |
| verify persistence after restart  | stub (TODO)  |

## Files

- `package.json` — pins `node-minecraft-protocol` + `minecraft-data` (both with
  1.21.8 support). `type: module`.
- `smoke.mjs` — the smoke test (implemented steps + clearly marked TODO stubs).
- `run-smoke.sh` — runner; assumes the server is already up, installs deps if
  needed, runs the smoke.
- `README.md` — this file.
