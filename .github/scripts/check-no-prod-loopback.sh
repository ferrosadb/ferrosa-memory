#!/usr/bin/env bash
# P0-11 W-01: forbid hardcoded loopback CQL addresses in NON-TEST source.
#
# The original grep also caught test fixtures that LITERALLY exist to verify
# the loopback validator works (and so must hardcode "localhost:19042"). This
# checker uses awk to skip everything inside #[cfg(test)] modules.
#
# Usage: .github/scripts/check-no-prod-loopback.sh

set -eu

ROOTS=(
  crates/ferrosa-memory-core/src
  crates/ferrosa-memory-mcp/src
)

PATTERN='(localhost|127\.0\.0\.1):1904[2-4]'

found=0

for root in "${ROOTS[@]}"; do
  while IFS= read -r -d '' f; do
    # Strip everything from the first `#[cfg(test)]` onward.
    # If the test module is at file end (the canonical case for this codebase)
    # this leaves the production prefix only.
    prod_only=$(awk '/^#\[cfg\(test\)\]/ { exit } { print }' "$f")
    if matches=$(echo "$prod_only" | grep -nE "$PATTERN" 2>/dev/null); then
      echo "ERROR: hardcoded loopback CQL address in production code (p0-11 W-01):"
      echo "$matches" | sed "s|^|  $f:|"
      found=1
    fi
  done < <(find "$root" -name '*.rs' -print0)
done

if [ "$found" -ne 0 ]; then
  echo
  echo "Production code must read CQL addresses from FERROSA_CQL_PROXY_ADDR (p0-11)."
  echo "Test fixtures testing the validator are allowed and must live inside"
  echo "#[cfg(test)] blocks."
  exit 1
fi
