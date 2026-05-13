#!/usr/bin/env bash
# graph-query-probe.sh — poll a /graph/query endpoint with a read-only Cypher
# query until it returns HTTP 200, or fail loud with diagnostics.
#
# Every attempt prints its outcome (HTTP status or curl error) so a silent
# stall in the graph engine is impossible — the workflow log shows exactly
# which iteration hangs, instead of a 17-minute blank screen that forces an
# operator cancellation.
#
# Failure mode: when the budget is exhausted, dump up to 200 lines of
# container logs (via --logs-cmd) and exit 1 with a workflow ::error::.
set -u

LABEL=""
URL=""
QUERY=""
KEYSPACE=""
ATTEMPTS=30
CURL_TIMEOUT=5
SLEEP_SECONDS=2
LOGS_CMD=""

while [ $# -gt 0 ]; do
  case "$1" in
    --label) LABEL="$2"; shift 2 ;;
    --url) URL="$2"; shift 2 ;;
    --query) QUERY="$2"; shift 2 ;;
    --keyspace) KEYSPACE="$2"; shift 2 ;;
    --attempts) ATTEMPTS="$2"; shift 2 ;;
    --curl-timeout) CURL_TIMEOUT="$2"; shift 2 ;;
    --sleep) SLEEP_SECONDS="$2"; shift 2 ;;
    --logs-cmd) LOGS_CMD="$2"; shift 2 ;;
    *) echo "graph-query-probe.sh: unknown arg: $1" >&2; exit 2 ;;
  esac
done

for required in LABEL URL QUERY KEYSPACE; do
  if [ -z "${!required}" ]; then
    echo "graph-query-probe.sh: missing required --${required,,} flag" >&2
    exit 2
  fi
done

PAYLOAD=$(printf '{"query":%s,"keyspace":%s}' \
  "$(printf '%s' "$QUERY" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')" \
  "$(printf '%s' "$KEYSPACE" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')")

echo "probing ${LABEL}: ${URL} (max ${ATTEMPTS} attempts, ${CURL_TIMEOUT}s each, ${SLEEP_SECONDS}s sleep)"

START=$(date +%s)
for i in $(seq 1 "$ATTEMPTS"); do
  # -sS for silent + show-errors. -w writes %{http_code} on its own line so
  # we can distinguish HTTP 4xx/5xx (curl exits 0 here) from network errors
  # (curl exits non-zero, no http_code line).
  RESP=$(curl -sS -m "$CURL_TIMEOUT" \
    -w '\n__HTTP__:%{http_code} __TIME__:%{time_total}s\n' \
    -X POST "$URL" \
    -H 'Content-Type: application/json' \
    -d "$PAYLOAD" 2>&1) || true
  STATUS=$(printf '%s' "$RESP" | grep -oE '__HTTP__:[0-9]+' | tail -1 | cut -d: -f2)
  ELAPSED=$(( $(date +%s) - START ))
  if [ "${STATUS:-0}" = "200" ]; then
    echo "  attempt ${i}/${ATTEMPTS} [+${ELAPSED}s]: HTTP 200 — ${LABEL} live"
    exit 0
  fi
  # Show one truncated line per attempt so the workflow log advances
  # visibly. The grep removes our probe's own marker lines so the response
  # body (or curl error message) is what shows up.
  ONE_LINE=$(printf '%s' "$RESP" | grep -v '__HTTP__\|__TIME__' | tr '\n' ' ' | head -c 240)
  echo "  attempt ${i}/${ATTEMPTS} [+${ELAPSED}s]: status=${STATUS:-curl-err} ${ONE_LINE}"
  sleep "$SLEEP_SECONDS"
done

ELAPSED=$(( $(date +%s) - START ))
echo "::error::${LABEL} did not return HTTP 200 within ${ELAPSED}s (${ATTEMPTS} attempts)"
if [ -n "$LOGS_CMD" ]; then
  echo "--- last 200 lines of cluster logs ---"
  bash -c "$LOGS_CMD" 2>/dev/null | tail -200 || true
fi
exit 1
