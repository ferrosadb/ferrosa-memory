# Product Specification: Datalog-Style Graph Inference and Materialized Knowledge on a Cassandra-Like Backend

## 1. Purpose

This document specifies a graph knowledge system that:

- stores a rich, evolving concept graph in a Cassandra-like database with Cypher query support,
- performs inference using a Datalog-style logical layer,
- persists derived knowledge as materialized facts for query performance and scale,
- uses a Cassandra-backed hot cache for ephemeral inferred results,
- keeps hot ephemeral data on NVMe where available,
- promotes selected inferred predicates and concepts into durable materialized tables based on observed workload.

The design assumes:

- the ontology will evolve over time,
- new concepts and rules must be addable without frequent storage-schema migrations,
- the full derived graph may exceed memory,
- the system should optimize for repeated query patterns and explainable derivations.

---

## 2. Product Goals

### 2.1 Goals

1. Support a dynamic concept graph with typed entities, typed relationships, metadata, provenance, and confidence.
2. Support Datalog-style derivation of new assertions from existing facts and rules.
3. Materialize selected derived relations to disk for scale and low-latency repeated query performance.
4. Preserve provenance for all derived facts.
5. Allow hot ephemeral inferred results to be cached in Cassandra tables with TTL.
6. Allow high-value hot cache tables to remain on fast local NVMe in a hierarchical storage deployment.
7. Avoid frequent DDL churn as the ontology evolves.
8. Allow Cypher-facing graph queries over both base and derived knowledge.

### 2.2 Non-Goals

1. This system is not a generic OLTP relational engine.
2. This system is not a full RDF/OWL standards-compliance product.
3. This system is not a general-purpose workflow engine or CEP engine.
4. This system is not primarily optimized for arbitrary global graph scans.
5. This system is not designed around unbounded all-pairs closure materialization.

---

## 3. Core Design Principles

1. **Ontology evolution is data evolution.**
   New concepts, edge types, rules, aliases, and taxonomy terms are inserted as rows, not introduced via frequent schema changes.

2. **Physical schema remains stable.**
   A small set of generic storage tables is preserved. Domain evolution happens within those tables.

3. **Inference layer is logical and relational.**
   The Cypher/property graph is normalized into canonical Datalog-style relations.

4. **Materialization is a physical optimization.**
   Derived facts can be persisted as memoized results when query patterns justify it.

5. **Promotion is workload-driven.**
   Concepts and predicates are promoted to durable materializations based on heat, reuse, size, and update cost.

6. **Cache is separate from source of truth.**
   Ephemeral inferred results live in dedicated cache tables with TTL and bounded compaction behavior.

7. **Provenance is first-class.**
   Every derived fact must be explainable.

---

## 4. High-Level Architecture

The system has five layers:

1. **Authoring / Ingestion Layer**
   - writes nodes, edges, properties, sources, and metadata into the base graph.
   - supports direct graph ingestion and document extraction pipelines.

2. **Canonical Fact Extraction Layer**
   - normalizes graph data into logical predicates.
   - example: property graph edges become `edge(src, pred, dst)`.

3. **Inference Layer**
   - evaluates Datalog rules over base facts and previously-derived facts.
   - supports batch rematerialization and incremental delta propagation.

4. **Ephemeral Cache Layer**
   - stores recently-computed inferred results in Cassandra TTL tables.
   - optimized for short-lived reuse.

5. **Durable Materialization Layer**
   - stores promoted derived relations in durable tables.
   - projects selected derived results back into the queryable graph.

---

## 5. Conceptual Data Model

### 5.1 Node Model

Each node represents a concept, entity, artifact, source, principle, tool, methodology component, or related object.

Core node attributes:

- `id`
- `primary_label`
- `kind`
- `name`
- `state`
- `confidence`
- `created_at`
- `props`

Example node classes:

- `Concept`
- `Methodology`
- `Phase`
- `Activity`
- `Principle`
- `Practice`
- `Tool`
- `CLI`
- `Command`
- `Language`
- `Framework`
- `TestingFramework`
- `Repository`
- `Paper`
- `MarkdownArtifact`
- `MaturityModel`
- `MaturityLevel`
- `Skill`
- `Assertion`
- `Evidence`
- `Rule`

### 5.2 Edge Model

Each edge is a typed relationship between nodes.

