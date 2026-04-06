#!/bin/bash
# Wait for podman machine to be ready, then start the ferrosa-memory cluster.
# Used by com.ferrosa-memory.cluster LaunchAgent.

export PATH="/opt/homebrew/bin:$PATH"

LOG="/tmp/ferrosa-memory-cluster.log"
exec >> "$LOG" 2>&1
echo "$(date): start-cluster.sh invoked"

PODMAN=/opt/homebrew/bin/podman
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

echo "$(date): podman ready after ${WAITED}s, starting cluster"
cd /Users/bkearns/src/ferrosa-memory
"$PODMAN" compose up -d

echo "$(date): podman compose up -d exited with $?"
