# Data Flow Diagrams

## Boundary Correction

These diagrams distinguish between:

- **current implementation** — where `ferrosa-memory` has completed public CQL/SPARQL/Cypher adaptation in operator surfaces, but still needs graph-boundary cleanup in serving-path writes
- **target implementation** — where `ferrosa-memory` uses public protocols/contracts at the right abstraction level

The target boundary is defined in [ADR-005](./decisions/adr-005-endpoint-only-ferrosa-client.md). Direct CQL is acceptable where `ferrosa-memory` is acting as an application client over app-owned tables. The remaining boundary gap is direct mutation of graph-owned backing tables from serving-path writes. If Ferrosa public query interfaces do not satisfy the required semantics, that is a Ferrosa bug and `ferrosa-memory` should fail loudly instead of papering over behavior locally.

## 1. Tool Call Flow (All Tools)

Target MCP tool flow:

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant C as MCP Client
    participant T as transport
    participant D as tool_dispatch
    participant A as auth
    participant R as tool_router
    participant H as Tool Handler
    participant F as Ferrosa public interface
    participant M as metrics

    C->>T: JSON-RPC tools/call
    T->>D: parse(tool_name, params)
    D->>A: authenticate(request)
    A-->>D: TenantContext { tenant_id }
    D->>R: route(query_context)
    R-->>D: Strategy
    D->>H: handle(params, tenant_ctx, strategy)
    H->>F: CQL / SPARQL / Cypher request
    F-->>H: Result rows / graph answers / derived facts
    H->>M: emit_metric(operation, latency)
    H-->>D: tool result
    D-->>T: JSON-RPC response
    T-->>C: result
```

Current implementation note: the serving path still includes graph-table writes through direct `CqlStorage` access while Datalog remains an explicit local inference layer.

Unless otherwise labeled as target-state, the remaining diagrams document the current implementation paths that still need to be refactored behind graph/public interfaces. They should not be read as the desired long-term architecture.

## 1b. Bulk `ingest_entities` Flow

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant C as MCP Client
    participant D as tool_dispatch
    participant A as auth
    participant BI as bulk_ingest
    participant E as embedding_client
    participant DB as Ferrosa app tables
    participant G as Ferrosa graph interface

    C->>D: ingest_entities(batch)
    D->>A: authenticate(request)
    A-->>D: TenantContext
    D->>BI: validate + execute(batch, tenant_ctx)
    BI->>BI: validate attrs, conflict mode, edge refs

    alt dry_run = true
        BI->>BI: compute write plan only
        BI-->>D: counts + failed[] + schema_version
    else dry_run = false
        alt missing embeddings and embed_missing = true
            BI->>E: embed(missing contexts)
            E-->>BI: vectors or per-row failures
        end
        BI->>DB: UPSERT app-owned entity rows
        BI->>G: write typed edges / graph-owned mutations
        BI-->>D: inserted/updated/skipped + failed[] + duration_ms
    end
    D-->>C: MCP result + progress notifications
```

Current implementation note: this is target-state architecture. The current codebase has adjacent ingest tools, but not yet a single server-owned `ingest_entities` contract with structured batch failure semantics.

## 2. Memoization Write Path

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant LLM as LLM Agent
    participant MCP as ferrosa-memory-mcp
    participant E as embedding_client
    participant K as compression
    participant DB as Ferrosa (memo_cache)

    LLM->>MCP: check_memo_cache(prompt, context_slice, model)
    MCP->>MCP: hash = SHA-256(normalize(prompt) + context_slice)
    MCP->>DB: SELECT WHERE content_hash = hash
    alt Cache Hit
        DB-->>MCP: result row
        MCP->>DB: UPDATE hit_count += 1, last_hit_at = now()
        MCP-->>LLM: { hit: true, result, hit_count }
    else Cache Miss
        DB-->>MCP: empty
        MCP-->>LLM: { hit: false }
        Note over LLM: LLM executes sub-call
        LLM->>MCP: store_memo_result(prompt, ctx, model, result, embedding)
        MCP->>E: embed(result)
        E-->>MCP: Vec<f32>
        alt token_count > threshold
            MCP->>K: compress(result)
            K-->>MCP: compressed_result
        end
        MCP->>DB: INSERT INTO memo_cache
        MCP-->>LLM: { stored: true, content_hash }
    end