Core edge attributes:

- `src_id`
- `pred`
- `dst_id`
- `is_derived`
- `confidence`
- `created_at`
- `rule_id`
- `props`

Preferred semantic edge types:

- `HAS_PHASE`
- `NEXT_PHASE`
- `INCLUDES_ACTIVITY`
- `EMBODIES_PRINCIPLE`
- `SUPPORTS_PRACTICE`
- `PREFERS_TOOL`
- `ALTERNATIVE_TO`
- `USES_LANGUAGE`
- `HAS_COMMAND`
- `DOCUMENTS`
- `DISTILLED_TO`
- `HAS_MATURITY_MODEL`
- `HAS_MATURITY_LEVEL`
- `APPLIES_TO`
- `PART_OF`
- `INSTANCE_OF`
- `SUBCLASS_OF`
- `SAME_AS`
- `MENTIONED_WITH`
- `DERIVED_FROM`

### 5.3 Evidence vs Semantic Edges

Weak evidence edges such as `MENTIONED_WITH` are not treated as primary semantic structure.
They are inputs into rule-based promotion, not the main representation of meaning.

---

## 6. Canonical Logical Schema

The inference engine operates on canonical predicates.

### 6.1 Core Predicates

```prolog
node(Id).
node_label(Id, Label).
node_name(Id, Name).
node_kind(Id, Kind).
node_state(Id, State).
node_confidence(Id, Score).
node_created_at(Id, Ts).
alias(Id, Alias).

edge(Src, Pred, Dst).
edge_confidence(Src, Pred, Dst, Score).
edge_state(Src, Pred, Dst, State).
edge_created_at(Src, Pred, Dst, Ts).
edge_source(Src, Pred, Dst, SourceId).

instance_of(Entity, Class).
subclass_of(ChildClass, ParentClass).
part_of(Child, Parent).
same_as(A, B).
```

### 6.2 Derived Predicates

```prolog
derived_edge(Src, Pred, Dst).
derived_by_rule(Src, Pred, Dst, RuleId).
derived_confidence(Src, Pred, Dst, Score).
derived_support_count(Src, Pred, Dst, N).
derived_batch(Src, Pred, Dst, BatchId).
derived_from(Src, Pred, Dst, ParentSrc, ParentPred, ParentDst).
```

### 6.3 Promoted Hot Predicates

Hot predicates may be promoted from the generic edge space into specialized logical relations:

```prolog
has_phase(Methodology, Phase, Ordinal).
includes_activity(Phase, Activity).
prefers_tool(Context, Better, Worse).
methodology_member(Item, Methodology).
class_ancestor(Child, Parent).
isa(Entity, Class).
```

Promotion is optional and workload-driven.

---

## 7. Example Graph Normalization

### 7.1 Example Source Statement

"Agentic Engineering is a 5-phase methodology: Ideation (research/distill/decide)"

### 7.2 Graph Representation

```cypher
(:Methodology:Concept {
  id: "methodology/agentic_engineering",
  name: "Agentic Engineering",
  state: "active",
  confidence: 0.80
})

(:Phase:Concept {
  id: "phase/ideation",
  name: "Ideation",
  ordinal: 1
})

(:Activity:Concept {id: "activity/research", name: "Research"})
(:Activity:Concept {id: "activity/distill", name: "Distill"})
(:Activity:Concept {id: "activity/decide", name: "Decide"})

(:Methodology {id:"methodology/agentic_engineering"})
  -[:HAS_PHASE {ordinal: 1}]->
(:Phase {id:"phase/ideation"})

(:Phase {id:"phase/ideation"})-[:INCLUDES_ACTIVITY]->(:Activity {id:"activity/research"})
(:Phase {id:"phase/ideation"})-[:INCLUDES_ACTIVITY]->(:Activity {id:"activity/distill"})
(:Phase {id:"phase/ideation"})-[:INCLUDES_ACTIVITY]->(:Activity {id:"activity/decide"})
```

### 7.3 Logical Normalization

