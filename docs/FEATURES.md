# FerrumC v2 — Implemented Features

> What actually works **end-to-end against a real vanilla client** today.
> Target: **Minecraft Java 1.21.8 / protocol 772** (offline/cracked mode only).
>
> This list is verified against the code, not the plan. Anything not here is on the
> [ROADMAP](ROADMAP.md). Items marked ⚠ are wired but known-buggy.

## How to connect (applies to every test below)

1. From `core/server`: `RUST_LOG=info cargo run -p ferrumc-app` (binds `127.0.0.1:25565`, creative, view-distance 10, spawn `(8, 64, 8)`).
2. A real **1.21.8** vanilla client in **offline mode** (the server does no auth/encryption). Add `localhost` to the server list — it should read **COMPATIBLE / FerrumC 1.21.8**.
3. Defaults that affect tests: every player is forced into **Creative**; permission level is **0** unless the player's name is in the config `ops` list (so `/gamemode` needs config — see below); spawn-protect is **off** (radius 0) by default.

---

## Networking / Protocol

- **Status ping (SLP).** Server list shows MOTD "FerrumC 1.21.8", proto 772, player count, and replies to ping.
  - *Test:* Add the server in multiplayer; it shows COMPATIBLE with a green ping bar before you connect.
- **Offline login + Configuration handshake.** Full Handshake → LoginStart → LoginSuccess (offline UUID) → KnownPacks negotiation → RegistryData → FinishConfiguration → Play.
  - *Test:* Click Join World; you pass "Logging in / Loading terrain" and spawn. Unknown config packets are tolerated, not fatal.
- **Correct join sequence (no "Loading terrain" hang).** JoinGame → self PlayerInfo → GameEvent(13) → SetCenterChunk → SetDefaultSpawnPosition → Commands → teleport (SynchronizePlayerPosition, id 1) → PlayerAbilities → starter inventory → spawn chunks, in that exact order.
  - *Test:* Join and you land in the flat world at (8, 64, 8) in creative with flight, no infinite terrain screen.
- **Keep-alive liveness.** Clientbound KeepAlive every 10 s; dead sockets dropped by the 30 s I/O timeout.
  - *Test:* Stand idle for 60 s — you stay connected. Kill the client's network and the connection is reaped.
- **Packet compression.** Honored when configured (off by default).
  - *Test:* Set `compression_threshold = 256` in a config TOML, restart, rejoin — still works (frames now zlib-compressed above the threshold).
- **Outbound backpressure + criticality envelope + mandatory delivery.** Priority writer (Critical > State > World) with per-player bounded queues; movement/block broadcasts are droppable, acks/spawns/keep-alive are mandatory and force a clean disconnect rather than silent loss when the queue overflows.
  - *Test:* Suspend the client process (e.g. `kill -STOP` the client) so it stops reading; the server eventually disconnects it with an outbound-overflow error instead of corrupting state.
- **Chat rate limiting.** Serverbound chat is throttled (~burst 8, ~2 lines/s) per player.
  - *Test:* Spam chat fast — excess lines are dropped without kicking you.

---

## World / Chunks

- **Flat world generation.** Deterministic superflat: bedrock at y=-64, stone to y=59, 3 dirt, grass at y=63, air above. Full -64..319 height (24 sections).
  - *Test:* Fly straight down — bedrock floor; the surface is an endless grass plain at y=63.
- **Real paletted chunk encoding.** Vanilla-correct chunk format: single/indirect/direct paletted block containers (no long-array length prefix, per 1.21.5+), MOTION_BLOCKING heightmap, single `plains` biome.
  - *Test:* Join — terrain renders with no missing-chunk holes; F3 confirms biome `minecraft:plains`.
- **Streaming to full view distance.** Join sends the spawn batch (5×5), then a 50 ms background pump fills out to the advertised view distance (21×21 at vd=10), nearest-first.
  - *Test:* Join and **stand still** — chunks keep filling outward to ~10-chunk radius without moving. F3 shows ~441 client chunks.
- **Center-chunk follow + unload.** Crossing a chunk border sends SetCenterChunk and streams new chunks; out-of-range chunks are unloaded and released.
  - *Test:* Walk in one direction; F3 "Chunk" center updates, new chunks appear ahead, far ones disappear behind.
- ⚠ **Lighting is a full-bright placeholder.** All sky light = 15, no real light engine.
  - *Test:* Dig a deep hole — the bottom is fully lit (no darkness). Cosmetic-only at flat scope.

---

## Blocks / Gameplay

- **Block breaking.** Creative insta-break via PlayerAction; validated (actor present, chunk resident, reach ≤6) and applied on the owning shard.
  - *Test:* Left-click a block in creative — it turns to air for you (and any nearby player).
- **Block placing from the hotbar.** Right-click a face with a placeable held item; the held item resolves to its block state and is placed.
  - *Test:* Select a hotbar block and right-click the ground/a wall — the block appears on the clicked face.
