#!/usr/bin/env bash
# graph-write-probe.sh — poll the /graph/query endpoint with a MERGE+edge
# write until it returns HTTP 200, or fail loud with diagnostics.
#
# The query mirrors the failing test
# (public_graph_write_round_trip_for_co_occurs_edges): two Entity MERGEs
# plus a CO_OCCURS_WITH edge MERGE + SET. Fresh UUIDs per attempt so each
# iteration actually exercises the create-write path rather than a no-op
# match. Each attempt prints HTTP status/curl error so a silent hang is
# impossible — the workflow log advances every iteration.
set -u

LABEL=""
URL=""
KEYSPACE=""
ATTEMPTS=30
CURL_TIMEOUT=10
SLEEP_SECONDS=3
LOGS_CMD=""

while [ $# -gt 0 ]; do
  case "$1" in
    --label) LABEL="$2"; shift 2 ;;
    --url) URL="$2"; shift 2 ;;
    --keyspace) KEYSPACE="$2"; shift 2 ;;
    --attempts) ATTEMPTS="$2"; shift 2 ;;
    --curl-timeout) CURL_TIMEOUT="$2"; shift 2 ;;
    --sleep) SLEEP_SECONDS="$2"; shift 2 ;;
    --logs-cmd) LOGS_CMD="$2"; shift 2 ;;
    *) echo "graph-write-probe.sh: unknown arg: $1" >&2; exit 2 ;;
  esac
done

for required in LABEL URL KEYSPACE; do
  if [ -z "${!required}" ]; then
    echo "graph-write-probe.sh: missing required --${required,,} flag" >&2
    exit 2
  fi
done

echo "probing ${LABEL}: ${URL} keyspace=${KEYSPACE} (max ${ATTEMPTS} attempts, ${CURL_TIMEOUT}s each, ${SLEEP_SECONDS}s sleep)"

gen_uuid() { python3 -c 'import uuid;print(uuid.uuid4())'; }

START=$(date +%s)
for i in $(seq 1 "$ATTEMPTS"); do
  T=$(gen_uuid); S=$(gen_uuid); A=$(gen_uuid); B=$(gen_uuid)
  Q="MERGE (a:Entity {tenant_id: '${T}', session_id: '${S}', entity_id: '${A}'}) MERGE (b:Entity {tenant_id: '${T}', session_id: '${S}', entity_id: '${B}'}) MERGE (a)-[r:CO_OCCURS_WITH {tenant_id: '${T}', session_id: '${S}'}]->(b) SET r.strength = 0.5 RETURN r"
  PAYLOAD=$(printf '{"query":%s,"keyspace":%s}' \
    "$(printf '%s' "$Q" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')" \
    "$(printf '%s' "$KEYSPACE" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')")
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
