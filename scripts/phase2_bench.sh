#!/usr/bin/env bash
# Phase-2 (edge resolution) benchmark — boots the working-tree binary on an
# ISOLATED RocksDB data dir but the SAME home-anchored settings.json (so API
# keys + embedding cache are shared; phase-1 re-embeds are cache hits and cheap).
# Triggers a fresh full rebuild of one repo, waits for it to finish, then prints
# the "PERF SUMMARY phase2" + "PERF SUMMARY full_rebuild" lines from the log.
#
# Usage:  scripts/phase2_bench.sh <repo_path> [port]
set -euo pipefail

REPO="${1:?usage: phase2_bench.sh <repo_path> [port]}"
PORT="${2:-7901}"
URL="http://127.0.0.1:${PORT}"

export LIBCLANG_PATH="${LIBCLANG_PATH:-/c/Program Files/LLVM/bin}"

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${DIR}/target/release/context-engine-rs.exe"
TMP_ROOT="${TMP_ROOT:-D:/projects/Python/ce_p2_tmp}"
DATA="${TMP_ROOT}/data"
LOG="${TMP_ROOT}/server.log"

# repo_id = urlsafe-base64(no-pad) of normalized (lowercased, backslash) path.
REPO_NORM="$(printf '%s' "$REPO" | tr '/' '\\' | tr 'A-Z' 'a-z')"
REPO_ID="$(printf '%s' "$REPO_NORM" | base64 | tr '+/' '-_' | tr -d '=')"

PID=""
cleanup() {
  local code=$?
  [ -n "$PID" ] && kill "$PID" 2>/dev/null || true
  sleep 2
  rm -rf "$TMP_ROOT" 2>/dev/null || true
  exit $code
}
trap cleanup EXIT

rm -rf "$TMP_ROOT" 2>/dev/null || true
mkdir -p "$DATA"

[ -x "$BIN" ] || { echo "ERROR: binary missing: $BIN" >&2; exit 1; }
echo "[p2] repo=${REPO}  repo_id=${REPO_ID}  port=${PORT}"

RUST_LOG="context_engine_rs=info,warn" "$BIN" \
  --port "$PORT" --bind 127.0.0.1 --data-dir "$DATA" >"$LOG" 2>&1 &
PID=$!

# Wait for server up.
for i in $(seq 1 60); do
  curl -fsS "${URL}/api/index-status" >/dev/null 2>&1 && break
  sleep 1
done

echo "[p2] triggering rebuild ..."
curl -fsS -X POST "${URL}/api/repos/${REPO_ID}/rebuild" >/dev/null
sleep 3

# Poll per-repo status until idle with a fresh last_indexed_at.
for i in $(seq 1 3600); do
  body="$(curl -fsS "${URL}/api/repos/${REPO_ID}/status" 2>/dev/null || true)"
  state="$(printf '%s' "$body" | grep -o '"state":"[a-z]*"' | head -1 | sed 's/.*:"//;s/"//' || true)"
  indexed_at="$(printf '%s' "$body" | grep -o '"last_indexed_at":"[^"]*"' | head -1 || true)"
  if [ "$state" = "error" ]; then echo "[p2] ERROR: $body" >&2; exit 1; fi
  if [ "$state" = "idle" ] && [ -n "$indexed_at" ]; then
    echo "[p2] done (${indexed_at})"; break
  fi
  if [ $((i % 10)) -eq 0 ]; then echo "[p2] indexing... (${i}s, state=${state:-?})"; fi
  sleep 1
done

echo "===== PERF SUMMARY lines ====="
grep -E "PERF SUMMARY (phase2|full_rebuild)" "$LOG" || true
echo "===== calls / symbol counts (for output-invariance check) ====="
grep -E "PERF SUMMARY full_rebuild" "$LOG" | tail -1

# Output-invariance digest: the call-graph endpoint reads calls.in_name/out_name
# (the v5 read path). A stable sorted digest lets us diff RELATION vs NORMAL.
echo "===== GRAPH DIGEST (sorted nodes+edges, sha) ====="
GRAPH_JSON="$(curl -fsS "${URL}/api/repos/${REPO_ID}/graph" 2>/dev/null || true)"
printf '%s' "$GRAPH_JSON" \
  | python -c "import sys,json,hashlib; d=json.load(sys.stdin); n=sorted(x.get('id','') for x in d.get('nodes',[])); e=sorted((x.get('source',''),x.get('target','')) for x in d.get('edges',[])); blob=repr((n,e)).encode(); print('nodes=%d edges=%d sha=%s'%(len(n),len(e),hashlib.sha256(blob).hexdigest()[:16]))" 2>/dev/null \
  || echo "graph digest unavailable: ${GRAPH_JSON:0:120}"

