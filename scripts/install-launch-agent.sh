#!/bin/bash
set -euo pipefail

export PATH="/opt/homebrew/bin:$PATH"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TEMPLATE="${REPO_ROOT}/launchd/com.ferrosa-memory.stack.plist.in"
TARGET_DIR="${HOME}/Library/LaunchAgents"
TARGET="${TARGET_DIR}/com.ferrosa-memory.stack.plist"
SCRIPT_PATH="${REPO_ROOT}/scripts/start-cluster.sh"

mkdir -p "${TARGET_DIR}"

sed "s|__SCRIPT_PATH__|${SCRIPT_PATH}|g" "${TEMPLATE}" > "${TARGET}"

# A plist that still carries a placeholder is not installable: launchd would
# try to exec a literal __SCRIPT_PATH__ and the job would silently never run.
# Found in the wild -- a copy of the template sat in LaunchAgents for four days
# looking like a registered job, so the login-startup it provides had quietly
# not existed since it was put there.
if grep -q "__[A-Z_]*__" "${TARGET}"; then
  echo "error: ${TARGET} still contains a placeholder after substitution" >&2
  grep -o "__[A-Z_]*__" "${TARGET}" | sort -u | sed 's/^/  /' >&2
  exit 1
fi


if launchctl print "gui/$(id -u)/com.ferrosa-memory.stack" >/dev/null 2>&1; then
    launchctl bootout "gui/$(id -u)" "${TARGET}" || true
fi

launchctl bootstrap "gui/$(id -u)" "${TARGET}"
launchctl enable "gui/$(id -u)/com.ferrosa-memory.stack"
launchctl kickstart -k "gui/$(id -u)/com.ferrosa-memory.stack"

echo "Installed LaunchAgent at ${TARGET}"
echo "Login startup is enabled for the ferrosa-memory stack."
