#!/bin/bash
set -euo pipefail

TARGET="${HOME}/Library/LaunchAgents/com.ferrosa-memory.stack.plist"
LABEL="gui/$(id -u)/com.ferrosa-memory.stack"

launchctl bootout "gui/$(id -u)" "${TARGET}" >/dev/null 2>&1 || true
rm -f "${TARGET}"

echo "Removed LaunchAgent ${LABEL}"