- **Shard-owned mutation path.** All world writes funnel through one tick-boundary funnel on the owning shard; networking never mutates the world directly.
  - *Test:* (architecture) Break/place rapidly — edits stay consistent; no torn state.
- **Block-change sequence acknowledgement.** Every edit carries the client's sequence number; accepted edits are acked, rejected ones get a resync + ack as an atomic pair (heals client prediction).
  - *Test:* Try to break a block >6 blocks away in a loaded chunk — your client's ghost block snaps back (resync).
- **Block-update broadcast.** Edits are fanned out to other players who can see the chunk.
  - *Test:* Two clients in view of each other; one places/breaks a block — the other sees the change.
- ⚠ **Block state / rotation / facing / waterlogging NOT derived.** Placement writes each item's **default** block state only — it ignores the click cursor position.
  - *Test:* Place a log (never gets an axis), stairs/furnace (always default facing), a slab (always bottom half), or a block "in" water (never waterlogs).

---

## Inventory / Items

- **1.21.8 item registry.** All 1416 items validated against the pinned registry (name, id, max stack, item→block mapping).
  - *Test:* Open the creative inventory (E) and search — item names and stack sizes match vanilla 1.21.8.
- **Creative inventory set-slot.** Serverbound SetCreativeSlot (0x37) accepted, slot-bounds-checked, sanitized, and echoed back authoritatively.
  - *Test:* In creative, grab an item from the creative menu into your inventory — it sticks (server echoes the slot).
- **Hardened slot wire types.** Untrusted (serverbound) item stacks are length-prefixed per component so hostile NBT/components are bounded, stripped, or rejected; count is clamped to the item's max stack.
  - *Test:* With a modified client, send an oversized/garbage-component stack — the server clamps/strips it and echoes the sanitized slot (no crash).
- **Per-player inventory + held slot.** Real 46-slot inventory seeded with a 9-item creative starter kit; held hotbar slot tracked via SetHeldItem (0x34).
  - *Test:* You spawn with stone/dirt/planks/glass/cobble/bricks/oak-log/wool/sand in the hotbar; scroll the hotbar and the selected slot updates server-side.
- ⚠ **Window-click is resync-only.** Any inventory click just re-sends the window; no item move/swap/drag logic, and the inventory is connection-local (the sim never sees it, not persisted).
  - *Test:* Drag items around your own inventory grid — they snap back to where they were.

---

## Multiplayer / Entities

- **See other players as players.** Remote players spawn as entity type 149 (`minecraft:player`) with a proper PlayerInfo entry.
  - *Test:* Two clients within view distance — each sees the other's player model.
- **PlayerInfoUpdate crash fixed.** The Add-Player body now includes the trailing `listed` flag, so a second joiner no longer triggers a Netty decode exception.
  - *Test:* Have a second player join while the first is online — both stay connected (this used to disconnect both).
- **Tab list.** Joiners are added to everyone's player list; leavers are removed (RemovePlayerInfo).
  - *Test:* Open the tab list (Tab) — online players are listed; one leaves and drops off the list.
- **Movement (position).** Walking players are broadcast via relative move / teleport deltas; entities are removed when a viewer moves out of range.
  - *Test:* Watch another player walk around — their position updates smoothly; they disappear when far and reappear when close.
- **Despawn on leave.** Disconnecting players are removed for everyone (RemoveEntities + RemovePlayerInfo).
  - *Test:* One player disconnects — their model and tab entry vanish immediately (no ghost).
- ⚠ **Rotation / head-yaw not broadcast.** Remote players always face north and never turn their head (only position is threaded through the sim).
  - *Test:* Have another player spin in place — to you they never rotate.
- ⚠ **Held item / armor not visible to others.** No SetEquipment is sent; your held slot is tracked server-side only.
  - *Test:* Hold a sword or wear armor — other players see empty hands and no armor.

---

## Chat / Commands

- **In-game chat relay.** Serverbound chat (0x08) is rate-limited, `§`-stripped, formatted `<name> message`, and rebroadcast to everyone as an (unsigned) system chat message.
  - *Test:* Type a message on one client — it shows as `<name> msg` on all clients.
- **System chat rendering (rich text).** SystemChat (0x72) encodes TextComponent as network NBT (color, bold/italic, children) — the carrier for chat, command feedback, and errors.
  - *Test:* Any command reply shows up in the chat box with color/formatting.
- **`/spawn` command.** Teleports the player to world spawn with confirmation feedback (no permission required).
  - *Test:* Walk away, run `/spawn` — you teleport back to (8, 64, 8) and see "teleported to spawn".
- **`/gamemode <0..3>` command.** Changes game mode for real (GameEvent reason 3 + authoritative sim state + inventory mirror); requires permission level 2.
  - *Test:* Put your name in `ops` in a config TOML, restart, run `/gamemode 0` — your HUD/abilities switch to survival; `/gamemode 1` back to creative.
- **Command feedback + errors.** Success messages and unknown-command / bad-arg / permission-denied errors all render (errors in red).
  - *Test:* Run `/nope` — you get a red "unknown command"; run `/gamemode` as a non-op — red "permission denied".
