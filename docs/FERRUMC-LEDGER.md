# FerrumC LEDGER — Where Everything Lives

> The single "where is everything / what do I reference" index for FerrumC v2.
> A high-performance Minecraft Java Edition server in Rust. Target: **Java 1.21.8 / protocol 772**.
> Owner: Saad (GitHub: Sweattypalms). License: MIT. Clean rewrite.
>
> If you read one doc before touching this repo, read [`CLAUDE.md`](../CLAUDE.md) (the law).
> This ledger is the map; CLAUDE.md is the rulebook; [`docs/agent-tasks/MILESTONES.md`](agent-tasks/MILESTONES.md) is the plan.

---

## 1. Repo geography

| Thing | Path | Notes |
|---|---|---|
| Git root | `/Users/saad/dev/personal/apps/ferrumc/core/` | Holds project meta ONLY (CLAUDE.md, docs, fixtures, hooks). This worktree is named "core". |
| Cargo workspace | `core/server/` | **All Rust lives here.** Run every `cargo` command from here. |
| Workspace manifest | `core/server/Cargo.toml` | `[workspace]` members, shared deps, lints, profiles. |
| The binary | `core/server/app/` | `ferrumc-app`, bin name `ferrumc`. The only crate that wires everything. |
| Crates | `core/server/crates/ferrumc-*/` | One lane per crate. See §2. |
| Plugins | `core/server/plugins/` | `ferrumc-plugin-fixture`, `ferrumc-plugin-spawn-protect`. |
| Dev task runner | `core/server/xtask/` | `cargo xtask generate [--check]`. |
| Docs | `core/docs/` | architecture, adr, agent-tasks, protocol, safety, experiments. |
| Fixtures (test data) | `core/fixtures/` | One level **above** the workspace. `protocol/1_21_8/`, `nbt/`, `anvil/`, `worlds/`. |
| Git hooks | `core/.githooks/` | `pre-commit` (fast), `pre-push` (heavy). See §5. |
| Toolchain pin | `core/server/rust-toolchain.toml` | Pins rustfmt — **critical** for deterministic codegen. |

**Active branch:** `rework/v2-skeleton`. **Main/PR base:** `master`.

Repo meta files worth knowing: `core/CLAUDE.md` (hard rules), `core/AGENTS.md`, each crate's
`README.md` + `INVARIANTS.md` (e.g. `app/INVARIANTS.md`), `docs/safety/<crate>.md` (where unsafe is allowed).

---

## 2. Crate map

Five hard-separated layers: `app → net → session → sim → storage`, with leaf crates
(`core`, `math`, `codec`, `nbt`, `registry`, `proto`) underneath. **You do not cross lanes.**
A PR touches ≤2 crates (one primary + `testkit` if needed).

Dependency columns are the **internal** ferrumc-crate edges (derived from each `Cargo.toml`,
cross-checked against the crate map in CLAUDE.md). External deps (tokio, bytes, serde, …) omitted.

