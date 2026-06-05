#!/usr/bin/env bash
set -euo pipefail

run_supervisor=false
if [[ "${1:-}" == "--run-supervisor" ]]; then
  run_supervisor=true
  shift
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
stamp="$(date -u +%Y%m%dT%H%M%SZ)"

output_root="${FMEM_BRIGHT_LOAD_OUTPUT_DIR:-${repo_root}/diagnostics/eval-runs/${stamp}-bright-pro-full-mcp-load}"
session_id="${FMEM_BRIGHT_LOAD_SESSION_ID:-00000000-0000-0000-0000-00000000b7f1}"
mcp_url="${FMEM_BRIGHT_LOAD_MCP_URL:-http://127.0.0.1:18765/mcp}"
mcp_user="${FMEM_BRIGHT_LOAD_MCP_USER:-ferrosa_user}"
mcp_password="${FMEM_BRIGHT_LOAD_MCP_PASSWORD:-ferrosa_user}"
timeout_seconds="${FMEM_BRIGHT_LOAD_TIMEOUT_SECONDS:-700}"
rate_limit_retries="${FMEM_BRIGHT_LOAD_RATE_LIMIT_RETRIES:-20}"
batch_size="${FMEM_BRIGHT_LOAD_BATCH_SIZE:-100}"
embed_missing="${FMEM_BRIGHT_LOAD_EMBED_MISSING:-false}"
heartbeat_seconds="${FMEM_BRIGHT_LOAD_HEARTBEAT_SECONDS:-60}"
start_method="${FMEM_BRIGHT_LOAD_START_METHOD:-auto}"
launch_label="${FMEM_BRIGHT_LOAD_LABEL:-com.ferrosa-memory.bright-pro-full-load}"

mkdir -p "${output_root}"

supervisor_pid_file="${output_root}/supervisor.pid"
worker_pid_file="${output_root}/worker.pid"
heartbeat_file="${output_root}/heartbeat.json"
progress_file="${output_root}/progress.json"
log_file="${output_root}/load.log"
supervisor_log_file="${output_root}/supervisor.log"
launch_stdout_file="${output_root}/launch.stdout.log"
launch_stderr_file="${output_root}/launch.stderr.log"
launch_plist_file="${output_root}/${launch_label}.plist"
report_file="${output_root}/bright-pro-report.json"

if [[ "${run_supervisor}" != "true" && -f "${supervisor_pid_file}" ]]; then
  supervisor_pid="$(cat "${supervisor_pid_file}")"
  if [[ -n "${supervisor_pid}" ]] && kill -0 "${supervisor_pid}" 2>/dev/null; then
    echo "BRIGHT-Pro load already running"
    echo "  output: ${output_root}"
    echo "  supervisor_pid: ${supervisor_pid}"
    echo "  heartbeat: ${heartbeat_file}"
    echo "  log: ${log_file}"
    echo "  supervisor_log: ${supervisor_log_file}"
    exit 0
  fi
fi

