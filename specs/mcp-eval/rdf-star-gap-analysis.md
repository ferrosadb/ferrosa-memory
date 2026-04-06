# RDF* Gap Analysis: ferrosa-memory

## Current Coverage: ~70% of RDF* Concepts

### What's Already There (the 70%)

| RDF* Feature | ferrosa-memory Implementation | Coverage |
|---|---|---|
| Triple structure (s,p,o) | `TypedEdge(src_id, edge_type, dst_id)` | 100% |
| Temporal validity | `valid_until` + `event_time` on TemporalEvent | 100% |
| Provenance chains | `ProvenanceStep` tracking in DerivedFact | 95% |
| Confidence/weight | `weight: f64` on edges, `confidence: f64` on facts | 100% |
| Context/named graphs | `session_id` + `tenant_id` scoping on all tables | 100% |
| Type system | `entity_types` + `edge_types` registry with constraints | 85% |
| Graph annotations | CQL schema extensions for vertex/edge labels | 90% |

### What's Missing (the 30%)

| RDF* Feature | Current State | Gap | Coverage |
|---|---|---|---|
| **Structured metadata on edges** | `metadata: Option<String>` (JSON blob) | Not queryable, not typed, not first-class | 60% |
| **Metadata in Datalog rules** | Rules see `edge(Src, Pred, Dst)` only | Can't filter by confidence/source in rules | 0% |
| **Reified metadata** (statements about statements about statements) | Not supported | Can't annotate annotations | 0% |
| **RDF URI/IRI system** | UUIDs + string names | No standard namespace support | 30% |
| **SPARQL query language** | Cypher + Datalog instead | Different query paradigm | 40% |
| **Metadata schema validation** | No property constraints | Can't enforce metadata shape | 0% |

## Path to 95% (3 key additions)

### 1. Structured Edge Metadata Table (2-3 days)

Replace the `metadata: Option<String>` blob with a first-class property table:

```sql
CREATE TABLE IF NOT EXISTS agent_memory.edge_annotations (
    tenant_id uuid,
    session_id uuid,
    src_id uuid,
    edge_type text,
    dst_id uuid,
    property_name text,
    property_value text,
    value_type text,        -- 'string', 'float', 'uuid', 'datetime'
    created_at timestamp,
    PRIMARY KEY ((tenant_id, session_id, src_id, edge_type, dst_id), property_name)
);
```

This enables:
- Multiple properties per edge (1:N)
- Typed values (not just string blobs)
- Queryable via CQL WHERE clauses
- Edge provenance: `created_by = 'consolidation'` vs `created_by = 'user_explicit'`

### 2. Metadata Predicates in Datalog (3-5 days)

Add built-in predicate `annotation/5` to Datalog:

```datalog
-- Only trust high-confidence relationships
trusted_related(X, Y) :-
    edge(X, related, Y),
    annotation(X, related, Y, confidence, C),
    C > 0.8.

-- Find relationships discovered by consolidation
emergent(X, Y) :-
    edge(X, co_occurs, Y),
    annotation(X, co_occurs, Y, created_by, "consolidation").
```

This directly enables:
- **Eval emergence scoring** (ET-E2): filter by `created_by` in Datalog
- **DIKW layer tagging**: annotate facts with their DIKW level
- **Confidence-gated inference**: only derive from trusted edges

### 3. Optional URI Support (2 days)

Add optional `uri: Option<String>` to EntityEntry and TypedEdge:

```rust
pub struct EntityEntry {
    // ... existing fields ...
    pub uri: Option<String>,  // e.g., "http://example.org/person/alice"
}
```

This enables:
- Interop with external RDF vocabularies (FOAF, PROV, Dublin Core)
- Federated linking to DBpedia, Wikidata, etc.
- Standard namespace prefixes in type system

## Impact on Eval Framework

### Emergence Scoring (FMEA EF02, RPN 245 → solved)

With `edge_annotations`, every edge carries `created_by`:
- `consolidation` — CO_OCCURS discovered by `run_consolidation`
- `datalog` — derived by Datalog rule evaluation
- `spread` — discovered by `spread_activation`
- `explicit` — created by user via `create_edge`
- `ingest` — created by `smart_ingest` SUPERSEDE

