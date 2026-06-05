# Ferrosa Memory Roadmap

## Local Runtime Target

- [x] **Dev MCP/viz must point at the persistent compose data tree.**
  - Correct dev target: CQL `127.0.0.1:19042`, `19043`, `19044` backed by bind mounts `/Users/bkearns/data/ferrosa-memory/node1`, `/node2`, `/node3`, with MinIO at `/Users/bkearns/data/ferrosa-memory/minio`.
  - Incorrect sparse target observed on 2026-06-04: `ferrosa-memory-ci-node1..3` mounted `.runtime/ferrosa-memory-ci/node1..3` on the same `19042-19044` ports.
  - Verification after correction: raw CQL showed `entity_store=25481`, `typed_edges=42801`, `document_chunks=1041`, `document_terms=133680`, `document_phonetic_terms=115360`; workbench summary showed `node_count=12469`.
  - Guardrail: before trusting eval/UI counts, inspect live container mounts with `podman inspect ferrosa-memory_node1_1 ferrosa-memory_node2_1 ferrosa-memory_node3_1 --format '{{json .Mounts}}'`. Do not hunt for data until the mounted source is confirmed.

## Active Retrieval Gaps

- [ ] **Hybrid search needs an async judge-rerank mode with streamed status.**
  - Current: live LLM reranking can be enabled and works against local Ollama (`qwen2.5-coder:7b` hot path is ~2-3s for 8 candidates), but it is still in-band with the MCP request.
  - Implemented: judge-model reranking asks for per-candidate relevance scores and records `"-"` abstentions separately from valid `-1/0/1` judgments. `record_feedback` also accepts `"-"` and tracks per-judge sums so caller LLM, human, and judge-model feedback can accumulate without losing abstention/error evidence.
  - Desired: return normal hybrid results immediately with `reranker.status = "queued" | "warming" | "running" | "complete" | "failed"`, stream progress over the existing event/workbench channel, and expose a stable rerank result lookup by `rerank_job_id`.
  - Rationale: cold local model loads and remote judges should not block first-token retrieval. The LLM/user can start with baseline candidates, then adopt the reranked list when the judge completes.
  - Acceptance: `hybrid_search(..., rerank_mode="async")` returns baseline results plus a job id; the job emits bounded progress; callers can fetch the reranked result set; failures leave baseline results valid and include the judge error.

- [ ] **Entity lexical/snippet retrieval is too brittle.**
  - Observed: `list` finds rich entities for normal phrases, while `hybrid_search` can miss unless phonetic matching hits. ANN errors are logged when legacy `entity_embedding` cells are not vector values, and phonetic hits may lack context snippets.
  - Implemented in this pass: phonetic entity candidates now fall back to `entity_name` when `context_snippet` is empty so judge reranking has meaningful text.
  - Next: add or wire an entity-name/context BM25 or exact-token candidate source so natural queries like "onboarding hooks Codex Claude Hermes" retrieve the known onboarding memory without relying on phonetic match quality.

## Knowledge Type Gaps

These came up while wiring official BRIGHT-Pro / MemoryBench eval corpora. They should stay near the top because missing or weak types force agents and eval harnesses to collapse rich artifacts into generic `concept` nodes, which degrades retrieval, UI filtering, and graph semantics.

- [x] **Document retrieval plane is first-class.**
  - Implemented: `document` and `benchmark_document` are default MCP schema types.
  - Implemented: semantic document chunking, prev/next chunk links, document BM25 term index, phonetic term index, chunk ANN index, and `chunk_ctx` expansion.
  - Implemented: `hybrid_search` now fuses document BM25, document phonetic, document ANN, context BM25, and context ANN candidates.
  - Remaining: full BRIGHT-Pro MCP runs need batch/runtime tuning; a 25-document batch timed out at the HTTP 30s request budget, while 5-document batches completed.