```

## 3. Fold Lifecycle

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant LLM as LLM Agent
    participant MCP as ferrosa-memory-mcp
    participant DB as Ferrosa (trajectory_folds)
    participant G as Ferrosa (graph layer)
    participant K as compression
    participant S3 as S3 Storage

    LLM->>MCP: start_fold(session, depth, parent_fold_id, initial_ctx)
    MCP->>DB: INSERT status='active'
    MCP-->>LLM: { fold_id }

    loop Each REPL turn
        LLM->>MCP: append_to_fold(fold_id, repl_turn)
        MCP->>DB: UPDATE raw_trajectory += repl_turn
        MCP-->>LLM: { appended, token_count }
    end

    LLM->>MCP: complete_fold(fold_id, summary, embedding)
    MCP->>DB: UPDATE status='folded', fold_summary, fold_embedding
    MCP->>G: CREATE (child)-[:FOLDED_INTO]->(parent)
    MCP-->>LLM: { folded, compression_ratio }

    Note over MCP,K: Background: compress raw_trajectory
    MCP->>K: compress(raw_trajectory)
    K-->>MCP: compressed
    MCP->>DB: UPDATE raw_trajectory = compressed

    Note over DB,S3: Lifecycle: archive after 30 days
    DB->>S3: status='archived' rows -> Glacier
```

## 4. Entity Discovery and Retrieval

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant LLM as LLM Agent
    participant MCP as ferrosa-memory-mcp
    participant E as embedding_client
    participant DB as Ferrosa (entity_store)
    participant G as Ferrosa (graph layer)

    LLM->>MCP: upsert_entity(name, type, snippet, embedding)
    MCP->>DB: SELECT WHERE phonetic_match(entity_name)
    alt Phonetic match found
        DB-->>MCP: existing entity_id
        MCP->>DB: UPDATE entity row
        MCP-->>LLM: { entity_id, is_new: false }
    else No match
        MCP->>DB: INSERT new entity row
        MCP->>G: CREATE (entity) vertex
        MCP->>G: CREATE (entity)-[:MENTIONED_IN]->(fold)
        MCP-->>LLM: { entity_id, is_new: true }
    end

    LLM->>MCP: retrieve_entities(query, strategy='both')
    par Phonetic path
        MCP->>DB: SELECT WHERE phonetic_match(query)
    and ANN path
        MCP->>E: embed(query)
        E-->>MCP: query_embedding
        MCP->>DB: SELECT ORDER BY embedding <=> query_embedding LIMIT k
    end
    MCP->>MCP: union-merge, deduplicate by entity_id
    MCP-->>LLM: { entities }
```

## 5. Temporal Event Chain

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant LLM as LLM Agent
    participant MCP as ferrosa-memory-mcp
    participant DB as Ferrosa (temporal_events)
    participant G as Ferrosa (graph layer)

    LLM->>MCP: (via entity tools) write temporal fact
    MCP->>DB: SELECT latest WHERE entity_id AND valid_until IS NULL
    alt Supersedes existing fact
        MCP->>DB: UPDATE prior row: valid_until = now()
        MCP->>DB: INSERT new fact with supersedes_id = prior.event_id
        MCP->>G: CREATE (new)-[:SUPERSEDES]->(old)
    else First fact for entity
        MCP->>DB: INSERT new fact
    end
```

## 6. Feedback and Routing Learning Loop

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant LLM as LLM Agent
    participant MCP as ferrosa-memory-mcp
    participant DB as Ferrosa (feedback_outcomes)
    participant BATCH as Nightly Batch Job (Rust)
    participant CFG as routing_guidelines config

    LLM->>MCP: record_outcome(query_id, program_type, succeeded, latency, cost)
    MCP->>DB: INSERT feedback row

    Note over BATCH: Nightly at 02:00
    BATCH->>DB: SELECT WHERE succeeded = false (failure pairs)
    BATCH->>BATCH: Analyze strategy distribution, compute updated routing weights
    BATCH->>CFG: Write new guideline_version
    Note over MCP: On next request, router loads latest guideline_version
