#!/usr/bin/env bash
# Demo setup: kill old proxies, build, start BOTH proxies (background, fresh logs),
# seed two fresh /tmp working copies. Run this ONCE, then run a.sh and b.sh in two
# other terminals.
#
#   bash demo/cost-ab/setup.sh
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1   # -> repo root

PORT_ON=47824          # pxpipe      -> b.sh (right)
PORT_OFF=47823         # passthrough -> a.sh (left, plain but logged)
LOG_ON="$HOME/.pxpipe/ab-on.jsonl"
LOG_OFF="$HOME/.pxpipe/ab-off.jsonl"
# Model under test: defaults to Fable 5 — the production default, where Opus is
# OFF. Pass a model as the first arg to ADD it to the proxy's compress scope:
#   bash setup.sh           -> Fable only (Opus off, matches production)
#   bash setup.sh opus      -> Fable + Opus 4.8   (then: a.sh opus / b.sh opus)
#   bash setup.sh sonnet|haiku|claude-...  -> Fable + that model
case "${1:-fable}" in
  fable)  MODEL=claude-fable-5 ;;
  opus)   MODEL=claude-opus-4-8 ;;
  sonnet) MODEL=claude-sonnet-4-6 ;;
  haiku)  MODEL=claude-haiku-4-5 ;;
  *)      MODEL="$1" ;;
esac
# Compress scope = Fable (production default) + the chosen model (Fable-only by default).
MODELS="claude-fable-5"; [ "$MODEL" = "claude-fable-5" ] || MODELS="claude-fable-5,$MODEL"

kill_port() { local p; p=$(lsof -ti tcp:"$1" 2>/dev/null || true); [ -n "$p" ] && kill "$p" 2>/dev/null || true; }

echo "[1/4] kill old proxies ($PORT_ON, $PORT_OFF)"
kill_port "$PORT_ON"; kill_port "$PORT_OFF"; sleep 1

echo "[2/4] build"
pnpm run build >/tmp/ab-build.log 2>&1 || { echo "  build FAILED -> /tmp/ab-build.log"; exit 1; }

echo "[3/4] start proxies (background, fresh logs)"
: >"$LOG_ON"; : >"$LOG_OFF"
PXPIPE_LOG="$LOG_ON"  PORT="$PORT_ON"  PXPIPE_MODELS="$MODELS"                  nohup node dist/node.js >/tmp/ab-on.log  2>&1 & disown
PXPIPE_LOG="$LOG_OFF" PORT="$PORT_OFF" PXPIPE_MODELS="$MODELS" PXPIPE_DISABLE=1 nohup node dist/node.js >/tmp/ab-off.log 2>&1 & disown
sleep 2

echo "[4/4] seed working copies"
node demo/cost-ab/setup.mjs >/dev/null

cat <<EOF

Ready. Proxies up: pxpipe :$PORT_ON  ·  passthrough :$PORT_OFF
Compress scope: $MODELS  (Opus is OFF by default — 'setup.sh opus' to include it; pass the SAME model to a.sh/b.sh)
(logs: $LOG_ON / $LOG_OFF ; stdout: /tmp/ab-on.log /tmp/ab-off.log)

In a browser, open the live dashboard (updates as the run goes — no commands):
  http://localhost:$PORT_ON     # pxpipe   -> "THIS SESSION — N% fewer tokens"
  http://localhost:$PORT_OFF    # plain    -> ~0% (the passthrough control)

Then, in TWO separate terminals:
  bash demo/cost-ab/a.sh        # LEFT  = normal  (interactive — you watch it)
  bash demo/cost-ab/b.sh        # RIGHT = pxpipe   (interactive)

(Optional CLI, if you don't want the browser:
  node eval/ab/savings.mjs                          # token compression, both arms
  node eval/ab/analyze.mjs $LOG_ON $LOG_OFF )
EOF
