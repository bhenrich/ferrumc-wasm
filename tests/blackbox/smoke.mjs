#!/usr/bin/env node
// =============================================================================
// FerrumC v2 — INDEPENDENT black-box protocol smoke test
// Minecraft Java 1.21.8 / protocol 772
//
// This script speaks to a *running* FerrumC server using PrismarineJS
// node-minecraft-protocol — a client we did NOT write. That independence is the
// whole point: a self-written fake client can "pass" against the same packet
// code that produced it while a real vanilla client rejects the bytes. See
// README.md for the full rationale.
//
// The block-state oracle is equally independent: placed-block state ids are
// decoded with PrismarineJS prismarine-block, and streamed chunks are parsed
// with prismarine-chunk. So when we assert "the log the server placed has
// axis=x", that fact is read back through a third-party decoder, not ours.
//
// Full end-to-end scenario (all implemented):
//   (a) Status ping        -> assert protocol === 772 and version === '1.21.8'
//   (b) Offline-mode login -> reach the 'play' state
//   (c) Wait for the first chunk column(s)
//   (d) Walk across a chunk boundary -> observe chunk LOAD + chunk UNLOAD
//   (e) Set creative hotbar slots (set_creative_slot) for stateful blocks
//   (f) Place STATEFUL blocks (log axis-from-face, slab half-from-cursor,
//       stairs facing-from-yaw) and assert the BlockUpdate + sequence Ack the
//       server returns carry the expected, state-bearing block-state id
//   (g) Break a block (PlayerAction dig) -> assert BlockUpdate to air + Ack
//   (h) Cross-chunk edit: move to another chunk, place a block, confirm
//   (i) Disconnect + reconnect -> verify the placed edits stream back in
//   (j) RESTART PERSISTENCE: stop the server PROCESS, start it again on the
//       same world dir, reconnect, verify the pattern persisted to disk
//
// (j) — and the server lifecycle around it — only runs in "managed" mode, when
// this script owns the server process (MC_MANAGE_SERVER=1, the run-smoke.sh
// default). In "external" mode (default when run by hand against a server you
// started yourself) steps (a)-(i) run and (j) is reported as SKIPPED, because
// we cannot restart a process we do not own.
//
// Usage:
//   node smoke.mjs [host] [port]                 # external server you started
//   MC_HOST=127.0.0.1 MC_PORT=25565 node smoke.mjs
//   MC_MANAGE_SERVER=1 node smoke.mjs            # spawn/stop/restart the server
//   (managed mode needs MC_SERVER_BIN or a built ../../server/target/debug/ferrumc)
// =============================================================================

import net from 'node:net';
import os from 'node:os';
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

import mc from 'minecraft-protocol';
import mcData from 'minecraft-data';
import mcRegistry from 'prismarine-registry';
import ChunkLoader from 'prismarine-chunk';
import BlockLoader from 'prismarine-block';
import Vec3pkg from 'vec3';

const Vec3 = Vec3pkg.Vec3 || Vec3pkg.default || Vec3pkg;
const __dirname = path.dirname(fileURLToPath(import.meta.url));

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------
const MC_VERSION = '1.21.8';
const EXPECTED_PROTOCOL = 772;
const EXPECTED_VERSION_NAME = '1.21.8';

// "Managed" mode: this script owns the server process and can restart it (the
// only way to prove on-disk persistence). Default OFF so the by-hand invocation
// stays a pure client against a server you started.
const MANAGE = ['1', 'true', 'yes'].includes(String(process.env.MC_MANAGE_SERVER || '').toLowerCase());

const HOST = process.argv[2] || process.env.MC_HOST || '127.0.0.1';
// Managed mode picks a non-default port so it never collides with a real server
// a developer may already be running on 25565.
const PORT = parseInt(process.argv[3] || process.env.MC_PORT || (MANAGE ? '25599' : '25565'), 10);
const USERNAME = process.env.MC_USERNAME || 'SmokeTester';

// Managed-mode server lifecycle knobs.
const SERVER_BIN = process.env.MC_SERVER_BIN ||
  path.resolve(__dirname, '../../server/target/debug/ferrumc');
const RUN_DIR = process.env.MC_RUN_DIR || path.join(os.tmpdir(), 'ferrumc-smoke-run');
const WORLD_DIR = path.join(RUN_DIR, 'world');
const CONFIG_PATH = path.join(RUN_DIR, 'config.toml');

