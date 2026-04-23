---
type: coverage
scope: all-crates
created: 2026-04-18
updated: 2026-04-18
---

# ferrosa-memory — Coverage Document

Covers all five crates: `ferrosa-memory-core`, `ferrosa-memory-mcp`,
`ferrosa-memory-batch`, `ferrosa-memory-sync`, `ferrosa-memory-eval`.

---

## 1. Feature Inventory per Crate

### 1.1 ferrosa-memory-core (`crates/ferrosa-memory-core/src/`)

The core library. Every MCP tool handler, storage abstraction, and subsystem
lives here. The other crates are thin consumers.

**Storage layer**
- `storage.rs` — `Storage` trait (31 methods), `MockStorage` in-memory impl
- `cql_storage.rs` — `CqlStorage` impl; `CqlSession` wrapper over `cdrs-tokio`
- `migration.rs` — forward-only migration runner; DDL 020-023 applied automatically; 001-019 as bootstrap constants

**Core types**
- `types.rs` — `EntityEntry`, `FoldEntry`, `TemporalEvent`, `MemoEntry`, `PlanNode`, `FeedbackOutcome`, `TenantContext`, `ToolUsageRow`
- `entity.rs` — entity dedup, phonetic matching, state machine (dormant/active/silent/unavailable)
- `temporal.rs` — temporal fact lifecycle, supersession chain
- `fold.rs` — fold create/append/complete/search
- `session.rs` — session scoping helpers
- `scope.rs` — scope primitives (session / global)

**MCP dispatch**
- `dispatch.rs` — `tool_definitions()` + `dispatch()` entry point; 54 tools registered

**Tool handlers (in dispatch.rs)**