The DIKW emergence analyzer queries:
```sql
SELECT count(*) FROM edge_annotations
WHERE property_name = 'created_by' AND property_value != 'explicit'
  AND tenant_id = ? AND session_id = ?
```

### Inference Auditing (FMEA EF03, RPN 224 → improved)

Datalog rules can now express confidence thresholds:
```datalog
verified_path(X, Y) :-
    edge(X, related, Z),
    annotation(X, related, Z, confidence, C1),
    edge(Z, related, Y),
    annotation(Z, related, Y, confidence, C2),
    C1 > 0.7, C2 > 0.7.
```

### DIKW Layer Tagging

Every fact annotated with its DIKW level:
```
annotation(alice, knows, bob, dikw_level, "data")        -- raw assertion
annotation(alice, related, acme, dikw_level, "knowledge") -- derived by consolidation
annotation(alice, needs, deploy, dikw_level, "wisdom")    -- intention-driven
```

## SPARQL Endpoint (Full Support)

### Architecture

Add a SPARQL endpoint to ferrosa-memory that translates SPARQL queries into the existing Datalog + CQL backend:

```
SPARQL query → SPARQL parser → Algebra tree → Datalog/CQL planner → Execute → RDF result set
```

**Endpoint:** `GET/POST /sparql` on the existing web console port (9090) or a new dedicated port.

**Supported SPARQL features:**

| Feature | Priority | Implementation Strategy |
|---|---|---|
| SELECT | P0 | Map to CQL SELECT + Datalog evaluation |
| WHERE (basic graph patterns) | P0 | Triple patterns → edge_list + entity queries |
| FILTER | P0 | Map to CQL WHERE + Rust predicate evaluation |
| OPTIONAL | P1 | Left-join semantics on result sets |
| UNION | P1 | Concat result sets |
| ORDER BY / LIMIT / OFFSET | P0 | Post-processing on result sets |
| RDF* triple annotations | P1 | `<< ?s ?p ?o >> ?prop ?val` → edge_annotations queries |
| CONSTRUCT | P2 | Build RDF graph from results |
| ASK | P1 | Boolean existence check |
| DESCRIBE | P2 | Entity neighborhood expansion |
| Property paths (`?s foaf:knows+ ?o`) | P2 | Map to `spread_activation` or BFS |
| Federated SPARQL (SERVICE) | P3 | Future — query external endpoints |

**Parser:** Use the `spargebra` crate (Rust SPARQL algebra parser, used by Oxigraph) or `sparql-parser`.

**Key translations:**
```sparql
-- SPARQL:
SELECT ?name ?type WHERE {
    ?e a ?type .
    ?e ex:name ?name .
    FILTER (?type = ex:Person)
}

-- Translates to CQL:
SELECT entity_name, entity_type FROM entity_store
WHERE tenant_id = ? AND session_id = ? AND entity_type = 'person'
```

```sparql
-- SPARQL* (annotated triples):
SELECT ?src ?dst ?conf WHERE {
    << ?src ex:related ?dst >> ex:confidence ?conf .
    FILTER (?conf > 0.8)
}

-- Translates to:
SELECT te.src_id, te.dst_id, ea.property_value
FROM typed_edges te
JOIN edge_annotations ea ON (te.src_id = ea.src_id AND te.edge_type = ea.edge_type AND te.dst_id = ea.dst_id)
WHERE ea.property_name = 'confidence' AND CAST(ea.property_value AS float) > 0.8
```

### Serialization Formats

| Format | Content-Type | Priority |
|---|---|---|
| SPARQL JSON Results | `application/sparql-results+json` | P0 |
| Turtle | `text/turtle` | P1 |
| N-Triples | `application/n-triples` | P1 |
| JSON-LD | `application/ld+json` | P2 |
| RDF/XML | `application/rdf+xml` | P3 (low priority) |

### New Crate: `ferrosa-memory-sparql`

