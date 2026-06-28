#!/usr/bin/env bash
# =============================================================================
# run-smoke.sh — driver for the FerrumC independent black-box smoke test.
#
# Two modes:
#
#   MANAGED (default): this script builds the server, and smoke.mjs spawns it,
#   runs the full end-to-end scenario (status -> login -> chunks -> move ->
#   set-slot -> place stateful blocks -> break -> cross-chunk edit -> reconnect
#   -> STOP the server process -> restart on the same world -> verify the edits
#   persisted). This is the alpha-credibility run: a real, independent client
#   against a real, restarted server.
#
#   EXTERNAL (MC_MANAGE_SERVER=0): the CALLER has already started a server; this
#   script just installs deps and runs smoke.mjs against $MC_HOST:$MC_PORT. The
#   restart-persistence step is skipped (we don't own the process).
#
# Usage:
#   ./run-smoke.sh                     # managed: build + spawn + full run (port 25599)
#   MC_MANAGE_SERVER=0 ./run-smoke.sh 127.0.0.1 25565   # external server
#   MC_PORT=25600 ./run-smoke.sh       # managed on a custom port
#
# Env knobs: MC_HOST, MC_PORT, MC_USERNAME, MC_MOVE_BLOCKS, MC_STEP_BLOCKS,
#            MC_STEP_MS, MC_SERVER_BIN, MC_RUN_DIR, MC_KEEP_RUN, RUST_LOG.
# =============================================================================
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

SERVER_DIR="$(cd "$HERE/../../server" && pwd)"
MC_MANAGE_SERVER="${MC_MANAGE_SERVER:-1}"
MC_HOST="${1:-${MC_HOST:-127.0.0.1}}"
if [ "$MC_MANAGE_SERVER" = "1" ]; then
  MC_PORT="${2:-${MC_PORT:-25599}}"
else
  MC_PORT="${2:-${MC_PORT:-25565}}"
fi
export MC_MANAGE_SERVER MC_HOST MC_PORT

echo "== FerrumC black-box smoke =="
echo "mode  : $([ "$MC_MANAGE_SERVER" = "1" ] && echo MANAGED || echo EXTERNAL)"
echo "target: ${MC_HOST}:${MC_PORT}"

# Ensure Node deps are present (uses pnpm, per project convention).
if [ ! -d "node_modules/minecraft-protocol" ] || [ ! -d "node_modules/prismarine-chunk" ]; then
  echo "node_modules missing or incomplete — installing with pnpm..."
  if ! command -v pnpm >/dev/null 2>&1; then
    echo "ERROR: pnpm not found on PATH. Install pnpm, then re-run." >&2
    exit 127
  fi
  pnpm install
fi

# Managed mode: build the server binary up front (smoke.mjs runs it directly so
# SIGINT reaches the server, triggering its graceful flush-on-shutdown).
if [ "$MC_MANAGE_SERVER" = "1" ]; then
  if ! command -v cargo >/dev/null 2>&1; then
    echo "ERROR: cargo not found on PATH (needed to build the server in managed mode)." >&2
    exit 127
  fi
  echo "building server (cargo build) in ${SERVER_DIR}..."
  ( cd "$SERVER_DIR" && cargo build )
  export MC_SERVER_BIN="${MC_SERVER_BIN:-$SERVER_DIR/target/debug/ferrumc}"
  echo "server binary: ${MC_SERVER_BIN}"
fi

echo "running smoke.mjs..."
exec node smoke.mjs "$MC_HOST" "$MC_PORT"
