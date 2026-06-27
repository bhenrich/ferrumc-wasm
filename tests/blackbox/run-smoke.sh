#!/usr/bin/env bash
# =============================================================================
# run-smoke.sh — driver for the FerrumC independent black-box smoke test.
#
# FLOW (what this script assumes and does):
#   1. The CALLER has ALREADY started a FerrumC server listening on $MC_PORT.
#      This script does NOT start the server (see the commented example below) —
#      keeping server lifecycle out of here avoids cargo build-lock contention
#      with other tooling and lets CI manage the process however it likes.
#   2. This script ensures the Node deps are installed, then runs smoke.mjs
#      against $MC_HOST:$MC_PORT.
#   3. Exit code is the smoke test's exit code (0 = pass).
#
# Usage:
#   ./run-smoke.sh                  # 127.0.0.1:25565
#   ./run-smoke.sh 127.0.0.1 25577  # explicit host/port
#   MC_HOST=10.0.0.5 MC_PORT=25565 ./run-smoke.sh
# =============================================================================
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

MC_HOST="${1:-${MC_HOST:-127.0.0.1}}"
MC_PORT="${2:-${MC_PORT:-25565}}"

echo "== FerrumC black-box smoke =="
echo "target: ${MC_HOST}:${MC_PORT}"

# -----------------------------------------------------------------------------
# HOW THE SERVER WOULD BE STARTED (intentionally NOT executed here).
# The caller / CI is responsible for bringing the server up first, e.g.:
#
#   # from the repo root (/Users/saad/dev/personal/apps/ferrumc):
#   cargo run --release -- --port "${MC_PORT}" &
#   SERVER_PID=$!
#   # ...wait until the port is accepting connections...
#   # then run this script, and on exit: kill "$SERVER_PID"
#
# Persistence/restart scenario (future): start server, run smoke through the
# place/break steps, stop the server, restart it on the same world dir, then run
# smoke again with the persistence-verification step enabled.
# -----------------------------------------------------------------------------

# Ensure deps are present (uses pnpm, per project convention).
if [ ! -d "node_modules/node-minecraft-protocol" ]; then
  echo "node_modules missing — installing with pnpm..."
  if ! command -v pnpm >/dev/null 2>&1; then
    echo "ERROR: pnpm not found on PATH. Install pnpm, then re-run." >&2
    exit 127
  fi
  pnpm install
fi

echo "running smoke.mjs..."
exec node smoke.mjs "$MC_HOST" "$MC_PORT"
