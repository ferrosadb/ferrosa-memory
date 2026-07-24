#!/bin/bash
# Wait for podman machine to be ready, then start the ferrosa-memory stack.
# Used by the user LaunchAgent at login.

export PATH="/opt/homebrew/bin:$PATH"
export PODMAN_COMPOSE_PROVIDER="${PODMAN_COMPOSE_PROVIDER:-podman-compose}"

LOG="/tmp/ferrosa-memory-cluster.log"
exec >> "$LOG" 2>&1
echo "$(date): start-cluster.sh invoked"

PODMAN=/opt/homebrew/bin/podman
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
MAX_WAIT=120
WAITED=0

# Wait for podman machine to be running
while ! "$PODMAN" machine info >/dev/null 2>&1; do
    if [ "$WAITED" -ge "$MAX_WAIT" ]; then
        echo "$(date): podman machine not ready after ${MAX_WAIT}s, giving up"
        exit 1
    fi
    sleep 5
    WAITED=$((WAITED + 5))
done

# Ensure the machine is started
if ! "$PODMAN" machine list --format '{{.Running}}' | grep -q true; then
    echo "$(date): starting podman machine"
    "$PODMAN" machine start
fi

echo "$(date): podman ready after ${WAITED}s, starting minio only (ferrosa nodes run natively via com.ferrosa.node* launchd jobs)"
cd "$REPO_ROOT"
"$PODMAN" compose up -d minio minio-init

echo "$(date): podman compose up -d minio minio-init exited with $?"
