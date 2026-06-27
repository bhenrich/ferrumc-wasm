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
// What it does today (implemented):
//   (a) Status ping        -> assert protocol === 772 and version === '1.21.8'
//   (b) Offline-mode login -> reach the 'play' state
//   (c) Wait for the first chunk column(s)
//   (d) Walk across a chunk boundary -> observe chunk LOAD + chunk UNLOAD
//   (e) Exit 0 on success, non-zero with a clear diagnostic on failure
//
// What is stubbed (clearly marked TODO below, ready to slot in):
//   - set creative-mode slot, select hotbar slot, place block, break block,
//     disconnect/reconnect, verify-persistence-after-restart.
//
// Usage:
//   node smoke.mjs [host] [port]
//   MC_HOST=127.0.0.1 MC_PORT=25565 node smoke.mjs
//   (defaults: 127.0.0.1:25565)
// =============================================================================

import mc from 'node-minecraft-protocol';

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------
const MC_VERSION = '1.21.8';
const EXPECTED_PROTOCOL = 772;
const EXPECTED_VERSION_NAME = '1.21.8';

const HOST = process.argv[2] || process.env.MC_HOST || '127.0.0.1';
const PORT = parseInt(process.argv[3] || process.env.MC_PORT || '25565', 10);
const USERNAME = process.env.MC_USERNAME || 'SmokeTester';

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
};

// ---------------------------------------------------------------------------
// Tiny logging + assertion helpers
// ---------------------------------------------------------------------------
const t0 = Date.now();
const ts = () => `+${((Date.now() - t0) / 1000).toFixed(2)}s`;
const log = (...a) => console.log(`[smoke ${ts()}]`, ...a);
const pass = (...a) => console.log(`[PASS  ${ts()}]`, ...a);
const info = (...a) => console.log(`[info  ${ts()}]`, ...a);
const fail = (...a) => console.error(`[FAIL  ${ts()}]`, ...a);

function assert(cond, message) {
  if (!cond) throw new Error(`Assertion failed: ${message}`);
}

function withTimeout(promise, ms, label) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`Timed out after ${ms}ms waiting for: ${label}`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

const chunkOf = (coord) => Math.floor(coord / 16);

