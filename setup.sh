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
  --mcp-user USER
      Basic-auth username for an auth-protected HTTP MCP endpoint.
      Default: $FERROSA_MEMORY_MCP_USER. Forwarded to the hook installer.
  --mcp-password PASS
      Basic-auth password for an auth-protected HTTP MCP endpoint.
      Default: $FERROSA_MEMORY_MCP_PASSWORD. Forwarded to the hook installer.
  --auth-header VALUE
      Full Authorization header value (e.g. "Basic <b64>") for the MCP
      endpoint. Default: $FERROSA_MEMORY_AUTH_HEADER. Overrides --mcp-user/pass.
  --config PATH
      Ferrosa Memory config file for native service install.
      Default: .runtime/ferrosa-memory-http-18765.toml when present.
  --skip-build
      Do not build ferrosa-memory-mcp.
  --skip-service
      Do not install/restart the native service. On Linux (no auto-install
      path here) start the server yourself, then re-run with hooks.
  --no-apply-config
      Write hook wrappers/snippets but do not patch harness config files.
  --dry-run
      Show hook changes without patching harness config files.
  --no-verify
      Skip MCP health and hook verification. Useful on Linux when you start
      the service manually after setup (pair with --skip-service).
  --help
      Show this help.

Linux note: native service auto-install is macOS-only here. On a Linux source
checkout, either start ferrosa-memory-mcp yourself first, or run with
--skip-service --no-verify and start it afterward. See
systemd/ferrosa-memory.service for a user-unit template.
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
mcp_user="${FERROSA_MEMORY_MCP_USER:-}"
mcp_password="${FERROSA_MEMORY_MCP_PASSWORD:-}"
auth_header="${FERROSA_MEMORY_AUTH_HEADER:-}"
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
        --mcp-user)
            mcp_user="${2:-}"
            [[ -n "$mcp_user" ]] || die "--mcp-user requires a value"
            shift 2
            ;;
        --mcp-password)
            mcp_password="${2:-}"
            [[ -n "$mcp_password" ]] || die "--mcp-password requires a value"
            shift 2
            ;;
        --auth-header)
            auth_header="${2:-}"
            [[ -n "$auth_header" ]] || die "--auth-header requires a value"
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

# Reconcile the per-install tenant before starting the service or generating
# hooks. This repairs releases that predate tenant provisioning: HTTP mode
# derives identity solely from the auth principal, while the hooks must send
# that same tenant on every per-turn ingest.
tenant_id=""
if [[ -f "$config_path" ]]; then
    auth_file=$(awk '
        /^\[server\]$/ { in_server=1; next }
        /^\[/ { in_server=0 }
        in_server && /^[[:space:]]*auth_file[[:space:]]*=/ {
            value=$0
            sub(/^[^=]*=[[:space:]]*/, "", value)
            gsub(/^[[:space:]\"]+|[[:space:]\"]+$/, "", value)
            print value
            exit
        }
    ' "$config_path")
    provision_args=(provision-tenant --config "$config_path")
    if [[ -n "$auth_file" ]]; then
        provision_args+=(--auth-file "$auth_file")
    fi
    log "reconciling per-install tenant and HTTP auth configuration"
    provision_output=$(target/release/ferrosa-memory "${provision_args[@]}")
    tenant_id=$(printf '%s\n' "$provision_output" | sed -n 's/^FERROSA_MEMORY_TENANT_ID=//p')
    [[ -n "$tenant_id" ]] || die "tenant provisioning produced no tenant id"
else
    log "config not found at $config_path; tenant reconciliation deferred"
fi

# Track whether THIS run actually installed/started a native service. The
# health-check loop below must only hard-fail when we installed one — otherwise
# a Linux checkout (no auto-install path) would always fail setup even though
# nothing was supposed to be listening yet.
service_installed=false
if [[ "$skip_service" == false ]]; then
    if [[ "$(uname -s)" == "Darwin" ]]; then
        [[ -f "$config_path" ]] || die "config file not found: $config_path"
        log "installing/restarting macOS LaunchAgent"
        FERROSA_MEMORY_CONFIG_FILE="$config_path" scripts/install-launch-agent-mcp.sh
        service_installed=true
    else
        log "native service auto-install is macOS-only in this repo; not starting a service"
        log "start it yourself, e.g.:"
        log "  FERROSA_MEMORY_CONFIG=$config_path target/release/ferrosa-memory-mcp"
        log "  (or install systemd/ferrosa-memory.service as a --user unit)"
    fi
else
    log "skipping service install/restart"
fi

if [[ "$verify" == true && "$service_installed" == true ]]; then
    # A service was installed here, so it must come up — fail loud if it doesn't.
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
elif [[ "$verify" == true ]]; then
    # No service was installed (e.g. Linux, or --skip-service). Best-effort probe
    # so a manually-started server is still detected, but DO NOT fail setup if it
    # isn't up yet — just tell the user how to proceed.
    health_base="${mcp_url%/mcp}"
    if curl -fsS "${health_base}/healthz/live" >/dev/null 2>&1; then
        log "MCP already live at ${health_base}/healthz/live"
    else
        log "no native service was installed and ${health_base}/healthz/live is not responding yet"
        log "start ferrosa-memory-mcp, then re-run; or pass --no-verify to skip this check"
    fi
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
# Forward HTTP credentials so the installer's auth-consistency preflight passes
# for auth-protected endpoints instead of refusing (exit 3) and aborting setup.
if [[ -n "$auth_header" ]]; then
    installer_args+=(--auth-header "$auth_header")
fi
if [[ -n "$mcp_user" ]]; then
    installer_args+=(--mcp-user "$mcp_user")
fi
if [[ -n "$mcp_password" ]]; then
    installer_args+=(--mcp-password "$mcp_password")
fi
if [[ -n "$tenant_id" ]]; then
    installer_args+=(--tenant-id "$tenant_id")
fi

log "installing agent memory hooks"
# Don't let `set -e` swallow the installer's exit code: a bare non-zero abort
# (especially exit 3 = auth required but no credentials) must produce an
# actionable message, not a silent stop.
set +e
python3 scripts/install-agent-hooks.py "${installer_args[@]}"
installer_rc=$?
set -e
if [[ "$installer_rc" -ne 0 ]]; then
    if [[ "$installer_rc" -eq 3 ]]; then
        die "hook install refused: the MCP endpoint at ${mcp_url} requires authentication but no credentials were provided. Re-run with --mcp-user USER --mcp-password PASS (or set FERROSA_MEMORY_MCP_USER/FERROSA_MEMORY_MCP_PASSWORD), or pass --auth-header 'Basic <base64>'."
    fi
    die "hook install failed (install-agent-hooks.py exit ${installer_rc})"
fi

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
