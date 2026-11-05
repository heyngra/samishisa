#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
DUR=${1:-10}; CONN=${2:-50}
OHA=${OHA:-$HOME/.local/bin/oha}
HOST_BIN=target/release/samishisa-proto
WASM=target/wasm32-wasip2/release/hello_guest.wasm
URL_PATH="/hello/Ada?shout=yes"

rss_kb() { awk '/VmRSS/{print $2}' /proc/$1/status; }
wait_up() { for _ in $(seq 100); do curl -sf "http://127.0.0.1:$1/health" >/dev/null && return; sleep 0.1; done; echo "server on :$1 did not start" >&2; exit 1; }

run() {
  local label=$1 port=$2 pid=$3 idle out
  idle=$(rss_kb "$pid")
  "$OHA" -z 2s -c "$CONN" --no-tui "http://127.0.0.1:$port$URL_PATH" >/dev/null
  out=$("$OHA" -z "${DUR}s" -c "$CONN" --no-tui --output-format json "http://127.0.0.1:$port$URL_PATH")
  python3 bench_fmt.py "$out" "$label" "$idle" "$(rss_kb "$pid")"
}

echo "== $(date -I) $(uname -m), ${DUR}s x ${CONN} connections, GET $URL_PATH"
curl -s "http://127.0.0.1:3001/health" >/dev/null 2>&1 && { echo "port 3001 busy" >&2; exit 1; }


for cfg in "fresh 1" "reuse 1" "reuse 2"; do
  set -- $cfg
  $HOST_BIN --wasm $WASM --port 3002 --mode "$1" --threads "$2" 2>bench/host.log & H=$!
  wait_up 3002; run "rust $1, $2 thread(s)" 3002 $H; kill $H; wait $H 2>/dev/null || true
done