| Crate | One-line purpose | Key source | Depends on (internal) | Depended on by |
|---|---|---|---|---|
| `ferrumc-core` | Shared leaf types: `PlayerId`, `EntityId`, `Tick`, `ServerError`/`Result<T>`, `TextComponent`, `GameMode` | `src/lib.rs` | — (nothing) | math, codec, session, storage, sim, command, permission, plugin-api, plugin-host, app |
| `ferrumc-math` | Typed coordinates: `BlockPos`, `ChunkPos`, `SectionPos`, `ShardPos` (8×8), `Vec3`, `Aabb`, `Direction` | `src/lib.rs` | core | session, world, storage, sim, plugin-api, app |
| `ferrumc-codec` | Hostile-input primitives: `VarInt`/`VarLong`, `BoundedReader`, `BoundedString`, `BoundedBytes` | `src/lib.rs` | core | nbt, proto, net, testkit, app |
| `ferrumc-nbt` | NBT parse/write with depth/size limits; `read_network_root_with_consumed` | `src/lib.rs` | codec | proto |
| `ferrumc-registry` | Pinned 1.21.8 data constants: block-state/biome/dimension IDs, protocol/data version | `src/lib.rs` | — (constants only; CLAUDE.md lists core as intended) | proto, world |
| `ferrumc-proto` | Typed 1.21.8 packets. `generated/` (codegen) + hand-written `wire`/`error` | `src/generated/*.rs`, `src/lib.rs` | codec, nbt, registry | net, session, testkit, app |
| `ferrumc-proto-gen` | **Build-time only** code generator. Reads `packets.toml`, emits `proto/src/generated/` | `src/{lib,spec,packets,emit,error}.rs` | — | xtask only |
| `ferrumc-net` | Tokio TCP, framing, compression, encryption, `StatusServer`, per-state limits, play reader/writer | `src/{server,lib}.rs` | codec, proto | session, app |
| `ferrumc-session` | net↔sim bridge: `NetEvent→GameInput`, `GameOutput→Packet`, player↔shard router | `src/lib.rs` | core, math, net, proto, sim | app |
| `ferrumc-world` | Pure world model: `Chunk`, `ChunkSection`, `PalettedContainer`, `PackedArray`, `Heightmap`, flat generator | `src/lib.rs` | math, registry | storage, sim, app |
| `ferrumc-storage` | `WorldStore`/`PlayerStore`/`PluginStore` traits + in-memory/redb backend | `src/lib.rs` | core, math, world | sim, app |
| `ferrumc-sim` | Tick coordinator, shard workers, chunk load-or-generate, entity systems | `src/lib.rs` | core, math, storage, world | session, app |
| `ferrumc-command` | Command tree, parsing, suggestions, execution | `src/lib.rs` | core | plugin-api, plugin-host, app |
| `ferrumc-permission` | Permission nodes, subjects, grants | `src/lib.rs` | core | plugin-api, app |
| `ferrumc-plugin-api` | Stable plugin-facing API: `WorldView`, `CommandSink`, `PermissionApi` | `src/lib.rs` | core, math, command, permission | plugin-host |
| `ferrumc-plugin-host` | Plugin registry, lifecycle, event dispatch, panic isolation, **C-ABI dynamic loader** | `src/lib.rs` | core, command, plugin-api | app |
| `ferrumc-anvil` | Vanilla Anvil import/export (skeleton; separate from native storage) | `src/lib.rs` | world, nbt (intended) | — |
| `ferrumc-observability` | Metrics/tracing helpers (skeleton) | `src/lib.rs` | — | (app, later) |
| `ferrumc-config` | Standalone config crate (skeleton). **Note:** the app currently uses its own `app/src/config.rs`, not this crate. | `src/lib.rs` | — | — |
| `ferrumc-testkit` | Fake clients, fixtures, deterministic tick harness (test-only) | `src/lib.rs` | codec, proto | (dev-deps) |
| `ferrumc-app` | Wires everything. Owns the connection loop, config, startup/shutdown. | `app/src/*` (see §3) | (almost) all | — |

> `ferrumc-plugin-host` is the **only** crate that does not `forbid(unsafe_code)` — it `deny`s it
> and scopes a single FFI surface for libloading. See `docs/safety/ferrumc-plugin-host.md`.

### app/src layout (the wiring you'll touch most)

| File | Role |
|---|---|
| `app/src/main.rs` | Entrypoint: install tracing, load config, `run`, wait for Ctrl-C, shutdown. |
| `app/src/lib.rs` | `run(&AppConfig)`, public `AppConfig` re-export, server handle. |
| `app/src/config.rs` | `AppConfig` + `RawConfig` (TOML shape, defaults, validation). |
| `app/src/server.rs` | Listener, accept loop, connection lifecycle, graceful shutdown. |
| `app/src/connection.rs` | **The app's own per-connection loop** (login → config → play). Separate from net's `StatusServer`. |
| `app/src/registries.rs` | Builds the Configuration-phase RegistryData sent to clients. |
| `app/src/world.rs` | Chunk → wire blob: paletted sections, heightmaps, light. |
| `app/src/driver.rs` | Ties the connection to the sim/session. |
| `app/src/command.rs`, `plugins.rs` | Command dispatch + plugin host wiring. |