// Movement tuning for the chunk-boundary crossing. Servers (and any future
// anti-cheat) reject teleport-sized jumps, so we step gradually. Default walk
// distance spans ~3 chunks so the view window is guaranteed to shift, forcing
// both new chunk LOADs at the leading edge and UNLOADs at the trailing edge.
const MOVE_BLOCKS = Number(process.env.MC_MOVE_BLOCKS || 48);
const STEP_BLOCKS = Number(process.env.MC_STEP_BLOCKS || 0.5);
const STEP_MS = Number(process.env.MC_STEP_MS || 50);

// Per-phase timeouts (ms).
const TIMEOUTS = {
  ping: 10_000,
  login: 20_000,
  firstChunk: 20_000,
  crossBoundary: 60_000,
  edit: 10_000,
  serverStart: 60_000,
  serverStop: 30_000,
};

// ---------------------------------------------------------------------------
// Independent block-state oracle (prismarine-*). Decodes a global block-state
// id into { name, props } the exact way a vanilla client would, so our
// assertions never lean on FerrumC's own encoding.
// ---------------------------------------------------------------------------
const REGISTRY = mcRegistry(MC_VERSION);
const Block = BlockLoader(REGISTRY);
const Chunk = ChunkLoader(REGISTRY);
const DATA = mcData(MC_VERSION);

/** Decode a block-state id to its block name and string-valued properties. */
function decodeState(stateId) {
  const b = Block.fromStateId(stateId, 0);
  return { name: b.name, props: b.getProperties ? b.getProperties() : {} };
}

/** Numeric item id for an item resource name (e.g. 'oak_log'). */
function itemId(name) {
  const it = DATA.itemsByName[name];
  if (!it) throw new Error(`unknown item: ${name}`);
  return it.id;
}

/** Default (lowest) block-state id for a block name (e.g. 'stone' -> 1). */
function defaultStateId(blockName) {
  const b = DATA.blocksByName[blockName];
  if (!b) throw new Error(`unknown block: ${blockName}`);
  return b.minStateId;
}

// ---------------------------------------------------------------------------
// Tiny logging + assertion helpers
// ---------------------------------------------------------------------------
const t0 = Date.now();
const ts = () => `+${((Date.now() - t0) / 1000).toFixed(2)}s`;
const log = (...a) => console.log(`[smoke ${ts()}]`, ...a);
const pass = (...a) => console.log(`[PASS  ${ts()}]`, ...a);
const info = (...a) => console.log(`[info  ${ts()}]`, ...a);
const skip = (...a) => console.log(`[SKIP  ${ts()}]`, ...a);
const fail = (...a) => console.error(`[FAIL  ${ts()}]`, ...a);