| Tool | What it does | File:region |
|------|-------------|-------------|
| `check_memo_cache` | Content-hash lookup for sub-call caching | dispatch.rs:190 |
| `store_memo_result` | Writes sub-call result to cache | dispatch.rs:203 |
| `write_plan_node` | Records hierarchical plan sub-task | dispatch.rs:222 |
| `get_plan_context` | Returns full plan tree for session | dispatch.rs:237 |
| `update_plan_node` | Marks plan node complete/failed | dispatch.rs:249 |
| `start_fold` | Opens a trajectory fold | dispatch.rs:265 |
| `append_to_fold` | Appends a REPL turn to active fold | dispatch.rs:279 |
| `complete_fold` | Seals fold with summary + embedding | dispatch.rs:292 |
| `retrieve_fold_context` | ANN search over prior fold summaries | dispatch.rs:306 |
| `upsert_entity` | Writes named entity; phonetic dedup | dispatch.rs:322 |
| `batch_ingest` | Batch upsert up to 100 entities | dispatch.rs:339 |
| `retrieve_entities` | Phonetic / ANN / both entity search | dispatch.rs:370 |
| `record_outcome` | Logs retrieval outcome for routing | dispatch.rs:386 |
| `delete_session` | Destroys all data for a session | dispatch.rs:404 |
| `smart_ingest` | Auto-decides CREATE/UPDATE/SUPERSEDE/SKIP | dispatch.rs:416 |
| `ingest_skill` | Ingests methodology into global catalog | dispatch.rs:433 |
| `retrieve_skills_for_context` | Semantic + keyword skill retrieval | dispatch.rs:465 |
| `invoke_skill` | Fetches structured steps for a named skill | dispatch.rs:480 |
| `ensure_parent_tag` | Idempotent PARENT_TAG edge creation | dispatch.rs:493 |
| `verify_skill` | Audits skill graph neighborhood | dispatch.rs:506 |
| `set_intention` | Sets prospective memory trigger | dispatch.rs:519 |
| `check_intentions` | Evaluates pending intentions vs. context | dispatch.rs:544 |
| `complete_intention` | Marks intention as done | dispatch.rs:556 |
| `list_intentions` | Lists intentions (session or all repos) | dispatch.rs:567 |
| `snooze_intention` | Resets triggered intention to pending | dispatch.rs:577 |
| `write_temporal_fact` | Records timestamped entity fact | dispatch.rs:589 |
| `get_temporal_chain` | Returns current (most recent) fact | dispatch.rs:603 |
| `explore_connections` | Cypher-backed graph traversal (4 modes) | dispatch.rs:616 |
| `hybrid_search` | RRF across entities, folds, facts | dispatch.rs:637 |
| `run_consolidation` | Dream consolidation; creates CO_OCCURS edges | dispatch.rs:656 |
| `enrich_entities` | LLM enrichment + annotation + lint | dispatch.rs:668 |
| `get_stats` | Session entity/fold/memo/intention counts | dispatch.rs:704 |
| `promote_memory` | Promotes entity state one level up | dispatch.rs:716 |
| `demote_memory` | Demotes entity state one level down | dispatch.rs:728 |
| `importance_score` | 4-channel importance scoring | dispatch.rs:741 |
| `find_memory_chain` | BFS shortest path between entities | dispatch.rs:754 |
| `predict_needed` | Co-access pattern prefetch predictions | dispatch.rs:769 |
| `spread_activation` | Collins-Loftus activation propagation | dispatch.rs:793 |
| `find_duplicates` | Jaccard-based entity dedup scan | dispatch.rs:814 |
| `recursive_explore` | Multi-pass Datalog-driven discovery | dispatch.rs:832 |
| `query_derived` | Query Datalog-derived facts with provenance | dispatch.rs:857 |
| `manage_rules` | CRUD for Datalog rule registry | dispatch.rs:877 |
| `manage_claims` | Expert-system claim artifacts | dispatch.rs:898 |
| `manage_approvals` | Governance approval append/inspect | dispatch.rs:919 |
| `manage_aliases` | Exact-scope tool alias management | dispatch.rs:938 |
| `explain_derived` | Bounded explanation with support chain | dispatch.rs:959 |
| `get_effective_rule_set` | Merged runtime rule set inspection | dispatch.rs:974 |
| `promote_predicate` | Promotes derived predicate to durable mat. | dispatch.rs:986 |
| `create_edge` | Single typed edge creation | dispatch.rs:1003 |
| `batch_create_edges` | Bulk typed edge creation (up to 200) | dispatch.rs:1025 |
| `list_derived_cache` | Lists derived cache for audit/debug | dispatch.rs:1052 |

**Total: 54 tools** (53 functional + 1 internal server-info entry at dispatch.rs:1094)

**Supporting subsystems**

| Module | Purpose |
|--------|---------|
| `embedding.rs` | Ollama HTTP client + in-process embedding cache |
| `graph.rs` | Cypher read HTTP client (port 7474) |
| `hybrid_search.rs` | RRF fusion logic |
| `recursive_explore.rs` | Multi-pass exploration driver |
| `enrich.rs` | LLM entity description generation |
| `datalog.rs` | Datalog engine: rule evaluation, derived cache |
| `promotion.rs` | Workload-driven predicate promotion |
| `spreading.rs` | Spreading activation algorithm |
| `importance.rs` | Importance scoring (novelty/arousal/reward/attention) |
| `skill.rs` | Skill ingest/invoke/verify/tag-normalize |
| `smart_ingest.rs` | Prediction-error gated CREATE/UPDATE/SUPERSEDE |
| `intention.rs` | Prospective memory: trigger eval, repo scope |
| `warmth.rs` | Ebbinghaus-decay warmth field |
| `audit.rs` | Append-only audit log writer |
| `auth.rs` | TenantContext derivation; stdio + HTTP Basic auth |
| `migration.rs` | Forward-only schema migration runner |
| `router.rs` | Query strategy routing guidelines |
| `dedup.rs` | Entity deduplication logic |
| `ner.rs` | Named-entity recognition helpers |
| `pagerank.rs` | PageRank scoring |
| `speculative.rs` | Speculative prefetch (predict_needed) |
| `chains.rs` | BFS memory chain search |
| `plan.rs` | Plan tree storage helpers |
| `memo.rs` | Memo cache storage helpers |
| `feedback.rs` | Feedback outcome recording |
| `dream.rs` | Dream consolidation orchestration |
| `metrics.rs` | Internal metrics / counters |
| `compression.rs` | Fold compression |
| `viz.rs` | Visualization subgraph helpers |
| `batch.rs` | Batch ingest helper |
| `transport.rs` | HTTP transport abstraction |
| `vector.rs` | Vector search helpers |
| `quota.rs` | Tenant quota enforcement |
| `expert_system.rs` | Expert-system runtime |
| `http.rs` | HTTP server router + SPARQL passthrough |
| `security_tests.rs` | In-module security assertions |
| `config.rs` | Server config (CQL, embedding, graph, SPARQL URLs) |
| `test_cluster.rs` | In-process test cluster helpers |

