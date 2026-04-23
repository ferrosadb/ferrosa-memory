#!/bin/bash
set -euo pipefail

export PATH="/opt/homebrew/bin:$PATH"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TEMPLATE="${REPO_ROOT}/launchd/com.ferrosa-memory.mcp.plist"
TARGET_DIR="${HOME}/Library/LaunchAgents"
TARGET="${TARGET_DIR}/com.ferrosa-memory.mcp.plist"
DEBUG_BINARY="${REPO_ROOT}/target/debug/ferrosa-memory-mcp"
RELEASE_BINARY="${REPO_ROOT}/target/release/ferrosa-memory-mcp"
DEFAULT_CONFIG_PATH="${REPO_ROOT}/.runtime/ferrosa-memory-http-18765.toml"
BINARY_PATH="${FERROSA_MEMORY_BINARY:-}"
CONFIG_PATH="${FERROSA_MEMORY_CONFIG_FILE:-${DEFAULT_CONFIG_PATH}}"
LABEL="com.ferrosa-memory.mcp"
DOMAIN="gui/$(id -u)"

if [[ -z "${BINARY_PATH}" ]]; then
    if [[ -x "${DEBUG_BINARY}" ]]; then
        BINARY_PATH="${DEBUG_BINARY}"
    else
        BINARY_PATH="${RELEASE_BINARY}"
    fi
fi

if [[ ! -x "${BINARY_PATH}" ]]; then
    echo "error: ${BINARY_PATH} not found or not executable" >&2
    echo "run: cargo build --package ferrosa-memory-mcp" >&2
    exit 1
fi

if [[ ! -f "${CONFIG_PATH}" ]]; then
    echo "error: ${CONFIG_PATH} not found" >&2
    exit 1
fi

mkdir -p "${TARGET_DIR}"

sed -e "s|__BINARY_PATH__|${BINARY_PATH}|g" \
    -e "s|__REPO_ROOT__|${REPO_ROOT}|g" \
    -e "s|__CONFIG_PATH__|${CONFIG_PATH}|g" \
    "${TEMPLATE}" > "${TARGET}"

if launchctl print "${DOMAIN}/${LABEL}" >/dev/null 2>&1; then
    launchctl bootout "${DOMAIN}" "${TARGET}" || true
fi

launchctl bootstrap "${DOMAIN}" "${TARGET}"
launchctl enable "${DOMAIN}/${LABEL}"
launchctl kickstart -k "${DOMAIN}/${LABEL}"

echo "Installed LaunchAgent at ${TARGET}"
echo "Using binary: ${BINARY_PATH}"
echo "Using config: ${CONFIG_PATH}"
if grep -q '^require_tls = true' "${CONFIG_PATH}"; then
    WORKBENCH_URL="https://127.0.0.1:18765/"
else
    WORKBENCH_URL="http://127.0.0.1:18765/"
fi
echo "Managed MCP path: ${WORKBENCH_URL} and http://127.0.0.1:18766/viz"
echo "Logs: /tmp/ferrosa-memory-mcp.log"