// ---------------------------------------------------------------------------
// Shared client-state tracker. A single instance is threaded through the phases
// so later steps (place/break/persist) can reuse position + inventory context.
// ---------------------------------------------------------------------------
function makeState() {
  return {
    inPlay: false,
    pos: { x: 0, y: 0, z: 0 },
    havePos: false,
    loadedChunks: new Set(), // "cx,cz"
    chunkLoadEvents: 0,
    chunkUnloadEvents: 0,
    lastError: null,
    endReason: null,
  };
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

  // node-minecraft-protocol returns the raw status JSON: { version: { name, protocol }, players, description, ... }
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
// (b) Offline login -> 'play' state, plus the housekeeping a real client must
//     do to STAY connected (keep-alive replies, teleport confirmations).
// ---------------------------------------------------------------------------
function createPlayClient(state) {
  const client = mc.createClient({
    host: HOST,
    port: PORT,
    username: USERNAME,
    auth: 'offline',
    version: MC_VERSION,
    // We respond to keep-alive ourselves (below) so behaviour is identical
    // regardless of library defaults — no double-replies.
    keepAlive: false,
  });

  client.on('state', (s) => info(`client state -> ${s}`));

  // Keep-alive: clientbound { keepAliveId: i64 } -> echo back serverbound.
  client.on('keep_alive', (packet) => {
    try {
      client.write('keep_alive', { keepAliveId: packet.keepAliveId });
    } catch (e) {
      info('keep_alive reply failed:', e.message);
    }
  });

  // Server teleport (clientbound 'position'): we MUST confirm the teleport id or
  // the server keeps re-sending it / never lets us move. Also track our position.
  client.on('position', (packet) => {
    try {
      client.write('teleport_confirm', { teleportId: packet.teleportId });
    } catch (e) {
      info('teleport_confirm failed:', e.message);
    }
    // NOTE: 1.21 positions can be relative per the `flags` bitfield; FerrumC's
    // spawn teleport is absolute (flags = 0), which is all we rely on here.
    state.pos = { x: packet.x, y: packet.y, z: packet.z };
    state.havePos = true;
    info(`teleported to x=${packet.x.toFixed(1)} y=${packet.y.toFixed(1)} z=${packet.z.toFixed(1)} (confirmed id ${packet.teleportId})`);
  });

  // Chunk bookkeeping. map_chunk = column loaded; unload_chunk = column dropped.
  client.on('map_chunk', (packet) => {
    const key = `${packet.x},${packet.z}`;
    if (!state.loadedChunks.has(key)) {
      state.loadedChunks.add(key);
      state.chunkLoadEvents++;
    }
  });
  client.on('unload_chunk', (packet) => {
    const key = `${packet.chunkX},${packet.chunkZ}`;
    state.loadedChunks.delete(key);
    state.chunkUnloadEvents++;
  });

  // Failure signals.
  client.on('error', (err) => {
    state.lastError = err;
    fail('client error:', err?.message || err);
  });
  client.on('end', (reason) => {
    state.endReason = reason;
    info('connection ended:', reason);
  });
  client.on('kick_disconnect', (packet) => {
    state.endReason = `kick: ${packet?.reason}`;
    fail('kicked during play:', packet?.reason);
  });
  client.on('disconnect', (packet) => {
    state.endReason = `disconnect: ${packet?.reason}`;
    fail('disconnected:', packet?.reason);
  });

  return client;
}

function loginToPlay(client, state) {
  const reachedPlay = new Promise((resolve, reject) => {
    // The clientbound play 'login' (join game) packet means we are truly in play.
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
// (c) Wait for the first chunk column(s)
// ---------------------------------------------------------------------------
function waitForFirstChunk(client, state) {
  log('Waiting for first chunk column(s)...');
  if (state.loadedChunks.size > 0) {
    pass(`First chunk already loaded (${state.loadedChunks.size} columns)`);
    return Promise.resolve();
  }
  const firstChunk = new Promise((resolve) => {
    client.once('map_chunk', () => resolve());
  });
  return withTimeout(firstChunk, TIMEOUTS.firstChunk, 'first map_chunk').then(() => {
    pass(`First chunk received (${state.loadedChunks.size} columns loaded so far)`);
  });
}

// ---------------------------------------------------------------------------
// (d) Cross a chunk boundary -> observe new chunk LOAD + chunk UNLOAD
// ---------------------------------------------------------------------------
async function crossChunkBoundary(client, state) {
  // Make sure we have a real position to walk from.
  if (!state.havePos) {
    await withTimeout(
      new Promise((resolve) => client.once('position', () => resolve())),
      TIMEOUTS.login,
      'an initial position from the server',
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

  // Walk forward in small steps so the server accepts the movement.
  let walked = 0;
  const walker = setInterval(() => {
    if (walked >= MOVE_BLOCKS) {
      clearInterval(walker);
      return;
    }
    walked += STEP_BLOCKS;
    const x = start.x + walked;
    const z = start.z;
    state.pos.x = x;
    try {
      client.write('position_look', {
        x,
        y: state.pos.y,
        z,
        yaw: 90, // facing +X
        pitch: 0,
        flags: { onGround: true },
      });
    } catch (e) {
      info('position_look write failed:', e.message);
    }
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

// ===========================================================================
// TODO STUBS — the full end-to-end scenario slots in here.
// Each is a real signature against the verified 1.21.8 packet field layouts
// (from minecraft-data) so wiring them up is mechanical. Field shapes are
// noted inline. None of these run yet.
// ===========================================================================

// TODO(step: select hotbar slot)
// Serverbound 'held_item_slot' { slotId: i16 }  (0..8)
// eslint-disable-next-line no-unused-vars
async function selectHotbarSlot(client, slotId = 0) {
  // client.write('held_item_slot', { slotId });
  throw new Error('selectHotbarSlot: not implemented yet');
}

// TODO(step: set creative-mode slot)
// Serverbound 'set_creative_slot' { slot: i16, item: UntrustedSlot }
// `slot` is a *container* slot index (hotbar 0 == container slot 36).
// `item` is a prismarine Slot: { present: true, itemId, itemCount, ... } or
// { present: false }. Requires the player to be in creative game mode.
// eslint-disable-next-line no-unused-vars
async function setCreativeSlot(client, containerSlot, item) {
  // client.write('set_creative_slot', { slot: containerSlot, item });
  throw new Error('setCreativeSlot: not implemented yet');
}

// TODO(step: place block)
// Serverbound 'block_place' {
//   hand: varint (0 = main hand),
//   location: position { x, y, z } (the block you click ON),
//   direction: varint (face, 0=down 1=up 2=north 3=south 4=west 5=east),
//   cursorX/Y/Z: f32 (0..1 within the face),
//   insideBlock: bool, worldBorderHit: bool,
//   sequence: varint (monotonic; server echoes it in block-change acks)
// }
// eslint-disable-next-line no-unused-vars
async function placeBlock(client, location, direction = 1, sequence = 0) {
  // client.write('block_place', { hand: 0, location, direction,
  //   cursorX: 0.5, cursorY: 0.5, cursorZ: 0.5, insideBlock: false,
  //   worldBorderHit: false, sequence });
  throw new Error('placeBlock: not implemented yet');
}

// TODO(step: break block)
// Serverbound 'block_dig' {
//   status: varint (0 = start digging, 2 = finish digging),
//   location: position { x, y, z },
//   face: i8, sequence: varint
// }
// Creative breaks are instant: send status 0 then status 2 (or just 2).
// eslint-disable-next-line no-unused-vars
async function breakBlock(client, location, face = 1, sequence = 0) {
  // client.write('block_dig', { status: 0, location, face, sequence });
  // client.write('block_dig', { status: 2, location, face, sequence: sequence + 1 });
  throw new Error('breakBlock: not implemented yet');
}

// TODO(step: disconnect / reconnect)
// Cleanly end this client, then build a fresh one and re-run login->play. Used
// to prove a player can rejoin (and, with the next stub, see persisted edits).
// eslint-disable-next-line no-unused-vars
async function disconnectAndReconnect(state) {
  // client.end('smoke: reconnect');  await delay(...);  return createPlayClient(state)
  throw new Error('disconnectAndReconnect: not implemented yet');
}

// TODO(step: verify persistence after restart)
// After place/break + a SERVER RESTART (driven by run-smoke.sh, not from here),
// reconnect and assert the edited blocks come back with the expected state.
// Will need block-state reads: track 'block_change' / 'multi_block_change' and
// the map_chunk palette for the target column.
// eslint-disable-next-line no-unused-vars
async function verifyPersistenceAfterRestart(client, expectedEdits) {
  throw new Error('verifyPersistenceAfterRestart: not implemented yet');
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------
async function main() {
  log(`FerrumC black-box smoke test — target ${HOST}:${PORT}, MC ${MC_VERSION} (protocol ${EXPECTED_PROTOCOL})`);

  // (a) Status ping — no client/login needed.
  await statusPing();

  // (b)-(d) require a live play connection.
  const state = makeState();
  const client = createPlayClient(state);

  try {
    await loginToPlay(client, state);     // (b)
    await waitForFirstChunk(client, state); // (c)
    await crossChunkBoundary(client, state); // (d)

    // ---- Full end-to-end scenario continues here once the stubs above land ----
    // await selectHotbarSlot(client, 0);
    // await setCreativeSlot(client, 36, somePickaxeOrBlock);
    // const target = { x: ..., y: ..., z: ... };
    // await placeBlock(client, target, /* direction */ 1, /* sequence */ 1);
    // await breakBlock(client, target, /* face */ 1, /* sequence */ 2);
    // const reconnected = await disconnectAndReconnect(state);
    // await verifyPersistenceAfterRestart(reconnected, [{ pos: target, expect: 'air' }]);
    // -------------------------------------------------------------------------

    pass('All implemented smoke steps passed (status -> login -> chunks -> move).');
  } finally {
    try { client.end('smoke: done'); } catch { /* ignore */ }
  }
}

main()
  .then(() => {
    log('SMOKE TEST PASSED');
    // Give the socket a beat to close cleanly, then exit success.
    setTimeout(() => process.exit(0), 250);
  })
  .catch((err) => {
    fail('SMOKE TEST FAILED:', err?.message || err);
    if (err?.stack) console.error(err.stack);
    console.error('--- diagnostics ---');
    console.error(`target           : ${HOST}:${PORT}`);
    console.error(`expected         : protocol ${EXPECTED_PROTOCOL}, version ${EXPECTED_VERSION_NAME}`);
    console.error('Common causes:');
    console.error('  * server not listening on that host/port');
    console.error('  * server reports a different protocol/version');
    console.error('  * server kicked the client mid-handshake (check server logs for the matching session)');
    console.error('  * no chunks streamed / no unload on movement (chunk view-distance logic)');
    setTimeout(() => process.exit(1), 250);
  });
