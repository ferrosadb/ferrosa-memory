#!/usr/bin/env bash
# What became of the pull requests a session opened.
#
# Separate from the scan: which pull requests an agent OPENED is a fact about
# the session and is read from the transcript. What happened to them afterwards
# is a fact only the forge holds, and asking it here keeps `session-claims`
# working on a local file with no network.
#
# Batched per repository — one call each, not one per pull request.
#
#   scripts/pr-outcomes.sh > /tmp/outcomes.json
set -euo pipefail

# The repositories come from the sessions, not from a list here: a hardcoded
# set silently files every merged pull request of a missing repository as a
# live claim, and the queue gives no hint why.
if [ "$#" -gt 0 ]; then
    REPOS="$*"
elif [ -n "${FERROSA_CLAIM_REPOS:-}" ]; then
    REPOS="$FERROSA_CLAIM_REPOS"
else
    echo "!! pass repositories, or pipe them from:" >&2
    echo "   session-claims --all --list-repos --tenant <uuid> --host <addr>" >&2
    exit 2
fi

command -v gh >/dev/null || { echo "!! gh is not on PATH" >&2; exit 1; }

{
    for repo in $REPOS; do
        # A repo that cannot be read is reported, not skipped silently: a
        # missing repo would file every one of its merged PRs as a live claim.
        if ! gh pr list --repo "$repo" --state all --limit 400 \
             --json url,state,mergedBy,author 2>/dev/null; then
            echo "!! could not list $repo" >&2
            echo "[]"
        fi
    done
} | python3 -c '
import json, sys
outcomes = {}
buf = sys.stdin.read()
depth = 0; start = None
# The per-repo arrays arrive concatenated; split them without a stream parser.
for i, ch in enumerate(buf):
    if ch == "[":
        if depth == 0: start = i
        depth += 1
    elif ch == "]":
        depth -= 1
        if depth == 0 and start is not None:
            for row in json.loads(buf[start:i+1]):
                who = (row.get("mergedBy") or {}).get("login") or (row.get("author") or {}).get("login")
                outcomes[row["url"]] = {"state": row["state"].lower(), "by": who}
            start = None
json.dump(outcomes, sys.stdout, indent=1)
print(f"  {len(outcomes)} pull requests", file=sys.stderr)
'
