# Progressive Disclosure Design

> **Historical design note:** The tool counts and one-shot discovery assumptions
> below predate the current 95-definition catalog. The implemented discovery
> contract is [bounded tool catalog pagination](../tool-catalog-pagination/README.md):
> `all_tools` remains public, adds deterministic search and named schema lookup,
> and returns source-paginated results under an exact 16 KiB final-result cap.
> This document remains useful for response-triggered recommendation behavior.

## Concept

Instead of exposing all 50 tools, expose ~15 "primary" tools. When a primary tool's response meets certain conditions, it includes a `hint` field suggesting a more specialized tool. The LLM learns about advanced tools exactly when they're relevant.

## Tool Tiers

### Tier 1: Always Visible (~15 tools, ~5,000 tokens)
| Tool | Purpose |
|------|---------|
| `smart_ingest` | Store new knowledge (replaces upsert_entity for LLM use) |
| `hybrid_search` | Find anything |
| `create_edge` | Connect entities |
| `batch_create_edges` | Connect many entities at once |
| `explore_connections` | Graph neighborhood |
| `check_intentions` | Session start + context change |
| `set_intention` | Prospective memory |
| `complete_intention` | Mark intention done |
| `get_stats` | Health check / counts |
| `write_temporal_fact` | Record fact with timestamp |
| `get_temporal_chain` | Fact history for entity |
| `start_fold` | Begin sub-task trajectory |
| `append_to_fold` | Add to trajectory |
| `complete_fold` | Seal trajectory |
| `write_plan_node` | Task hierarchy |

### Tier 2: Suggested by Tier 1 responses (~20 tools)
| Tool | Suggested When |
|------|---------------|
| `recursive_explore` | `hybrid_search` returns < 3 results |
| `spread_activation` | `explore_connections` returns < 2 connections |
| `find_memory_chain` | User asks "how does X relate to Y" |
| `query_derived` | Need transitive/inferred relationships |
| `run_consolidation` | 10+ new entities in session, no edges discovered yet |
| `manage_rules` | User defines custom inference rules |
| `find_duplicates` | `smart_ingest` returns high similarity scores |
| `importance_score` | Need to prioritize among many entities |
| `predict_needed` | Proactive context loading |
| `batch_ingest` | Storing 5+ entities at once |
| `retrieve_entities` | Direct entity lookup by name |
| `retrieve_fold_context` | Search prior trajectory summaries |
| `list_intentions` | See all pending intentions |
| `snooze_intention` | Defer a triggered intention |
| `get_plan_context` | View full plan tree |
| `update_plan_node` | Mark plan task complete |
| `record_outcome` | Log retrieval success/failure for routing improvement |
| `promote_memory` | Upgrade dormant entity to active |
| `demote_memory` | Downgrade unused entity |
| `delete_session` | Right to deletion |

### Tier 3: Internal / Rarely Needed (~15 tools)
Hidden from initial tool list. Only exposed if explicitly needed.
| Tool | When Exposed |
|------|-------------|
| `upsert_entity` | Never for LLM (use smart_ingest) |
| `check_memo_cache` | Internal memoization |
| `store_memo_result` | Internal memoization |
| `promote_predicate` | Datalog optimization |
| `run_consolidation` | Promoted to Tier 2 via hint |

## Progressive Disclosure Hints

### Format
Add a `_hint` field to tool responses when conditions are met:

```json
{
  "entities": [...],
  "count": 1,
  "_hint": {
    "tool": "recursive_explore",
    "reason": "Only 1 result found. For deeper search across multiple passes, try recursive_explore with the same query.",
    "example_args": { "query": "<original query>" }
  }
}
```

### Trigger Rules

| Tier 1 Tool | Condition | Suggested Tool | Hint Message |
|-------------|-----------|---------------|-------------|
| `hybrid_search` | results < 3 | `recursive_explore` | "Few results. Try recursive_explore for multi-pass decomposed search." |
| `hybrid_search` | results = 0 | `retrieve_entities` | "No results. Try retrieve_entities with phonetic search for name variations." |
| `explore_connections` | edges < 2 | `spread_activation` | "Few connections. Try spread_activation for broader associative recall." |
| `explore_connections` | edges = 0 | `run_consolidation` | "No connections found. Run run_consolidation to discover CO_OCCURS patterns." |
| `smart_ingest` | action = "Skip" | (none) | "Content too similar to existing entity. No action taken." |
| `smart_ingest` | action = "Supersede" | `get_temporal_chain` | "Previous fact superseded. Use get_temporal_chain to see fact evolution." |
| `smart_ingest` | action = "Created" AND session entity count > 10 | `run_consolidation` | "10+ entities stored. Consider run_consolidation to discover relationships." |
| `create_edge` | (always) | `explore_connections` | "Edge created. Use explore_connections to see the entity's full neighborhood." |
| `check_intentions` | triggered > 0 | `complete_intention` | "N intentions triggered. Complete them with complete_intention when done." |
| `check_intentions` | triggered = 0 AND pending > 3 | `list_intentions` | "No triggers but 3+ pending. Use list_intentions to review." |
| `get_stats` | entity_count > 20 AND edge_count < 5 | `run_consolidation` | "Many entities but few edges. Run run_consolidation to discover connections." |
| `get_stats` | entity_count > 0 AND edge_count = 0 | `create_edge` | "Entities exist but no connections. Use create_edge to link related entities." |
| `query_derived` | results = 0 | `manage_rules` | "No derived facts. Use manage_rules to define inference rules." |

## Implementation

### 1. MCP server initialize instructions
Wire MEMORY_GUIDE.md content into the `server_info.instructions` field of the initialize response.

### 2. Tool list filtering
In `tool_definitions()`, add a `tier: u8` field to `ToolDef`. The dispatch handler filters by tier when responding to `tools/list`:
- Default: return Tier 1 only
- With `?include_tier=2`: return Tier 1 + 2
- With `?include_tier=3`: return all

### 3. Response hints
In each handler function, after computing the result, check trigger conditions and append `_hint` to the response JSON.

### 4. Token budget
- Tier 1 tools: ~5,000 tokens (15 tools × ~330 avg)
- MEMORY_GUIDE.md in instructions: ~500 tokens
- Per-response hints: ~50 tokens when triggered
- **Total: ~5,500 tokens** (down from ~17,500)
- **Savings: ~12,000 tokens per conversation**
