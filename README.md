# ferrosa-memory-mcp

A structured memory backend for LLM agent trajectories, exposed as an [MCP](https://modelcontextprotocol.io/) server over Ferrosa DB.

LLM agents running inside Claude Code (and other MCP-compatible clients) currently lose all working context between sessions. Sub-calls re-derive results the parent already computed. Plans evaporate when the REPL tears down. Entities discovered in one trajectory are invisible to the next.

ferrosa-memory-mcp fixes this by providing durable, typed memory tools over Ferrosa's public query interfaces and indexed storage capabilities:

- **Memoization** — cache sub-call results by content hash, skip redundant LLM invocations
- **Plan state** — hierarchical plan trees with O(depth) range scans for structured re-injection
- **Trajectory folds** — branch-and-collapse pattern with semantic retrieval over fold summaries
- **Entity graph** — named entity tracking with phonetic deduplication and multi-hop Cypher queries
- **Temporal chains** — timestamped facts with supersession tracking (most-recent-valid retrieval)
- **Feedback loop** — record retrieval strategy outcomes for offline guideline refinement

## Architecture

```mermaid
%%{init: {'theme':'dark','themeVariables':{'primaryColor':'#16161f','primaryTextColor':'#e8e8ed','primaryBorderColor':'#e2725b','lineColor':'#9494a3','secondaryColor':'#1c1c28','tertiaryColor':'#111118','clusterBkg':'#111118','clusterBorder':'#1e1e2a','edgeLabelBackground':'#111118','nodeTextColor':'#e8e8ed'}}}%%
graph TD
    subgraph Clients
        CC[Claude Code]
        CA[Claude.ai]
        TP[Third-party MCP clients]
    end

    subgraph MCP["ferrosa-memory-mcp"]
        TR[Tool Router]
        AT[Auth]
        CU[Compression]
    end

    subgraph DB["Ferrosa DB"]
        KS[agent_memory keyspace]
        IX["HNSW · Phonetic · B-tree"]
        GR["Property graph (Cypher)"]
        ST["NVMe → S3 → Glacier"]
    end

    CC -->|stdio| TR
    CA -->|HTTP+SSE| TR
    TP -->|HTTP+SSE| TR
    TR --> AT
    AT -->|CQL + Cypher| KS
    CU -->|compress before write| KS
    KS --- IX
    KS --- GR
    KS --- ST
```

The server should be a thin adapter that maps MCP and operator-console requests onto Ferrosa at the right abstraction level. All intelligence stays in the LLM; durability and query semantics stay in Ferrosa. If a Ferrosa public interface misbehaves, `ferrosa-memory` should surface the error rather than emulate alternate semantics locally.

What is public vs internal here:

| Layer | What `ferrosa-memory` does | Classification |
|-------|-----------------------------|----------------|
| Wire protocol | CQL over port `9042` via `cdrs-tokio` | Public protocol |
| Graph storage tables | `typed_edges`, `folded_into`, `mentioned_in`, `co_occurs_with`, `supersedes`, `derived_edges_by_pred`, `derived_edges_by_src`, ... | Internal schema owned by `ferrosa-graph` |

CQL itself is not the problem. The problem is crossing the graph-engine boundary and treating graph-owned tables like a public API. The rule is:

- direct CQL usage for app tables is allowed
- graph reads and writes should go through graph interfaces, not direct table mutation
- workbench CQL and SPARQL surfaces should be passthrough/fail-loud contract checks
- Datalog is owned by `ferrosa-memory` and executes locally over Ferrosa-backed graph/app data; it is not a Ferrosa wire-protocol contract

Workbench query contract surfaces are explicit:

- `/workbench/api/cql/query` and `/workbench/api/sparql/query` are authenticated pass-through contracts to Ferrosa public query endpoints.
- `/workbench/api/datalog/query` is a local ferrosa-memory inference service and remains repo-owned.

Analogy: talking to PostgreSQL over its wire protocol is fine; writing directly to `pg_index` instead of running `CREATE INDEX` is not. Same protocol, wrong abstraction level.

## Tools

61 MCP tools are defined in `crates/ferrosa-memory-core/src/dispatch.rs` and
returned by `tools/list`. Keep this section aligned with the dispatch registry
when adding or removing tools.

| Group | Tools | Purpose |
|-------|-------|---------|
| Context and folds | `ingest_context_segments`, `search_context_segments`, `get_context_window`, `start_fold`, `append_to_fold`, `complete_fold`, `retrieve_fold_context` | Store raw context windows and fold long trajectories into retrievable summaries |
| Memo and plans | `check_memo_cache`, `store_memo_result`, `write_plan_node`, `get_plan_context`, `update_plan_node` | Cache sub-call results and maintain hierarchical plan state |
| Core memory | `smart_ingest`, `upsert_entity`, `batch_ingest`, `ingest_entities`, `retrieve_entities`, `list_entities`, `hybrid_search`, `count_entities_by_type` | Ingest, retrieve, and structured-filter entities; semantic + lexical + phonetic + graph search |
| Graph / edges | `create_edge`, `batch_create_edges`, `batch_update_edges`, `batch_delete_edges`, `explore_connections`, `find_memory_chain` | Build and traverse the knowledge graph |
| Temporal | `write_temporal_fact`, `get_temporal_chain` | Timestamped facts with supersession tracking |
| Skills | `ingest_skill`, `invoke_skill`, `verify_skill`, `retrieve_skills_for_context` | Skill lifecycle: ingest, invoke, verify, retrieve |
| Intentions | `set_intention`, `check_intentions`, `complete_intention`, `list_intentions`, `snooze_intention` | Prospective memory — deferred actions triggered by topic, file pattern, duration, or context |
| Batch ops | `batch_update_entities`, `batch_delete_entities`, `delete_session` | Bulk entity updates, deletions, and scoped cleanup |
| Knowledge plane | `run_consolidation`, `enrich_entities`, `importance_score`, `predict_needed`, `spread_activation`, `find_duplicates`, `recursive_explore`, `query_derived`, `list_derived_cache` | Consolidation, scoring, associative recall, recursive search, and derived-fact access |
| Governance | `manage_rules`, `manage_claims`, `manage_approvals`, `manage_aliases`, `explain_derived`, `get_effective_rule_set`, `promote_predicate`, `promote_memory`, `demote_memory` | Rule governance, claims, approvals, explanations, and memory promotion |
| Maintenance | `get_stats`, `record_outcome`, `ensure_parent_tag` | Health stats, strategy feedback, and tag hierarchy |

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
cp examples/http-auth.toml ./http-auth.toml
# Update TLS paths, contact points, graph URL, and auth principals

FERROSA_MEMORY_CONFIG=./ferrosa-memory-http.toml cargo run --bin ferrosa-memory-mcp
# Listens on port 8765 by default and exposes:
#   GET /health
#   GET /healthz/live
#   GET /healthz/ready
#   POST /mcp
```

Shared HTTP startup is fail-closed. The binary refuses to bind the listener unless all of the following are true:

- `server.require_tls = true`
- `server.cert_path` and `server.key_path` are set
- `server.auth_file` is set
- `server.tenant_id` is omitted

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

For shared deployments, use:

- [`examples/ferrosa-memory-http.toml`](examples/ferrosa-memory-http.toml) for the HTTP server
- [`examples/http-auth.toml`](examples/http-auth.toml) for principal-to-tenant auth mapping

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