### 1.2 ferrosa-memory-mcp (`crates/ferrosa-memory-mcp/src/`)

Thin binary crate. Composes the HTTP+SSE MCP server and stdio MCP server.

- `main.rs` — binary entry point; initializes `SharedState`, implements `Storage` trait via delegation to `CqlStorage` (live) or `MockStorage` (test); `SparqlPassthrough` connector; `initialize` handler; health endpoints; workbench HTTP routes
- `tools/fix_edge_sessions.rs` — migration utility: backfills missing `session_id` on three edge tables (`co_occurs_with`, `mentioned_in`, `folded_into`)

### 1.3 ferrosa-memory-batch (`crates/ferrosa-memory-batch/src/`)

Batch/backfill binary (~595 lines).

- `main.rs` — CLI subcommands:
  - `migrate-session` — backfills entity session-id partitioning
  - `retype-entities` — bulk entity_type reclassification
  - `rename-entities` — entity name normalization pass
  - `run-guidelines` — generates and writes routing guideline versions
  - `backfill-rich-entities` — enriches entities for rich schema (020 migration)

### 1.4 ferrosa-memory-sync (`crates/ferrosa-memory-sync/src/`)

Single-file sync binary (~460 lines).

- `main.rs` — CLI subcommands:
  - `sync` — copies folds, entities, temporal events, and edges from one CQL cluster to another
  - `discover` — probes source cluster topology

### 1.5 ferrosa-memory-eval (`crates/ferrosa-memory-eval/src/`)

Evaluation harness (~6941 lines total across 9 source files).

- `runner.rs` — scenario runner; DIKW pipeline stages; loads YAML scenario files
- `mcp_client.rs` — HTTP MCP client for eval
- `report.rs` — eval report generation
- `scenario.rs` — `EvalScenario` type + loader
- `config.rs` — eval harness config
- `grading/tool_usage.rs` — tool call frequency metrics (P0: did it use the right tools?)
- `grading/programmatic.rs` — schema/output correctness checks
- `grading/claim_rubric.rs` — rubric-based grading
- `dikw/data_info.rs`, `info_knowledge.rs`, `knowledge_wisdom.rs`, `emergence.rs` — four-level DIKW pipeline stages
- `semantic/mod.rs` — semantic similarity grading

### 1.6 scripts/

| Script | Purpose |
|--------|---------|
| `backfill-embeddings.sh` / `backfill-embeddings-v2.py` | Embedding backfill for entity_store |
| `backup-memory.sh` / `restore-memory.sh` | CQL dump / restore |
| `start-cluster.sh` / `start-test-cluster.sh` / `stop-test-cluster.sh` | Cluster lifecycle wrappers |
| `install-launch-agent.sh` / `install-launch-agent-mcp.sh` / `uninstall-launch-agent.sh` | macOS launchd management |
| `memory-watchdog.sh` | Health-poll + auto-restart |
| `coverage_gap.py` | Coverage analysis helper |
| `ingest_ferrosa_graph.py` | Ferrosa codebase graph ingestion |
| `mcp_helper.py` | MCP wire-level debug helper |
| `test-data-loss.sh` | Data-loss scenario script (links to BUG-P0-WRITE-LOSS.md) |
| `test-dikw-pipeline.sh` | DIKW pipeline smoke test |

