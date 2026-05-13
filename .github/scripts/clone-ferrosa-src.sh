#!/usr/bin/env bash
# clone-ferrosa-src.sh — clone ferrosa source for the cluster image build,
# with a stale-branch guard.
#
# Tries each --candidate branch in order. A matched branch is kept ONLY if
# `origin/main` is reachable from its HEAD (i.e. matched branch contains
# every commit main has, possibly plus its own). Three cases this rejects:
#
#   * Strictly-behind branches (stale mirror left over from earlier work) —
#     missing main's recent fixes. Seen in CI run 25784440206 with a stale
#     `chore/integrate-pr12-plus-pr10` mirror.
#   * Divergent branches that have their own commits but never picked up
#     main's fixes — seen in CI run 25806209774 with
#     `ferrosadb/ferrosa@local/pr4` which forked May 11 (perf work) and
#     missed PR#34 (adjacency-write retry) that landed May 13.
#   * Anything else where the matched branch is not a clean superset of main.
#
# Falls back to main if no candidate branch is a main-descendant. Cross-repo
# coordination still works — push the matching branch on ferrosadb/ferrosa
# AFTER rebasing onto main, and this guard accepts it.
#
# Usage:
#   clone-ferrosa-src.sh \
#     --remote https://github.com/ferrosadb/ferrosa.git \
#     --dest   /tmp/ferrosa-src \
#     --candidate "${GITHUB_HEAD_REF:-}" \
#     --candidate "${GITHUB_REF_NAME:-}"
set -u

REMOTE=""
DEST=""
CANDIDATES=()
MAIN_BRANCH="main"

while [ $# -gt 0 ]; do
  case "$1" in
    --remote) REMOTE="$2"; shift 2 ;;
    --dest) DEST="$2"; shift 2 ;;
    --candidate) CANDIDATES+=("$2"); shift 2 ;;
    --main) MAIN_BRANCH="$2"; shift 2 ;;
    *) echo "clone-ferrosa-src.sh: unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [ -z "$REMOTE" ] || [ -z "$DEST" ]; then
  echo "clone-ferrosa-src.sh: --remote and --dest are required" >&2
  exit 2
fi

# Try each candidate branch. Stop at the first one that clones AND isn't an
# ancestor of main. Stale matches drop through to the main fallback below.
CLONED=""
# `${arr[@]}` is unbound under `set -u` when the array has never had an
# element appended. Guard with `${arr[@]+...}` so a no-candidate run flows
# straight to the main fallback.
for BRANCH in "${CANDIDATES[@]+"${CANDIDATES[@]}"}"; do
  [ -z "${BRANCH}" ] && continue
  [ "${BRANCH}" = "${MAIN_BRANCH}" ] && continue
  echo "Trying to clone ${REMOTE} @ ${BRANCH}..."
  rm -rf "${DEST}"
  # Fetch enough history (depth=50) that merge-base can compare against main.
  # depth=1 would leave HEAD detached with no ancestor info, defeating the
  # stale-branch check below.
  if git clone --depth=50 --branch "${BRANCH}" "${REMOTE}" "${DEST}" 2>/dev/null; then
    # Fetch main into the same clone so we can ancestor-compare.
    (
      cd "${DEST}" || exit 1
      git fetch --depth=50 origin "${MAIN_BRANCH}":"refs/remotes/origin/${MAIN_BRANCH}" 2>/dev/null
    )
    MATCHED_HEAD=$(cd "${DEST}" && git rev-parse HEAD)
    MAIN_HEAD=$(cd "${DEST}" && git rev-parse "refs/remotes/origin/${MAIN_BRANCH}" 2>/dev/null || echo "")
    if [ -z "${MAIN_HEAD}" ]; then
      # No view of main — can't safely compare. Fail open: keep this match.
      echo "Cloned ${BRANCH} (HEAD=${MATCHED_HEAD}) — main HEAD unavailable, skipping guard."
      CLONED="${BRANCH}"
      break
    fi
    if [ "${MATCHED_HEAD}" = "${MAIN_HEAD}" ] \
       || (cd "${DEST}" && git merge-base --is-ancestor "${MAIN_HEAD}" "${MATCHED_HEAD}" 2>/dev/null); then
      # Matched branch is a main-descendant: contains every commit main has,
      # possibly plus its own. Safe to use.
      echo "Cloned ${BRANCH} (HEAD=${MATCHED_HEAD}) — main is an ancestor, keeping."
      CLONED="${BRANCH}"
      break
    fi
    echo "Branch ${BRANCH} (HEAD=${MATCHED_HEAD}) is not a descendant of ${MAIN_BRANCH} (HEAD=${MAIN_HEAD}) — discarding and trying next candidate."
    rm -rf "${DEST}"
    continue
  fi
done

if [ -z "${CLONED}" ]; then
  echo "No live matching feature branch on ${REMOTE}; falling back to ${MAIN_BRANCH}..."
  rm -rf "${DEST}"
  git clone --depth=1 --branch "${MAIN_BRANCH}" "${REMOTE}" "${DEST}"
  CLONED="${MAIN_BRANCH}"
fi

# Sanity check: the cluster image build will COPY Cargo.lock; fail loud here
# if the cloned source doesn't have it.
if [ ! -f "${DEST}/Cargo.lock" ]; then
  echo "ERROR: ${DEST}/Cargo.lock missing — cluster image build will fail." >&2
  exit 1
fi

echo "ferrosa source: ${DEST} (branch=${CLONED})"