- **Command graph + tab-complete (permission-filtered).** Clientbound Commands graph drives the `/` autocomplete; TabCompleteRequest/Response is answered live; both hide commands the player isn't allowed to use.
  - *Test:* Type `/` — valid commands show white with arg hints; type `/sp`+Tab → completes `spawn`. A non-op never sees `gamemode` in the list.

---

## Persistence

- **redb is the default backend.** The shipping binary persists to a `world/` redb database (in-memory store is test-only).
  - *Test:* Run the server; a `world` directory with a redb file appears next to the binary's working dir.
- **Player-edited chunks persisted (overlays + journal).** Only player-modified sections are written as schema-versioned overlays over the regenerated baseline, plus an append-only block-mutation journal. Flushed every tick, before any chunk unload, and on shutdown (worker commits ~every 200 ms / 128 edits / on shutdown).
  - *Test:* Edit blocks **outside** the spawn area, fly far so the chunk unloads, fly back — your edits are still there (within one run).
- ⚠ **Edits in the spawn area are lost on rejoin (survive restart).** The per-join spawn-chunk "JoinKit" is built once at startup and never rebuilt, so a reconnecting player is re-sent the original spawn chunks. The edit is in redb but isn't re-shown until the server restarts.
  - *Test:* Edit a block near spawn, disconnect, reconnect (no restart) — the edit looks gone. Now restart the server and rejoin — it's back.
- ⚠ **Player state not persisted.** Position, game mode, and inventory are not saved/loaded (the redb `PlayerStore` is implemented but never called).
  - *Test:* Move, change hotbar, relog — you're back at spawn in creative with the default starter kit.

---

## Plugins (in-process)

- **Live block before/after events.** `before_block_break` / `before_block_place` run on the real packet path before the world is touched; `after_*` fire on accepted edits. Plugin host is shared across all connections.
  - *Test:* Enable spawn-protect (`spawn_protect_radius = 16`), restart, try to break a block near spawn as a non-bypass player — it's denied and reverts.
- **Decision model: Deny / Replace / EmitIntents.** Folded by the host (first Deny wins and is absorbing; Replace is last-writer; intents capped at 64) and applied back on the connection (Deny → ack-heal + optional message; Replace → place the substitute; SetBlock/Message intents routed).
  - *Test:* The block-rules sample denies placing **bedrock** in creative — put bedrock in a slot and try to place it; it's rejected and the ghost block heals.
- ⚠ **Replace never fires from the sample plugin (constant mismatch).** The dispatch wiring is correct, but block-rules watches the wrong block-state ids (199/9279) — the real glass item places state 562, so the rule's condition never matches. Deny works only because its bedrock constant happens to equal the real id.
  - *Test:* Place glass with block-rules active — it stays plain glass (no tinted-glass substitution).
- **Panic isolation.** A panicking plugin hook is caught, the plugin is disabled, and the edit fails safe (Deny).
  - *Test:* (sample-level) A faulty plugin can't crash the server; its block ops just get denied.
- **Spawn-protect sample plugin.** Vetoes edits inside a configurable spawn radius unless the actor holds the bypass permission; welcomes joiners with a message. **Off by default (radius 0).**
  - *Test:* Set `spawn_protect_radius = 16` and add yourself to `spawn_protect_bypass`; you can edit inside spawn, an unlisted player can't and sees a denial message.
- **Permission levels + bypass.** Players default to level 0; names in `ops` get level 4; `spawn_protect_bypass` grants the bypass node. Levels gate commands and autocomplete.
  - *Test:* Non-op can't `/gamemode`; add to `ops`, restart — now they can.

> ⚠ **Dynamic cdylib loading is a stub.** It only loads when `plugins_dir` is set (default off), loads into a throwaway host, and the C-ABI exposes only init/shutdown — no event hooks. Only the two in-process sample plugins make real decisions. Plugin-registered commands are likewise not merged into the live command tree.

---

## Observability / Ops

- **Per-session packet traces.** Each connection keeps ring buffers (256 inbound / 512 outbound frames); on disconnect or any decode error the trace is dumped as one JSON log event.
  - *Test:* Run with default `RUST_LOG=info`, join then disconnect — a `ferrumc::observability::session` JSON event lists the recent packets for that session.
- **Counter + tick metrics (logged).** Counters for chunks sent/unloaded, block mutations (accepted/rejected), storage-flush timing, decode errors, queue lengths; per-tick metrics (duration, inputs/outputs, players).
  - *Test:* Run with `RUST_LOG=ferrumc::observability::tick=debug` — per-tick metrics print every tick.
- ⚠ **No metrics endpoint.** Metrics surface only via tracing logs; the aggregate snapshot (`dump_metrics`) has no automatic trigger (no HTTP/Prometheus, no SIGUSR1).
  - *Test:* There is no `/metrics` HTTP endpoint to scrape — logs only.
</content>
</invoke>