```
crates/ferrosa-memory-sparql/
    Cargo.toml          (depends on spargebra, ferrosa-memory-core)
    src/
        lib.rs
        parser.rs       # SPARQL text → algebra tree (via spargebra)
        planner.rs      # Algebra → execution plan (CQL queries + Datalog)
        executor.rs     # Run plan against Storage trait
        results.rs      # Format results (JSON, Turtle, N-Triples)
        endpoint.rs     # HTTP handler (axum) for /sparql
        rdf_star.rs     # RDF* annotation query support
        namespace.rs    # Standard prefix management (foaf, dc, prov, rdf, rdfs)
```

### Impact on Eval Framework

SPARQL becomes a **fourth query mode** for the Semantic Analyzer:
- L3 multi-hop tests can use SPARQL property paths
- L3 inference tests can query derived facts via SPARQL
- L3 RDF* annotation queries verify edge provenance
- Eval scenarios can include SPARQL queries as verification steps

## What We're NOT Doing (and why)

| Feature | Decision | Rationale |
|---|---|---|
| Nested reification | Skip | Statements about statements about statements is academic. Provenance chains cover practical needs. |
| OWL reasoning | Skip | Datalog covers our inference needs. OWL adds complexity without practical benefit for memory systems. |

### SPARQL Update (Write Support)

Full SPARQL Update (RFC 3068) support for mutations:

**Supported operations:**

| Operation | Priority | Maps To |
|---|---|---|
| `INSERT DATA { ... }` | P0 | `upsert_entity` + `create_edge` + `edge_annotations` |
| `DELETE DATA { ... }` | P0 | `delete_session` (scoped), edge/entity removal |
| `INSERT { ... } WHERE { ... }` | P1 | Pattern-matched bulk insert (like `batch_create_edges` with a query filter) |
| `DELETE { ... } WHERE { ... }` | P1 | Pattern-matched bulk delete |
| `DELETE/INSERT (MODIFY)` | P1 | Atomic update: delete old + insert new (maps to `smart_ingest` SUPERSEDE) |
| `LOAD <uri>` | P2 | Import Turtle/N-Triples file into a session |
| `CLEAR GRAPH <uri>` | P2 | `delete_session` for a named graph |
| `DROP GRAPH <uri>` | P3 | Remove graph metadata |

**Key design decisions:**

1. **Writes go through the same Storage trait** — SPARQL UPDATE doesn't bypass the MCP tool layer's validation (confidence gating, dedup, type checking). The planner translates SPARQL mutations into the equivalent `Storage` trait calls.

2. **RDF* annotations on INSERT** — SPARQL* syntax for annotated inserts:
   ```sparql
   INSERT DATA {
       << ex:alice ex:knows ex:bob >> ex:confidence 0.95 ;
                                      ex:created_by "sparql" ;
                                      ex:dikw_level "data" .
   }
   ```
   Translates to: `typed_edge_put` + `annotation_put`.

3. **Tenant/session scoping** — all SPARQL writes are scoped to the authenticated tenant + session from the HTTP request context. No cross-tenant writes possible.

4. **Audit logging** — every SPARQL UPDATE logs to the audit trail with the full query text, affected triple count, and tenant context. Same as MCP tool audit logging.

5. **Transaction semantics** — `DELETE/INSERT` (MODIFY) is atomic within a single partition key. Cross-partition atomicity uses the existing batchlog protocol from ferrosa-cluster.

**New module:**
```
crates/ferrosa-memory-sparql/src/
    update.rs       # SPARQL UPDATE parser + planner
    write_plan.rs   # Translate UPDATE algebra → Storage trait calls
```

## Files to Modify

| File | Change |
|---|---|
| `ddl/` | New `edge_annotations` table DDL |
| `types.rs` | `EdgeAnnotation` struct, optional `uri` on EntityEntry/TypedEdge |
| `cql_storage.rs` | CRUD for edge_annotations table |
| `storage.rs` | `Storage` trait: `annotation_put`, `annotation_get`, `annotation_list` |
| `datalog.rs` | Add `annotation/5` built-in predicate |
| `dispatch.rs` | Update `create_edge`/`batch_create_edges` to accept annotations |
| `dream.rs` | `run_consolidation` writes `created_by: "consolidation"` annotation |
| `smart_ingest.rs` | SUPERSEDE writes `created_by: "ingest"` annotation |
