#!/bin/bash
set -euo pipefail

export PATH="/opt/homebrew/bin:$PATH"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TEMPLATE="${REPO_ROOT}/launchd/com.ferrosa-memory.mcp.plist"
TARGET_DIR="${HOME}/Library/LaunchAgents"
TARGET="${TARGET_DIR}/com.ferrosa-memory.mcp.plist"
BINARY_PATH="${REPO_ROOT}/target/release/ferrosa-memory-mcp"
LABEL="com.ferrosa-memory.mcp"
DOMAIN="gui/$(id -u)"

if [[ ! -x "${BINARY_PATH}" ]]; then
    echo "error: ${BINARY_PATH} not found or not executable" >&2
    echo "run: cargo build --release --package ferrosa-memory-mcp" >&2
    exit 1
fi

if [[ ! -f "${REPO_ROOT}/.runtime/ferrosa-memory-http.toml" ]]; then
    echo "error: ${REPO_ROOT}/.runtime/ferrosa-memory-http.toml not found" >&2
    exit 1
fi

mkdir -p "${TARGET_DIR}"

sed -e "s|__BINARY_PATH__|${BINARY_PATH}|g" \
    -e "s|__REPO_ROOT__|${REPO_ROOT}|g" \
    "${TEMPLATE}" > "${TARGET}"

if launchctl print "${DOMAIN}/${LABEL}" >/dev/null 2>&1; then
    launchctl bootout "${DOMAIN}" "${TARGET}" || true
fi

launchctl bootstrap "${DOMAIN}" "${TARGET}"
launchctl enable "${DOMAIN}/${LABEL}"
launchctl kickstart -k "${DOMAIN}/${LABEL}"

echo "Installed LaunchAgent at ${TARGET}"
echo "MCP HTTP server will start at login; logs at /tmp/ferrosa-memory-mcp.log"
