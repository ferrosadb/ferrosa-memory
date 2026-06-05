#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

suite="${1:-all}"
case "${suite}" in
  all|bright-pro|memorybench) ;;
  *)
    echo "usage: $0 [all|bright-pro|memorybench]" >&2
    exit 2
    ;;
esac

output_root="${FMEM_EVAL_OUTPUT_DIR:-${repo_root}/diagnostics/eval-runs/long-recall-$(date -u +%Y%m%dT%H%M%SZ)}"
mcp_url="${FMEM_EVAL_MCP_URL:-http://127.0.0.1:18765/mcp}"
mcp_user="${FMEM_EVAL_MCP_USER:-ferrosa_user}"
mcp_password="${FMEM_EVAL_MCP_PASSWORD:-ferrosa_user}"
bright_session_id="${FMEM_EVAL_BRIGHT_SESSION_ID:-00000000-0000-0000-0000-00000000b7f1}"
memorybench_session_id="${FMEM_EVAL_MEMORYBENCH_SESSION_ID:-00000000-0000-0000-0000-00000000b7f2}"
memorybench_variant="${FMEM_EVAL_MEMORYBENCH_VARIANT:-full}"
timeout_seconds="${FMEM_EVAL_MCP_TIMEOUT_SECONDS:-180}"
batch_size="${FMEM_EVAL_MCP_BATCH_SIZE:-25}"
k="${FMEM_EVAL_K:-25}"
rerank_candidates="${FMEM_EVAL_RERANK_CANDIDATES:-25}"

common_mcp_args=(
  --mcp-url "${mcp_url}"
  --mcp-user "${mcp_user}"
  --mcp-password "${mcp_password}"
  --mcp-timeout-seconds "${timeout_seconds}"
  --mcp-batch-size "${batch_size}"
  --mcp-rerank-candidates "${rerank_candidates}"
  --mcp-embed-missing
  --progress
)

if [[ -n "${FMEM_EVAL_MCP_TENANT_ID:-}" ]]; then
  common_mcp_args+=(--mcp-tenant-id "${FMEM_EVAL_MCP_TENANT_ID}")
fi

if [[ "${FMEM_EVAL_SKIP_INGEST:-false}" == "true" ]]; then
  common_mcp_args+=(--mcp-skip-ingest)
fi

mkdir -p "${output_root}"

if [[ "${suite}" == "all" || "${suite}" == "bright-pro" ]]; then
  "${repo_root}/scripts/run-official-evals.sh" bright-pro \
    --backend mcp-http \
    --full-corpus \
    --k "${k}" \
    --mcp-session-id "${bright_session_id}" \
    --output-dir "${output_root}/bright-pro" \
    --include-cases \
    "${common_mcp_args[@]}"
fi

if [[ "${suite}" == "all" || "${suite}" == "memorybench" ]]; then
  "${repo_root}/scripts/run-official-evals.sh" memorybench \
    --backend mcp-http \
    --full-corpus \
    --memorybench-variant "${memorybench_variant}" \
    --k "${k}" \
    --mcp-session-id "${memorybench_session_id}" \
    --output-dir "${output_root}/memorybench" \
    --include-cases \
    "${common_mcp_args[@]}"
fi

echo "wrote ${output_root}"