- [ ] **Eval corpus isolation and baselines need hardening.**
  - Observed: capped BRIGHT-Pro MCP runs can score 0.0 when relevant support docs are outside the capped ingest window.
  - Impact: small live slices validate wiring but cannot be compared to paper systems unless the official support corpus is fully ingested or the harness ingests support-closed subsets.
  - Implemented: `scripts/run-official-evals.py bright-pro --backend mcp-http --mcp-max-docs N` now samples only examples whose support docs are fully inside the capped ingest window.
  - Implemented: full-corpus MCP profiles use deterministic persisted corpus sessions for BRIGHT-Pro and MemoryBench via `scripts/run-long-recall-baseline.sh`.
  - Implemented: MemoryBench now has an MCP retrieval-proxy baseline that ingests official dialog/feedback rows and reports answer-containing evidence retrieval. This is intentionally separate from the paper's task-native judge score.
  - Next: add JSON baseline comparison and CI/manual-dispatch gates before treating BRIGHT-Pro or MemoryBench scores as regression blockers.

- [ ] **Benchmark/corpus passage isolation is incomplete.**
  - Implemented: `benchmark_document` is available as a default type and is indexed through the document chunk plane.
  - Expected: benchmark passages should be distinguishable from user memories, notes, documents, and code symbols with required suite/run metadata.
  - Impact: eval data can still pollute normal memory retrieval if callers use shared sessions or omit benchmark properties.
  - Candidate: enforce or helper-fill `properties.benchmark`, `properties.split`, `properties.doc_id`, and run/session scoping.

- [ ] **Feedback/procedural memory type is too implicit.**
  - Expected: MemoryBench feedback logs should map to an explicit type for service-time procedural knowledge, not only generic entities or `record_outcome`.
  - Impact: later retrieval can find text, but the system cannot reliably separate declarative facts from procedural preferences/corrections learned from feedback.
  - Candidate: add `feedback`, `procedure`, or `policy_preference` type plus explicit edges to affected task/domain/user/session entities.

- [ ] **Evaluation run/artifact type is missing.**
  - Expected: official eval runs, reports, failures, corpus manifests, and benchmark baselines should be recordable as first-class memory artifacts.
  - Impact: agents cannot ask memory for prior benchmark runs, regressions, or failure clusters without relying on filesystem scanning.
  - Candidate: add `eval_run`, `eval_failure`, and `corpus_manifest` types with edges to `document`/`benchmark_document` nodes.

- [ ] **Conversation/message/turn types are missing for multi-agent memory evals.**
  - Expected: synthetic and real two-agent conversations should preserve turns, speakers, tasks, feedback, and derived durable facts as queryable graph objects.
  - Impact: MemoryBench-style fixtures can test retrieval text, but they cannot verify whether Ferrosa preserves the conversation structure that produced a memory.
  - Candidate: add `conversation`, `message`, and `turn` types, with `mentions`, `responds_to`, `derived_fact`, and `has_feedback` edges.

- [ ] **Remote knowledge artifact type is missing.**
  - Expected: remote paper/file discoveries, SSH-reachable paths, local summaries, and source repo references should be modelable without flattening everything into a note.
  - Impact: agents cannot reliably ask for "the summary plus the original local PDF path on another machine" because location, artifact kind, and access method are only text.
  - Candidate: add `knowledge_artifact` or specialize `document` with `artifact_kind`, `host`, `path`, `uri`, `checksum`, and `summary_entity_id` properties.

## P0 Gap Closure Blueprint

Scope: `feature/p0-gap-closure`

This roadmap tracks the current memory-server gaps that have the highest impact on correctness, operator visibility, and agent ergonomics.

## Checklist

- [x] Graph write completion: verify Ferrosa public `TYPED_EDGE` `MERGE` materialization.
  - Result: adjacent Ferrosa graph integration test `canonical_typed_edge_merge_infers_scope_from_existing_entities` passes when run with `ferrosa-storage/macos-standard-sync`.
  - Critical issue raised: macOS validation fails at compile time unless Ferrosa tests select either `ferrosa-storage/macos-standard-sync` or `ferrosa-storage/macos-fullfsync`.

