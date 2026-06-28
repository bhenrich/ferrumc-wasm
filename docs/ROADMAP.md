# FerrumC v2 — Vanilla-Parity Roadmap

> The full vanilla-server surface, tracked against the current build.
> Target: **Minecraft Java 1.21.8 / protocol 772**.
>
> Legend: `[x]` done · `[~]` partial / buggy · `[ ]` not started.
> See [FEATURES.md](FEATURES.md) for how to test the done items. Scope guardrails
> (what we are deliberately *not* building yet) live in [`../CLAUDE.md`](../CLAUDE.md).

---

## Next (highest leverage)

These are the near-term items that unblock the most player-visible parity, ordered.

- [ ] **Fix spawn-area edits lost on rejoin.** Rebuild (or re-stream from store) the spawn-chunk JoinKit per connection instead of freezing it at startup. Confirmed bug; biggest "my builds vanished" surprise.
- [~] **Block state on placement: facing / axis / half / waterlogging.** Use the UseItemOn cursor + face to derive the real state instead of the item's default state. Needs a block-state property model in `ferrumc-world`.
- [ ] **Held-item & armor equipment visibility.** Add the SetEquipment packet + sim plumbing so other players see your hand/armor.
- [ ] **Remote player rotation & head yaw.** Thread yaw/pitch/head through the sim and send the move-and-rotate / head-rotation packets (already in the enum, currently never sent).
- [ ] **Player state persistence.** Wire the existing redb `PlayerStore` into join/leave/shutdown for position, game mode, and inventory.
- [ ] **Fix sample-plugin Replace.** Point block-rules at real block-state ids (glass 562 → tinted-glass 23377) so the Replace path is actually exercised at runtime.
- [ ] **Wire the serverbound packet budget.** The 300 fps token bucket (`PlayReader`/`PacketBudget`) exists but is never constructed; the play loop is currently unbudgeted.
- [ ] **Block entities (chests/signs).** First container/block-entity type — prerequisite for survival storage and a lot of gameplay.

---

## Networking / Protocol

- [x] Status ping (SLP) + legacy-safe handshake
- [x] Offline-mode login
- [x] Configuration phase + RegistryData handshake (KnownPacks negotiation)
- [x] Correct Play join sequence (no terrain hang)
- [x] Keep-alive + I/O-timeout reaping
- [x] Packet compression (off by default)
- [x] Outbound priority queues + backpressure + mandatory-delivery envelope
- [~] Full Play-packet coverage (26 clientbound / 12 serverbound modeled; large surface still missing)
- [ ] Chunk batching handshake (ChunkBatchStart/Finished/Received flow control)
- [ ] Online-mode authentication (Mojang/Yggdrasil) + packet encryption
- [ ] Proxy support (Velocity/BungeeCord modern forwarding)

## Anti-cheat / Limits / Ops

- [~] Hostile-input hardening (VarInt/frame/string caps, trailing-byte rejection at the codec/net layer)
- [x] Chat rate limiting
- [x] Server-side reach validation on block edits
- [ ] Serverbound per-connection packet budget (implemented but not wired)
- [ ] Per-IP rate limiting / connection throttling
- [ ] Movement / fly / speed anti-cheat
- [ ] RCON + admin console
- [ ] Metrics HTTP/Prometheus endpoint (currently logs only)
- [x] Per-session packet tracing + counter/tick metrics (log-based)

## World / Chunks

- [x] Flat world generation
- [x] Real paletted chunk encoding (blocks + biome + heightmap)
- [x] Chunk streaming to full view distance (background pump)
- [x] Center-chunk follow + chunk unload/eviction
- [ ] Real terrain generation (biomes, features, caves, structures)
- [ ] Real lighting engine (currently full-bright placeholder)
- [ ] Fluids (water/lava flow, levels)
- [ ] Block & random ticks (crops, fire, leaf decay, etc.)
- [ ] Redstone
- [ ] World border
- [ ] Time of day / day-night cycle
- [ ] Weather

## Blocks / Gameplay

