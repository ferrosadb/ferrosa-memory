#!/bin/bash
set -euo pipefail

export PATH="/opt/homebrew/bin:$PATH"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TEMPLATE="${REPO_ROOT}/launchd/com.ferrosa-memory.stack.plist"
TARGET_DIR="${HOME}/Library/LaunchAgents"
TARGET="${TARGET_DIR}/com.ferrosa-memory.stack.plist"
SCRIPT_PATH="${REPO_ROOT}/scripts/start-cluster.sh"

mkdir -p "${TARGET_DIR}"

sed "s|__SCRIPT_PATH__|${SCRIPT_PATH}|g" "${TEMPLATE}" > "${TARGET}"

if launchctl print "gui/$(id -u)/com.ferrosa-memory.stack" >/dev/null 2>&1; then
    launchctl bootout "gui/$(id -u)" "${TARGET}" || true
fi

launchctl bootstrap "gui/$(id -u)" "${TARGET}"
launchctl enable "gui/$(id -u)/com.ferrosa-memory.stack"
launchctl kickstart -k "gui/$(id -u)/com.ferrosa-memory.stack"

echo "Installed LaunchAgent at ${TARGET}"
echo "Login startup is enabled for the ferrosa-memory stack."
