#!/bin/bash
# Per-minute snapshot of the fmem cluster's resource state, plus
# forensic capture on any container exit.
#
# Writes to two files:
#   /tmp/ferrosa-memory-stats.log     — one line per (timestamp, container) with
#                                        memory / CPU / restart-count / status
#   /tmp/ferrosa-memory-watchdog.log  — notable events: container deaths,
#                                        OOMKilled, restart-count changes,
#                                        host memory pressure crossings
#
# Runs via launchd (com.ferrosa-memory.watchdog.plist) every 60 seconds.
# Deliberately lightweight: three `podman inspect`/`podman stats` calls,
# so the watchdog itself can't be the source of memory pressure.

set -euo pipefail

export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:$PATH"

STATS_LOG=/tmp/ferrosa-memory-stats.log
EVENT_LOG=/tmp/ferrosa-memory-watchdog.log
STATE_DIR=/tmp/ferrosa-memory-watchdog
mkdir -p "$STATE_DIR"

# NOTE: ferrosa nodes now run natively (com.ferrosa.node* launchd jobs);
# only minio remains in podman. podman-compose names use underscores —
# the old dash-style names here silently matched nothing.
CONTAINERS=(
    ferrosa-memory_minio_1
)

TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

# --- Host memory pressure ---
# Report pages-free in MB and swap I/O. Thresholds: warn < 4 GB free,
# warn swapout deltas > 1000 pages/min.
HOST_FREE_MB=$(vm_stat | awk '/Pages free/ {gsub(/\./,""); printf "%d", $3 * 16 / 1024}')
HOST_SWAPOUT=$(vm_stat | awk '/Swapouts/ {gsub(/\./,""); print $2}')

LAST_SWAPOUT_FILE="$STATE_DIR/last_swapout"
LAST_SWAPOUT=$(cat "$LAST_SWAPOUT_FILE" 2>/dev/null || echo "$HOST_SWAPOUT")
SWAPOUT_DELTA=$((HOST_SWAPOUT - LAST_SWAPOUT))
echo "$HOST_SWAPOUT" > "$LAST_SWAPOUT_FILE"

echo "$TS HOST free_mb=$HOST_FREE_MB swapout_delta=$SWAPOUT_DELTA" >> "$STATS_LOG"
if [ "$HOST_FREE_MB" -lt 4096 ]; then
    echo "$TS WARN host_memory_low free_mb=$HOST_FREE_MB (threshold 4096)" >> "$EVENT_LOG"
fi
if [ "$SWAPOUT_DELTA" -gt 1000 ]; then
    echo "$TS WARN host_swap_thrashing delta=$SWAPOUT_DELTA pages/min" >> "$EVENT_LOG"
fi

# --- Per-container snapshot + event detection ---
for c in "${CONTAINERS[@]}"; do
    # inspect returns "status|exit_code|oom_killed|restart_count|started_at|finished_at"
    INFO=$(podman inspect -f '{{.State.Status}}|{{.State.ExitCode}}|{{.State.OOMKilled}}|{{.RestartCount}}|{{.State.StartedAt}}|{{.State.FinishedAt}}' "$c" 2>/dev/null || echo "missing||||")

    IFS='|' read -r STATUS EXIT_CODE OOM RESTARTS STARTED FINISHED <<< "$INFO"

    # Stats only make sense for running containers.
    if [ "$STATUS" = "running" ]; then
        STATS=$(podman stats --no-stream --format '{{.MemUsage}}|{{.MemPerc}}|{{.CPUPerc}}' "$c" 2>/dev/null || echo "||")
    else
        STATS="||"
    fi
    echo "$TS STATS $c status=$STATUS exit=$EXIT_CODE oom=$OOM restarts=$RESTARTS stats=$STATS" >> "$STATS_LOG"

    # Compare restart count to last observation — any change is an event.
    LAST_FILE="$STATE_DIR/$c.restarts"
    LAST_RESTARTS=$(cat "$LAST_FILE" 2>/dev/null || echo "-1")
    if [ "$RESTARTS" != "$LAST_RESTARTS" ] && [ "$LAST_RESTARTS" != "-1" ]; then
        echo "$TS EVENT $c restart_count_changed from=$LAST_RESTARTS to=$RESTARTS" >> "$EVENT_LOG"
        echo "$TS EVENT $c state status=$STATUS exit=$EXIT_CODE oom=$OOM finished=$FINISHED" >> "$EVENT_LOG"
        # Dump forensics to a per-event file so the main log stays small.
        FORENSIC="/tmp/ferrosa-memory-death-$(echo $c | tr ' ' _)-${TS//:/-}.log"
        {
            echo "=== $TS $c died (restart $LAST_RESTARTS -> $RESTARTS) ==="
            echo "# inspect"
            podman inspect "$c" 2>&1 | head -200
            echo ""
            echo "# last 200 log lines"
            podman logs --tail 200 "$c" 2>&1
        } > "$FORENSIC" 2>&1 || true
        echo "$TS EVENT $c forensics_saved path=$FORENSIC" >> "$EVENT_LOG"
    fi
    echo "$RESTARTS" > "$LAST_FILE"

    # If OOMKilled by cgroup (our mem_limit tripped), capture loudly even
    # on first observation.
    if [ "$OOM" = "true" ]; then
        OOM_FLAG_FILE="$STATE_DIR/$c.last_oom"
        LAST_OOM_FINISHED=$(cat "$OOM_FLAG_FILE" 2>/dev/null || echo "")
        if [ "$FINISHED" != "$LAST_OOM_FINISHED" ]; then
            echo "$TS ALERT $c cgroup_oom_kill finished=$FINISHED exit=$EXIT_CODE restarts=$RESTARTS" >> "$EVENT_LOG"
            echo "$FINISHED" > "$OOM_FLAG_FILE"
        fi
    fi

    # Memory-high warning: any running container at >80% of its limit.
    if [ "$STATUS" = "running" ] && [ -n "$STATS" ]; then
        PCT=$(echo "$STATS" | awk -F'|' '{gsub(/%/,"",$2); print int($2)}')
        if [ -n "$PCT" ] && [ "$PCT" -gt 80 ]; then
            echo "$TS WARN $c mem_usage_high pct=$PCT stats=$STATS" >> "$EVENT_LOG"
        fi
    fi
done

# --- Rotation: keep stats log under ~50MB by truncating from the head ---
if [ -f "$STATS_LOG" ]; then
    SIZE=$(stat -f%z "$STATS_LOG" 2>/dev/null || echo 0)
    if [ "$SIZE" -gt 52428800 ]; then
        tail -c 26214400 "$STATS_LOG" > "$STATS_LOG.tmp" && mv "$STATS_LOG.tmp" "$STATS_LOG"
    fi
fi
