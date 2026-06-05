#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

output_root="${FMEM_EVAL_OUTPUT_DIR:-${repo_root}/diagnostics/eval-runs/fusion-ablations-$(date -u +%Y%m%dT%H%M%SZ)}"
profiles="${FMEM_EVAL_FUSION_PROFILES:-bm25-only semantic-only bm25-semantic bm25-semantic-phonetic bm25-semantic-phonetic-workspace all}"
session_id="${FMEM_EVAL_BRIGHT_SESSION_ID:-00000000-0000-0000-0000-00000000b7f4}"
split="${FMEM_EVAL_BRIGHT_SPLIT:-biology}"
limit_examples="${FMEM_EVAL_LIMIT_EXAMPLES:-5}"
max_docs="${FMEM_EVAL_MCP_MAX_DOCS:-200}"
candidate_limit="${FMEM_EVAL_CANDIDATE_LIMIT:-50}"
rerank_candidates="${FMEM_EVAL_RERANK_CANDIDATES:-25}"
query_decomposition="${FMEM_EVAL_QUERY_DECOMPOSITION:-none}"
query_variant_limit="${FMEM_EVAL_QUERY_VARIANT_LIMIT:-5}"
query_embed_variants="${FMEM_EVAL_QUERY_EMBED_VARIANTS:-false}"
chunk_expansion="${FMEM_EVAL_CHUNK_EXPANSION:-none}"
chunk_prev="${FMEM_EVAL_CHUNK_PREV:-1}"
chunk_next="${FMEM_EVAL_CHUNK_NEXT:-1}"
chunk_max_tokens="${FMEM_EVAL_CHUNK_MAX_TOKENS:-1600}"
query_embed_args=()
if [[ "${query_embed_variants}" == "true" ]]; then
  query_embed_args+=(--mcp-query-embed-variants)
fi

mkdir -p "${output_root}"

for profile in ${profiles}; do
  "${repo_root}/scripts/run-official-evals.sh" bright-pro \
    --backend mcp-http \
    --split "${split}" \
    --limit-examples "${limit_examples}" \
    --mcp-max-docs "${max_docs}" \
    --mcp-session-id "${session_id}" \
    --mcp-skip-ingest \
    --mcp-candidate-limit "${candidate_limit}" \
    --mcp-fusion-profile "${profile}" \
    --mcp-query-decomposition "${query_decomposition}" \
    --mcp-query-variant-limit "${query_variant_limit}" \
    --mcp-chunk-expansion "${chunk_expansion}" \
    --mcp-chunk-prev "${chunk_prev}" \
    --mcp-chunk-next "${chunk_next}" \
    --mcp-chunk-max-tokens "${chunk_max_tokens}" \
    --mcp-no-rerank \
    --mcp-rerank-candidates "${rerank_candidates}" \
    --output-dir "${output_root}/${profile}" \
    --include-cases \
    "${query_embed_args[@]}"
done

if [[ "${FMEM_EVAL_INCLUDE_LLM_RERANK:-false}" == "true" ]]; then
  "${repo_root}/scripts/run-official-evals.sh" bright-pro \
    --backend mcp-http \
    --split "${split}" \
    --limit-examples "${limit_examples}" \
    --mcp-max-docs "${max_docs}" \
    --mcp-session-id "${session_id}" \
    --mcp-skip-ingest \
    --mcp-candidate-limit "${candidate_limit}" \
    --mcp-fusion-profile all \
    --mcp-query-decomposition "${query_decomposition}" \
    --mcp-query-variant-limit "${query_variant_limit}" \
    --mcp-chunk-expansion "${chunk_expansion}" \
    --mcp-chunk-prev "${chunk_prev}" \
    --mcp-chunk-next "${chunk_next}" \
    --mcp-chunk-max-tokens "${chunk_max_tokens}" \
    --mcp-rerank \
    --mcp-rerank-candidates "${rerank_candidates}" \
    --output-dir "${output_root}/all-llm-rerank" \
    --include-cases \
    "${query_embed_args[@]}"
fi

python3 - "${output_root}" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
rows = []
for report_path in sorted(root.glob("*/bright-pro-report.json")):
    report = json.loads(report_path.read_text())
    summary = report["summary"]
    rows.append({
        "profile": report_path.parent.name,
        "alpha_ndcg": summary["alpha_ndcg"]["mean"],
        "aspect_recall": summary["aspect_recall"]["mean"],
        "recall": summary["recall"]["mean"],
        "ndcg": summary["ndcg"]["mean"],
        "cases": report["case_count"],
        "failures": report["failure_count"],
        "query_decomposition": report.get("mcp_query_decomposition"),
        "query_variant_limit": report.get("mcp_query_variant_limit"),
        "query_embed_variants": report.get("mcp_query_embed_variants"),
    })

print(json.dumps(rows, indent=2, sort_keys=True))
PY

echo "wrote ${output_root}"
