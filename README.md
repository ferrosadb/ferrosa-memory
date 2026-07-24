# Ferrosa Memory

Ferrosa Memory is durable working memory for AI agents. It turns one-off agent
sessions into a long-running, searchable knowledge system: plans survive restarts,
important facts keep their history, documents become retrievable by meaning, and
future sessions can pick up work without rebuilding the same context from scratch.

It runs as an [MCP](https://modelcontextprotocol.io/) server backed by Ferrosa DB.
Agents call normal MCP tools; Ferrosa Memory handles storage, retrieval,
consolidation, and governance behind those tools.

## Why it exists

Most agent workflows still behave like disposable chat windows. When the process
ends, the useful context disappears with it:

- subagents recompute work that already happened;
- plans and decisions get buried in transcripts;
- entities discovered in one session are invisible to the next;
- retrieval returns loose fragments instead of the surrounding project frame;
- stale deadlines and superseded facts keep polluting prompts.

Ferrosa Memory fixes that by giving agents a typed memory plane instead of a pile
of logs. It stores facts, plans, folds, documents, graph edges, skills, feedback,
and future constraints as first-class records that can be searched, explained,
aged out, superseded, or deleted.

## What it gives agents

- **Context that survives restarts** — restore the active workspace, branch,
  entities, plan state, and recent decisions at session start.
- **Less repeated work** — memoize sub-call results by content hash and reuse
  previously computed answers.
- **Long-running plans** — keep hierarchical plan nodes in storage and retrieve
  only the relevant path when work resumes.
- **Entity-aware recall** — track people, projects, files, concepts, and links
  with phonetic deduplication and graph traversal.
- **Temporal memory** — store timestamped facts with supersession so “latest
  valid answer” beats stale context.
- **Time-bounded foresight** — represent future constraints with
  `valid_from`/`valid_until`, then automatically stop surfacing them after they
  expire.
- **Document recall** — ingest documents as document, section, and chunk records
  with BM25, vector, phonetic, and graph-based candidate generation.
- **Consolidated scenes** — background consolidation folds clusters of related
  entities into coherent retrievable scenes, then expands them back to member
  entities during recall.
- **Retrieval feedback** — record traces, abstentions, judge results, and user
  feedback for offline tuning and regression detection.
- **Modern MCP draft support** — shared HTTP clients can discover capabilities,
  read task resources, and subscribe to workspace task updates with the
  `2026-07-28` per-request protocol.
- **Operational controls** — tenant isolation, audit trails, reversible
  forgetting, rule governance, and fail-loud health checks.

## How it works

Ferrosa Memory is a thin product layer over Ferrosa's public data interfaces. MCP
clients talk to Ferrosa Memory; Ferrosa Memory decides which memory tool is being
used, authenticates the request, writes app-owned memory records through CQL,
and uses Ferrosa graph/query interfaces for graph-owned data.

```mermaid
%%{init: {'theme':'dark','themeVariables':{'primaryColor':'#16161f','primaryTextColor':'#e8e8ed','primaryBorderColor':'#e2725b','lineColor':'#9494a3','secondaryColor':'#1c1c28','tertiaryColor':'#111118','clusterBkg':'#111118','clusterBorder':'#1e1e2a','edgeLabelBackground':'#111118','nodeTextColor':'#e8e8ed'}}}%%
graph TD
    subgraph Clients
        CC[Claude Code]
        CA[Claude.ai]
        TP[Third-party MCP clients]
        WB[Workbench]
    end

    subgraph MCP["Ferrosa Memory MCP server"]
        TR[Tool router]
        AT[Auth and tenant scope]
        MEM[Memory tools]
        DAT[Datalog engine]
        CU[Compression and upkeep]
    end

    subgraph DB["Ferrosa DB"]
        APP[App-owned memory tables]
        IDX["BM25 · Vector · Phonetic · B-tree"]
        GRAPH["Graph interfaces"]
        ST["NVMe → S3 → Glacier"]
    end

    CC -->|stdio| TR
    CA -->|HTTP+SSE| TR
    TP -->|HTTP+SSE| TR
    WB -->|CQL / SPARQL / Datalog surfaces| TR
    TR --> AT
    AT --> MEM
    MEM -->|direct CQL for memory records| APP
    MEM -->|graph reads/writes| GRAPH
    DAT -->|local inference over memory data| MEM
    CU -->|compression and background consolidation| MEM
    APP --- IDX
    GRAPH --- IDX
    APP --- ST
    GRAPH --- ST
```

The key boundary is ownership, not syntax. Direct CQL is correct for tables owned
by Ferrosa Memory, such as entities, folds, context segments, temporal facts,
feedback, configuration, and retrieval traces. Graph-owned backing tables are not
a public API; graph reads and writes go through graph interfaces.

| Surface | Contract |
|---------|----------|
| App-owned memory tables | Direct CQL is allowed. Ferrosa Memory owns the schema and migrations. |
| Graph reads and writes | Use graph interfaces, not direct mutation of graph backing tables such as `typed_edges`, `folded_into`, `mentioned_in`, `co_occurs_with`, or `supersedes`. |
| Workbench CQL | Authenticated pass-through to Ferrosa's public CQL contract. Fail loud if Ferrosa rejects the query. |
| Workbench SPARQL | Authenticated pass-through to Ferrosa's public SPARQL contract. Fail loud if Ferrosa rejects the query. |
| Workbench Datalog | Local Ferrosa Memory inference over Ferrosa-backed graph and app data. It is not a Ferrosa wire-protocol contract. |

That keeps the product honest: Ferrosa Memory uses Ferrosa instead of cloning
Ferrosa behavior locally. If a public Ferrosa interface cannot express a needed
operation, the fix belongs in Ferrosa's public graph/query surface rather than in
a private backing-table workaround.

## Tools

65 MCP tools are defined in `crates/ferrosa-memory-core/src/dispatch.rs` and
returned by `tools/list`. Keep this section aligned with the dispatch registry
when adding or removing tools. (`describe` is a management tool excluded from
the tier-1 default `tools/list`; request it with `include_all`.)

| Group | Tools | Purpose |
|-------|-------|---------|
| Context and folds | `ingest_context_segments`, `search_context_segments`, `get_context_window`, `start_fold`, `append_to_fold`, `complete_fold`, `retrieve_fold_context` | Store raw context windows and fold long trajectories into retrievable summaries |
| Memo and plans | `check_memo_cache`, `store_memo_result`, `write_plan_node`, `get_plan_context`, `update_plan_node` | Cache sub-call results and maintain hierarchical plan state |
| Core memory | `smart_ingest`, `upsert_entity`, `batch_ingest`, `ingest_entities`, `retrieve_entities`, `list_entities`, `hybrid_search`, `count_entities_by_type` | Ingest, retrieve, and structured-filter entities; semantic + lexical + phonetic + graph search |
| Graph / edges | `create_edge`, `batch_create_edges`, `batch_update_edges`, `batch_delete_edges`, `explore_connections`, `find_memory_chain` | Build and traverse the knowledge graph |
| Temporal | `write_temporal_fact`, `get_temporal_chain`, `set_foresight` | Timestamped facts with supersession tracking; `set_foresight` declares time-bounded facts (`valid_from`/`valid_until`) surfaced only while valid |
| Skills | `ingest_skill`, `invoke_skill`, `verify_skill`, `retrieve_skills_for_context` | Skill lifecycle: ingest, invoke, verify, retrieve |
| Intentions | `set_intention`, `check_intentions`, `complete_intention`, `list_intentions`, `snooze_intention` | Prospective memory — deferred actions triggered by topic, file pattern, duration, or context |
| Batch ops | `batch_update_entities`, `batch_delete_entities`, `delete_session` | Bulk entity updates, deletions, and scoped cleanup |
| Knowledge plane | `run_consolidation`, `enrich_entities`, `importance_score`, `predict_needed`, `spread_activation`, `find_duplicates`, `recursive_explore`, `query_derived`, `list_derived_cache` | Consolidation, scoring, associative recall, recursive search, and derived-fact access |
| Governance | `manage_rules`, `manage_claims`, `manage_approvals`, `manage_aliases`, `explain_derived`, `get_effective_rule_set`, `promote_predicate`, `promote_memory`, `demote_memory` | Rule governance, claims, approvals, explanations, and memory promotion |
| Maintenance | `get_stats`, `migration_status`, `record_outcome`, `ensure_parent_tag` | Health stats, schema status, strategy feedback, and tag hierarchy |
| Management | `describe` | Read-only, management-safe self-description (contract `ferrosa-memory.system.describe.v1`): identity, runtime/store health, redacted config, live ferrosa cluster info, summary statistics, schema drift, capabilities, and management actions |
| Forgetting | `forget`, `restore_forgotten` | Candidate-confirmed forgetting: propose candidates (with blast radius) → confirm to retract (reversible, audited, restorable) or hard-delete; `restore_forgotten` reverses a retraction |

## MCP draft support

Shared HTTP deployments support the MCP draft per-request protocol version `2026-07-28` for modern clients. The supported surface includes `server/discover`, tools, prompts, task resources, and task-resource subscriptions over Server-Sent Events.

Use this when a client wants to discover capabilities without a legacy `initialize` session, read active task resources directly, or subscribe to workspace task updates for multi-agent coordination. See the [protocol guide](docs/mcp-draft-support.md), [compatibility matrix](docs/mcp-compatibility.md), and [demo runbook](docs/mcp-draft-demo.md) for the implemented claim boundary and reproducible evidence.
### Entity Scope Defaults

`list_entities`, `get_stats` (and its `stats` alias), and
`count_entities_by_type` are tenant-wide by default. Supply both
`scope: "session"` and `session_id` to scope a read to one session.
Passing `session_id` alone does not change the read scope.

## Recall quality

Beyond raw storage, ferrosa-memory actively improves what comes back from a search. Most
of this is automatic — a background **dream cycle** runs on a periodic tick (default every
~20 s when there is new data) and builds the structures below for free. See
[`docs/recall-quality.md`](docs/recall-quality.md) for the full guide.

**Time-bounded foresight** — declare a fact that only holds for a window; search filters
it out automatically once it expires (or before it starts):

```jsonc
set_foresight { "content": "Code freeze on main until the 0.17 cut",
                "valid_until": "2026-07-01T00:00:00Z" }
// A later hybrid_search for "freeze" surfaces it (result_type: "foresight") only
// while valid; set valid_until in the past and it is dropped before fusion.
```

**Consolidation scenes** — when 3+ related entities cluster, consolidation folds them into
a durable *scene* (a coherent unit with a member-centroid embedding). A search that matches
the scene returns it **and expands to its member entities** — including members that didn't
match the query on their own — so you recall the whole cluster, not loose fragments. Scenes
match both lexically and **semantically** (cosine vs. the centroid).

**Workspace profiles** — each consolidated session gets a compact profile (active entities,
repo/branch/task context) that `hybrid_search` injects as always-on context, so every query
starts with the session's frame.

**Retrieval traces** — every search durably records its query, candidate sources, and
results, enabling offline tuning of fusion weights and regression detection.

Measured impact of the recall-quality controller on the evaluation corpus: precision
**0.40 → 1.00**, context clutter **0.75 → 0.00**, accepted tokens **840 → 210** (≈4× leaner),
with recall held at **1.00**.

## Skills

The [`skills/`](skills/) directory ships portable agent **skills** (slash-command
playbooks) that the installer copies into your agent skill directory
(`~/.claude/skills/` by default; `--no-skills` to opt out):

- `/memory-session-start` — restore context (`check_intentions` + `hybrid_search`) at session start
- `/set-foresight` — declare a time-bounded fact
- `/consolidate-wrapup` — force a consolidation pass at the end of a session
- `/defer`, `/whats-next`, `/roadmap` — capture and surface follow-up work (needs the [`forge`](https://github.com/ferrosadb/forge) companion task board)

See [`skills/README.md`](skills/README.md).

## Quick Start

### Build

```sh
cargo build --workspace
```

### Run (stdio mode, for Claude Code)

```sh
# Create a config file
cp examples/ferrosa-memory.toml ./ferrosa-memory.toml
# Edit contact_points to your Ferrosa instance

# Run
cargo run --bin ferrosa-memory-mcp
```

### Run (Shared HTTP mode)

```sh
cp examples/ferrosa-memory-http.toml ./ferrosa-memory-http.toml
cp examples/http-auth-single-tenant.toml ./http-auth.toml
# Update TLS paths, contact points, graph URL, and the single auth principal.
# This example maps ferrosa_user to the default stdio tenant, preserving data
# already written with config/ferrosa-memory.example.toml.

FERROSA_MEMORY_CONFIG=./ferrosa-memory-http.toml cargo run --bin ferrosa-memory-mcp
# Listens on port 8765 by default and exposes:
#   GET /health
#   GET /healthz/live
#   GET /healthz/ready
#   POST /mcp
```

For a binary install, do not replace the generated
`~/.ferrosa/config/http-auth.toml` with the repository example. The installer
provisions that file and the installed config with one matching per-install
tenant. The HTTP-auth comments in
`~/.ferrosa/config/ferrosa-memory.toml` contain the exact reconciliation
command to run after changing transport or auth settings.

For a shared deployment with separate tenant data, start from
[`examples/http-auth.toml`](examples/http-auth.toml) or
[`examples/http-auth-multi-tenant.toml`](examples/http-auth-multi-tenant.toml).
Seed or migrate data into each listed tenant before clients connect; principal
credentials select that tenant and cannot see the local single-user tenant.

Shared HTTP startup is fail-closed. The binary refuses to bind the listener unless all of the following are true:

- `server.require_tls = true`
- `server.cert_path` and `server.key_path` are set
- `server.auth_file` is set
- `server.tenant_id` is omitted

For a source checkout, run `./setup.sh --config <path>` after building or
upgrading. Setup reconciles the per-install tenant across the HTTP auth file,
`[viz].tenant_id`, and agent-hook environment before restarting the service;
it removes obsolete `server.tenant_id` fallbacks from HTTP configs. Hook
verification sends a representative per-turn write and fails the setup rather
than claiming success when that write is rejected.

Probe semantics differ on purpose:

- `GET /healthz/live` reports process liveness only
- `GET /health` is an alias of `/healthz/live`
- `GET /healthz/ready` reports whether the Ferrosa-facing data-plane clients are currently ready for MCP traffic. Under the role-aware boundary, that means auth + app-table CQL + enabled public query endpoints are healthy. It must not require graph-table `MODIFY` to report ready.

```sh
curl -sk https://127.0.0.1:8765/health
curl -sk https://127.0.0.1:8765/healthz/live
curl -sk https://127.0.0.1:8765/healthz/ready
```

Streaming and request safety defaults:

| Limit | Default |
|-------|---------|
| Request body cap | 8 MiB |
| Per-connection request budget | 30 seconds |
| TLS accept budget | 10 seconds |
| Per-IP rate limit | 50 requests/minute |
| SPARQL passthrough response cap | 4 MiB |

The shared HTTP template keeps `viz` disabled. If you enable it under HTTP mode, the viz listener stays loopback-only and requires an explicit `[viz].tenant_id`.

For Podman-based container startup on macOS, prefer `PODMAN_COMPOSE_PROVIDER=podman-compose`
so `podman compose` does not delegate to Docker Desktop's `docker-compose`.

### Start On Login (macOS)

```sh
./scripts/install-launch-agent.sh
```

This installs a per-user `launchd` agent that, at login:

- starts the Podman machine if needed
- runs `podman compose up -d` from this repo
- brings up the Ferrosa stack defined in [docker-compose.yml](docker-compose.yml)

To remove it later:

```sh
./scripts/uninstall-launch-agent.sh
```

### Client Configs

Shared HTTP example for Codex-compatible remote MCP clients:

```toml
[mcp_servers.ferrosa-memory]
url = "https://ferrosa_user:ferrosa_user@memory.example.com:8765/mcp"
```

Use [`examples/codex-shared-http.toml`](examples/codex-shared-http.toml) as the copy-pasteable template. The URL encodes one seeded principal from [`examples/http-auth.toml`](examples/http-auth.toml), and it must point at `/mcp`.

- `ferrosa_user / ferrosa_user`: standard/shared operator client
- `ferrosa_admin / ferrosa_admin`: admin-capable operator client for readiness or bootstrap checks

Local stdio fallback for Claude Code:

Add to `~/.claude/settings.json`:

```json
{
  "mcpServers": {
    "ferrosa-memory": {
      "command": "/path/to/ferrosa-memory-mcp",
      "env": {
        "FERROSA_MEMORY_CONFIG": "/path/to/ferrosa-memory.toml"
      }
    }
  }
}
```

Use [`examples/claude-code-settings.json`](examples/claude-code-settings.json) with [`examples/ferrosa-memory.toml`](examples/ferrosa-memory.toml) for that local fallback path.

### Modern MCP draft clients

Draft clients can use the per-request HTTP protocol by adding the modern protocol headers and `_meta` fields to each JSON-RPC request. Start with `server/discover`:

```sh
curl -sS -u ferrosa_user:ferrosa_user \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: server/discover' \
  --data '{
    "jsonrpc":"2.0",
    "id":1,
    "method":"server/discover",
    "params":{
      "_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{},
        "io.modelcontextprotocol/clientInfo":{"name":"example-client","version":"0.1.0"}
      }
    }
  }' \
  https://memory.example.com:8765/mcp
```

A successful discovery response advertises tools, prompts, resources, and `subscriptions/listen`. See [`docs/mcp-draft-support.md`](docs/mcp-draft-support.md) for complete examples, including `tools/call`, `prompts/get`, `resources/read`, and workspace task updates.

### Database Setup

Apply the DDL scripts against your Ferrosa instance:

```sh
cqlsh -f ddl/001_keyspace.cql
cqlsh -f ddl/002_folds_entities.cql
```

## Configuration

See [`examples/ferrosa-memory.toml`](examples/ferrosa-memory.toml) for all options. Key sections:

| Section | Controls |
|---------|----------|
| `[server]` | Transport (stdio/http), port, log level |
| `[ferrosa]` | CQL contact points, keyspace, replication, consistency, seeded Ferrosa credentials |
| `[graph]` | Public graph endpoint location and Ferrosa graph credentials |
| `[sparql]` | Public SPARQL endpoint enablement and URL |
| `[datalog]` | Local ferrosa-memory Datalog engine limits and cache behavior |
| `[memory]` | TTL, compression threshold, confidence gate, memo limits |
| `[retrieval]` | Default result count for retrieval tools when `k`/`limit` is omitted |
| `[eval]` | Eval harness settings such as BRIGHT-Pro/MemoryBench `retrieval_k` |
| `[embeddings]` | Provider (Ollama/OpenAI), model, dimensions |
| `[security]` | Audit log, anomaly detection, sigma threshold |
| `[routing]` | Guideline version, feedback export schedule |

Current local defaults:

- Live retrieval keeps `[retrieval].default_limit = 10` so ordinary agent turns do not overfill context. Agents or users can lower it at runtime with the `config` MCP tool when token use is too high.
- Semantic retrieval uses Ollama `nomic-embed-text-v2-moe` with 768-dimensional embeddings when Ollama is available. If the embedding provider is absent, the server logs a warning and continues with lexical, phonetic, graph, and exact lookup behavior.
- The live LLM judge/reranker is enabled by default with local Ollama `qwen2.5-coder:7b`. If that model or endpoint is unavailable, retrieval still returns the fused baseline results and reports the judge failure in rerank diagnostics; set `[judge].enabled = false` to opt out explicitly.

## 0.13 Long-Recall Evaluation Profile

The best-known BRIGHT-Pro support-doc-closed MCP slice profile as of the 0.13 preview is:

```text
retrieval_k=25
candidate_limit=50
fusion_profile=all
query_decomposition=llm
query_task=bright_pro
query_variant_limit=5
query_embed_variants=true
chunk_expansion=none
rerank=false
```

On the 200-document biology support-closed slice, that measured:

| Metric | Score |
|--------|-------|
| alpha_nDCG | 0.816 |
| NDCG | 0.799 |
| aspect_recall | 0.940 |
| recall | 0.796 |

Use the helper scripts to reproduce and extend the runs:

```sh
# Deterministic harness self-test
scripts/run-official-evals.sh --self-test

# Fusion/decomposition ablations against an already-ingested MCP slice
FMEM_EVAL_QUERY_DECOMPOSITION=llm \
FMEM_EVAL_QUERY_EMBED_VARIANTS=true \
scripts/run-fusion-ablations.sh

# Start a resumable full-corpus BRIGHT-Pro MCP ingest with heartbeat files
scripts/start-bright-pro-full-load.sh
```

Full-corpus BRIGHT-Pro currently contains hundreds of thousands of support documents. The loader writes `heartbeat.json`, `progress.json`, and `load.log` under `diagnostics/eval-runs/...` so overnight runs can be checked without attaching to the process.

For shared deployments, use:

- [`examples/ferrosa-memory-http.toml`](examples/ferrosa-memory-http.toml) for the HTTP server
- [`examples/http-auth.toml`](examples/http-auth.toml) for the shared multi-tenant principal mapping
- [`examples/http-auth-single-tenant.toml`](examples/http-auth-single-tenant.toml) to preserve source-checkout data when changing one local tenant from stdio to HTTP
- [`examples/http-auth-multi-tenant.toml`](examples/http-auth-multi-tenant.toml) for an alternate shared tenant mapping after each tenant is seeded or migrated

Shared HTTP mode requires:

- `server.require_tls = true`
- `server.cert_path` and `server.key_path`
- `server.auth_file`
- no `server.tenant_id` fallback

`http-auth.toml` currently stores lowercase SHA-256 password digests. `viz` is disabled by default for the shared HTTP template; if you enable it under HTTP mode, set `[viz].tenant_id` and keep it on the loopback-only listener.

## Project Structure

```
crates/
  ferrosa-memory-core/   Shared library, storage abstractions, tool handlers, HTTP/workbench
  ferrosa-memory-mcp/    MCP server binary
  ferrosa-memory-batch/  Nightly routing guideline job
  ferrosa-memory-sync/   Cross-device sync binary
  ferrosa-memory-eval/   Evaluation runner and scenarios
ddl/                     CQL schema definitions
specs/                   Architecture, DSM, threat model, FMEA, project plan
product/                 Product specification
examples/                Config file templates
skills/                  Portable agent skills (installed by install-memory.sh)
docs/                    Guides (recall-quality.md), brand assets, the installer
```

## Security

The design addresses threats identified by MCPShield, MemoryGraft, and the LLM Agent Memory privacy paper:

- **Tenant isolation** — all queries scoped by `tenant_id` from auth context, never client-supplied
- **Memory poisoning defense** — confidence gating, retrieval anomaly detection (>3σ), append-only audit log
- **Injection prevention** — all CQL/Cypher queries use parameterized prepared statements
- **Privacy** — raw trajectories compressed and archived to Glacier within 30 days; cascade delete per session

See [`specs/threat-model.md`](specs/threat-model.md) for the full STRIDE analysis.

## Research Foundation

The design is grounded in recent work on recursive language models, agent memory architectures, and MCP security:

- **RLM / SRLM** — memoization requirement, program selection as primary performance driver
- **ReCAP** — structured plan re-injection with linear depth scaling
- **Context-Folding** — branch-and-collapse with 10x context reduction
- **Zep** — temporal knowledge graphs outperform static RAG by 18.5%
- **MIRIX** — six memory types needed for real-world agent workloads
- **ACON** — failure-pair datasets for guideline refinement
- **MCPShield / MemoryGraft** — MCP trust model, memory poisoning attacks

Full citations in [`product/spec.md`](product/spec.md).

## Development

```sh
# Format, lint, test
cargo fmt
cargo clippy --workspace -- -D warnings
cargo test --workspace

# Coverage
cargo llvm-cov --workspace

# Docs
cargo doc --workspace --no-deps
```

CI runs on every PR: format, clippy, build, test + coverage (80% gate), complexity analysis, and doc generation.

## Contributing

Contributions welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for the development
workflow and what we accept. By contributing, you agree to the
[Code of Conduct](CODE_OF_CONDUCT.md).

Security issues should be reported privately — see [SECURITY.md](SECURITY.md).

## License

Ferrosa Memory is licensed under the Apache License, Version 2.0.
See [LICENSE](LICENSE) and [NOTICE](NOTICE) for details.

Copyright 2026 Ferrosa, Inc.