```

## 7. Recursive Exploration Flow

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant C as MCP Client
    participant RE as recursive_explore
    participant HS as hybrid_search
    participant DL as datalog
    participant W as warmth
    participant S as cql_client

    C->>RE: recursive_explore(session_id, query, embedding)
    RE->>RE: decompose_query(query) -> sub_queries

    Note over RE: Pass 1: Initial retrieval
    loop Each sub_query
        RE->>HS: hybrid_search(sub_query, embedding, 5-signal RRF)
        HS-->>RE: seed entities + folds
    end

    loop Pass 2..N (max 5 passes)
        RE->>DL: load_session_facts(session_id)
        DL->>S: entity_list_session + edge_list_session + warmth_list_session
        S-->>DL: raw rows
        DL-->>DL: normalize to canonical predicates

        RE->>DL: evaluate(rules, facts, max_iter=100)
        DL->>DL: semi-naive fixpoint (related, cluster, reachable)
        DL-->>RE: derived_facts + provenance

        RE->>RE: discover new entities from derived facts

        alt New entities found (novelty > threshold)
            RE->>HS: hybrid_search(new entity contexts)
            HS-->>RE: additional results
        else Converged (no new facts OR novelty < threshold)
            RE->>RE: break loop
        end
    end

    Note over RE,W: Warmth boost for all returned entities
    loop Each returned entity
        RE->>W: boost_on_access(entity_id, session_id, decay_zone)
        W->>S: warmth_boost + 1-hop neighbor spread
    end

    RE-->>C: RecursiveExploreResult {results, sub_queries, passes, converged, provenance}
```

## 8. Data Tiering Path

```mermaid
%%{init: {'theme':'dark','themeVariables':{'primaryColor':'#16161f','primaryTextColor':'#e8e8ed','primaryBorderColor':'#e2725b','lineColor':'#9494a3','secondaryColor':'#1c1c28','tertiaryColor':'#111118','clusterBkg':'#111118','clusterBorder':'#1e1e2a','edgeLabelBackground':'#111118','nodeTextColor':'#e8e8ed'}}}%%
graph LR
    subgraph "Hot (NVMe)"
        A[Active folds]
        B[memo_cache < 7d]
        C[entity_store]
    end

    subgraph "Warm (S3 Standard)"
        D[Folded trajectories, compressed]
        E[memo_cache 7-30d]
    end

    subgraph "Cold (S3 Glacier)"
        F[Archived trajectories > 30d]
        G[Expired memo results]
    end

    A -->|complete_fold + compress| D
    B -->|TTL expiry sweep| E
    D -->|lifecycle 30d| F
    E -->|lifecycle 30d| G
```

## 9. Type Registry and Tool Schema Generation

Current implementation note: startup still loads type metadata through direct CQL storage bindings. That is acceptable only if those remain app-owned tables compatible with the `app_reader` role boundary; it should not require graph-table mutation or privileged graph ownership.

Target behavior: type/system metadata should come from Ferrosa public interfaces or an explicitly versioned metadata contract where possible, but direct CQL reads remain acceptable if they stay within supported app-table scope.

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant MCP as ferrosa-memory
    participant API as Ferrosa public interface

    MCP->>API: connect(auth, tenant)
    MCP->>API: read type / schema metadata
    API-->>MCP: entity + edge types / query capabilities
    Note over MCP: Build tool schemas with<br/>dynamic entity_type enums
    MCP->>MCP: tool_definitions(entity_types)