---

## 3. Build / run / gate commands

All `cargo` commands run from **`core/server/`** (the workspace root).

### Run the server

```bash
RUST_LOG=info cargo run -p ferrumc-app          # logs default to `info` if RUST_LOG unset
RUST_LOG=debug,ferrumc_net=trace cargo run -p ferrumc-app
```

### Configuration

Config is resolved in this precedence (see `app/src/main.rs`):

1. **First CLI argument** — a path to a TOML file: `cargo run -p ferrumc-app -- ./my-config.toml`
2. **`FERRUMC_CONFIG`** env var — a path to a TOML file.
3. **Built-in defaults** — if neither is set (`AppConfig::default()`).

TOML keys (all optional, merge over defaults; `deny_unknown_fields` — a typo is a hard error).
Source of truth: `app/src/config.rs`.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `bind` | string `"ip:port"` | `127.0.0.1:25565` | TCP listen address |
| `max_connections` | int | `256` | concurrent connection ceiling |
| `io_timeout_secs` | int | `30` | per-socket read/write deadline |
| `compression_threshold` | int | none | packet compression threshold (omit = off) |
| `view_distance` | int | `10` | advertised view distance (chunks) |
| `simulation_distance` | int | `10` | advertised simulation distance (chunks) |
| `spawn` | `[x, y, z]` f64 | `[8.0, 64.0, 8.0]` | world spawn (one block above flat grass at y=63) |
| `spawn_chunk_radius` | int (u8) | `2` | resident spawn area radius `(2r+1)²` |
| `ticks_per_second` | int (>0) | `20` | sim tick rate |
| `plugins_dir` | string path | none | dir scanned for cdylib plugins (omit = no dynamic plugins) |
| `spawn_protect_radius` | int | `0` | spawn-protect block radius (Chebyshev; 0 = off) |
| `spawn_protect_bypass` | `[string]` | `[]` | players granted the bypass permission |
| `keep_alive_interval_ms` | int | `10000` | play-phase Keep Alive interval (client times out at 20 s) |

### Quality gates (must pass before commit/push)

```bash
# from core/server
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace --doc           # doc-tests (pre-push runs these)
# per-crate while iterating:
cargo fmt -p <crate> -- --check
cargo clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
```

Lints are workspace-wide: `unsafe_code = forbid`, clippy `all = deny`, `pedantic = warn`
(with a few noisy pedantic lints allowed — see `[workspace.lints]` in `Cargo.toml`).

### Protocol codegen

```bash
cargo xtask generate            # regenerate crates/ferrumc-proto/src/generated/ from packets.toml
cargo xtask generate --check    # fail (with a diff) if the committed tree has drifted — keep this CLEAN
```

See §6 for the full add-a-packet workflow.

---

## 4. Protocol references (ground truth)

**Target: Minecraft Java 1.21.8 = protocol 772, dataVersion 4440.** (1.21.7 is also 772 but
dataVersion 4438 — pin by version/dataVersion, not protocol number alone.)

> ⚠️ **DO NOT use the live minecraft.wiki.** It has drifted to **1.21.9 / protocol 773** and
> will give you wrong packet IDs and field layouts. Use the pinned sources below, only.

### The three references, in priority order

1. **`core/fixtures/protocol/1_21_8/protocol.json` — AUTHORITATIVE.**
   PrismarineJS `minecraft-data` @ commit `a8cf733fe44f3069f87f63e7ec3d74b521840ded`.
   This is the ground truth the packet generator is verified against. **When anything disagrees,
   protocol.json wins.** Layout: id↔name maps at `<state>.<dir>.types.packet[1][0].type[1].mappings`;
   field layout at `<state>.<dir>.types.packet_<name>`. (`toClient` = clientbound, `toServer` = serverbound.)
   Provenance + checksums in `core/fixtures/protocol/1_21_8/manifest.toml`; attribution in `NOTICE`.

