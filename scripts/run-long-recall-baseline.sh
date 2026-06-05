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
candidate_limit="${FMEM_EVAL_CANDIDATE_LIMIT:-50}"
fusion_profile="${FMEM_EVAL_FUSION_PROFILE:-all}"
query_decomposition="${FMEM_EVAL_QUERY_DECOMPOSITION:-none}"
query_variant_limit="${FMEM_EVAL_QUERY_VARIANT_LIMIT:-5}"
query_embed_variants="${FMEM_EVAL_QUERY_EMBED_VARIANTS:-false}"
chunk_expansion="${FMEM_EVAL_CHUNK_EXPANSION:-none}"
chunk_prev="${FMEM_EVAL_CHUNK_PREV:-1}"
chunk_next="${FMEM_EVAL_CHUNK_NEXT:-1}"
chunk_max_tokens="${FMEM_EVAL_CHUNK_MAX_TOKENS:-1600}"

common_mcp_args=(
  --mcp-url "${mcp_url}"
  --mcp-user "${mcp_user}"
  --mcp-password "${mcp_password}"
  --mcp-timeout-seconds "${timeout_seconds}"
  --mcp-batch-size "${batch_size}"
  --mcp-rerank-candidates "${rerank_candidates}"
  --mcp-candidate-limit "${candidate_limit}"
  --mcp-fusion-profile "${fusion_profile}"
  --mcp-query-decomposition "${query_decomposition}"
  --mcp-query-variant-limit "${query_variant_limit}"
  --mcp-chunk-expansion "${chunk_expansion}"
  --mcp-chunk-prev "${chunk_prev}"
  --mcp-chunk-next "${chunk_next}"
  --mcp-chunk-max-tokens "${chunk_max_tokens}"
  --mcp-embed-missing
  --progress
)

if [[ "${query_embed_variants}" == "true" ]]; then
  common_mcp_args+=(--mcp-query-embed-variants)
fi

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