```prolog
node("methodology/agentic_engineering").
node_label("methodology/agentic_engineering", "Methodology").
node_name("methodology/agentic_engineering", "Agentic Engineering").

node("phase/ideation").
node_label("phase/ideation", "Phase").
node_name("phase/ideation", "Ideation").

edge("methodology/agentic_engineering", "HAS_PHASE", "phase/ideation").
edge("phase/ideation", "INCLUDES_ACTIVITY", "activity/research").
edge("phase/ideation", "INCLUDES_ACTIVITY", "activity/distill").
edge("phase/ideation", "INCLUDES_ACTIVITY", "activity/decide").
```

---

## 8. Physical Storage Schema

The storage backend is Cassandra-like. The schema is query-oriented and denormalized.

### 8.1 Base Node Table

```sql
CREATE TABLE nodes_by_id (
  tenant text,
  node_id text,
  primary_label text,
  kind text,
  state text,
  confidence double,
  created_at timestamp,
  name text,
  props map<text, text>,
  PRIMARY KEY ((tenant, node_id))
);
```

### 8.2 Base Edge Tables

```sql
CREATE TABLE edges_by_src (
  tenant text,
  src_id text,
  shard smallint,
  pred text,
  dst_id text,
  is_derived boolean,
  rule_id text,
  confidence double,
  created_at timestamp,
  props map<text, text>,
  PRIMARY KEY ((tenant, src_id, shard), pred, dst_id)
);
```

```sql
CREATE TABLE edges_by_dst (
  tenant text,
  dst_id text,
  shard smallint,
  pred text,
  src_id text,
  is_derived boolean,
  rule_id text,
  confidence double,
  created_at timestamp,
  props map<text, text>,
  PRIMARY KEY ((tenant, dst_id, shard), pred, src_id)
);
```

```sql
CREATE TABLE edges_by_pred (
  tenant text,
  pred text,
  shard smallint,
  src_id text,
  dst_id text,
  is_derived boolean,
  confidence double,
  created_at timestamp,
  PRIMARY KEY ((tenant, pred, shard), src_id, dst_id)
);
```

### 8.3 Durable Materialization Tables

```sql
CREATE TABLE derived_edges_by_src (
  tenant text,
  src_id text,
  shard smallint,
  pred text,
  dst_id text,
  rule_id text,
  support_count int,
  confidence double,
  batch_id text,
  materialized_at timestamp,
  PRIMARY KEY ((tenant, src_id, shard), pred, dst_id)
);
```

```sql
CREATE TABLE derived_edges_by_pred (
  tenant text,
  pred text,
  shard smallint,
  src_id text,
  dst_id text,
  rule_id text,
  support_count int,
  confidence double,
  batch_id text,
  materialized_at timestamp,
  PRIMARY KEY ((tenant, pred, shard), src_id, dst_id)
);
```

### 8.4 Provenance Table

```sql
CREATE TABLE derivation_provenance (
  tenant text,
  derived_edge_id text,
  seq int,
  parent_src text,
  parent_pred text,
  parent_dst text,
  parent_kind text,
  PRIMARY KEY ((tenant, derived_edge_id), seq)
);
```

### 8.5 Rule Registry Table

```sql
CREATE TABLE rules_by_id (
  tenant text,
  rule_id text,
  version int,
  name text,
  family text,
  state text,
  rule_body text,
  rule_weight double,
  incremental boolean,
  created_at timestamp,
  updated_at timestamp,
  PRIMARY KEY ((tenant, rule_id), version)
);
```

### 8.6 Rule Family Index

```sql
CREATE TABLE rules_by_family (
  tenant text,
  family text,
  state text,
  rule_id text,
  version int,
  updated_at timestamp,
  PRIMARY KEY ((tenant, family, state), rule_id, version)
);
```

### 8.7 Ephemeral Cache Tables

#### Cache by Query Pattern

```sql
CREATE TABLE derived_cache_by_query (
  tenant text,
  cache_key text,
  seq int,
  src_id text,
  pred text,
  dst_id text,
  confidence double,
  rule_id text,
  computed_at timestamp,
  PRIMARY KEY ((tenant, cache_key), seq)
) WITH default_time_to_live = 3600;
```

#### Cache by Predicate

```sql
CREATE TABLE derived_cache_by_pred (
  tenant text,
  pred text,
  bucket text,
  src_id text,
  dst_id text,
  confidence double,
  rule_id text,
  computed_at timestamp,
  PRIMARY KEY ((tenant, pred, bucket), src_id, dst_id)
) WITH default_time_to_live = 3600;
```

### 8.8 Heat / Promotion Telemetry

