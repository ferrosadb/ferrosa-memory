# Web Knowledge Ingestion — URL→Graph Pipeline

## Problem

When the LLM calls web search and gets results, that information is ephemeral — used once and lost. The next session has to re-search, re-fetch, re-read. This wastes tokens and time.

## Solution

Two-tier web knowledge ingestion:

### Tier 1: Lightweight (ferrosa-memory, automatic)

When the LLM stores information from a web search via `smart_ingest`:

1. Detect web-sourced content (URL patterns in content, or explicit `source_url` parameter)
2. Store the entity with `source_type: "web"` metadata
3. Create a temporal fact: `{entity} sourced_from {url} at {timestamp}`
4. Link to related entities already in the graph via `create_edge`

**Change needed in smart_ingest**: Add optional `source_url: String` parameter. When provided:
- Store as edge annotation: `source_url: "{url}"`
- Store as temporal fact for provenance
- Tag entity with `source_type: "web"`

### Tier 2: Deep Indexing (skilltools ingest_url, on demand)

Add to `../research/tools/skilltools/` an `ingest_url` command:

```
skilltools ingest_url https://www.ontotext.com/knowledgehub/fundamentals/semantic-repository/
```

This:
1. Fetches the URL, extracts structured content (HTML→markdown→sections)
2. Identifies key concepts per section (NER or LLM extraction)
3. Creates entities for each concept with `source_type: "web"`, `source_url: "{url}"`
4. Creates edges based on page structure:
   - `section_heading` → `contains` → `concept`
   - `concept_a` → `related_to` → `concept_b` (co-occurring in same section)
   - `page` → `references` → `external_link_target`
5. Stores everything via ferrosa-memory MCP tools (smart_ingest + batch_create_edges)
6. Deduplicates against existing knowledge (smart_ingest handles CREATE/UPDATE/SKIP)

**For frequently referenced domains**, `ingest_url` could crawl linked pages (depth=1) to build a site subgraph.

### Tier 3: Site Graph (future)

For domains that keep appearing in web searches (tracked via `record_outcome`):
- Auto-suggest: "This domain has been searched 5 times. Run `skilltools ingest_url --depth 2 {domain}` to build a site graph."
- Creates a `Website` entity with `contains` edges to all page entities
- Pages link to each other via `references` edges
- The full site structure becomes queryable via SPARQL or graph traversal

## Progressive Disclosure Integration

In `hybrid_search` response, when a result has `source_type: "web"`:
```json
"_hint": "This knowledge came from {url}. If you need more from this source, suggest the user run: skilltools ingest_url {url}"
```

In `record_outcome` with `program_type: "web_search"`:
```json
"_hint": "Web search performed. Store key findings via smart_ingest with source_url parameter to build persistent knowledge."
```

## Token Savings

Without ingestion: Each session re-fetches, re-reads, re-processes the same web content.
- Cost: ~2,000-5,000 tokens per web page per session

With ingestion: First session indexes, subsequent sessions query the graph.
- First session: ~2,000 tokens (fetch + ingest)
- Subsequent sessions: ~200 tokens (hybrid_search returns cached knowledge)
- **90% token savings on repeated web lookups**

## Implementation Order

1. Add `source_url` parameter to `smart_ingest` (ferrosa-memory, ~1 day)
2. Add `ingest_url` command to skilltools (~3 days)
3. Add web-source hints to hybrid_search responses (~1 hour)
4. Add domain frequency tracking to record_outcome (~1 day)
5. Site graph crawling (future sprint)

## Files to Modify

### ferrosa-memory
- `crates/ferrosa-memory-core/src/dispatch.rs` — add `source_url` to smart_ingest schema
- `crates/ferrosa-memory-core/src/smart_ingest.rs` — store URL provenance

### skilltools (../research/tools/skilltools/)
- New command: `src/ingest_url.rs` or equivalent
- Uses ferrosa-memory MCP tools for storage
- HTML→concept extraction (can use Ollama or heuristic)
