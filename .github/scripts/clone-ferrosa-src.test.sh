#!/usr/bin/env bash
# Self-contained test for clone-ferrosa-src.sh. Creates a throwaway bare repo
# that mimics ferrosadb/ferrosa with:
#
#   * main = C0 → C1 → C2 (Cargo.lock present)
#   * fresh-feature   = C0 → C1 → F1   (forked from C1, NOT an ancestor of main)
#   * stale-feature   = C0 → C1        (one commit behind main — ancestor of main)
#
# Asserts:
#   1. No candidate → falls back to main.
#   2. Candidate matches a fresh feature → uses that feature branch.
#   3. Candidate matches a stale feature → falls back to main (the bug we fixed).
#   4. Candidate doesn't exist on remote → falls back to main.
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
git branch fresh-feature "${C1_SHA}"

echo "src/lib.rs body 2 — main fix" > sentinel.txt
git add sentinel.txt && git commit -q -m "C2: main-only fix"

git checkout -q fresh-feature
echo "src/lib.rs body 2 — feature work" > feature.txt
git add feature.txt && git commit -q -m "F1: fresh feature work"

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

echo
echo "All cases passed."
