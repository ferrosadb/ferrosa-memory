#!/usr/bin/env bash
# Install the task-link pass as a periodic job.
#
# It reads the board, finds identifiers people wrote into task titles and block
# reasons — QA-0009, MAAS-T-35, another task's id — and writes them down as real
# links. Without it those cross-references only exist as prose, and the graph
# the app draws is empty for exactly the tasks that have the most to say.
#
# Periodic rather than on-write because a link is a relationship BETWEEN tasks:
# the second task of a pair creates the relationship, and nothing about writing
# the first one can know that. The right long-term home is ferrosa-memory's
# consolidation pass — the dream cycle already runs periodically and already
# exists to find connections nobody stated. That integration is tracked
# separately; this makes the pass real and periodic today.
#
#   ./scripts/install-task-links.sh            install and start
#   ./scripts/install-task-links.sh --uninstall
#
# Verify with: launchctl list | grep task-links
#         and: tail -f /tmp/ferrosa-task-links.log

set -euo pipefail

LABEL="com.ferrosa.task-links"
PLIST="$HOME/Library/LaunchAgents/${LABEL}.plist"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="$REPO/target/release/task-links"
# Every 30 minutes. Often enough that a link exists before anyone looks for it,
# rare enough that a full board scan is not a background cost worth noticing.
INTERVAL=1800

if [[ "${1:-}" == "--uninstall" ]]; then
  launchctl bootout "gui/$(id -u)/${LABEL}" 2>/dev/null || true
  rm -f "$PLIST"
  echo "removed ${LABEL}"
  exit 0
fi

# Built here rather than assumed: a plist pointing at a binary that does not
# exist fails silently every half hour, which is the same as not being
# installed except that it looks installed.
echo "building the linker…"
cargo build --release -p ferrosa-memory-sync --bin task-links --manifest-path "$REPO/Cargo.toml"
test -x "$BINARY" || { echo "the build produced no binary at $BINARY" >&2; exit 1; }

# Proved to work BEFORE being scheduled, for the same reason.
echo "checking it can reach the board…"
FORGE_CQL_HOST="${FORGE_CQL_HOST:-127.0.0.1:19042}" "$BINARY" --dry-run --quiet

mkdir -p "$(dirname "$PLIST")"
cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>${LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${BINARY}</string>
    <string>--quiet</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>FORGE_CQL_HOST</key><string>${FORGE_CQL_HOST:-127.0.0.1:19042}</string>
  </dict>
  <key>StartInterval</key><integer>${INTERVAL}</integer>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>/tmp/ferrosa-task-links.log</string>
  <key>StandardErrorPath</key><string>/tmp/ferrosa-task-links.log</string>
</dict>
</plist>
PLIST_EOF

launchctl bootout "gui/$(id -u)/${LABEL}" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
echo "installed ${LABEL}, every ${INTERVAL}s — log at /tmp/ferrosa-task-links.log"
