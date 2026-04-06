# Memory Guide — How to Build Knowledge

You have a semantic memory system. Use it BEFORE grep, find, or reading files. It should be your first source of context, not a fallback.

## Session Start

1. Call `check_intentions` with the current branch/context — act on any triggered intentions
2. Call `hybrid_search` with what you're working on ��� recall prior knowledge
3. Tell the user what you remember
4. Do this BEFORE reading files or grepping

## Searching (Memory First)

- **`hybrid_search`** — your primary search. Uses 5 signals (phonetic, vector, fold, warmth, pagerank). If it returns what you need, you're done — no grep/find/read needed.
- If results are insufficient, the response suggests deeper tools
- Only fall back to file-system tools if memory genuinely doesn't have what you need

## Storing Knowledge

- **`smart_ingest`** — ALWAYS use this to store new information. It automatically decides CREATE/UPDATE/SUPERSEDE/SKIP. Do NOT use `upsert_entity` directly.
- Store insights, decisions, relationships, and facts — not raw file contents
- The system learns what's worth keeping

## Creating Connections

- **`create_edge`** — Link related entities. After learning 2+ related facts, connect them
- Edge types: `depends_on`, `contains`, `part_of`, `related_to`, `calls`, `implements`, `uses`, `references`
- Connected facts are knowledge. Isolated facts are just data.
- **`batch_create_edges`** — When you discover multiple relationships at once

## Intentions (Prospective Memory)

- **`set_intention`** — "Remember to do X when Y happens"
- **`check_intentions`** — At session start and when context changes
- **`complete_intention`** — Mark done when acted on
- Trigger types: Topic (keyword match), FilePattern (file glob), Duration (time delay), Context (condition)

## Going Deeper

The core tools above cover 90% of cases. When you hit limits, responses suggest what to try:

| Situation | Tool suggested |
|-----------|---------------|
| `hybrid_search` returns few results | `recursive_explore` (multi-pass decomposed search) |
| `explore_connections` finds few edges | `spread_activation` (broader associative recall) |
| "How does X relate to Y?" | `find_memory_chain` (shortest path) |
| Need transitive/inferred relationships | `query_derived` (Datalog inference) |
| 10+ new entities, no edges discovered | `run_consolidation` (discover CO_OCCURS patterns) |

## Feedback: When Memory Falls Short

**If you had to use grep, find, or read files to get context that SHOULD have been in memory, call `record_outcome` with:**

```json
{
  "program_type": "retrieval_miss",
  "task_complexity": "description of what you were looking for",
  "success": false,
  "latency_ms": 0
}
```

Every retrieval miss trains the system to store that kind of information in the future. This feedback loop is how the memory system improves.

## What NOT to Do

- Don't grep/find/read files before checking memory
- Don't use `upsert_entity` — use `smart_ingest`
- Don't store every sentence — store the insight worth remembering
- Don't skip edge creation — unconnected entities are wasted knowledge
- Don't ignore `check_intentions` — that's where your future self left you notes
- Don't call `run_consolidation` after every entity — do it after significant learning