function assert(cond, message) {
  if (!cond) throw new Error(`Assertion failed: ${message}`);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function withTimeout(promise, ms, label) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`Timed out after ${ms}ms waiting for: ${label}`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** Poll `fn` until it returns truthy or `ms` elapses. State updates from packet
 *  handlers are synchronous, so polling is a simple, race-free way to wait. */
async function waitUntil(fn, ms, label) {
  const deadline = Date.now() + ms;
  for (;;) {
    if (fn()) return;
    if (Date.now() > deadline) throw new Error(`Timed out after ${ms}ms waiting for: ${label}`);
    await sleep(25);
  }
}

const chunkOf = (coord) => Math.floor(coord / 16);
const blockKey = (x, y, z) => `${x},${y},${z}`;

// Monotonic sequence stamps for block actions; the server echoes each one back
// in an acknowledge_player_digging packet.
let SEQ = 0;
const nextSeq = () => ++SEQ;

// ---------------------------------------------------------------------------
// Shared client-state tracker. A fresh instance is created per connection so a
// reconnect starts with an empty world view (proving the server re-streams it).
// ---------------------------------------------------------------------------
function makeState() {
  return {
    inPlay: false,
    pos: { x: 0, y: 0, z: 0 },
    yaw: 0,
    havePos: false,
    loadedChunks: new Set(), // "cx,cz"
    chunkLoadEvents: 0,
    chunkUnloadEvents: 0,
    columns: new Map(), // "cx,cz" -> prismarine-chunk column
    blockChanges: new Map(), // "x,y,z" -> latest state id from block_change
    acks: new Set(), // sequence ids the server acknowledged
    lastError: null,
    endReason: null,
    expectClose: false, // set before an intentional disconnect/restart so the
                        // socket teardown is not reported as a failure
  };
}

/** Intentionally end a connection without the teardown looking like a failure. */
function endClient(client, state, reason) {
  state.expectClose = true;
  try { client.end(reason); } catch { /* ignore */ }
}

// World reads, via the independent prismarine-chunk parser.
function columnLoaded(state, [wx, , wz]) {
  return state.columns.has(`${wx >> 4},${wz >> 4}`);
}
function readBlock(state, [wx, wy, wz]) {
  const col = state.columns.get(`${wx >> 4},${wz >> 4}`);
  if (!col) return { name: 'NO_COLUMN', props: {} };
  const lx = ((wx % 16) + 16) % 16;
  const lz = ((wz % 16) + 16) % 16;
  const b = col.getBlock(new Vec3(lx, wy, lz));
  return { name: b.name, props: b.getProperties ? b.getProperties() : {} };
}

// ---------------------------------------------------------------------------
// Serverbound packet helpers (verified against minecraft-data 1.21.8 layouts).
// ---------------------------------------------------------------------------
function selectSlot(client, slotId) {
  client.write('held_item_slot', { slotId });
}

function setCreativeSlot(client, containerSlot, item) {
  // UntrustedSlot with no components. `slot` is a *container* index (hotbar N
  // == container slot 36 + N). Requires the player to be in creative mode.
  client.write('set_creative_slot', {
    slot: containerSlot,
    item: {
      itemCount: 1,
      itemId: item,
      addedComponentCount: 0,
      removedComponentCount: 0,
      components: [],
      removeComponents: [],
    },
  });
}

function lookYaw(client, state, yaw) {
  // The server derives stair/facing placement from the player's most recent
  // yaw, so we set it explicitly (absolute) before a facing-sensitive place.
  state.yaw = yaw;
  client.write('position_look', {
    x: state.pos.x, y: state.pos.y, z: state.pos.z,
    yaw, pitch: 0, flags: { onGround: true },
  });
}

function placeBlock(client, location, direction, sequence, cursor = { x: 0.5, y: 0.5, z: 0.5 }) {
  client.write('block_place', {
    hand: 0, location, direction,
    cursorX: cursor.x, cursorY: cursor.y, cursorZ: cursor.z,
    insideBlock: false, worldBorderHit: false, sequence,
  });
}

function digBlock(client, location, status, sequence, face = 1) {
  client.write('block_dig', { status, location, face, sequence });
}

// ---------------------------------------------------------------------------
// (b) Offline login -> 'play' state, plus the housekeeping a real client must
//     do to STAY connected (keep-alive replies, teleport confirmations), and
//     the world-tracking a real client does to render (chunk parse + block
//     updates). createPlayClient is called once per connection.
// ---------------------------------------------------------------------------
function createPlayClient(state) {
  const client = mc.createClient({
    host: HOST,
    port: PORT,
    username: USERNAME,
    auth: 'offline',
    version: MC_VERSION,
    keepAlive: false, // we reply ourselves below, identical regardless of lib defaults
  });

  client.on('state', (s) => info(`client state -> ${s}`));

  client.on('keep_alive', (packet) => {
    try { client.write('keep_alive', { keepAliveId: packet.keepAliveId }); }
    catch (e) { info('keep_alive reply failed:', e.message); }
  });

  // Server teleport: confirm the id (or the server re-sends it / blocks movement)
  // and track our absolute position. FerrumC's spawn teleport is absolute.
  client.on('position', (packet) => {
    try { client.write('teleport_confirm', { teleportId: packet.teleportId }); }
    catch (e) { info('teleport_confirm failed:', e.message); }
    state.pos = { x: packet.x, y: packet.y, z: packet.z };
    state.havePos = true;
    info(`teleported to x=${packet.x.toFixed(1)} y=${packet.y.toFixed(1)} z=${packet.z.toFixed(1)} (confirmed id ${packet.teleportId})`);
  });

  // Chunk bookkeeping + independent parse into a readable world model.
  client.on('map_chunk', (packet) => {
    const key = `${packet.x},${packet.z}`;
    if (!state.loadedChunks.has(key)) {
      state.loadedChunks.add(key);
      state.chunkLoadEvents++;
    }
    try {
      const col = new Chunk({ x: packet.x, z: packet.z });
      col.load(packet.chunkData, true, false);
      state.columns.set(key, col);
    } catch (e) {
      info(`chunk parse failed for ${key}:`, e.message);
    }
  });
  client.on('unload_chunk', (packet) => {
    const key = `${packet.chunkX},${packet.chunkZ}`;
    state.loadedChunks.delete(key);
    state.columns.delete(key);
    state.chunkUnloadEvents++;
  });

  // Block updates (single + bulk) and the sequence acks for our edits.
  client.on('block_change', (packet) => {
    const { x, y, z } = packet.location;
    state.blockChanges.set(blockKey(x, y, z), packet.type);
    // keep the in-session world view consistent with the authoritative update
    const col = state.columns.get(`${x >> 4},${z >> 4}`);
    if (col) {
      try { col.setBlockStateId(new Vec3(((x % 16) + 16) % 16, y, ((z % 16) + 16) % 16), packet.type); }
      catch { /* ignore */ }
    }
  });
  client.on('acknowledge_player_digging', (packet) => {
    state.acks.add(packet.sequenceId);
  });

  // Failure signals. Once we've asked to close (intentional disconnect, or a
  // managed server restart), socket teardown is expected — log it as info.
  client.on('error', (err) => {
    state.lastError = err;
    if (state.expectClose) info('socket closed during shutdown:', err?.message || err);
    else fail('client error:', err?.message || err);
  });
  client.on('end', (reason) => { state.endReason = reason; info('connection ended:', reason); });
  client.on('kick_disconnect', (packet) => {
    state.endReason = `kick: ${packet?.reason}`;
    if (!state.expectClose) fail('kicked during play:', packet?.reason);
  });
  client.on('disconnect', (packet) => {
    state.endReason = `disconnect: ${packet?.reason}`;
    if (!state.expectClose) fail('disconnected:', packet?.reason);
  });

  return client;
}

function loginToPlay(client, state) {
  const reachedPlay = new Promise((resolve, reject) => {
    client.once('login', (packet) => {
      state.inPlay = true;
      info(`join game: entityId=${packet?.entityId} dimension=${packet?.worldName ?? packet?.dimension}`);
      resolve(packet);
    });
    client.once('error', reject);
    client.once('kick_disconnect', (p) => reject(new Error(`kicked before play: ${p?.reason}`)));
    client.once('end', (r) => reject(new Error(`connection ended before play: ${r}`)));
  });

  log(`Logging in (offline) as "${USERNAME}" -> reaching 'play' state...`);
  return withTimeout(reachedPlay, TIMEOUTS.login, "'play' state").then((p) => {
    pass("Login OK — reached 'play' state");
    return p;
  });
}

// ---------------------------------------------------------------------------
// (a) Status ping
// ---------------------------------------------------------------------------
async function statusPing() {
  log(`Status ping -> ${HOST}:${PORT} (version ${MC_VERSION})`);
  const res = await withTimeout(
    mc.ping({ host: HOST, port: PORT, version: MC_VERSION }),
    TIMEOUTS.ping,
    'status ping response',
  );
  const name = res?.version?.name;
  const protocol = res?.version?.protocol;
  info('Status response version block:', JSON.stringify(res?.version));
  assert(protocol === EXPECTED_PROTOCOL, `protocol number is ${protocol}, expected ${EXPECTED_PROTOCOL}`);
  assert(
    typeof name === 'string' && name.includes(EXPECTED_VERSION_NAME),
    `version string is "${name}", expected to contain "${EXPECTED_VERSION_NAME}"`,
  );
  pass(`Status ping OK — version "${name}", protocol ${protocol}`);
  return res;
}

// ---------------------------------------------------------------------------
// (c) Wait for the first chunk column(s)
// ---------------------------------------------------------------------------
function waitForFirstChunk(client, state) {
  log('Waiting for first chunk column(s)...');
  if (state.loadedChunks.size > 0) {
    pass(`First chunk already loaded (${state.loadedChunks.size} columns)`);
    return Promise.resolve();
  }
  const firstChunk = new Promise((resolve) => client.once('map_chunk', () => resolve()));
  return withTimeout(firstChunk, TIMEOUTS.firstChunk, 'first map_chunk').then(() => {
    pass(`First chunk received (${state.loadedChunks.size} columns loaded so far)`);
  });
}

// ---------------------------------------------------------------------------
// (d) Cross a chunk boundary -> observe new chunk LOAD + chunk UNLOAD
// ---------------------------------------------------------------------------
async function crossChunkBoundary(client, state) {
  if (!state.havePos) {
    await withTimeout(
      new Promise((resolve) => client.once('position', () => resolve())),
      TIMEOUTS.login, 'an initial position from the server',
    );
  }

  const start = { ...state.pos };
  const startChunks = new Set(state.loadedChunks);
  const baselineUnloads = state.chunkUnloadEvents;
  const startCx = chunkOf(start.x);
  const startCz = chunkOf(start.z);
  log(`Crossing chunk boundary: walking +X ${MOVE_BLOCKS} blocks from chunk (${startCx},${startCz}) at ` +
      `x=${start.x.toFixed(1)} z=${start.z.toFixed(1)} in ${STEP_BLOCKS}-block steps`);

  const done = new Promise((resolve) => {
    const check = () => {
      const sawNewLoad = [...state.loadedChunks].some((k) => !startChunks.has(k));
      const sawUnload = state.chunkUnloadEvents > baselineUnloads;
      if (sawNewLoad && sawUnload) resolve({ sawNewLoad, sawUnload });
    };
    client.on('map_chunk', check);
    client.on('unload_chunk', check);
  });

  let walked = 0;
  const walker = setInterval(() => {
    if (walked >= MOVE_BLOCKS) { clearInterval(walker); return; }
    walked += STEP_BLOCKS;
    const x = start.x + walked;
    const z = start.z;
    state.pos.x = x;
    try {
      client.write('position_look', { x, y: state.pos.y, z, yaw: 90, pitch: 0, flags: { onGround: true } });
    } catch (e) { info('position_look write failed:', e.message); }
  }, STEP_MS);

  try {
    await withTimeout(done, TIMEOUTS.crossBoundary, 'chunk LOAD + UNLOAD after crossing boundary');
  } finally {
    clearInterval(walker);
  }

  const endCx = chunkOf(state.pos.x);
  const endCz = chunkOf(state.pos.z);
  pass(
    `Chunk streaming OK — crossed (${startCx},${startCz}) -> (${endCx},${endCz}); ` +
    `${state.chunkLoadEvents} load events, ${state.chunkUnloadEvents} unload events, ` +
    `${state.loadedChunks.size} columns currently loaded`,
  );
}

// ---------------------------------------------------------------------------
// Edit primitives with assertions on the server's authoritative response.
// ---------------------------------------------------------------------------

/**
 * Place a block and assert the server's BlockUpdate + sequence Ack reflect the
 * expected, state-bearing block-state id. The state id is decoded by
 * prismarine-block (independent oracle), then its properties are checked.
 */
async function placeStateful(client, state, o) {
  if (o.item !== undefined) { setCreativeSlot(client, 36 + o.slot, itemId(o.item)); await sleep(150); }
  if (o.slot !== undefined) selectSlot(client, o.slot);
  if (o.yaw !== undefined) { lookYaw(client, state, o.yaw); await sleep(150); }

  const seq = nextSeq();
  placeBlock(client, o.clickLoc, o.dir, seq, o.cursor);

  const [ex, ey, ez] = o.expectLoc;
  const key = blockKey(ex, ey, ez);
  await waitUntil(
    () => state.blockChanges.has(key) && state.acks.has(seq),
    TIMEOUTS.edit, `block_change@${key} + ack ${seq} for ${o.label}`,
  );

  const stateId = state.blockChanges.get(key);
  const got = decodeState(stateId);
  assert(got.name === o.expectName,
    `${o.label}: server placed ${got.name} (state ${stateId}) at ${key}, expected ${o.expectName}`);
  for (const [k, v] of Object.entries(o.expectProps || {})) {
    assert(got.props[k] === v,
      `${o.label}: ${o.expectName}@${key} prop ${k}=${JSON.stringify(got.props[k])}, expected ${JSON.stringify(v)} (state ${stateId})`);
  }
  pass(`${o.label}: ${got.name} ${JSON.stringify(got.props)} @ ${key} (state ${stateId}, ack seq ${seq})`);
  return { stateId, ...got };
}

/** Break a block (creative insta-mine) and assert the BlockUpdate->air + Ack. */
async function breakAndVerify(client, state, o) {
  const [bx, by, bz] = o.loc;
  const key = blockKey(bx, by, bz);
  const seq = nextSeq();
  digBlock(client, { x: bx, y: by, z: bz }, 0, seq); // status 0 = start (insta in creative)
  await waitUntil(
    () => state.blockChanges.get(key) === 0 && state.acks.has(seq),
    TIMEOUTS.edit, `block_change->air@${key} + ack ${seq} for ${o.label}`,
  );
  const got = decodeState(state.blockChanges.get(key));
  assert(got.name === 'air', `${o.label}: ${key} became ${got.name}, expected air`);
  pass(`${o.label}: ${key} -> air (state 0, ack seq ${seq})`);
}

/**
 * Verify a persisted pattern by reading streamed chunks through prismarine-chunk.
 * Used after a reconnect and after a full server restart.
 */
async function verifyPattern(state, pattern, label) {
  await waitUntil(
    () => pattern.every((r) => columnLoaded(state, r.pos)),
    TIMEOUTS.firstChunk,
    `${label}: chunks covering the pattern`,
  );
  let allOk = true;
  for (const r of pattern) {
    const got = readBlock(state, r.pos);
    let ok = got.name === r.name;
    for (const [k, v] of Object.entries(r.props || {})) if (got.props[k] !== v) ok = false;
    if (ok) {
      pass(`${label}: ${r.name}${r.props ? ' ' + JSON.stringify(r.props) : ''} @ [${r.pos}] present`);
    } else {
      fail(`${label}: @ [${r.pos}] expected ${r.name} ${JSON.stringify(r.props || {})}, got ${got.name} ${JSON.stringify(got.props)}`);
      allOk = false;
    }
  }
  assert(allOk, `${label}: one or more blocks did not match the expected pattern`);
  pass(`${label}: all ${pattern.length} pattern blocks verified`);
}

// ---------------------------------------------------------------------------
// (e)-(h) Build a recognizable, state-bearing pattern near spawn + one block in
// a far chunk. Returns the pattern (positions + expected decoded blocks) for
// later persistence verification.
//
// Geometry (flat creative world: grass top at y=63, air at y=64, player feet at
// spawn y=64). We anchor a base cube and side-click it so each stateful block
// lands in an empty cell with a clear, non-default property:
//   base  stone     @ (10,64,8)  click grass (10,63,8) top
//   log   oak_log   @ (11,64,8)  click base east face  -> axis=x   (non-default)
//   slab  oak_slab  @ (10,64,9)  click base south face cy=0.9 -> type=top (non-default)
//   stair oak_stairs@ (10,64,7)  yaw=0, click base north face cy=0.1 -> facing=south, half=bottom
//   then BREAK the slab -> (10,64,9) becomes air
// ---------------------------------------------------------------------------
async function buildAndVerifyEdits(client, state) {
  await waitUntil(() => state.havePos, TIMEOUTS.login, 'spawn position before editing');
  log(`Editing near spawn (player at x=${state.pos.x.toFixed(1)} z=${state.pos.z.toFixed(1)})...`);

  // (e) creative slots for the stateful blocks (slot 0 already holds stone).
  log('Setting creative hotbar slots (oak_log, oak_slab, oak_stairs)...');

  // base stone — proves item->block + simple-cube placement + the Ack path.
  await placeStateful(client, state, {
    label: 'place base stone', slot: 0,
    clickLoc: { x: 10, y: 63, z: 8 }, dir: 1, // click grass top
    expectLoc: [10, 64, 8], expectName: 'stone',
  });

  // (f) log: axis from the clicked face (east -> x, the non-default axis).
  await placeStateful(client, state, {
    label: 'place oak_log (axis from face)', slot: 1, item: 'oak_log',
    clickLoc: { x: 10, y: 64, z: 8 }, dir: 5, // east face of base
    expectLoc: [11, 64, 8], expectName: 'oak_log', expectProps: { axis: 'x' },
  });

  // (f) slab: top/bottom from cursor-Y on a side face (cy=0.9 -> top half).
  await placeStateful(client, state, {
    label: 'place oak_slab (half from cursor)', slot: 2, item: 'oak_slab',
    clickLoc: { x: 10, y: 64, z: 8 }, dir: 3, cursor: { x: 0.5, y: 0.9, z: 0.5 }, // south face, upper half
    expectLoc: [10, 64, 9], expectName: 'oak_slab', expectProps: { type: 'top' },
  });

  // (f) stairs: facing from yaw (yaw 0 -> south), half from cursor (cy=0.1 -> bottom).
  await placeStateful(client, state, {
    label: 'place oak_stairs (facing from yaw)', slot: 3, item: 'oak_stairs', yaw: 0,
    clickLoc: { x: 10, y: 64, z: 8 }, dir: 2, cursor: { x: 0.5, y: 0.1, z: 0.5 }, // north face, lower half
    expectLoc: [10, 64, 7], expectName: 'oak_stairs', expectProps: { facing: 'south', half: 'bottom' },
  });

  // (g) break the slab -> air (proves the dig path + that a break is durable).
  await breakAndVerify(client, state, { label: 'break oak_slab', loc: [10, 64, 9] });

  pass('Set-slot + stateful place + break all verified against the server response.');

  // The spawn-chunk portion of the persistence pattern.
  return [
    { pos: [10, 64, 8], name: 'stone' },
    { pos: [11, 64, 8], name: 'oak_log', props: { axis: 'x' } },
    { pos: [10, 64, 7], name: 'oak_stairs', props: { facing: 'south', half: 'bottom' } },
    { pos: [10, 64, 9], name: 'air' }, // broken slab stays gone
  ];
}

// Walk the player forward (+X) to a target X in small, server-acceptable steps.
async function walkForwardTo(client, state, targetX) {
  await waitUntil(() => state.havePos, TIMEOUTS.login, 'position before walking');
  while (state.pos.x < targetX) {
    state.pos.x = Math.min(targetX, state.pos.x + STEP_BLOCKS);
    try {
      client.write('position_look', {
        x: state.pos.x, y: state.pos.y, z: state.pos.z, yaw: 90, pitch: 0, flags: { onGround: true },
      });
    } catch (e) { info('walk write failed:', e.message); }
    await sleep(STEP_MS);
  }
}

// (h) cross-chunk edit: walk the player well past the spawn chunk, then place a
// block on the ground in reach — proving the edit funnel works away from spawn
// and lands in a chunk that must be loaded/streamed independently.
async function crossChunkEdit(client, state) {
  // Walk into chunk >= 2 so a "2 blocks back" placement is still a non-spawn chunk.
  const targetX = 40; // chunk 2 (32..47)
  log(`Cross-chunk edit: walking to x=${targetX} (chunk ${chunkOf(targetX)})...`);
  await walkForwardTo(client, state, targetX);

  const fx = Math.floor(state.pos.x) - 2; // 2 blocks back, comfortably in reach
  const fy = 64;
  const fz = Math.floor(state.pos.z);
  const cx = chunkOf(fx);
  log(`Cross-chunk edit: placing stone at [${fx},${fy},${fz}] (chunk ${cx},${chunkOf(fz)}, spawn is chunk 0)...`);
  assert(cx !== 0, `cross-chunk target chunk is ${cx}; expected a non-spawn chunk after crossing`);

  selectSlot(client, 0); // stone
  await placeStateful(client, state, {
    label: 'cross-chunk place stone',
    clickLoc: { x: fx, y: 63, z: fz }, dir: 1, // click grass top
    expectLoc: [fx, fy, fz], expectName: 'stone',
  });
  return { pos: [fx, fy, fz], name: 'stone' };
}

// ---------------------------------------------------------------------------
// (i)/(j) reconnect helper: fresh client + state, login, wait for play.
// ---------------------------------------------------------------------------
async function connectAndJoin() {
  const state = makeState();
  const client = createPlayClient(state);
  await loginToPlay(client, state);
  return { client, state };
}

// ===========================================================================
// Managed-mode server lifecycle. Runs the built binary directly (NOT via cargo)
// so our SIGINT reaches the server and triggers its graceful flush-on-shutdown
// (FerrumC handles ctrl_c / SIGINT only — SIGTERM/SIGKILL would skip the flush).
// ===========================================================================
let SERVER = null; // current child process handle

function waitForPort(host, port, ms) {
  const deadline = Date.now() + ms;
  return new Promise((resolve, reject) => {
    const tryOnce = () => {
      const sock = net.connect({ host, port });
      sock.once('connect', () => { sock.destroy(); resolve(); });
      sock.once('error', () => {
        sock.destroy();
        if (Date.now() > deadline) reject(new Error(`port ${host}:${port} not open within ${ms}ms`));
        else setTimeout(tryOnce, 250);
      });
    };
    tryOnce();
  });
}

function writeConfig() {
  fs.mkdirSync(RUN_DIR, { recursive: true });
  const toml =
    `bind = "${HOST}:${PORT}"\n` +
    `world_dir = "${WORLD_DIR}"\n` +
    `dashboard_enabled = false\n` + // no dashboard port to collide with
    `keep_alive_interval_ms = 10000\n`;
  fs.writeFileSync(CONFIG_PATH, toml);
}

async function startServer(label) {
  if (!fs.existsSync(SERVER_BIN)) {
    throw new Error(`server binary not found at ${SERVER_BIN}; build it first (cargo build) or set MC_SERVER_BIN`);
  }
  const logPath = path.join(RUN_DIR, `server-${label}.log`);
  const out = fs.openSync(logPath, 'a');
  log(`Starting server (${label}) -> ${SERVER_BIN} --config ${CONFIG_PATH}`);
  const child = spawn(SERVER_BIN, ['--config', CONFIG_PATH], {
    cwd: RUN_DIR,
    env: { ...process.env, RUST_LOG: process.env.RUST_LOG || 'info' },
    stdio: ['ignore', out, out],
    detached: false,
  });
  SERVER = child;
  child.on('exit', (code, sig) => { if (SERVER === child) SERVER = null; info(`server (${label}) exited code=${code} sig=${sig}`); });
  await withTimeout(waitForPort(HOST, PORT, TIMEOUTS.serverStart), TIMEOUTS.serverStart, `server (${label}) to listen`);
  pass(`Server (${label}) is listening on ${HOST}:${PORT} (log: ${logPath})`);
  return child;
}

async function stopServer(child) {
  if (!child || child.exitCode !== null) return;
  log('Stopping server gracefully (SIGINT -> flush-on-shutdown)...');
  const exited = new Promise((resolve) => child.once('exit', () => resolve()));
  child.kill('SIGINT');
  try {
    await withTimeout(exited, TIMEOUTS.serverStop, 'graceful server exit');
    pass('Server stopped gracefully (edits flushed to disk).');
  } catch (e) {
    fail('graceful stop timed out; forcing SIGKILL:', e.message);
    try { child.kill('SIGKILL'); } catch { /* ignore */ }
    await exited;
  }
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------
async function main() {
  log(`FerrumC black-box smoke test — target ${HOST}:${PORT}, MC ${MC_VERSION} (protocol ${EXPECTED_PROTOCOL})`);
  log(`mode: ${MANAGE ? 'MANAGED (script owns server lifecycle, restart-persistence enabled)' : 'EXTERNAL (server started elsewhere, restart-persistence skipped)'}`);

  if (MANAGE) {
    // Fresh world for a deterministic run; preserved across the restart below.
    if (process.env.MC_KEEP_RUN !== '1') fs.rmSync(RUN_DIR, { recursive: true, force: true });
    writeConfig();
    await startServer('initial');
  }

  // (a) status ping — no login needed.
  await statusPing();

  // (b)-(h) on the first connection: login, chunks, move, edits.
  let pattern;
  {
    const { client, state } = await connectAndJoin();
    try {
      await waitForFirstChunk(client, state);          // (c)
      const spawnPattern = await buildAndVerifyEdits(client, state); // (e)-(g)
      await crossChunkBoundary(client, state);         // (d)
      const farEdit = await crossChunkEdit(client, state); // (h)
      pattern = [...spawnPattern, farEdit];
      log(`Recognizable pattern (${pattern.length} blocks): ` +
          pattern.map((r) => `${r.name}@[${r.pos}]`).join(', '));
    } finally {
      endClient(client, state, 'smoke: phase 1 done');
    }
  }
  await sleep(500); // let the disconnect leave-save flush complete

  // (i) reconnect (same server) -> edits stream back in.
  {
    log('--- (i) DISCONNECT + RECONNECT (same server) ---');
    const { client, state } = await connectAndJoin();
    try {
      await verifyPattern(state, pattern, 'reconnect');
    } finally {
      endClient(client, state, 'smoke: reconnect verify done');
    }
  }
  await sleep(500);

  // (j) restart persistence — the kill-shot. Managed mode only.
  if (MANAGE) {
    log('--- (j) RESTART PERSISTENCE (stop server process, restart same world) ---');
    await stopServer(SERVER);
    await startServer('restarted');
    const { client, state } = await connectAndJoin();
    try {
      await verifyPattern(state, pattern, 'after-restart');
      pass('RESTART PERSISTENCE OK — the pattern survived a full server process restart.');
    } finally {
      endClient(client, state, 'smoke: restart verify done');
    }
  } else {
    skip('(j) restart persistence: requires managed mode (MC_MANAGE_SERVER=1 / run-smoke.sh).');
  }

  pass('All implemented smoke steps passed.');
}

// Ensure we never leak a managed server process.
function cleanup() {
  if (SERVER && SERVER.exitCode === null) {
    try { SERVER.kill('SIGINT'); } catch { /* ignore */ }
  }
}
process.on('exit', cleanup);
process.on('SIGINT', () => { cleanup(); process.exit(130); });
process.on('SIGTERM', () => { cleanup(); process.exit(143); });

main()
  .then(async () => {
    if (MANAGE) await stopServer(SERVER);
    log('SMOKE TEST PASSED');
    setTimeout(() => process.exit(0), 250);
  })
  .catch(async (err) => {
    fail('SMOKE TEST FAILED:', err?.message || err);
    if (err?.stack) console.error(err.stack);
    console.error('--- diagnostics ---');
    console.error(`target           : ${HOST}:${PORT}`);
    console.error(`mode             : ${MANAGE ? 'managed' : 'external'}`);
    console.error(`expected         : protocol ${EXPECTED_PROTOCOL}, version ${EXPECTED_VERSION_NAME}`);
    if (MANAGE) console.error(`server logs      : ${RUN_DIR}/server-*.log`);
    console.error('Common causes:');
    console.error('  * server not listening on that host/port');
    console.error('  * server reports a different protocol/version');
    console.error('  * server kicked the client mid-handshake (check server logs)');
    console.error('  * a placed block came back with the wrong state id (placement engine)');
    console.error('  * edits did not persist across reconnect/restart (storage flush)');
    try { if (MANAGE) await stopServer(SERVER); } catch { /* ignore */ }
    setTimeout(() => process.exit(1), 250);
  });
