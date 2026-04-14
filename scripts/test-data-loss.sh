#!/bin/bash
# Automated reproduction of P0 data loss bug.
# Requires: running 3-node ferrosa cluster on ports 19042-19044, forge installed.
#
# Usage: ./scripts/test-data-loss.sh
#
# Expected: all canaries survive after each ingest step.
# Bug: canaries disappear after large ingest due to SSTable corruption.

set -euo pipefail

CQL_HOST="${FMEM_CQL_HOST:-localhost}"
CQL_PORT="${FMEM_CQL_PORT:-19042}"
TENANT="9a5f8fbf-d842-4d30-8ea5-1aa931e618a8"
SESSION="00000000-0000-0000-0000-000000000000"
NUM_CANARIES=100
PASS=true

echo "=== P0 Data Loss Reproduction Test ==="
echo "Cluster: $CQL_HOST:$CQL_PORT"
echo ""

check_canaries() {
    local step_name=$1
    local result
    result=$(python3 -c "
from cassandra.cluster import Cluster
from cassandra.policies import RoundRobinPolicy
import uuid, sys
cluster = Cluster(['$CQL_HOST'], port=$CQL_PORT, load_balancing_policy=RoundRobinPolicy(), protocol_version=4, connect_timeout=10)
session = cluster.connect('agent_memory')
t = uuid.UUID('$TENANT')
n = uuid.UUID('$SESSION')
total = session.execute('SELECT count(*) FROM agent_memory.entity_store WHERE tenant_id = %s AND session_id = %s', (t, n)).one()[0]
alive = sum(1 for i in range($NUM_CANARIES) if session.execute('SELECT entity_name FROM agent_memory.entity_store WHERE tenant_id = %s AND session_id = %s AND entity_id = %s', (t, n, uuid.uuid5(uuid.NAMESPACE_DNS, f'canary-{i}'))).one())
cluster.shutdown()
print(f'{total},{alive}')
" 2>/dev/null)

    local total=$(echo "$result" | cut -d, -f1)
    local alive=$(echo "$result" | cut -d, -f2)

    if [ "$alive" -eq "$NUM_CANARIES" ]; then
        echo "  ✓ $step_name: $total entities, canaries: $alive/$NUM_CANARIES"
    else
        echo "  ✗ $step_name: $total entities, canaries: $alive/$NUM_CANARIES — DATA LOSS DETECTED"
        PASS=false
    fi
}

# Step 1: Insert canary entities
echo "Step 1: Inserting $NUM_CANARIES canary entities..."
python3 -c "
from cassandra.cluster import Cluster
from cassandra.policies import RoundRobinPolicy
import uuid
cluster = Cluster(['$CQL_HOST'], port=$CQL_PORT, load_balancing_policy=RoundRobinPolicy(), protocol_version=4, connect_timeout=10)
session = cluster.connect('agent_memory')
t = uuid.UUID('$TENANT')
n = uuid.UUID('$SESSION')
for i in range($NUM_CANARIES):
    eid = uuid.uuid5(uuid.NAMESPACE_DNS, f'canary-{i}')
    session.execute('INSERT INTO agent_memory.entity_store (tenant_id, session_id, entity_id, entity_name, entity_type, context_snippet, confidence, state, created_at) VALUES (%s,%s,%s,%s,%s,%s,%s,%s,toTimestamp(now()))', (t, n, eid, f'CANARY_{i}', 'concept', f'canary entity {i}', 1.0, 'active'))
cluster.shutdown()
" 2>/dev/null
check_canaries "After canary insert"

# Step 2: Small ingest (ferrosa-memory)
echo ""
echo "Step 2: Ingesting ferrosa-memory (~2,800 entities)..."
frg ingest --cql "$CQL_HOST:$CQL_PORT" "$(dirname "$0")/.." > /dev/null 2>&1
check_canaries "After ferrosa-memory ingest"

# Step 3: Large ingest (ferrosa) — this triggers the bug
echo ""
echo "Step 3: Ingesting ferrosa (~11,000 entities) — THIS IS THE TRIGGER..."
if [ -d "$(dirname "$0")/../../ferrosa" ]; then
    frg ingest --cql "$CQL_HOST:$CQL_PORT" "$(dirname "$0")/../../ferrosa" > /dev/null 2>&1
    check_canaries "After ferrosa ingest (immediate)"
else
    echo "  SKIP: ../ferrosa directory not found"
fi

# Step 4: Wait for compaction and check again
echo ""
echo "Step 4: Waiting 30s for compaction..."
sleep 30
check_canaries "After 30s wait (post-compaction)"

# Step 5: Wait longer
echo ""
echo "Step 5: Waiting another 60s..."
sleep 60
check_canaries "After 90s total wait"

# Result
echo ""
echo "======================================="
if $PASS; then
    echo "RESULT: PASS — all canaries survived"
else
    echo "RESULT: FAIL — data loss detected"
    echo ""
    echo "Check ferrosa node logs for SSTable corruption:"
    echo "  podman logs ferrosa-memory_node1_1 2>&1 | grep -i 'corrupt\|skipping\|data.loss'"
fi