---

## 2. DDL Migration Inventory

23 migration files in `ddl/`. Files 001-019 are applied as bootstrap for
pre-versioning installs; files 020-023 are applied automatically by
`migration.rs` at startup.

| File | What it creates / alters |
|------|--------------------------|
| `001_keyspace.cql` | `agent_memory` keyspace; `memo_cache` table; `plan_state` table |
| `002_folds_entities.cql` | `trajectory_folds`, `entity_store`, `temporal_events`, `feedback_outcomes` tables |
| `003_edge_tables.cql` | `folded_into`, `mentioned_in`, `co_occurs_with`, `supersedes` tables; graph vertex/edge extensions on all four |
| `004_audit_anomaly.cql` | `audit_log`, `entity_retrieval_counts` tables |
| `005_vector_columns.cql` | Adds `vector<float,768>` columns + ANN indexes to `memo_cache`, `trajectory_folds`, `entity_store` |
| `006_entity_state.cql` | `ALTER entity_store ADD state text` |
| `007_intentions.cql` | `intentions` table (initial schema) |
| `008_intentions_repo_scope.cql` | `intentions` table with `repo` partition column |
| `008_routing_guidelines.cql` | `routing_guidelines` table |
| `009_secondary_indexes.cql` | 7 secondary indexes on entity, edge tables to eliminate ALLOW FILTERING |
| `009_tool_usage_log.cql` | `tool_usage_log` table |
| `010_edge_strength.cql` | `ALTER co_occurs_with ADD strength float, last_reinforced timestamp` |
| `011_warmth_field.cql` | `entity_warmth` table; warmth session index |
| `012_datalog_rules.cql` | `rules_by_id`, `rules_by_family` tables |
| `013_derived_cache.cql` | `derived_cache_by_query`, `derived_cache_by_pred` tables (TTL-bounded) |
| `014_derivation_provenance.cql` | `derivation_provenance` table |
| `015_heat_telemetry.cql` | `query_heat_by_predicate_day`, `compute_cost_by_predicate_day` tables |
| `016_durable_materialization.cql` | `derived_edges_by_src`, `derived_edges_by_pred`, `promoted_predicates` tables |
| `017_typed_edges.cql` | `typed_edges` table; graph edge extension |
| `018_edge_session_indexes.cql` | Session-id indexes on `co_occurs_with`, `mentioned_in`, `folded_into` |
| `019_type_registry.cql` | `entity_types`, `edge_types` tables |
| `020_rich_entity_schema.cql` | `ALTER entity_store` — adds `description`, `description_embedding`, `tags`, `properties`, `content_hash`, `updated_at`, `scope`, `ingested_by_session` |
| `021_derived_cache_ttl.cql` | `derived_cache_ttl_track` table |
| `022_approval_store.cql` | `approvals_by_target` table |
| `023_alias_store.cql` | `aliases_by_name` table |

**Total: 23 DDL files, ~35 tables/views created, ~15 secondary indexes.**

---

## 3. Spec Coverage Matrix

