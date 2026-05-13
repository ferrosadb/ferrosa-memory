#!/usr/bin/env bash
# Self-contained test for clone-ferrosa-src.sh. Creates a throwaway bare repo
# that mimics ferrosadb/ferrosa with:
#
#   * main             = C0 → C1 → C2          (Cargo.lock present)
#   * fresh-feature    = C0 → C1 → C2 → F1     (forked off main HEAD; main IS
#                                                ancestor — cross-repo work
#                                                that rebased onto main)
#   * stale-feature    = C0 → C1               (strictly behind main)
#   * divergent-feature= C0 → C1 → D1          (forked at C1, has its own work,
#                                                missing main's C2 — the
#                                                ferrosa-memory PR#5 case
#                                                where ferrosadb/ferrosa@local/pr4
#                                                lacked PR#34 adjacency fix)
#
# Asserts:
#   1. No candidate → falls back to main.
#   2. Candidate matches a fresh feature (main-descendant) → uses it.
#   3. Candidate matches a stale feature (strictly behind) → falls back to main.
#   4. Candidate doesn't exist on remote → falls back to main.
#   5. Stale-then-fresh → prefers fresh.
#   6. Candidate matches a divergent feature (has own commits but missing main
#      commits) → falls back to main. This is the new case — the original
#      ancestor-only check passed divergent branches.
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLONE_SCRIPT="${SCRIPT_DIR}/clone-ferrosa-src.sh"

if [ ! -x "${CLONE_SCRIPT}" ]; then
  echo "FAIL: ${CLONE_SCRIPT} not executable" >&2
  exit 1
fi

WORKDIR="$(mktemp -d)"
cleanup() { rm -rf "${WORKDIR}"; }
trap cleanup EXIT

REMOTE="${WORKDIR}/fake-ferrosa.git"
DEST="${WORKDIR}/clone-dest"

# Build the fake remote.
mkdir -p "${WORKDIR}/seed"
cd "${WORKDIR}/seed" || exit 1
git init -q -b main
git config user.email test@example.com
git config user.name test

cat > Cargo.lock <<'CL'
# fake Cargo.lock
CL
git add Cargo.lock && git commit -q -m "C0: initial"

echo "src/lib.rs body 1" > sentinel.txt
git add sentinel.txt && git commit -q -m "C1: shared base"
C1_SHA=$(git rev-parse HEAD)

git branch stale-feature "${C1_SHA}"
git branch divergent-feature "${C1_SHA}"

echo "src/lib.rs body 2 — main fix" > sentinel.txt
git add sentinel.txt && git commit -q -m "C2: main-only fix"

# `fresh-feature` forks off main HEAD AFTER C2 — main is an ancestor, so the
# feature branch contains every fix main has plus its own work. This is the
# shape a properly-rebased cross-repo coordination branch takes.
git checkout -q -b fresh-feature main
echo "src/lib.rs body 2 — feature work" > feature.txt
git add feature.txt && git commit -q -m "F1: fresh feature work"

# `divergent-feature` forked off C1 and has its own D1 commit but never picked
# up C2 — exactly the shape of `ferrosadb/ferrosa@local/pr4` on 2026-05-13
# which has perf commits + a bounded-materialization fix, but is missing
# PR#34 (adjacency-write retry) that's on main. The old ancestor-only check
# happily picked this up because matched HEAD wasn't BEHIND main; the
# downstream cluster-int job then exploded on the missing fix.
git checkout -q divergent-feature
echo "src/lib.rs body 2 — divergent work" > divergent.txt
git add divergent.txt && git commit -q -m "D1: divergent work without main fixes"

git checkout -q main

# Convert the seed into a bare remote.
git clone --bare -q . "${REMOTE}"

# Helper to run the clone script and capture which branch it chose.
run_and_grep() {
  local label="$1"; shift
  local out
  if ! out=$("${CLONE_SCRIPT}" --remote "${REMOTE}" --dest "${DEST}" "$@" 2>&1); then
    echo "FAIL: ${label} — script exited non-zero"
    echo "${out}"
    exit 1
  fi
  printf '%s\n' "${out}"
}

assert_chose() {
  local label="$1"; local expected="$2"; local out="$3"
  local last_line
  last_line=$(printf '%s\n' "${out}" | grep '^ferrosa source:' | tail -1)
  if [ -z "${last_line}" ]; then
    echo "FAIL: ${label} — no 'ferrosa source:' line in output"
    echo "${out}"
    exit 1
  fi
  if ! printf '%s\n' "${last_line}" | grep -q "branch=${expected}"; then
    echo "FAIL: ${label} — expected branch=${expected}, got: ${last_line}"
    echo "----- full output -----"
    echo "${out}"
    exit 1
  fi
  echo "PASS: ${label}"
}

# Case 1: no candidates.
rm -rf "${DEST}"
out=$(run_and_grep "no candidates" )
assert_chose "no candidates" "main" "${out}"

# Case 2: matched candidate is fresh (not ancestor of main).
rm -rf "${DEST}"
out=$(run_and_grep "fresh feature" --candidate fresh-feature)
assert_chose "fresh feature" "fresh-feature" "${out}"

# Case 3: matched candidate is stale (ancestor of main) — must drop to main.
rm -rf "${DEST}"
out=$(run_and_grep "stale feature" --candidate stale-feature)
assert_chose "stale feature drops to main" "main" "${out}"

# Case 4: candidate doesn't exist on remote.
rm -rf "${DEST}"
out=$(run_and_grep "unknown candidate" --candidate does-not-exist)
assert_chose "unknown candidate falls through" "main" "${out}"

# Case 5: multiple candidates, first stale, second fresh.
rm -rf "${DEST}"
out=$(run_and_grep "stale-then-fresh" --candidate stale-feature --candidate fresh-feature)
assert_chose "stale then fresh prefers fresh" "fresh-feature" "${out}"

# Case 6: divergent feature — has own commits BUT is missing main's recent
# fixes. The ancestor-only guard let this through and broke ferrosa-memory CI
# run 25806209774 by building a cluster image without PR#34.
rm -rf "${DEST}"
out=$(run_and_grep "divergent feature drops to main" --candidate divergent-feature)
assert_chose "divergent feature drops to main" "main" "${out}"

echo
echo "All cases passed."