Do not scan cache tables to infer heat.
Record heat separately.

```sql
CREATE TABLE query_heat_by_predicate_day (
  tenant text,
  day text,
  pred text,
  hits counter,
  PRIMARY KEY ((tenant, day), pred)
);
```

```sql
CREATE TABLE compute_cost_by_predicate_day (
  tenant text,
  day text,
  pred text,
  total_compute_ms counter,
  total_requests counter,
  PRIMARY KEY ((tenant, day), pred)
);
```

### 8.9 Specialized Durable Tables for Promoted Predicates

```sql
CREATE TABLE methodology_members_by_methodology (
  tenant text,
  methodology_id text,
  member_type text,
  member_id text,
  source_pred text,
  confidence double,
  batch_id text,
  PRIMARY KEY ((tenant, methodology_id), member_type, member_id)
);
```

```sql
CREATE TABLE tool_preferences_by_context (
  tenant text,
  context_id text,
  better_tool_id text,
  worse_tool_id text,
  confidence double,
  rule_id text,
  batch_id text,
  PRIMARY KEY ((tenant, context_id), better_tool_id, worse_tool_id)
);
```

---

## 9. Partitioning and Sharding Strategy

1. Never rely on giant unbounded partitions.
2. Use a shard key where cardinality can become large.
3. For TTL-heavy cache tables, bucket by time or query hash.
4. Keep partition size bounded for both write and read predictability.
5. Split read paths into dedicated tables instead of joins.

Recommended shard formula:

```text
shard = hash(src_id or pred) % N
```

Recommended cache bucket formula:

```text
bucket = YYYYMMDDHH or hash_prefix
```

---

## 10. Datalog Rules

### 10.1 Taxonomy Closure

```prolog
class_ancestor(C, P) :- subclass_of(C, P).
class_ancestor(C, P) :- subclass_of(C, M), class_ancestor(M, P).

isa(E, C) :- instance_of(E, C).
isa(E, P) :- instance_of(E, C), class_ancestor(C, P).
```

### 10.2 Part-Of Closure

```prolog
ancestor_part(X, Y) :- part_of(X, Y).
ancestor_part(X, Z) :- part_of(X, Y), ancestor_part(Y, Z).
```

### 10.3 Methodology Membership

```prolog
has_phase(M, P, Ordinal) :- edge(M, "HAS_PHASE", P), edge_ordinal(M, "HAS_PHASE", P, Ordinal).
includes_activity(P, A) :- edge(P, "INCLUDES_ACTIVITY", A).

methodology_member(P, M) :- has_phase(M, P, _).
methodology_member(A, M) :- has_phase(M, P, _), includes_activity(P, A).
```

### 10.4 Tool Recommendation

```prolog
prefers_tool(Context, Better, Worse) :- edge(Context, "PREFERS_TOOL", Better), edge(Context, "ALTERNATIVE_TO", Worse).
recommended_tool(Context, Better) :- prefers_tool(Context, Better, _).
```

### 10.5 Weak Evidence Promotion

```prolog
candidate_related(A, B) :-
  edge(A, "MENTIONED_WITH", B),
  cooccur_support(A, B, N),
  N >= 3.
```

Only promote `candidate_related` to stronger typed edges through a curated or supervised rule family.

### 10.6 Principle Embedding

```prolog
embodies_principle(Methodology, Principle) :- edge(Methodology, "EMBODIES_PRINCIPLE", Principle).
practice_applies_to(Practice, Methodology) :- edge(Practice, "APPLIES_TO", Methodology).
```

---

## 11. Confidence and Provenance Model

Each derived fact stores:

- `rule_id`
- `batch_id`
- `support_count`
- `derived_confidence`
- provenance parents

Recommended confidence combination:

```text
derived_confidence = min(parent_confidences) * rule_weight
```

Recommended support policy:

- keep `support_count` separate from `derived_confidence`
- avoid inflating confidence simply because many weak paths exist
- use support count for ranking and explanation

Example provenance record:

```text
derived_edge(methodology/agentic_engineering, methodology_member, activity/research)
  derived_by_rule = rule/methodology_member/v1
  derived_from = [
    (methodology/agentic_engineering, HAS_PHASE, phase/ideation),
    (phase/ideation, INCLUDES_ACTIVITY, activity/research)
  ]
```