| Feature / Subsystem | Covering Spec(s) |
|--------------------|-----------------|
| Memo cache, plan tree, fold lifecycle | `specs/overview.md`, `specs/components.md`, `specs/data-flow.md` |
| Entity store, phonetic dedup, state machine | `specs/overview.md`, `specs/components.md` |
| Hybrid search (RRF) | `specs/ARCHITECTURE.md`, `specs/components.md` |
| Temporal facts + supersession | `specs/memory-lifecycle.md` |
| Graph traversal (`explore_connections`) | `specs/ARCHITECTURE.md`, `specs/data-flow.md` |
| Skills layer (ingest/invoke/verify/ensure_parent_tag) | `specs/skills-layer-design.md`, `specs/launch-plan-skills-and-richer-entities.md` |
| Datalog rules + derived cache | `specs/datalog-materialization.md`, `specs/ARCHITECTURE.md` |
| Expert system (claims, approvals, aliases) | `specs/expert-system-knowledge-plane.md` |
| Intentions (prospective memory) | `specs/memory-lifecycle.md`, `specs/components.md` |
| Spreading activation / importance / find_memory_chain | `specs/ARCHITECTURE.md` (brief mention only) |
| Promotion / demotion / predict_needed | `specs/memory-lifecycle.md` (state machine section) |
| Dream consolidation | `specs/memory-lifecycle.md` |
| Enrichment pipeline | `specs/launch-plan-skills-and-richer-entities.md` |
| Auth (stdio + HTTP Basic) | `specs/threat-model.md`, `specs/ARCHITECTURE.md` |
| Migration runner + schema versioning | `specs/in-process/feat-schema-versioning.md` (implemented) |
| Shared HTTP deployment (workbench) | `specs/shared-http-deployment.md`, `specs/in-process/feat-concurrent-http-server.md` |
| SPARQL passthrough (workbench only) | `specs/ARCHITECTURE.md` (aspirational note) |
| ferrosa-memory-eval harness | `specs/mcp-eval/project-plan.md`, `specs/mcp-eval/overview.md` |
| ferrosa-memory-sync | `specs/memory-sync.md` |
| LSP code indexing | `specs/lsp-code-indexing.md` |
| Web knowledge ingestion | `specs/web-knowledge-ingestion.md` |
| Visualization | `specs/visualization.md`, `specs/todo/viz-subgraph-exploration.md` |
| Threat model (STRIDE) | `specs/threat-model.md` |
| FMEA | `specs/fmea.md` |
| DSM coupling analysis | `specs/dsm-analysis.md` |
| CQL client paging | `specs/todo/cql-client-paging.md` |
| Graph write bypass bug | `specs/todo/bug-ferrosa-memory-bypasses-graph-api-for-writes.md` |
| Graph client Cypher-write extension | `specs/todo/todo-extend-ferrosa-memory-graph-client-with-cypher-writes.md` |
| Endpoint-only ferrosa client refactor | `specs/todo/feat-endpoint-only-ferrosa-client.md`, `specs/decisions/adr-005-endpoint-only-ferrosa-client.md` |
| CQL role auth enforcement | `specs/decisions/design-cql-role-auth-rollout.md` (ferrosa), `specs/todo/todo-enable-cql-role-auth-for-graph-table-isolation.md` (not in this repo, referenced) |

**Not covered by any spec:**
- `predict_needed` / `spread_activation` / `find_memory_chain` — only brief references in `ARCHITECTURE.md`; no dedicated spec
- `ferrosa-memory-batch` subcommand behavior — no spec; implementation is the sole documentation
- `scripts/` behavior contracts — no spec; scripts are undocumented beyond inline comments

---

## 4. Test Coverage

### 4.1 Rust integration tests (`crates/ferrosa-memory-core/tests/`)

15 test files, ~130 test functions total.

