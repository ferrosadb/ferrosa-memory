#!/usr/bin/env bash
set -euo pipefail

# Generate local-only runtime files for the compose MCP service.
# Safe by default: creates missing directories/files and never deletes data.

RUNTIME_DIR="${RUNTIME_DIR:-.runtime}"
DATA_DIR="${FERROSA_MEMORY_DATA_DIR:-$HOME/data/ferrosa-memory}"
AUTH_SRC="${AUTH_SRC:-examples/http-auth.toml}"
CONFIG_PATH="$RUNTIME_DIR/ferrosa-memory-http-podman.toml"
AUTH_PATH="$RUNTIME_DIR/http-auth.toml"

printf 'Initializing Ferrosa Memory local runtime files\n'
printf 'Runtime dir: %s\n' "$RUNTIME_DIR"
printf 'Data dir:    %s\n' "$DATA_DIR"

mkdir -p "$RUNTIME_DIR"
mkdir -p "$DATA_DIR/minio" "$DATA_DIR/node1" "$DATA_DIR/node2" "$DATA_DIR/node3"

if [[ ! -f "$AUTH_PATH" ]]; then
  cp "$AUTH_SRC" "$AUTH_PATH"
  printf 'Wrote %s from %s\n' "$AUTH_PATH" "$AUTH_SRC"
else
  printf 'Keeping existing %s\n' "$AUTH_PATH"
fi

if [[ ! -f "$CONFIG_PATH" ]]; then
  cat > "$CONFIG_PATH" <<'TOML'
# Local compose/Podman config for ferrosa-memory-mcp.
# docker-compose.yml runs this service with network_mode: host, so loopback
# endpoints are the host-published Ferrosa ports. Plain HTTP is intentionally
# loopback-only; do not change bind_addr to 0.0.0.0 unless require_tls is true.

[server]
transport = "http"
bind_addr = "127.0.0.1"
http_port = 18765
public_port = 18765
log_level = "info"
require_tls = false
auth_file = "/run/secrets/ferrosa-memory/http-auth.toml"
# tenant_id intentionally omitted in HTTP mode; callers authenticate via auth_file.

[ferrosa]
contact_points = ["127.0.0.1:19042", "127.0.0.1:19043", "127.0.0.1:19044"]
keyspace = "agent_memory"
replication_factor = 3
consistency = "LOCAL_QUORUM"
username = "ferrosa_user"
password = "ferrosa_user"
admin_username = "ferrosa_admin"
admin_password = "ferrosa_admin"

[embeddings]
provider = "ollama"
ollama_base_url = "http://127.0.0.1:11434"
model = "nomic-embed-text-v2-moe"
dimensions = 768

[graph]
bolt_uri = "bolt://127.0.0.1:17687"
http_url = "http://127.0.0.1:17474"
username = "ferrosa_admin"
password = "ferrosa_admin"

[sparql]
enabled = true
http_url = "http://127.0.0.1:18080"

[viz]
enabled = false
TOML
  printf 'Wrote %s\n' "$CONFIG_PATH"
else
  printf 'Keeping existing %s\n' "$CONFIG_PATH"
fi

printf '\nRuntime files are ready. Next:\n'
printf '  make build-podman-binary\n'
printf '  docker compose up -d\n'
printf '  curl -fsS http://127.0.0.1:18765/healthz/live && echo\n'