```

## 10. Tool Usage Logging (DDL 009)

Every MCP tool call is logged to the `tool_usage_log` table for token usage analytics. The table is partitioned by `(tenant_id, day)` with `call_id` (timeuuid) clustering in descending order for efficient recent-first queries.

**Columns recorded:** `tool_name`, `repo`, `input_bytes`, `output_bytes`, `estimated_tokens`, `latency_ms`, `error` (boolean).

**Write path:** After each tool handler returns, `tool_dispatch` writes a row to `tool_usage_log` with the call metrics. This is fire-and-forget (does not block the tool response).

**Read path:** Analytics queries aggregate by day and tool name for cost attribution and usage trending. Not exposed as an MCP tool — queried via Ferrosa's public CQL interface.

## 11. External Codebase Ingestion (via forge)

Current implementation note: external ingestion still uses direct CQL/table ownership. The corrected architecture should treat graph publication as another Ferrosa client and target graph/public write interfaces instead of direct graph-table inserts.

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant ST as frg ingest
    participant API as Ferrosa public interface
    participant VIZ as Viz Server

    ST->>ST: extract(dir) → entities + edges
    Note over ST: Rust: crates, modules, deps<br/>Markdown: documents, sections<br/>Cross-refs: section→code entity
    ST->>API: publish entities + edges
    Note over VIZ: Next browser connect<br/>sees new entities in snapshot
```

## 12. Expert-System Knowledge Plane Flow

The expert-system path extends the existing Datalog layer with an explicit effective-rule-set loader, governance-backed symbolic artifacts, and explanation queries. Backend convergence is implemented: the effective loader is the active source for runtime rule evaluation in registry-facing paths.

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant C as MCP / Workbench Client
    participant D as dispatch
    participant E as EffectiveRuleSet
    participant R as RuleRegistry
    participant CL as ClaimStore
    participant AP as ApprovalStore
    participant AL as AliasStore
    participant DL as datalog / provenance
    participant DB as Ferrosa DB

    C->>D: manage_rules / manage_claims / manage_approvals / manage_aliases
    D->>R: load active stored rules
    D->>CL: read/write symbolic claims
    D->>AP: read/write reviewer decisions
    D->>AL: exact alias lookup / update

    C->>D: query_derived / recursive_explore / explain_derived
    D->>E: load_effective_rules(scope, family?)
    E->>R: list active stored rules
    E->>DB: load built-in baseline metadata
    E-->>D: merged effective rule set
    D->>DL: evaluate(rules, facts) or reconstruct support chain
    DL->>DB: derived cache + provenance + supporting facts
    DL-->>D: derived facts / explanation payload
    D-->>C: effective rules, claim state, approvals, aliases, explanations

    Note over E,DL: `manage_rules`, `query_derived`, `recursive_explore`, and `promotion` all call<br/>the same effective-rule loader today.
```

## 13. Operator Workbench

The operator surface should stop treating `/viz` as the de facto home page. Instead, expose an integrated workbench that links the operator workflows together and makes raw data exploration explicit.

Current implementation note: backend convergence and governance APIs are implemented in MCP/core, and the operator workbench is now rooted at `/` with API-backed CQL, Datalog, rules, approvals, and summary routes. The remaining architectural correction is to keep CQL/SPARQL on Ferrosa public APIs while documenting Datalog honestly as a local ferrosa-memory engine over Ferrosa-backed state.

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant O as Operator Browser
    participant UI as Workbench Home (/)
    participant V as Viz (/viz)
    participant CQ as CQL Explorer
    participant DQ as Datalog Explorer
    participant RM as Rules Manager
    participant API as Operator APIs
    participant FE as Ferrosa public APIs

    O->>UI: GET /
    UI-->>O: Home page with system status + nav

    alt Explore raw data
        O->>CQ: Open CQL explorer
        CQ->>API: submit CQL passthrough query
        API->>FE: forward CQL request
        FE-->>API: rows + errors
        API-->>CQ: rows + timing + surfaced errors
    else Explore derived knowledge
        O->>DQ: Open Datalog explorer
        DQ->>API: submit local Datalog query
        API->>API: evaluate repo-owned Datalog engine over scoped data
        API-->>DQ: derived facts + provenance + surfaced errors
    else Manage rule base
        O->>RM: Open rules manager
        RM->>API: list builtin / registry / effective rules
        API-->>RM: rules + activation state + diff
    else Visualize graph
        O->>V: Open viz
        V->>API: snapshot / ws / derived facts
        API-->>V: graph + live updates
    end

    Note over UI,API: The home page becomes the workbench root.<br/>Viz remains one mode, not the only one.
```