2. **`~/.claude/skills/mc-protocol-1_21_8/protocol-1.21.8.txt` — human-readable prose.**
   minecraft.wiki pinned revision `oldid=3258009` (the correct 1.21.8 / 772 era), raw wikitext.
   Best for a packet's *purpose*, notes, field semantics, enum meanings, the state machine.
   Grep it; cross-check the bytes against protocol.json.

3. **The `mc-protocol-1_21_8` Claude Code skill** — wraps the two above with lookup recipes
   (grep patterns, jq/python snippets, packet counts, already-learned gotchas). **Invoke this skill
   for any packet ID / field-layout / direction / state-machine question** instead of guessing.

Other vendored data in `core/fixtures/protocol/1_21_8/`: `blocks.json` (block-state IDs),
`biomes.json`, `version.json`. All checksummed in `manifest.toml` and re-verified by a
`ferrumc-registry` test.

### Reference Troves (curated wiki dump + v1 impl)

Two durable reference troves live at the **ferrumc ROOT** (`/Users/saad/dev/personal/apps/ferrumc/`,
a sibling of `core/` — which is why they were missed before). They sit **below** the three references
above in the precedence chain:

> **Precedence:** `fixtures/protocol/1_21_8/protocol.json` = machine-readable **ground truth** →
> `wiki/protocol/` = human-readable **1.21.8 reference** → `ref/` = **v1 impl, inspiration only**.
> When anything disagrees, protocol.json wins.

The earlier warning still stands: **the LIVE minecraft.wiki is 1.21.9 / protocol 773 — do not use it.**
The `wiki/` trove below is a **pinned, curated 1.21.8 snapshot saved to disk** (safe to use), NOT the
live site. Licensing: the wiki pages are **CC BY-SA 3.0 Unported** — fine to reference for
implementation, but attribute appropriately in any derivative/reproduction.

> ⚠️ **Version caveat inside the dump:** the main packets file IS pinned to 1.21.8/772, but a few of the
> "current list" articles were saved at a LATER version: **`Entity_metadata.txt` is 26.1**,
> **`Particles.txt` is 1.21.10**, **`Protocol_History.txt` goes up to 26.1.2 / 775**. Their *structure*
> is fine, but **verify any concrete entity / particle / status IDs against the fixtures**, not against these.

#### TROVE 1 — `wiki/` (curated minecraft.wiki 1.21.8 dump, CC BY-SA 3.0)

`wiki/protocol/` — the human-readable companion to `fixtures/protocol/1_21_8/protocol.json`:

