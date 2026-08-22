#!/usr/bin/env bash
# Stage and tar a ferrosa-memory release for one target triple.
#
# Produces:  dist/ferrosa-memory-${GITHUB_REF_NAME}-<target>.tar.gz
#
# Top-level layout inside the tarball (no wrapper directory):
#   ferrosa-memory
#   ferrosa-memory-mcp
#   fmem                                 (enrolment CLI)
#   memory-sync                          (control listener)
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
# fmem writes the per-system device key that everything else authenticates with,
# so it is required on every target. Hard error, like the two above: a release
# that quietly ships fewer binaries than the last one is how a missing tool is
# discovered by a user instead of by CI.
if [[ ! -x "${BIN_DIR}/fmem" ]]; then
  echo "ERROR: missing fmem enrolment CLI at ${BIN_DIR}/fmem" >&2
  exit 1
fi

# memory-sync answers the live control sessions a phone opens. Required on
# every target: a hosted memory on linux is the case where remote control
# matters most, and a binary that exists on one platform and not another is a
# support question nobody wants.
if [[ ! -x "${BIN_DIR}/memory-sync" ]]; then
  echo "ERROR: missing memory-sync control listener at ${BIN_DIR}/memory-sync" >&2
  echo "       Built with: cargo build -p ferrosa-memory-sync --features webrtc-transport" >&2
  exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

cp "${BIN_DIR}/ferrosa-memory" "${STAGE}/ferrosa-memory"
cp "${BIN_DIR}/ferrosa-memory-mcp" "${STAGE}/ferrosa-memory-mcp"
cp "${BIN_DIR}/fmem" "${STAGE}/fmem"
chmod 755 "${STAGE}/ferrosa-memory"
chmod 755 "${STAGE}/ferrosa-memory-mcp"
chmod 755 "${STAGE}/fmem"

cp "${BIN_DIR}/memory-sync" "${STAGE}/memory-sync"
chmod 755 "${STAGE}/memory-sync"

# Prove the feature flag was on. A memory-sync built without webrtc-transport is
# the right name and the wrong binary, and the failure it causes is a phone
# timing out after thirty seconds with nothing naming the listener. Cheap here;
# expensive there.
#
# Only where the binary can be EXECUTED. macos-14 builds aarch64-apple-darwin
# natively, so the check runs; the musl targets are cross-compiled and running
# them on the builder would fail for the wrong reason. Skipped loudly rather
# than silently, so nobody reads its silence as a pass.
case "$TARGET" in
  *-apple-darwin)
    if ! "${STAGE}/memory-sync" control-listen --help >/dev/null 2>&1; then
      echo "ERROR: staged memory-sync has no control-listen subcommand" >&2
      echo "       It was built without --features webrtc-transport" >&2
      exit 1
    fi
    ;;
  *)
    echo "NOTE: ${TARGET} is cross-compiled; control-listen presence not executed here" >&2
    ;;
esac

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
