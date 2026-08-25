#!/usr/bin/env bash
set -euo pipefail

# Does [workspace.package] version agree with the release history?
#
# WHY THIS EXISTS
#
# promote-release.yml cuts a tag, then opens a PR to sync main's version to it.
# That PR is `continue-on-error: true` and its body says "Cosmetic; merge at
# convenience". On 2026-08-17 the branch chore/sync-version-v0.27.0 was pushed
# and `gh pr create` did not produce a PR. The failure was swallowed by design,
# nobody merged anything, and main sat at 0.23.0 while the released tag said
# v0.27.0 -- for five days, across twenty-one commits, unnoticed.
#
# The cost is not cosmetic. Binaries self-report the Cargo version, so a
# v0.28.0 release would have shipped binaries announcing 0.23.0, and every
# downstream update check compares a tag against that self-report. A version
# that disagrees with its own tag makes "are we behind?" unanswerable.
#
# WHAT IT CHECKS
#
# main declares the version of the LATEST RELEASED TAG. That is this repo's
# model, set by promote-release: the tag is cut first from Conventional Commit
# history, then main is synced to it. This check does not invent a second model;
# it enforces the one already in use.
#
# It also REPORTS what the next release would be, from the same
# next-release-version.sh the release path uses -- so the semver implied by the
# commits since the tag is visible while they are being written, not only when
# somebody cuts a release.

cd "$(git rev-parse --show-toplevel)"

declared="$(perl -0ne 'print $1 if /\[workspace\.package\][^\[]*?version\s*=\s*"([^"]+)"/s' Cargo.toml)"
if [[ -z "$declared" ]]; then
  echo "error: could not read [workspace.package] version from Cargo.toml" >&2
  exit 2
fi

latest_tag="$({ git tag --list 'v[0-9]*.[0-9]*.[0-9]*' || true; } \
  | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -n 1 || true)"

if [[ -z "$latest_tag" ]]; then
  echo "ok: no stable tag yet; nothing to be in step with (declared ${declared})"
  exit 0
fi

tag_version="${latest_tag#v}"

# Reuse the release path's own arithmetic. Computing it a second way here is how
# a check and the thing it checks come to disagree.
next="$(bash .github/scripts/next-release-version.sh auto true 2>/dev/null \
  | grep -E '^(next_version|bump|commit_count)=' || true)"
next_version="$(sed -n 's/^next_version=//p' <<<"$next")"
bump="$(sed -n 's/^bump=//p' <<<"$next")"
commits="$(sed -n 's/^commit_count=//p' <<<"$next")"

if [[ "$declared" != "$tag_version" ]]; then
  cat >&2 <<MSG
error: the workspace version and the latest release tag disagree.

  Cargo.toml [workspace.package] version = ${declared}
  latest stable tag                      = ${latest_tag}

Binaries self-report the Cargo version, so a release cut now would ship
binaries announcing ${declared} under a tag that says otherwise, and every
downstream update check compares the two.

Set it to ${tag_version}:

  perl -0pi -e 's/(\[workspace\.package\]\s+version\s*=\s*)"[^"]+"/\${1}"${tag_version}"/s' Cargo.toml
  cargo update -w

If a release is being cut right now, cut it first and sync to the NEW tag --
promote-release does this for you, but only when its sync PR is actually
merged. A pushed chore/sync-version-* branch with no PR is the failure this
check exists to catch.
MSG
  exit 1
fi

echo "ok: workspace version ${declared} matches ${latest_tag}"
if [[ -n "${commits:-}" && "${commits}" != "0" ]]; then
  echo "    ${commits} commit(s) since ${latest_tag} imply a ${bump} bump -> v${next_version}"
fi