| File | Use it for |
|---|---|
| `Java_Edition_protocol_Packets_1.21.8_pv772.txt` | **THE authoritative 1.21.8 / pv772 packet list** — every packet per state + direction, with fields, types, and notes. 264 KB; the prose companion to protocol.json. |
| `Chunk_format.txt` | Chunk Data & Update Light internals: chunk columns vs sections, global/local palettes, paletted containers, bits-per-entry, heightmaps, light. **Source of GOTCHA #1–#2 in §7.** |
| `Data_types.txt` | Primitive wire encodings: VarInt/VarLong, Boolean, numeric types, Position, big-endian rules. |
| `Slot_Data.txt` | The Slot structure (item stacks): item count, item id, structured-component add/remove arrays. Item serialization. |
| `Inventory.txt` | Inventory window types + slot-index ranges/meanings per window (chest, furnace, horse, …). |
| `Entity_metadata.txt` | Per-entity metadata field layouts + the entity-type ID table. ⚠ content is **26.1** — verify IDs vs fixtures. |
| `Entity_statuses.txt` | Entity status/event codes (Entity Event packet) per entity. |
| `Object_Data.txt` | Meaning of the `Data` field in Spawn Entity, per entity type (e.g. item-frame orientation). |
| `Block_actions.txt` | Block Action packet action IDs per block (note block, piston, chest, …). |
| `Command_data.txt` | Command-graph structure: root/literal/argument nodes, flags, redirects (Commands packet). |
| `Registries.txt` | Built-in vs data-driven vs synchronized registries; which registries are sent on join. |
| `Particles.txt` | Particle type IDs + per-particle data formats. ⚠ content is **1.21.10** — verify IDs vs fixtures. |
| `Plugin_channels.txt` | Built-in plugin channels (`minecraft:brand`, register/unregister, …) over custom payload. |
| `Server_List_Ping.txt` | Status ping (SLP) flow + status-response JSON; modern and legacy 1.6 ping. |
| `Protocol_Encryption.txt` | Online-mode login encryption: Encryption Request/Response, server-ID hash, AES. |
| `Protocol_FAQ.txt` | Protocol overview, the normal login sequence, common Q&A. |
| `Protocol_History.txt` | Per-version protocol changelog ("what changed when"). ⚠ runs up to **26.1.2 / 775**. |
| `Protocol_version_numbers.txt` | MC release ↔ protocol-number table (confirm 1.21.8 = 772). |

Other `wiki/` subdirs (peek before relying on them):

- **`wiki/auth/`** — online-mode auth. `Microsoft_authentication.txt` (Xbox/MSA OAuth2 flow),
  `Mojang_API.txt` (profile/UUID/skin endpoints + ratelimits), `Yggdrasil.txt` + `Legacy_Minecraft_authentication.txt`
  (both **outdated**, pre-MSA). Pair with `Protocol_Encryption.txt` for online-mode login.
- **`wiki/storage/`** — `NBT.txt` (NBT binary format), `Anvil_file_format.txt` (Anvil chunk storage),
  `Region_Files.txt` (region container). For `ferrumc-nbt` / `ferrumc-anvil` / `ferrumc-storage`.
- **`wiki/text/`** — `Raw_JSON_text_format.txt` (text component / SNBT since 1.21.5), `Text_formatting.txt`
  (legacy `§` color codes), `Chat.txt` (chat system + chat modes). For `TextComponent` / chat.
- **`wiki/registry/`** — `Data_Generators.txt` (how Mojang's data generators produce registry/block reports —
  i.e. how the fixtures were generated).
- **`wiki/misc/`** — `Map_item_format.txt` (map items), `Query.txt` (UDP query protocol), `RCON.txt`
  (remote-console TCP protocol), `Units_of_Measurement.txt` (block/pixel/coordinate units).

#### TROVE 2 — `ref/` (FerrumC **v1**, a complete older Rust MC server — inspiration only)