| File | What it tests |
|------|--------------|
| `cql_live.rs` | Low-level CQL driver connectivity (3 tests — all `#[ignore]`) |
| `cql_isolate.rs` | CQL isolation properties |
| `cql_storage_live.rs` | CqlStorage impl, round-trips (1 `#[ignore]`) |
| `sprint3_e2e.rs` | Entity + fold E2E (1 `#[ignore]`) |
| `skill_e2e_live.rs` | Skill ingest/invoke/verify E2E (2 `#[ignore]`) |
| `graph_live.rs` | Graph HTTP client traversals (2 `#[ignore]`) |
| `vector_live.rs` | Vector ANN queries (1 `#[ignore]`) |
| `expert_system_rules_spec.rs` | Datalog rule CRUD, cache invalidation |
| `expert_system_governance_spec.rs` | manage_claims, manage_approvals, manage_aliases, explain_derived |
| `ferrosa_bugs.rs` | Regression tests for known bugs |
| `ferrosa_2i_validation.rs` | Secondary index validation |
| `http_concurrency.rs` | Concurrent HTTP server stress |
| `ingest_skill_cancellation.rs` | Skill ingest cancellation safety |
| `shared_http_deployment_spec.rs` | Shared HTTP deployment boundary |
| `launch_gates_g3_g4.rs` | G3/G4 launch gate checks |

**Ignored tests: 10 across 6 files.** These are live-cluster tests that
connect to `127.0.0.1:19042` directly. They are marked `#[ignore]` instead
of gating on `FERROSA_TEST_CONTAINERS=1` per the codebase test policy. This
is a policy violation (see §5, Gap P0-A).

### 4.2 Python integration and system tests (`tests/`)

| File | What it tests |
|------|--------------|
| `integration/test_expert_system_runtime.py` | Expert system runtime behavior |
| `integration/test_query_surfaces.py` | Query surface contracts |
| `system/test_performance_budgets.py` | Latency budgets per tool |
| `system/test_shared_http_workbench.py` | Workbench HTTP contract |
| `property/test_expert_system_properties.py` | Property-based tests for expert system |

### 4.3 ferrosa-memory-eval scenarios

`tests/baselines/` contains YAML eval scenarios; `ferrosa-memory-eval` runs
them through the DIKW grading pipeline. Coverage: tool_usage (graded), claim
rubric (graded), semantic similarity (graded). No automated CI integration
documented.

### 4.4 Notable gaps in test coverage

- `predict_needed`, `spread_activation`, `find_memory_chain` — no dedicated tests found
- `run_consolidation` — tested indirectly via `ferrosa_bugs.rs` regressions; no direct scenario
- `batch_create_edges` — not exercised in any test file
- `list_derived_cache` — no direct test
- `ferrosa-memory-sync` — no Rust tests; no Python tests
- `ferrosa-memory-batch` subcommands — no tests found

---

## 5. Gaps

### P0-A — `#[ignore]` in live-cluster tests violates policy

**Files:** `cql_live.rs` (3), `cql_storage_live.rs` (1), `sprint3_e2e.rs` (1),
`skill_e2e_live.rs` (2), `graph_live.rs` (2), `vector_live.rs` (1) — 10 total.

The ferrosa test policy explicitly states: *"No `#[ignore]` — Zero legitimately
ignored tests in this codebase."* These tests connect to `127.0.0.1:19042`
without gating on `FERROSA_TEST_CONTAINERS=1`. They have been dark since at
minimum migration 007 (repo history suggests longer). Dead ignored tests give
false confidence: the storage layer can regress without any test catching it.

**Required fix:** Replace `#[ignore]` with an env-var guard
(`FERROSA_TEST_CONTAINERS`) and a `panic!` with setup instructions if the guard
is absent. Add these tests to the containerized CI matrix.

### P0-B — Direct CQL writes into graph-owned tables bypass Cypher invariants

**Bug:** `specs/todo/bug-ferrosa-memory-bypasses-graph-api-for-writes.md`

`ferrosa-memory-core/src/cql_storage.rs` issues raw `INSERT INTO {ks}.typed_edges`,
`folded_into`, `mentioned_in`, `co_occurs_with`, `supersedes`, `derived_edges_by_src`,
and `derived_edges_by_pred` statements. Reads go through the Cypher HTTP endpoint
(`graph.rs`); writes do not. This bypasses all graph engine invariants (uniqueness,
reverse-index consistency, property typing, telemetry hooks). The 2026-04-19
`tool_usage_log` corruption incident is a direct antecedent.