---

## 12. Dynamic Ontology Evolution

### 12.1 What Can Change Dynamically

Inserted as data, not DDL:

- new node concepts
- new taxonomy terms
- new semantic edge labels
- new instances
- new aliases
- new rules
- new rule versions
- confidence weights
- deprecation states

### 12.2 What Should Stay Stable

- base node tables
- base edge tables
- derived edge tables
- provenance tables
- rule registry tables
- cache tables
- promotion pipeline tables

### 12.3 Rule Change Management

When a rule changes:

1. insert a new rule version,
2. mark prior version as deprecated or superseded,
3. compute impacted predicate families,
4. incrementally rematerialize if dependency region is bounded,
5. otherwise run batch rematerialization for that rule family,
6. stamp new results with the new `rule_id` and `batch_id`.

---

## 13. Cache Design

### 13.1 Why Use Cassandra as Hot Cache

Use Cassandra cache tables instead of Redis when:

- a single operational datastore is preferred,
- cache hit latency requirements are acceptable at database latencies,
- inference results are large and easier to co-locate with the graph store,
- the platform already supports hierarchical storage placement.

### 13.2 Cache Design Rules

1. Cache tables are not sources of truth.
2. Cache tables use TTL.
3. Cache tables should be append-oriented, not update/delete-heavy.
4. Cache tables should be bounded by bucket and shard.
5. Cache tables must not be scanned to infer hotness.
6. Heat is recorded in separate telemetry tables or external metrics.

### 13.3 NVMe Placement

If the storage platform supports pinning a hot table to NVMe:

- place TTL-heavy cache tables on NVMe,
- keep source-of-truth and colder materialized data on slower tiers as needed,
- use the fastest storage for the most churn-heavy ephemeral derived data.

### 13.4 Tombstones

TTL expiration still creates tombstones.
Pinning cache SSTables to NVMe reduces the cost of tombstone-heavy reads and compaction, but does not eliminate tombstones.

To reduce tombstone pain:

1. prefer uniform TTL per cache table,
2. avoid explicit deletes,
3. avoid frequent overwrites of the same cache key,
4. bound partition sizes,
5. use time-window-oriented compaction for TTL-heavy tables,
6. keep cache tables small enough that working compaction stays on fast storage.

---

## 14. Compaction and Hierarchical Storage Guidance

### 14.1 Recommended Strategy for Cache Tables

For TTL-heavy cache tables:

- use time-window compaction,
- use a single default TTL if possible,
- avoid mixed TTLs unless necessary,
- write once, expire naturally, do not churn rows.

### 14.2 Recommended Strategy for Durable Materializations

For durable materialized tables:

- use a compaction strategy suitable for stable read-heavy data,
- optimize around read access patterns,
- avoid frequent rewrites of large partitions.

### 14.3 Hierarchical Storage Policy

- cache tables pinned to NVMe,
- active durable materializations on fast local block storage,
- cold history and old snapshots on slower block or object-backed tiers,
- batch rematerialization outputs staged before compaction settles them.

---

## 15. Materialization and Promotion Policy

### 15.1 Philosophy

Materialization is memoization of inference results.
The ontology remains dynamic even when physical materializations are added.

### 15.2 Promotion Criteria

A predicate or concept becomes a candidate for durable materialization when most of the following are true:

- query frequency is high,
- recomputation cost is high,
- reuse factor is high,
- update churn is moderate or low,
- materialized size is bounded,
- latency requirements justify precomputation.

### 15.3 Promotion Score

```text
promotion_score = query_count_7d * median_compute_ms * reuse_factor / max(update_rate_7d, 1)
```

Additional gate:

```text
materialize only if estimated_materialized_rows <= size_budget
```

### 15.4 Good Early Candidates

- `class_ancestor`
- `isa`
- `ancestor_part`
- `methodology_member`
- normalized tool recommendations by context
- document-to-concept link tables

### 15.5 Bad Early Candidates

- full `MENTIONED_WITH` closure
- broad similarity graphs
- unbounded all-pairs reachability on noisy subgraphs
- anything with explosive fanout and poor reuse

---

## 16. Inference Execution Model

### 16.1 Batch Mode

Used for:

- initial backfill,
- rule family changes,
- ontology refactors,
- large import waves.

Flow:

1. read base facts,
2. normalize to predicates,
3. execute rule families,
4. write durable derived tables,
5. write provenance,
6. update projection indexes.

### 16.2 Incremental Mode

Used for:

- localized fact additions,
- bounded edits,
- hot-path operational updates.

Flow:

1. ingest base delta,
2. compute impacted rule families,
3. derive new deltas,
4. write ephemeral cache rows,
5. optionally write durable tables if predicate is promoted,
6. update provenance incrementally.

### 16.3 Query-Time Mode

For non-promoted predicates:

1. look for result in cache,
2. if miss, compute on demand,
3. write to cache with TTL,
4. record heat and compute cost,
5. periodically evaluate for promotion.

---

## 17. Query Patterns

### 17.1 Base Graph Queries

- fetch node by id
- outgoing edges for a node
- incoming edges for a node
- all edges by predicate

### 17.2 Derived Knowledge Queries

- what is the full methodology membership for a methodology?
- what principles does a methodology embody?
- what tools are recommended for a context?
- what classes does an entity belong to by inheritance?
- what facts were derived by a rule family?

### 17.3 Explanation Queries

- why does derived edge X exist?
- what source facts support derived edge X?
- which rule version produced derived edge X?
- how many support paths does this assertion have?

---

## 18. Projection Back to Cypher

Derived edges should be visible to graph consumers.
Projection options:

1. write derived edges back as graph-visible relationships with `is_derived = true`, or
2. maintain graph-view adapter queries that merge base and derived tables.

Recommended derived relationship properties:

- `is_derived`
- `rule_id`
- `batch_id`
- `confidence`
- `support_count`
- `materialized_at`

---

## 19. Example End-to-End Flow

### 19.1 Ingest

Input statement:

"Paper distillation to markdown is critical for agentic engineering."

Create nodes:

- `practice/paper_distillation`
- `artifact/markdown`
- `methodology/agentic_engineering`

Create edges:

- `(practice/paper_distillation)-[:DISTILLED_TO]->(artifact/markdown)`
- `(practice/paper_distillation)-[:APPLIES_TO]->(methodology/agentic_engineering)`

### 19.2 Normalize

```prolog
edge("practice/paper_distillation", "DISTILLED_TO", "artifact/markdown").
edge("practice/paper_distillation", "APPLIES_TO", "methodology/agentic_engineering").
```

### 19.3 Infer

```prolog
critical_practice_for_methodology(P, M) :-
  edge(P, "DISTILLED_TO", _),
  edge(P, "APPLIES_TO", M).
```

### 19.4 Cache or Materialize

- if this predicate is not yet promoted, write to `derived_cache_by_pred`
- if it is promoted, write to `derived_edges_by_pred` and `derivation_provenance`

### 19.5 Explain

User asks: "Why is paper distillation critical for Agentic Engineering?"

System looks up provenance by derived edge id and returns rule + parent facts.

---

## 20. Operational Guardrails

1. No broad scans over cache tables.
2. No unbounded partitions.
3. No dependence on memtable-to-SSTable flush behavior for semantic promotion decisions.
4. No mixing of cache hotness logic with storage-engine internals.
5. No explicit deletes for normal cache expiry.
6. No all-pairs closure materialization without explicit budget approval.
7. Every durable materialization must have:
   - owner,
   - promotion rationale,
   - size budget,
   - refresh policy,
   - rollback plan.

---

## 21. Implementation Patterns

### Pattern A: Dynamic Ontology, Generic Relations

Use when:

- ontology is evolving rapidly,
- concept vocabulary is not stable,
- inference patterns are still being discovered.

Representation:

- `edge(src,pred,dst)`
- `instance_of(entity,class)`
- `subclass_of(child,parent)`

### Pattern B: Promote Hot Predicates

Use when:

- a predicate dominates query volume,
- it is heavily reused,
- it has bounded size,
- it is expensive to recompute.

Representation:

- specialized durable tables
- specialized logical relations
- precomputed rollups

### Pattern C: Cassandra-Backed Ephemeral Cache

Use when:

- you want one storage stack,
- cache hit latency can be database-class,
- inferred result sets are large,
- TTL-based reuse is sufficient.

### Pattern D: NVMe-Pinned Hot Cache

Use when:

- hierarchical storage is available,
- cache table churn is high,
- read/write/compaction cost must stay local and fast.

