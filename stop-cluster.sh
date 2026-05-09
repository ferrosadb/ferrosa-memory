#!/usr/bin/env bash
# stop-cluster.sh — Stop the ferrosa-memory cluster and disable auto-start
set -euo pipefail

cd "$(dirname "$0")"

echo "Stopping docker compose services..."
docker compose down

echo "Disabling ferrosa-memory systemd service..."
systemctl --user disable ferrosa-memory.service
systemctl --user stop ferrosa-memory.service 2>/dev/null || true

echo "Done. Cluster stopped and will not restart on reboot."
echo "To re-enable: systemctl --user enable --now ferrosa-memory.service"
