#!/bin/bash
# Stop the isolated ferrosa-memory TEST cluster. Does NOT touch the dev
# cluster. Safe to run when the test cluster isn't running.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PODMAN="${PODMAN:-/opt/homebrew/bin/podman}"
export PODMAN_COMPOSE_PROVIDER="${PODMAN_COMPOSE_PROVIDER:-podman-compose}"

cd "${REPO_ROOT}"
"${PODMAN}" compose -f docker-compose.test.yml down
echo "test cluster stopped (dev cluster untouched)"
