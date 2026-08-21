#!/usr/bin/env bash
# Stage and tar a ferrosa-memory release for one target triple.
#
# Produces:  dist/ferrosa-memory-${GITHUB_REF_NAME}-<target>.tar.gz
#
# Top-level layout inside the tarball (no wrapper directory):
#   ferrosa-memory
#   ferrosa-memory-mcp
#   LICENSE
#   NOTICE
#   README.md
#   config/ferrosa-memory.example.toml   (if present)
#   examples/<template>                  (HTTP/auth templates for binary installs)
#   launchd/com.ferrosa-memory.mcp.plist.in (if present)
#   systemd/ferrosa-memory.service       (if present)
#   skills/<skill>/SKILL.md              (portable agent skills, if present)

set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "ERROR: usage: $0 <target-triple> <release-bin-dir>" >&2
  exit 2
fi

TARGET="$1"
BIN_DIR="$2"
: "${GITHUB_REF_NAME:?must be set (run from a tag push)}"

REF_NAME="$GITHUB_REF_NAME"
TARBALL="dist/ferrosa-memory-${REF_NAME}-${TARGET}.tar.gz"

if [[ ! -x "${BIN_DIR}/ferrosa-memory-mcp" ]]; then
  echo "ERROR: missing ferrosa-memory-mcp binary at ${BIN_DIR}/ferrosa-memory-mcp" >&2
  exit 1
fi
if [[ ! -x "${BIN_DIR}/ferrosa-memory" ]]; then
  echo "ERROR: missing ferrosa-memory management binary at ${BIN_DIR}/ferrosa-memory" >&2
  exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

cp "${BIN_DIR}/ferrosa-memory" "${STAGE}/ferrosa-memory"
cp "${BIN_DIR}/ferrosa-memory-mcp" "${STAGE}/ferrosa-memory-mcp"
chmod 755 "${STAGE}/ferrosa-memory"
chmod 755 "${STAGE}/ferrosa-memory-mcp"

for f in LICENSE NOTICE README.md; do
  [[ -f "$f" ]] && cp "$f" "${STAGE}/$f" || echo "WARN: $f missing" >&2
done

for entry in \
    "config/ferrosa-memory.example.toml" \
    "launchd/com.ferrosa-memory.mcp.plist.in" \
    "systemd/ferrosa-memory.service"; do
  if [[ -f "$entry" ]]; then
    mkdir -p "${STAGE}/$(dirname "$entry")"
    cp "$entry" "${STAGE}/$entry"
  else
    echo "WARN: $entry not found, skipping" >&2
  fi
done

# Binary installs must retain the published HTTP/auth templates. Requiring this
# directory makes an incomplete release fail during staging rather than leaving
# quick-start users to discover the missing documentation after installation.
if [[ ! -d examples ]]; then
  echo "ERROR: examples/ not found; cannot stage binary-install templates" >&2
  exit 1
fi
mkdir -p "${STAGE}/examples"
cp -R examples/. "${STAGE}/examples/"

# Bundle the portable agent skills (a directory tree) so the installer can place
# them in the user's agent skill directory.
if [[ -d skills ]]; then
  mkdir -p "${STAGE}/skills"
  cp -R skills/. "${STAGE}/skills/"
else
  echo "WARN: skills/ not found, skipping" >&2
fi

mkdir -p dist
( cd "$STAGE" && find . -mindepth 1 -print0 | LC_ALL=C sort -z \
    | tar --null --no-recursion -czf - --files-from=- ) > "$TARBALL"

echo "Wrote ${TARBALL}"
ls -lh "$TARBALL"
tar tzf "$TARBALL"