### Pattern E: Batch + Incremental Hybrid

Use when:

- initial data volume is large,
- rule changes affect big regions,
- day-to-day updates are localized.

---

## 22. Pseudocode: Query-Time Derivation and Promotion

```python

def query_predicate(tenant, pred, params):
    cache_key = make_cache_key(pred, params)

    rows = read_cache(tenant, cache_key)
    if rows:
        record_heat(tenant, pred, hit=True)
        return rows

    start = now_ms()
    rows = compute_predicate_from_base_and_rules(tenant, pred, params)
    compute_ms = now_ms() - start

    write_cache(tenant, cache_key, rows, ttl_seconds=3600)
    record_heat(tenant, pred, hit=False)
    record_compute_cost(tenant, pred, compute_ms)

    if should_promote(tenant, pred):
        enqueue_promotion_job(tenant, pred)

    return rows
```

```python

def should_promote(tenant, pred):
    query_count = get_query_count_7d(tenant, pred)
    median_compute_ms = get_median_compute_ms_7d(tenant, pred)
    reuse_factor = estimate_reuse_factor(tenant, pred)
    update_rate = get_update_rate_7d(tenant, pred)
    estimated_rows = estimate_materialized_rows(tenant, pred)
    size_budget = get_size_budget(pred)

    score = query_count * median_compute_ms * reuse_factor / max(update_rate, 1)
    return score >= promotion_threshold(pred) and estimated_rows <= size_budget
```

---

## 23. Pseudocode: Rule Version Change

```python

def publish_rule_version(tenant, rule_id, version, rule_body, family, rule_weight):
    insert_rule_version(tenant, rule_id, version, rule_body, family, rule_weight)
    mark_previous_versions_superseded(tenant, rule_id, version)
    impacted_preds = determine_impacted_predicates(rule_body, family)
    enqueue_rematerialization(tenant, family, impacted_preds, version)
```

```python

def rematerialize_family(tenant, family, version):
    base_facts = load_base_facts_for_family(tenant, family)
    derived = run_datalog_family(base_facts, family, version)
    write_durable_materializations(tenant, family, derived, version)
    write_provenance(tenant, family, derived, version)
```

---

## 24. Recommended MVP Scope

### 24.1 Must-Have

- base node and edge tables
- canonical fact extraction
- rule registry
- 5 to 10 core rule families
- provenance storage
- TTL cache tables
- promotion telemetry
- 2 to 4 durable materialized predicates
- Cypher projection for base + derived edges

### 24.2 First Rule Families

1. taxonomy closure
2. part-of closure
3. methodology membership
4. context tool preference
5. practice-to-methodology links

### 24.3 First Durable Materializations

1. `isa`
2. `class_ancestor`
3. `methodology_member`
4. `tool_preferences_by_context`

---

## 25. Risks and Mitigations

### Risk: materialized explosion
Mitigation:
- budget estimates,
- promotion gates,
- bounded fanout policies,
- no blind closure materialization.

### Risk: tombstone-heavy cache churn
Mitigation:
- TTL-based design,
- no explicit deletes,
- uniform TTL,
- NVMe pinning,
- time-window compaction,
- bounded partitions.

### Risk: ontology drift and synonym chaos
Mitigation:
- alias table,
- `same_as` and taxonomy control,
- rule family reviews,
- concept curation tooling.

### Risk: explanation gaps
Mitigation:
- mandatory provenance for durable materializations,
- rule versioning,
- support count tracking.

### Risk: durable table sprawl
Mitigation:
- promotion review process,
- explicit owner and rollback plan for each promoted predicate.

---

## 26. Summary Recommendation

The recommended architecture is:

- **dynamic ontology represented in generic graph and logical relations**,
- **Datalog-style inference over canonical predicates**,
- **Cassandra-backed TTL cache for ephemeral derived results**,
- **separate telemetry for hotness and compute cost**,
- **durable materialized tables for promoted predicates**,
- **provenance attached to every durable derived fact**,
- **NVMe pinning for hot cache SSTables in hierarchical storage deployments**.

This gives:

- ontology flexibility,
- explainable inference,
- scalable persistence beyond memory,
- query-oriented physical optimization,
- an operationally coherent path from generic dynamic concepts to stable high-value materializations.
