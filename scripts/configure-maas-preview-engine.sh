#!/usr/bin/env bash
set -euo pipefail

umask 077

engine_app="${MAAS_ENGINE_APP:-maas-dev-v2-engine}"
account_id="${MAAS_ACCOUNT_ID:-}"
control_endpoint="${MAAS_CONTROL_CQL_ENDPOINT:-maas-dev-v2-control.internal:9042}"
credential_file="${MAAS_CREDENTIAL_FILE:-$HOME/.ferrosa/maas-dev-v2-control/credentials.env}"
state_dir="${MAAS_ENGINE_STATE_DIR:-$HOME/.ferrosa/maas-dev-v2-engine}"

command -v fly >/dev/null || {
  printf 'flyctl is required.\n' >&2
  exit 1
}
command -v openssl >/dev/null || {
  printf 'openssl is required.\n' >&2
  exit 1
}

if [[ -z "$account_id" ]]; then
  read -r -p "Durable DBaaS account UUID: " account_id
fi
if [[ ! "$account_id" =~ ^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[1-8][0-9a-fA-F]{3}-[89aAbB][0-9a-fA-F]{3}-[0-9a-fA-F]{12}$ ]]; then
  printf 'MAAS_ACCOUNT_ID must be a UUID.\n' >&2
  exit 1
fi
if [[ ! -f "$credential_file" ]]; then
  printf 'Control-cluster credential file does not exist: %s\n' "$credential_file" >&2
  exit 1
fi

# This file is generated locally by the control-cluster provisioner with mode
# 0600 and contains shell-safe generated hexadecimal values.
# shellcheck disable=SC1090
source "$credential_file"
: "${ADMIN_PW:?ADMIN_PW is missing from the credential file}"
: "${ENGINE_HTTP_PW:?ENGINE_HTTP_PW is missing from the credential file}"

mkdir -p "$state_dir"
chmod 700 "$state_dir"

ca_key="$state_dir/ca-key.pem"
ca_cert="$state_dir/ca-cert.pem"
tls_key="$state_dir/tls-key.pem"
tls_csr="$state_dir/tls.csr"
tls_cert="$state_dir/tls-cert.pem"
tls_ext="$state_dir/tls-ext.cnf"

if [[ ! -f "$ca_key" || ! -f "$ca_cert" ]]; then
  openssl req -x509 -newkey rsa:3072 -sha256 -days 3650 -nodes \
    -subj "/CN=Ferrosa MaaS Preview Engine CA" \
    -keyout "$ca_key" -out "$ca_cert" >/dev/null 2>&1
fi
if [[ ! -f "$tls_key" || ! -f "$tls_cert" ]]; then
  openssl req -newkey rsa:3072 -nodes \
    -subj "/CN=${engine_app}.internal" \
    -keyout "$tls_key" -out "$tls_csr" >/dev/null 2>&1
  printf 'subjectAltName=DNS:%s.internal\nextendedKeyUsage=serverAuth\n' "$engine_app" >"$tls_ext"
  openssl x509 -req -sha256 -days 825 -in "$tls_csr" \
    -CA "$ca_cert" -CAkey "$ca_key" -CAcreateserial \
    -extfile "$tls_ext" -out "$tls_cert" >/dev/null 2>&1
fi

temp_dir="$(mktemp -d)"
config_path="$temp_dir/config.toml"
auth_path="$temp_dir/http-auth.toml"
cleanup() {
  rm -f "$config_path" "$auth_path"
  rmdir "$temp_dir" 2>/dev/null || true
}
trap cleanup EXIT

toml_escape() {
  printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

admin_password="$(toml_escape "$ADMIN_PW")"
engine_password_hash="$(printf '%s' "$ENGINE_HTTP_PW" | openssl dgst -sha256 -r | awk '{print $1}')"

cat >"$auth_path" <<EOF
[[principal]]
username = "maas-gateway"
password_sha256 = "$engine_password_hash"
tenant_id = "$account_id"
EOF

cat >"$config_path" <<EOF
[server]
transport = "http"
bind_addr = "[::]"
http_port = 8765
log_level = "info"
require_tls = true
cert_path = "/etc/ferrosa-memory/tls.crt"
key_path = "/etc/ferrosa-memory/tls.key"
auth_file = "/etc/ferrosa-memory/http-auth.toml"
request_timeout_seconds = 30
rate_limit_per_minute = 600

[ferrosa]
contact_points = ["$control_endpoint"]
keyspace = "agent_memory"
replication_factor = 3
consistency = "LOCAL_QUORUM"
username = "ferrosaadmin"
password = "$admin_password"
admin_username = "ferrosaadmin"
admin_password = "$admin_password"
tls_ca_path = "/etc/ferrosa-memory/control-ca.pem"
tls_skip_hostname_verify = false

[embeddings]
# Preview-only deterministic embeddings keep MaaS functional without a paid or
# separately deployed model service. Replace this before production rollout.
provider = "synthetic"
model = "synthetic-preview-v1"
dimensions = 768

[graph]
# The current control cluster does not yet publish graph HTTP on its private
# interface. Graph-only tools therefore fail visibly; CQL-backed memory remains
# available. Do not point this at localhost from a separate Fly Machine.
http_url = "http://maas-dev-v2-control.internal:7474"
username = "ferrosaadmin"
password = "$admin_password"

[sparql]
enabled = false

[viz]
enabled = false

[judge]
enabled = false

[consolidation]
enabled = false
EOF

encode_file() {
  base64 <"$1" | tr -d '\n'
}

fly secrets import --app "$engine_app" --stage <<EOF
MEMORY_ENGINE_CONFIG_TOML=$(encode_file "$config_path")
MEMORY_ENGINE_AUTH_TOML=$(encode_file "$auth_path")
MEMORY_ENGINE_TLS_CERT=$(encode_file "$tls_cert")
MEMORY_ENGINE_TLS_KEY=$(encode_file "$tls_key")
CONTROL_CLUSTER_CA_PEM=$(encode_file "$HOME/.ferrosa/maas-dev-v2-control/ca-cert.pem")
EOF

printf 'Staged the engine config, principal, TLS identity, and control CA for %s.\n' "$engine_app"
printf 'Engine credential for the gateway is stored locally in %s (ENGINE_HTTP_PW).\n' "$credential_file"
printf 'Deploy with: fly deploy -c fly.maas-dev-v2-engine.toml --ha=false --remote-only\n'
