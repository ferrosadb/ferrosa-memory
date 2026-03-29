# Data Flow Diagrams

## 1. Tool Call Flow (All Tools)

Every MCP tool call follows this path:

```mermaid
%%{init: {'theme':'dark','themeVariables':{'actorBkg':'#16161f','actorTextColor':'#e8e8ed','actorBorder':'#e2725b','signalColor':'#9494a3','signalTextColor':'#e8e8ed','labelBoxBkgColor':'#16161f','labelBoxBorderColor':'#e2725b','labelTextColor':'#e8e8ed','loopTextColor':'#e8e8ed','noteBkgColor':'#1c1c28','noteBorderColor':'#d4a574','noteTextColor':'#e8e8ed','activationBkgColor':'#1c1c28','activationBorderColor':'#e2725b'}}}%%
sequenceDiagram
    participant C as MCP Client
    participant T as transport
    participant D as tool_dispatch
    participant A as auth
    participant R as tool_router
    participant H as Tool Handler
    participant S as cql_client
    participant M as metrics

    C->>T: JSON-RPC tools/call
    T->>D: parse(tool_name, params)
    D->>A: authenticate(request)
    A-->>D: TenantContext { tenant_id }
    D->>R: route(query_context)
    R-->>D: Strategy
    D->>H: handle(params, tenant_ctx, strategy)
    H->>S: CQL query (tenant-scoped)
    S-->>H: Result rows
    H->>M: emit_metric(operation, latency)
    H-->>D: tool result
    D-->>T: JSON-RPC response
    T-->>C: result
```

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