`ref/` is a full v1 implementation (it's a git worktree of this same repo: `core/.git/worktrees/ref`).
Architecture differs sharply from v2: **Bevy ECS + Tokio + LMDB (heed) + DashMap chunk cache + phf
registries** — i.e. a global-ish ECS, not v2's actor-sharded lanes. Its own `CLAUDE.md` also claims
1.21.8/772, **but it is a separate codebase with hand-maintained data** — so treat its IDs/layouts as
*inspiration* and **verify against `protocol.json` before copying**.

Consult `ref/` for **"how did FerrumC v1 implement X"** — chunk encoding, packet handling, NBT/Anvil,
encryption, registries — not for ground-truth wire bytes. Useful entry points:

| Path | What's there |
|---|---|
| `ref/assets/data/packets.json` | v1's packet id↔name map per state/direction (`protocol_id`). |
| `ref/assets/data/registry_packets.json` | v1's registry-data packet payloads. |
| `ref/assets/extracted/*.json` (49 files) | v1's data-generator dumps: `packets.json`, `blocks.json`, `items.json`, `entities.json`, `damage_type.json`, `synced_registries.json`, `particles.json`, `entity_statuses.json`, … (NOTE the path is `assets/extracted/`, **not** `assets/data/extracted/`). |
| `ref/scripts/new_packet.py` | How v1 scaffolded a packet (`#[packet(packet_id, state)]` + `NetEncode`/`NetDecode` macros) — contrast with v2's `packets.toml` codegen (§6). |
| `ref/src/lib/` (~30 crates) | `net` (+`codec`, `encryption` AES-128-CFB8+RSA), `adapters/nbt`, `adapters/anvil` (memmap2+yazi), `storage` (heed/LMDB), `world`/`world_gen`, `registry`, `commands`, `inventories`, `particles`, `physics`, `plugins`. |
| `ref/src/bin/src/` | `game_loop.rs`, `packet_handlers/play_packets/*` (real handlers: `place_block`, `player_action`, `set_creative_mode_slot`, `chunk_batch_ack`, …), `systems/*` (`chunk_sending`, `physics/*`, `keep_alive`, `mobs/*`). |
| `ref/docs/` | `protocol-flow.md` (state machine + code refs), `vanilla_parity_feature_list_aggregations_todolist.md` (big feature checklist), `ci/`. |
| `ref/.etc/` | Sample binary fixtures: NBT files, `r.0.0.mca` region, `registry.nbt`/`registry.packet`, `raw_chunk.dat` — handy test-data references. |

### Transient / scout staging (NOT durable)

- Source artifacts that produced the pinned skill `.txt` (NOT durable — `/tmp` is wiped):
  `/private/tmp/protocol-1.21.8.{txt,raw.wikitext,rendered.html}`. The `.txt` is byte-identical to the
  **skill copy**; treat the skill copy as canonical and don't reference `/tmp` paths in code or docs.
- Scout/spec notes from the real-join session (also transient `/tmp/...scratchpad/`, copy anything worth
  keeping into `docs/`): `realjoin-spec.md` (login→spawn byte spec), `scout-protocol-lane-notes.md`
  (packet subset + IDs), `scout-m04-registry-notes.md` (registry data).

---

## 5. Git hooks (`core/.githooks/`)

Enable once: `git config core.hooksPath .githooks` (from `core/`).

- **`pre-commit`** (fast): blocks hand-edited `*/src/generated/` files unless a generator/fixture/spec
  also changed; then, if Rust is staged, runs `cargo fmt --all --check` + `cargo metadata --no-deps`.
- **`pre-push`** (heavy): `cargo clippy --workspace --all-targets -- -D warnings`, then tests
  (`cargo nextest run` if installed, else `cargo test --workspace`), then `cargo test --workspace --doc`.

The generated-file guard is why **you never hand-edit `crates/ferrumc-proto/src/generated/`** — the
hook will reject the commit. Fix the spec or the generator instead (§6).

---

## 6. Codegen workflow (add / change a packet)

Generated packet code is **checked in and verified**. The pipeline:

```
docs/protocol/1_21_8/packets.toml   ← edit THIS (the declarative spec)
        │  cargo xtask generate
        ▼
crates/ferrumc-proto/src/generated/{mod,handshake,status,login,configuration,play}.rs   ← NEVER hand-edit
        │  + hand-written crates/ferrumc-proto/src/{wire,error}.rs for primitives the generator doesn't emit
        ▼
roundtrip + malformed-input tests in ferrumc-proto
        │
        ▼
cargo xtask generate --check   ← must stay clean (CI + pre-commit enforce this)
```

- `packets.toml` is the **single source of truth**. Its header documents the closed field-type grammar
  (`varint`, `u16`, `bool`, `uuid`, `position`, `string(N)`, `identifier`, `nbt`, `prefixed_bytes(N)`,
  `remaining_bytes`, `optional<T>`, `prefixed_array<T>`, `<StructName>`). The generator **rejects**
  anything outside that grammar — extend the generator, don't smuggle a new type.
- Each generated packet carries `pub const PACKET_ID`, a `decode` (does NOT read the id) and an `encode`
  (writes the id then fields). Generated code calls only `wire::*` + bounded codec types + `ferrumc_nbt`.
- The generator header is exactly `// @generated by ferrumc-proto-gen. Do not edit by hand.`
- A `ferrumc-proto-gen` test asserts every spec packet id matches the vendored `protocol.json` 772 ids.
  **Verify IDs/fields against `protocol.json` (via the `mc-protocol-1_21_8` skill) before generating.**
- Determinism guardrails (do not break): `rust-toolchain.toml` pins rustfmt; `.gitattributes` forces LF
  on `generated/`; output is sorted (BTreeMap); no timestamps/usernames/abs-paths in output.

---

## 7. PROTOCOL GOTCHAS (learned the hard way)

These cost real debugging time against a real 1.21.8 client. Read before any chunk/registry/join work.

1. **Chunk-section `PalettedContainer` long arrays have NO VarInt length prefix (as of 1.21.5).**
   The client computes the word count itself: `ceil(entries / floor(64 / bits_per_entry))`, where
   `entries = 4096` for blocks and `64` for biomes. **BUT** the heightmap arrays and the light arrays
   in the *same* Chunk Data packet **DO** carry varint counts. Do not conflate them. (Conflating
   `FriendlyByteBuf.writeLongArray`'s always-prefix with the section container's no-prefix is what
   broke chunks and crashed the real client with *"No value with id N"*.)

2. **Block palette bits-per-entry must follow vanilla exactly:**
   `single = 0`; linear/indirect uses **min 4 bits** (`max(4, ceil(log2(palette_len)))`) up to 8;
   **direct = 15 on the wire** (NOT 32) above that. (The world crate's `DIRECT_BITS` was 32 — wrong on
   the wire; flat world doesn't hit direct mode, but fix it before non-flat worlds.)

3. **The Configuration phase must IGNORE serverbound packets the server doesn't model.**
   Treating an unknown id as fatal kicks real clients. Decode-and-ignore: `0x01` cookie response,
   `0x02` plugin message (`minecraft:brand`), `0x04` keep-alive, `0x05` pong, `0x06` resource-pack
   response. Only *react* to `0x00` ClientInformation, `0x07` ServerboundKnownPacks, `0x03`
   AckFinishConfiguration.

4. **"No value with id N" client crash** = a registry/IdMap `byId` lookup on an id not present in a
   **session** registry (the server only registers what it sends in RegistryData). Usually a symptom of
   a chunk-parse desync making the client read from the wrong byte offset — start by auditing the chunk
   bytes (see gotcha #1), not the registries.

5. **Configuration registry handshake order:** send `ClientboundKnownPacks` advertising
   `minecraft:core` version `"1.21.8"`, **WAIT for the client's echo** (`ServerboundKnownPacks`), *then*
   send `RegistryData` for every required registry with **NBT omitted** (`data = None` — the client fills
   from its built-in core pack). You **cannot** send empty registry data — you must enumerate every entry
   used. Required registries: `dimension_type`, `worldgen/biome`, `damage_type` (**all 49 keys**),
   `painting_variant`, `wolf_variant`, `wolf_sound_variant`, `cat_variant`, `cow_variant`, `pig_variant`,
   `chicken_variant`, `frog_variant`. **Entry index = send order**, and those indices are the numeric ids
   the client uses (so `dimension_type[overworld]` must be index 0 = `JoinGame.dimension_type`;
   `biome[plains]` must be index 0 = the id your chunk palettes reference).

6. **`LoginSuccess` (1.21.8) has NO trailing `strict_error_handling` bool** — that field was removed
   after protocol 767. Don't emit it. (Fields: uuid, username, properties[].)

7. **Play join order (get this wrong and the client hangs on "Loading terrain"):**
   `JoinGame` → `GameEvent(reason=13, level_chunks_load_start)` → `SetCenterChunk` →
   `SynchronizePlayerPosition` (**teleport_id NONZERO**, sent **BEFORE** chunks) →
   `ChunkData` (with real light + heightmaps, one per chunk in view, must include the player's chunk) →
   `KeepAlive` every **<20 s** or the client times out. Set Center Chunk **must** precede chunk data.

8. **The app has its OWN connection loop in `app/src/connection.rs`,** separate from net's `StatusServer`.
   New play/config/login features must be wired into the **app loop** — it is not enough for the logic to
   exist in `ferrumc-net` or `ferrumc-sim`. The status-ping path (`StatusServer`) and the real-join path
   (app `connection.rs`) are different code.

> Deeper byte-level spec for the full login→spawn sequence: see the scout's `realjoin-spec.md`
> (referenced in §4; copy into `docs/` if you want it durable). It enumerates packet ids, the 49
> damage_type keys, the light-mask layout (26 sections, full-bright sky), and the heightmap packing
> (9 bpe MOTION_BLOCKING, 37 longs).

---

## 8. Milestones & lanes (quick pointer)

Full plan: [`docs/agent-tasks/MILESTONES.md`](agent-tasks/MILESTONES.md) (M00–M29, the vertical-slice MVP).
Lanes (max **3** active implementation agents at once):

- **Lane A** protocol/net: M01 → M04 → M05 → M06 → M08 → M09 → M10 → M11 → M14 → M15
- **Lane B** world/storage/sim: M02 → M03 → M12 → M13 → M16 → M17 → M18 → M19 → M20
- **Lane C** commands/plugins: M02 → M26 → M27 → M28 → M29
- **Lane D** testkit/tooling: M00 → M07 (supports all lanes)

**Never parallelize two milestones that both touch a chokepoint crate:** `ferrumc-proto`,
`ferrumc-net`, `ferrumc-sim`, `ferrumc-plugin-api`. (See §9 of the `ferrumc-dev` skill for the
orchestration rules that operationalize this.)

---

## 9. Tooling recommendations (not yet applied — for the owner)

These are suggestions, **not changes made by this doc**. Apply via `/update-config` or by editing the
hooks yourself if you want them.

- **Pre-commit `cargo metadata` warning:** the hook runs `cargo metadata --no-deps`, which emits a
  harmless `--format-version` deprecation warning. Pin it: `cargo metadata --no-deps --format-version 1`.
- **CI generated-files drift check:** once the generator is stable, add a CI step running
  `cargo xtask generate --check` (and/or `git diff --exit-code` after `generate`) so drift fails the
  build, not just local pre-commit.
- **`cargo-nextest`:** pre-push prefers it if installed (`cargo nextest run`). Installing it speeds the
  push gate; otherwise it falls back to `cargo test`.
- **`cargo xtask doctor`** (planned in MILESTONES cross-cutting work): a single command to validate
  generated files current, fixture checksums valid, dep graph legal, no forbidden patterns. Worth
  building once xtask grows.

---

## 10. Reference index (one-liners)

| Want to… | Go to |
|---|---|
| Know the rules / what not to build | `core/CLAUDE.md` |
| See the plan / next milestone | `docs/agent-tasks/MILESTONES.md` |
| Look up a packet id / field layout | `mc-protocol-1_21_8` skill → `protocol.json` (authoritative) |
| Run the orchestration workflow for a milestone | `ferrumc-dev` skill (`~/.claude/skills/ferrumc-dev/SKILL.md`) |
| Add/change a packet | `docs/protocol/1_21_8/packets.toml` + `cargo xtask generate` (§6) |
| Understand a layer's design | `docs/architecture/{overview,networking,simulation,storage,plugins}.md` |
| Know why a decision was made | `docs/adr/000N-*.md` |
| Where unsafe is allowed | `docs/safety/<crate>.md` |
| Config knobs | `app/src/config.rs` (§3) |
| The real-client join sequence | §7 here + scout `realjoin-spec.md` |
</content>
</invoke>
