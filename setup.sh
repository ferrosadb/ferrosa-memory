#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Ferrosa Memory local setup

Usage: ./setup.sh [options]

Options:
  --harness auto|all|codex|claude|hermes|generic
      Agent hooks to install. Default: auto.
  --mcp-url URL
      MCP JSON-RPC endpoint. Default: http://127.0.0.1:18765/mcp.
  --config PATH
      Ferrosa Memory config file for native service install.
      Default: .runtime/ferrosa-memory-http-18765.toml when present.
  --skip-build
      Do not build ferrosa-memory-mcp.
  --skip-service
      Do not install/restart the native service.
  --no-apply-config
      Write hook wrappers/snippets but do not patch harness config files.
  --dry-run
      Show hook changes without patching harness config files.
  --no-verify
      Skip MCP health and hook verification.
  --help
      Show this help.
EOF
}

log() {
    printf '[setup] %s\n' "$*"
}

die() {
    printf '[setup] error: %s\n' "$*" >&2
    exit 1
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$script_dir"

harness="auto"
mcp_url="${FERROSA_MEMORY_MCP_URL:-http://127.0.0.1:18765/mcp}"
config_path="${FERROSA_MEMORY_CONFIG_FILE:-${script_dir}/.runtime/ferrosa-memory-http-18765.toml}"
skip_build=false
skip_service=false
apply_config=true
dry_run=false
verify=true

while [[ $# -gt 0 ]]; do
    case "$1" in
        --harness)
            harness="${2:-}"
            [[ -n "$harness" ]] || die "--harness requires a value"
            shift 2
            ;;
        --mcp-url)
            mcp_url="${2:-}"
            [[ -n "$mcp_url" ]] || die "--mcp-url requires a value"
            shift 2
            ;;
        --config)
            config_path="${2:-}"
            [[ -n "$config_path" ]] || die "--config requires a value"
            shift 2
            ;;
        --skip-build)
            skip_build=true
            shift
            ;;
        --skip-service)
            skip_service=true
            shift
            ;;
        --no-apply-config)
            apply_config=false
            shift
            ;;
        --dry-run)
            dry_run=true
            shift
            ;;
        --no-verify)
            verify=false
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

case "$harness" in
    auto|all|codex|claude|hermes|generic) ;;
    *) die "--harness must be auto, all, codex, claude, hermes, or generic" ;;
esac

command -v python3 >/dev/null 2>&1 || die "python3 is required"

if [[ "$skip_build" == false ]]; then
    command -v cargo >/dev/null 2>&1 || die "cargo is required unless --skip-build is used"
    log "building ferrosa-memory-mcp"
    cargo build --release --package ferrosa-memory-mcp
else
    log "skipping build"
fi

if [[ "$skip_service" == false ]]; then
    if [[ "$(uname -s)" == "Darwin" ]]; then
        [[ -f "$config_path" ]] || die "config file not found: $config_path"
        log "installing/restarting macOS LaunchAgent"
        FERROSA_MEMORY_CONFIG_FILE="$config_path" scripts/install-launch-agent-mcp.sh
    else
        log "native service auto-install is only implemented for macOS in this repo"
        log "start ferrosa-memory-mcp with FERROSA_MEMORY_CONFIG=$config_path before using hooks"
    fi
else
    log "skipping service install/restart"
fi

if [[ "$verify" == true ]]; then
    health_base="${mcp_url%/mcp}"
    log "checking MCP liveness at ${health_base}/healthz/live"
    for attempt in {1..30}; do
        if curl -fsS "${health_base}/healthz/live" >/dev/null 2>&1; then
            break
        fi
        if [[ "$attempt" -eq 30 ]]; then
            die "MCP liveness check failed at ${health_base}/healthz/live"
        fi
        sleep 1
    done
else
    log "skipping MCP health verification"
fi

installer_args=(--harness "$harness" --mcp-url "$mcp_url")
if [[ "$apply_config" == false ]]; then
    installer_args+=(--no-apply-config)
fi
if [[ "$dry_run" == true ]]; then
    installer_args+=(--dry-run)
fi
if [[ "$verify" == true ]]; then
    installer_args+=(--verify)
fi

log "installing agent memory hooks"
python3 scripts/install-agent-hooks.py "${installer_args[@]}"

if [[ "$verify" == true ]]; then
    log "checking default MCP tool catalog includes ingest"
    hook_env="${HOME}/.config/ferrosa-memory/hooks/env"
    if [[ -f "$hook_env" ]]; then
        # shellcheck disable=SC1090
        . "$hook_env"
    fi
    python3 - "$mcp_url" <<'PY'
import base64
import json
import os
import sys
import urllib.request

url = sys.argv[1]

def auth_header():
    header = os.environ.get("FERROSA_MEMORY_AUTH_HEADER")
    if header:
        return header
    token = os.environ.get("FERROSA_MEMORY_MCP_AUTH")
    if token:
        return token if token.lower().startswith("basic ") else f"Basic {token}"
    user = os.environ.get("FERROSA_MEMORY_MCP_USER")
    password = os.environ.get("FERROSA_MEMORY_MCP_PASSWORD")
    if user and password:
        encoded = base64.b64encode(f"{user}:{password}".encode()).decode()
        return f"Basic {encoded}"
    return None

def request(method, params=None, ident=1):
    body = json.dumps({"jsonrpc": "2.0", "id": ident, "method": method, "params": params or {}}).encode()
    req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"}, method="POST")
    header = auth_header()
    if header:
        req.add_header("Authorization", header)
    with urllib.request.urlopen(req, timeout=10) as resp:
        parsed = json.loads(resp.read().decode())
    if "error" in parsed:
        raise SystemExit(parsed["error"])
    return parsed.get("result")

request("initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "ferrosa-memory-setup", "version": "0.1"}})
tools = request("tools/list", {}, 2).get("tools", [])
names = [tool.get("name") for tool in tools]
if "ingest" not in names:
    raise SystemExit(f"default tool list missing ingest: {names}")
print("default tools:", ", ".join(names))
PY
fi

log "done"
log "hook manifest: ${HOME}/.config/ferrosa-memory/hooks/manifest.json"