**Resolution tracked in:**
- `specs/todo/todo-extend-ferrosa-memory-graph-client-with-cypher-writes.md`
- `specs/todo/feat-endpoint-only-ferrosa-client.md`
- `specs/decisions/adr-005-endpoint-only-ferrosa-client.md`

### P1 — Auth uses file-based credentials; no CQL role enforcement

**Current state:** HTTP transport uses HTTP Basic auth validated against a TOML
auth file (`auth.rs`). stdio transport inherits process credentials. The config
defaults `ferrosa_user` as the CQL username but there is no CQL role-level
enforcement — `FERROSA_AUTH_DISABLED=true` is set in the deployed
`docker-compose.yml`.

**Planned state:** Per `ferrosa/specs/decisions/design-cql-role-auth-rollout.md`,
`ferrosa_admin` / `ferrosa_user` CQL roles should restrict which tables each
client can touch, providing the enforcement layer that prevents the P0-B bypass
even if the code is buggy. This is not yet implemented in ferrosa or wired into
ferrosa-memory.

**Impact:** Until CQL roles are enforced, any client with CQL access can write
to graph-owned tables. The auth file controls MCP API access but not the
underlying CQL schema boundary.

### P1 — SPARQL is UI-only passthrough; no server-side query support

The workbench HTML exposes a SPARQL Explorer panel. The server side
(`http.rs:711`) proxies the query body directly to `ferrosa-sparql`'s HTTP
endpoint. There is no query rewriting, result normalization, or error
translation. `ARCHITECTURE.md` marks SPARQL integration as aspirational.
Callers of the workbench SPARQL surface receive raw ferrosa-sparql errors
with no abstraction.

**Spec reference:** `specs/ARCHITECTURE.md` section on future query surfaces.

### P2 — Speculative retrieval tools lack tests and spec coverage

`predict_needed`, `spread_activation`, and `find_memory_chain` are fully
implemented but have no dedicated tests and only brief mentions in
`ARCHITECTURE.md`. For production robustness these need:

1. Unit tests with `MockStorage`
2. A spec documenting the algorithm contracts, expected latency, and failure modes
3. Integration into `ferrosa-memory-eval` DIKW scenarios

---

## 6. Recommendations

1. **Convert `#[ignore]` tests to env-var-gated tests (P0-A).** Add
   `FERROSA_TEST_CONTAINERS` guards and a `panic!` explaining setup. Add
   the container matrix to CI. This is the highest-leverage single action
   because it restores the storage layer regression safety net.

2. **Land the Cypher-write refactor (P0-B).** Implement
   `todo-extend-ferrosa-memory-graph-client-with-cypher-writes.md` so every
   edge mutation travels through `POST /graph/cypher`. The graph engine must
   be the sole writer of its own tables. Until then, every storage layout
   change in ferrosa-graph is a latent data-loss event in ferrosa-memory.

3. **Write dedicated tests for speculative tools (P2).** `predict_needed`,
   `spread_activation`, and `find_memory_chain` are algorithmic enough to
   need contract tests (expected BFS depth, activation decay, co-access
   threshold). Use `MockStorage` — no live cluster required.

4. **Spec the `ferrosa-memory-batch` subcommands.** The batch binary is
   operational infrastructure (backfill, re-typing, guideline generation) with
   no spec. A one-page runbook per subcommand with preconditions, postconditions,
   and rollback procedure would reduce operational risk.

5. **Upgrade the SPARQL passthrough to fail loud.** Currently the workbench
   silently proxies ferrosa-sparql errors. Add HTTP status translation and a
   structured error body so callers know whether the failure is in
   ferrosa-memory or ferrosa-sparql. Log every non-200 SPARQL response at WARN
   with the query fingerprint.
