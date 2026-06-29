#!/bin/bash
# Start the isolated ferrosa-memory TEST cluster (+500 port offset).
#
# Uses a separate data dir (~/data/ferrosa-memory-test/), separate container
# names, and separate keyspace (agent_memory_test). By default this refuses
# to start while the dev cluster is already running, because two local
# clusters have caused cgroup OOM kills on constrained Podman VMs.
#
# Usage:
#   scripts/start-test-cluster.sh
#   export $(scripts/start-test-cluster.sh --env)  # print env vars to stdout
#
# After cluster is healthy, apply DDLs and export env for live tests:
#   FERROSA_TEST_CQL_PORT=19542 FERROSA_TEST_GRAPH_URL=http://localhost:17974 \
#     cargo test --features live-cql

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

if [[ "${1:-}" == "--env" ]]; then
    cat <<'EOF'
FERROSA_TEST_CQL_HOST=localhost
FERROSA_TEST_CQL_PORT=19542
FERROSA_TEST_GRAPH_URL=http://localhost:17974
FERROSA_TEST_KEYSPACE=agent_memory_test
FERROSA_TEST_S3_ENDPOINT=http://localhost:19500
EOF
    exit 0
fi

# --- container engine detection ---
PODMAN="${PODMAN:-$(command -v podman 2>/dev/null || true)}"
if [[ -z "${PODMAN}" ]]; then
    PODMAN="${PODMAN:-$(command -v docker 2>/dev/null || true)}"
fi
if [[ -z "${PODMAN}" ]]; then
    echo "ERROR: no container engine found (tried podman, docker)" >&2
    exit 1
fi

# --- compose provider detection ---
if command -v podman-compose &>/dev/null; then
    COMPOSE_CMD=(podman-compose)
elif command -v docker &>/dev/null; then
    COMPOSE_CMD=(docker compose)
else
    COMPOSE_CMD=("${PODMAN}" compose)
fi

COMPOSE_FILE="${REPO_ROOT}/docker-compose.test.yml"

dev_cluster_running() {
    local container running
    for container in \
        fmem-dev-node1-1 fmem-dev-node2-1 fmem-dev-node3-1 fmem-dev-minio-1 \
        ferrosa-memory-node1-1 ferrosa-memory-node2-1 ferrosa-memory-node3-1 ferrosa-memory-minio-1
    do
        running="$("${PODMAN}" inspect -f '{{.State.Running}}' "$container" 2>/dev/null || true)"
        if [[ "${running}" == "true" ]]; then
            return 0
        fi
    done
    return 1
}

if [[ "${FERROSA_ALLOW_PARALLEL_CLUSTERS:-}" != "1" ]] && dev_cluster_running; then
    cat >&2 <<'EOF'
ERROR: dev cluster is already running.

Starting the isolated test cluster beside the dev cluster has previously
caused P0 cgroup OOM kills. Stop the dev cluster first, or set
FERROSA_ALLOW_PARALLEL_CLUSTERS=1 if you have intentionally provisioned
enough Podman VM memory for both stacks.
EOF
    exit 2
fi

echo "starting test cluster via ${COMPOSE_FILE}"
cd "${REPO_ROOT}"
"${COMPOSE_CMD[@]}" -f docker-compose.test.yml up -d

echo "waiting for CQL on localhost:19542 ..."
for _ in $(seq 1 60); do
    if bash -c '</dev/tcp/localhost/19542' 2>/dev/null; then
        echo "CQL port 19542 is up"
        break
    fi
    sleep 2
done

if ! bash -c '</dev/tcp/localhost/19542' 2>/dev/null; then
    echo "CQL port 19542 did not come up in 120s — check container logs" >&2
    exit 1
fi

echo
echo "Test cluster running. Env for tests:"
"${SCRIPT_DIR}/start-test-cluster.sh" --env
echo
echo "Apply DDLs against the test keyspace with:"
echo "  FERROSA_KEYSPACE=agent_memory_test scripts/apply-ddls.sh 19542"
echo "  (or the fmem binary will auto-migrate on first connect)"
