#!/usr/bin/env bash
set -euo pipefail

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

mkdir -p "${output_root}"

supervisor_pid_file="${output_root}/supervisor.pid"
worker_pid_file="${output_root}/worker.pid"
heartbeat_file="${output_root}/heartbeat.json"
progress_file="${output_root}/progress.json"
log_file="${output_root}/load.log"
report_file="${output_root}/bright-pro-report.json"

if [[ -f "${supervisor_pid_file}" ]]; then
  supervisor_pid="$(cat "${supervisor_pid_file}")"
  if [[ -n "${supervisor_pid}" ]] && kill -0 "${supervisor_pid}" 2>/dev/null; then
    echo "BRIGHT-Pro load already running"
    echo "  output: ${output_root}"
    echo "  supervisor_pid: ${supervisor_pid}"
    echo "  heartbeat: ${heartbeat_file}"
    echo "  log: ${log_file}"
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
    "last_log_line": last_line(os.environ["LOG_FILE"]),
    "progress": progress,
}
Path(os.environ["HEARTBEAT_FILE"]).write_text(
    json.dumps(heartbeat, indent=2, sort_keys=True) + "\n",
    encoding="utf-8",
)
PY
}

(
  echo "${BASHPID}" > "${supervisor_pid_file}"
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
) >/dev/null 2>&1 &

supervisor_pid="$!"
echo "${supervisor_pid}" > "${supervisor_pid_file}"
write_heartbeat "running" "" ""

echo "Started BRIGHT-Pro full MCP load"
echo "  output: ${output_root}"
echo "  supervisor_pid: ${supervisor_pid}"
echo "  heartbeat: ${heartbeat_file}"
echo "  progress: ${progress_file}"
echo "  log: ${log_file}"
echo "  report: ${report_file}"
