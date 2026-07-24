#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Install and validate the Forge CLI (`frg`) for Ferrosa Memory workflows.

Usage: scripts/install-forge.sh [options]

Options:
  --repo URL
      Forge git repository. Default: https://github.com/ferrosadb/forge.git.
  --dir PATH
      Local source checkout. Default: ~/.cache/ferrosa-memory/forge.
  --bin-dir PATH
      Directory where `frg` is installed. Default: ~/.local/bin.
  --verify-only
      Validate an existing --bin-dir/frg without fetching or building Forge.
  --help
      Show this help.

The validation starts the installed MCP server and requires both a working
`project_summary` call and a dry-run `ingest` call. It does not persist data.
EOF
}

log() {
    printf '[install-forge] %s\n' "$*"
}

die() {
    printf '[install-forge] error: %s\n' "$*" >&2
    exit 1
}

validate_forge() {
    local frg_bin="$1"
    local validation_dir
    local verify_rc=0

    command -v python3 >/dev/null 2>&1 || die "python3 is required to validate Forge capabilities"
    validation_dir="$(mktemp -d "${TMPDIR:-/tmp}/ferrosa-memory-forge-check.XXXXXX")"
    mkdir -p "$validation_dir/src"
    printf '[package]\nname = "forge-validation-fixture"\nversion = "0.1.0"\nedition = "2021"\n' >"$validation_dir/Cargo.toml"
    printf 'pub fn validation_fixture() {}\n' >"$validation_dir/src/lib.rs"

    python3 - "$frg_bin" "$validation_dir" <<'PY' || verify_rc=$?
import json
import subprocess
import sys

frg_bin, fixture_path = sys.argv[1:]

requests = [
    {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "ferrosa-memory-forge-install", "version": "0.1"},
        },
    },
    {
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": "project_summary", "arguments": {"path": fixture_path}},
    },
    {
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": "ingest", "arguments": {"path": fixture_path, "dry_run": True}},
    },
]

try:
    completed = subprocess.run(
        [frg_bin, "--mcp"],
        input="\n".join(json.dumps(request) for request in requests) + "\n",
        capture_output=True,
        text=True,
        timeout=30,
        check=False,
    )
except (OSError, subprocess.TimeoutExpired) as error:
    raise SystemExit(f"failed to start Forge MCP server: {error}") from error

if completed.returncode:
    raise SystemExit(
        f"Forge MCP server exited {completed.returncode}: {completed.stderr.strip()[:1000]}"
    )

responses = {}
for line in completed.stdout.splitlines():
    try:
        response = json.loads(line)
    except json.JSONDecodeError:
        continue
    if "id" in response:
        responses[response["id"]] = response

for request_id, capability in ((2, "project_summary"), (3, "ingest")):
    response = responses.get(request_id)
    if response is None:
        raise SystemExit(f"Forge MCP server returned no response for {capability}")
    if "error" in response:
        raise SystemExit(f"Forge {capability} failed: {response['error']}")
    result = response.get("result")
    if not isinstance(result, dict) or result.get("isError") is True:
        raise SystemExit(f"Forge {capability} returned an error result: {result}")
    if not result.get("content"):
        raise SystemExit(f"Forge {capability} returned no content")

print("validated Forge MCP project_summary and dry-run ingest")
PY

    rm -rf "$validation_dir"
    return "$verify_rc"
}

forge_repo="${FERROSA_FORGE_REPO:-https://github.com/ferrosadb/forge.git}"
forge_dir="${FERROSA_FORGE_DIR:-${HOME}/.cache/ferrosa-memory/forge}"
forge_bin_dir="${FERROSA_FORGE_BIN_DIR:-${HOME}/.local/bin}"
verify_only=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo)
            forge_repo="${2:-}"
            [[ -n "$forge_repo" ]] || die "--repo requires a value"
            shift 2
            ;;
        --dir)
            forge_dir="${2:-}"
            [[ -n "$forge_dir" ]] || die "--dir requires a value"
            shift 2
            ;;
        --bin-dir)
            forge_bin_dir="${2:-}"
            [[ -n "$forge_bin_dir" ]] || die "--bin-dir requires a value"
            shift 2
            ;;
        --verify-only)
            verify_only=true
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

frg_bin="$forge_bin_dir/frg"
if [[ "$verify_only" == true ]]; then
    [[ -x "$frg_bin" ]] || die "Forge executable not found: $frg_bin"
    validate_forge "$frg_bin" || die "Forge capability validation failed"
    log "Forge CLI is ready: $frg_bin"
    exit 0
fi

command -v git >/dev/null 2>&1 || die "git is required to fetch Forge"
command -v cargo >/dev/null 2>&1 || die "cargo is required to build Forge"

mkdir -p "$(dirname "$forge_dir")" "$forge_bin_dir"

if [[ -d "$forge_dir/.git" ]]; then
    log "updating Forge checkout at $forge_dir"
    git -C "$forge_dir" fetch --prune origin
    git -C "$forge_dir" pull --ff-only
elif [[ -e "$forge_dir" ]]; then
    die "Forge dir exists but is not a git checkout: $forge_dir"
else
    log "cloning Forge from $forge_repo to $forge_dir"
    git clone --depth 1 "$forge_repo" "$forge_dir"
fi

log "building Forge CLI"
cargo build --release --manifest-path "$forge_dir/Cargo.toml" --bin frg

frg_src="$forge_dir/target/release/frg"
[[ -x "$frg_src" ]] || die "Forge build did not produce executable: $frg_src"

install -m 0755 "$frg_src" "$frg_bin"
"$frg_bin" --version >/dev/null
validate_forge "$frg_bin" || die "Forge capability validation failed"
log "Forge CLI is ready: $frg_bin"