- [x] Consolidation reliability visibility: surface consolidation run state.
  - Implemented: `SessionState.last_consolidation_status`.
  - Implemented: `run_consolidation` records queued status.
  - Implemented: idle worker records running/success/failure status.
  - Implemented: `get_stats` returns `last_consolidation_status`.
  - Remaining hardening: durable consolidation run history and explicit schema-drift fail-fast diagnostics are still follow-up work.

- [x] Migration visibility: add operator-facing schema status.
  - Implemented: read-only `migration_status` MCP tool.
  - Implemented: storage-level migration status API with CQL override.
  - Implemented: startup log line with `db_version`, `binary_version`, and `pending`.
  - Remaining hardening: a durable `migration_log` table is still a follow-up; current status is derived from `schema_version`.

- [x] Sprint 10 bulk ingest polish: add bounded progress observability and smoke coverage.
  - Implemented: `ingest_entities` returns bounded progress markers in the tool result.
  - Existing coverage already exercises dry-run, strict-edge failure, and write visibility checks.
  - Remaining hardening: true MCP progress notifications require a transport-level notification channel.

- [x] Duplicate entity suppression: remove manual consolidation counting and preserve smart ingest duplicate checks.
  - Implemented: `smart_ingest` automatically queues consolidation after 10 new entities per session.
  - Existing implementation already checks exact name, ANN, and phonetic candidates before create.
  - Implemented: exact `(tenant, entity_name, entity_type)` cross-session duplicate suppression before create.
  - Remaining hardening: cross-session fuzzy/ANN candidate checks need property-backed thresholds before broadening beyond exact matches.

- [ ] Evaluation regression gates: make evals a CI quality ratchet.
  - Status: planned in `specs/in-process/feat-eval-regression-gates.md`.
  - Implemented: deterministic `ferrosa-memory-eval fixture-smoke` runner for BRIGHT-Pro and MemoryBench-style fixtures.
  - Implemented: property/metamorphic tests for synthetic MemoryBench retrieval and BRIGHT-Pro monotonic recall.
  - Remaining: JSON baseline comparison and CI job wiring.

- [ ] Automatic session capture: extract durable facts at wrap-up and ingest with duplicate checks.
  - Status: planned follow-up. This is larger than a quick bug-sweep item because it needs a session-close trigger, extraction policy, confidence rules, and dedupe behavior.

## Verification

- Passed Ferrosa typed-edge validation:
  - `cargo test -p ferrosa-graph --features ferrosa-storage/macos-standard-sync --test graph_http_integration canonical_typed_edge_merge_infers_scope_from_existing_entities -- --nocapture`
- Passed Ferrosa-memory targeted verification:
  - `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo check -p ferrosa-memory-core`
  - `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo check -p ferrosa-memory-mcp`
  - `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p ferrosa-memory-core --lib migration_status_returns_binary_schema_status -- --nocapture`
  - `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p ferrosa-memory-core --lib get_stats_reports_last_consolidation_status -- --nocapture`
  - `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p ferrosa-memory-core --lib ingest_entities_upserts_entities_and_skips_duplicate_edges -- --nocapture`
  - `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p ferrosa-memory-core --lib smart_ingest_auto_queues_consolidation_after_ten_creates -- --nocapture`
  - `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1 cargo test -p ferrosa-memory-core --lib record_outcome_retrieval_miss_penalizes_entity_reputation -- --nocapture`
- Verification note:
  - Filtered `cargo test -p ferrosa-memory-core <name>` without `--lib` reaches `tests/skill_e2e_live.rs` and can hang after the matching unit test passes. Use `--lib` for these focused dispatch regressions.

## Next Quick Wins

1. Add durable `consolidation_runs` or reuse an existing diagnostics table for run history and schema-drift failures.
2. Add a deterministic eval baseline checker and PR CI gate for `ferrosa-memory-eval fixture-smoke`.
3. Add a live MCP/Ferrosa retriever adapter behind the corpus fixture runner.
4. Add a transport-level MCP progress notification path so `ingest_entities` can emit live progress rather than only response metadata.
5. Blueprint automatic session capture as a separate feature with policy tests before implementation.