- [x] Block breaking (creative insta-break)
- [x] Block placing from hotbar
- [x] Shard-owned block mutation funnel
- [x] Block-change sequence ack + resync
- [x] Block-update broadcast to viewers
- [x] Block-state IDs carried on the wire (paletted)
- [~] Block state on placement (default-state only; no rotation/facing/half/waterlogging)
- [ ] Block entities (chests, signs, furnaces, …)
- [ ] Item entities (dropped items, pickup)
- [ ] Sounds
- [ ] Particles

## Inventory / Items

- [x] 1.21.8 item registry (1416 items)
- [x] Hardened trusted/untrusted slot wire types
- [x] Creative inventory set-slot
- [x] Place-from-hotbar (held item → block)
- [x] Per-player inventory + held-slot tracking (connection-local)
- [~] Container window sync (content/slot sync + creative set wired; click logic is resync-only)
- [ ] Survival inventory mutation (real clicks: move/swap/drag/split)
- [ ] Crafting (grid + recipes)
- [ ] Smelting / furnaces
- [ ] External containers / GUIs (chests, hoppers, …)
- [ ] Item durability / components / enchantments

## Entities / Mobs / Combat

- [x] Other players spawn as `minecraft:player` (type 149)
- [x] PlayerInfo add/remove + tab list
- [x] Remote player movement (position deltas / teleport)
- [x] Despawn on leave / out-of-range
- [~] Remote player rotation & head yaw (packets exist; never sent)
- [ ] Held-item & armor equipment visibility (SetEquipment)
- [ ] Mobs + AI
- [ ] Projectiles
- [ ] Combat / attack / knockback / damage
- [ ] Player health / hunger
- [ ] Experience / levels
- [ ] Death + respawn handshake
- [ ] Player abilities authority (flight enforced server-side)

## Chat / Commands

- [x] System chat rendering (TextComponent → network NBT)
- [x] Serverbound chat relay (unsigned)
- [x] Command tree (`/spawn`, `/gamemode`)
- [x] Command feedback + error reporting
- [x] Clientbound command graph + tab-complete (permission-filtered)
- [ ] Signed player chat / chat sessions (secure chat)
- [~] Plugin-registered commands (host supports; app doesn't merge them)
- [ ] Broad vanilla command set (/tp, /give, /time, /weather, /kill, …)

## Persistence

- [x] redb default backend
- [x] Player-edited chunk overlays + mutation journal (per-tick / pre-unload / shutdown flush)
- [x] Edits survive restart
- [~] Edits survive rejoin (broken for spawn-area chunks; frozen JoinKit)
- [ ] Player state persistence (position / game mode / inventory)
- [ ] Anvil import/export (skeleton crate only; not wired)
- [ ] Mutation-journal replay/recovery (journal is currently write-only)

## Plugins

- [x] In-process plugin host + lifecycle
- [x] Live block before/after events
- [x] Decision model: Deny / Replace / EmitIntents (folded + applied)
- [x] Panic isolation (fail-safe to Deny)
- [x] Permission levels + node grants (ops, spawn-protect bypass)
- [x] Sample plugins (spawn-protect, block-rules)
- [~] Mutation intents (SetBlock/Message routed; Teleport intent dropped; plugins get a null world view — can't read world state)
- [~] Dynamic cdylib loading (loads into a throwaway host; C-ABI has no event/command hooks; off by default)
- [ ] WASM plugin runtime (explicitly out of current scope)

## Presentation / Cosmetic

- [ ] Titles / subtitles / action bar
- [ ] Boss bars
- [ ] Scoreboards / teams
- [ ] Resource packs (config-phase response currently ignored)
- [ ] Player skins / properties in tab list (names only today)
- [ ] Map items

## Architecture / Scale

- [~] Actor-sharded simulation with shard-owned mutation (in place; multi-shard parallel tick lightly exercised on a single flat world)
- [ ] Cross-shard entity transfer at scale
- [ ] Distributed multi-server (architecture allows; out of current scope)
</content>