write_heartbeat() {
  local status="$1"
  local worker_pid="${2:-}"
  local exit_code="${3:-}"
  HEARTBEAT_FILE="${heartbeat_file}" \
  PROGRESS_FILE="${progress_file}" \
  LOG_FILE="${log_file}" \
  SUPERVISOR_LOG_FILE="${supervisor_log_file}" \
  LAUNCH_STDOUT_FILE="${launch_stdout_file}" \
  LAUNCH_STDERR_FILE="${launch_stderr_file}" \
  REPORT_FILE="${report_file}" \
  OUTPUT_ROOT="${output_root}" \
  SESSION_ID="${session_id}" \
  STATUS="${status}" \
  WORKER_PID="${worker_pid}" \
  EXIT_CODE="${exit_code}" \
  python3 - <<'PY'
import datetime
import json
import os
from pathlib import Path

def read_json(path: str) -> dict:
    if not path:
        return {}
    p = Path(path)
    if not p.exists():
        return {}
    try:
        return json.loads(p.read_text(encoding="utf-8"))
    except Exception as exc:
        return {"read_error": str(exc)}

def last_line(path: str) -> str:
    p = Path(path)
    if not p.exists():
        return ""
    try:
        lines = p.read_text(encoding="utf-8", errors="replace").splitlines()
    except Exception as exc:
        return f"log read error: {exc}"
    return lines[-1] if lines else ""

progress = read_json(os.environ["PROGRESS_FILE"])
heartbeat = {
    "status": os.environ["STATUS"],
    "updated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "session_id": os.environ["SESSION_ID"],
    "worker_pid": os.environ.get("WORKER_PID") or None,
    "exit_code": os.environ.get("EXIT_CODE") or None,
    "output_root": os.environ["OUTPUT_ROOT"],
    "progress_file": os.environ["PROGRESS_FILE"],
    "report_file": os.environ["REPORT_FILE"],
    "log_file": os.environ["LOG_FILE"],
    "supervisor_log_file": os.environ["SUPERVISOR_LOG_FILE"],
    "launch_stdout_file": os.environ["LAUNCH_STDOUT_FILE"],
    "launch_stderr_file": os.environ["LAUNCH_STDERR_FILE"],
    "last_log_line": last_line(os.environ["LOG_FILE"]),
    "progress": progress,
}
Path(os.environ["HEARTBEAT_FILE"]).write_text(
    json.dumps(heartbeat, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
PY
}

if [[ "${run_supervisor}" == "true" ]]; then
  echo "$$" > "${supervisor_pid_file}"
  trap 'rc=$?; echo "supervisor failed rc=${rc} line=${LINENO}" >&2; write_heartbeat "failed" "${worker_pid:-}" "${rc}" || true; exit "${rc}"' ERR
  write_heartbeat "starting" "" ""

  cmd=(
    "${repo_root}/scripts/run-official-evals.sh" bright-pro
    --backend mcp-http
    --full-corpus
    --mcp-session-id "${session_id}"
    --mcp-url "${mcp_url}"
    --mcp-user "${mcp_user}"
    --mcp-password "${mcp_password}"
    --mcp-timeout-seconds "${timeout_seconds}"
    --mcp-rate-limit-retries "${rate_limit_retries}"
    --mcp-batch-size "${batch_size}"
    --mcp-ingest-only
    --mcp-progress-file "${progress_file}"
    --output-dir "${output_root}"
    --progress
  )

  if [[ "${embed_missing}" == "true" ]]; then
    cmd+=(--mcp-embed-missing)
  fi

  {
    echo "started_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "session_id=${session_id}"
    echo "embed_missing=${embed_missing}"
    echo "batch_size=${batch_size}"
    printf 'command='
    printf '%q ' "${cmd[@]}"
    printf '\n'
  } >> "${log_file}"

  "${cmd[@]}" >> "${log_file}" 2>&1 &
  worker_pid="$!"
  echo "${worker_pid}" > "${worker_pid_file}"
  write_heartbeat "running" "${worker_pid}" ""

  while kill -0 "${worker_pid}" 2>/dev/null; do
    write_heartbeat "running" "${worker_pid}" ""
    sleep "${heartbeat_seconds}"
  done

  if wait "${worker_pid}"; then
    write_heartbeat "complete" "${worker_pid}" "0"
  else
    rc="$?"
    write_heartbeat "failed" "${worker_pid}" "${rc}"
    exit "${rc}"
  fi
  exit 0
fi

use_launchctl=false
if [[ "${start_method}" == "launchctl" ]]; then
  use_launchctl=true
elif [[ "${start_method}" == "auto" ]] && [[ "$(uname -s)" == "Darwin" ]] && command -v launchctl >/dev/null 2>&1; then
  use_launchctl=true
fi

if [[ "${use_launchctl}" == "true" ]]; then
  OUTPUT_ROOT="${output_root}" \
  REPO_ROOT="${repo_root}" \
  SCRIPT_PATH="$0" \
  LABEL="${launch_label}" \
  SESSION_ID="${session_id}" \
  MCP_URL="${mcp_url}" \
  MCP_USER="${mcp_user}" \
  MCP_PASSWORD="${mcp_password}" \
  TIMEOUT_SECONDS="${timeout_seconds}" \
  RATE_LIMIT_RETRIES="${rate_limit_retries}" \
  BATCH_SIZE="${batch_size}" \
  EMBED_MISSING="${embed_missing}" \
  HEARTBEAT_SECONDS="${heartbeat_seconds}" \
  STDOUT_PATH="${launch_stdout_file}" \
  STDERR_PATH="${launch_stderr_file}" \
  PLIST_PATH="${launch_plist_file}" \
  python3 - <<'PY'
import os
import plistlib
from pathlib import Path

env = {
    "FMEM_BRIGHT_LOAD_OUTPUT_DIR": os.environ["OUTPUT_ROOT"],
    "FMEM_BRIGHT_LOAD_SESSION_ID": os.environ["SESSION_ID"],
    "FMEM_BRIGHT_LOAD_MCP_URL": os.environ["MCP_URL"],
    "FMEM_BRIGHT_LOAD_MCP_USER": os.environ["MCP_USER"],
    "FMEM_BRIGHT_LOAD_MCP_PASSWORD": os.environ["MCP_PASSWORD"],
    "FMEM_BRIGHT_LOAD_TIMEOUT_SECONDS": os.environ["TIMEOUT_SECONDS"],
    "FMEM_BRIGHT_LOAD_RATE_LIMIT_RETRIES": os.environ["RATE_LIMIT_RETRIES"],
    "FMEM_BRIGHT_LOAD_BATCH_SIZE": os.environ["BATCH_SIZE"],
    "FMEM_BRIGHT_LOAD_EMBED_MISSING": os.environ["EMBED_MISSING"],
    "FMEM_BRIGHT_LOAD_HEARTBEAT_SECONDS": os.environ["HEARTBEAT_SECONDS"],
}
plist = {
    "Label": os.environ["LABEL"],
    "ProgramArguments": ["/bin/bash", os.environ["SCRIPT_PATH"], "--run-supervisor"],
    "WorkingDirectory": os.environ["REPO_ROOT"],
    "EnvironmentVariables": env,
    "RunAtLoad": True,
    "KeepAlive": False,
    "StandardOutPath": os.environ["STDOUT_PATH"],
    "StandardErrorPath": os.environ["STDERR_PATH"],
}
path = Path(os.environ["PLIST_PATH"])
path.parent.mkdir(parents=True, exist_ok=True)
path.write_bytes(plistlib.dumps(plist))
PY
  domain="gui/$(id -u)"
  launchctl bootout "${domain}/${launch_label}" >/dev/null 2>&1 || true
  launchctl bootstrap "${domain}" "${launch_plist_file}"
  supervisor_pid=""
else
  FMEM_BRIGHT_LOAD_OUTPUT_DIR="${output_root}" \
  FMEM_BRIGHT_LOAD_SESSION_ID="${session_id}" \
  FMEM_BRIGHT_LOAD_MCP_URL="${mcp_url}" \
  FMEM_BRIGHT_LOAD_MCP_USER="${mcp_user}" \
  FMEM_BRIGHT_LOAD_MCP_PASSWORD="${mcp_password}" \
  FMEM_BRIGHT_LOAD_TIMEOUT_SECONDS="${timeout_seconds}" \
  FMEM_BRIGHT_LOAD_RATE_LIMIT_RETRIES="${rate_limit_retries}" \
  FMEM_BRIGHT_LOAD_BATCH_SIZE="${batch_size}" \
  FMEM_BRIGHT_LOAD_EMBED_MISSING="${embed_missing}" \
  FMEM_BRIGHT_LOAD_HEARTBEAT_SECONDS="${heartbeat_seconds}" \
  nohup "$0" --run-supervisor > "${supervisor_log_file}" 2>&1 &

  supervisor_pid="$!"
  echo "${supervisor_pid}" > "${supervisor_pid_file}"
fi
write_heartbeat "running" "" ""

echo "Started BRIGHT-Pro full MCP load"
echo "  output: ${output_root}"
if [[ -n "${supervisor_pid:-}" ]]; then
  echo "  supervisor_pid: ${supervisor_pid}"
else
  echo "  launch_label: ${launch_label}"
  echo "  launch_plist: ${launch_plist_file}"
fi
echo "  heartbeat: ${heartbeat_file}"
echo "  progress: ${progress_file}"
echo "  log: ${log_file}"
echo "  supervisor_log: ${supervisor_log_file}"
echo "  report: ${report_file}"
