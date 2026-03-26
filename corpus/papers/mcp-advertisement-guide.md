# How to Advertise ferrosa-memory-mcp to an LLM

## Why This Document Exists

An MCP server is only as useful as the LLM's understanding of when and how to use it. The server manifest — its name, description, and tool descriptions — is read by the LLM before every session. These strings are not documentation for humans: they are *prompts* that shape model behavior. A poorly written manifest results in an LLM that ignores available tools, calls them in the wrong order, or calls them when it shouldn't.

This document synthesizes guidance from the research literature into concrete, tested patterns for writing the server and tool descriptions that will ship in `ferrosa-memory.toml` and the MCP manifest JSON.

---

## Research Basis

Five papers directly inform the advertisement design:

- **SRLM** — program selection quality is the primary performance driver; the LLM must understand the decision criteria to select well
- **MemR³** — closed-loop retrieval (retrieve → reflect → answer) must be made explicit; models default to single-shot retrieve-then-answer without prompting
- **THREAD** — tool usage patterns embedded as prose examples in descriptions shape model behavior without fine-tuning
- **MCPShield** — agents validate MCP servers by reading their metadata; vague or inaccurate descriptions trigger distrust and non-use
- **Think, But Don't Overthink** — models over-call tools on simple tasks unless given explicit bypass conditions

---

## Part 1: Server-Level Description

The server description is the first thing the LLM reads. It must accomplish three things in roughly 150 words: explain what the memory hierarchy contains, when to use it vs. not, and what the expected usage loop looks like.

### Template

```
ferrosa-memory: Durable memory backend for long-horizon agent tasks backed by
Ferrosa DB (Rust-native Cassandra with S3 storage, vector indexes, and a graph
layer).

WHEN TO USE: Tasks that process large document corpora, require multi-session
continuity, involve named entity tracking across many sources, or have
sub-tasks whose results might be reused. Check the memo cache before any
sub-LLM call. Use plan_state to track hierarchical task decomposition. Use
trajectory_folds to store and retrieve sub-trajectory summaries.

WHEN TO SKIP: Simple single-step questions answerable from current context.
Tasks where the full input fits in your context window and no reuse is expected.

USAGE LOOP (MemR³ pattern):
  1. check_memo_cache → cache hit: use result directly
  2. On miss: retrieve_fold_context or retrieve_entities for relevant prior context
  3. Execute task / invoke sub-LLM
  4. store_memo_result + upsert_entity + record_outcome
  5. If task has sub-structure: write_plan_node → start_fold → ... → complete_fold

COST SIGNALS: memo hit ~1ms, ANN retrieval ~10ms, Cypher traversal ~100ms.
```

### Design Principles Applied

**"WHEN TO SKIP" is mandatory.** The "Think, But Don't Overthink" paper showed that RLMs hurt on simple tasks. Without explicit bypass conditions, an LLM will check the memory cache for every single response. The bypass condition ("full input fits in context window, no reuse expected") must be present to avoid this.

**The usage loop must be numbered and explicit.** MemR³ found that models default to single-shot retrieve-then-answer without closed-loop prompting. Numbering the steps forces the LLM to treat this as a procedure to follow rather than a description to understand. The `→` connectors make the dependency chain clear.

**Cost signals belong in the server description, not buried in tool docs.** SRLM showed that program selection quality depends on the model having the information needed to choose. Latency cost is a primary selection criterion — the LLM needs to know that a Cypher traversal is 10× more expensive than a memo lookup before it decides whether to use it.

---

## Part 2: Tool Descriptions

Each tool description follows a four-part structure: **what it does** (one sentence), **when to call it** (specific trigger condition), **when not to call it** (explicit bypass), and **what to do with the result** (closes the loop). This structure is derived from MCPShield's finding that tool descriptions function as the primary trust signal — vague descriptions are the single largest predictor of tool non-use or misuse.

---

### `check_memo_cache`

```
Looks up a prior sub-LLM call result by content hash. Returns the cached
result if found, or a miss signal if not.

CALL WHEN: Before every sub-LLM invocation within a long-horizon task.
This is the first step in the usage loop.

DO NOT CALL: For top-level queries or tasks where you are not making
sub-LLM calls. Do not call more than once per sub-call (check once, act
on the result).

ON HIT: Use the cached result directly. Do not invoke the sub-LLM. Call
record_outcome with program_type='memo_hit'.
ON MISS: Proceed with the sub-LLM call. After it completes, call
store_memo_result with the result.

Cost: ~1ms. Zero token cost.
```

---

### `store_memo_result`

```
Stores a completed sub-LLM result for future reuse. Compresses to NL
capsule format before storage.

CALL WHEN: Immediately after any sub-LLM call completes on a task where
the same chunk might be processed again (same corpus, same session, or
recurring queries).

DO NOT CALL: For top-level responses or ephemeral computations that will
never be reused. Do not call if check_memo_cache returned a hit for this
same input.

Provide the full result text; compression happens server-side. The
content_hash returned can be used to reference this entry in plan notes
or fold summaries.

Cost: ~5ms write. Storage cost is negligible (compressed).
```

---

### `write_plan_node`

```
Records a sub-task node in the hierarchical plan tree for this session.
Enables structured re-injection of parent plan context on recursive return
(ReCAP pattern).

CALL WHEN: At the start of each sub-task, before execution. Always call
this when decomposing a complex task into sub-tasks. Depth=0 is the root
goal; each level of decomposition increments depth.

DO NOT CALL: For single-step tasks with no decomposition.

Provide goal_text as a compact NL description of what this node is
trying to achieve — one to three sentences. This text will be
re-injected into sub-agent context on return.
```

---

### `get_plan_context`

```
Returns the full plan tree for the current session as a compact JSON
structure. Use this to re-inject parent context when returning from a
recursive sub-task call.

CALL WHEN: At the start of each sub-task execution (after write_plan_node)
and on return from a sub-task call. This is the structured re-injection
step of the ReCAP pattern.

Include the returned plan tree in your prompt preamble with the label
"Current task hierarchy:" to prevent goal drift across recursive levels.

Cost: ~2ms. O(depth) tokens in the returned JSON — negligible for
typical task depths of 3–7 levels.
```

---

### `update_plan_node`

```
Marks a plan node complete or failed and records an outcome summary.

CALL WHEN: When a sub-task finishes (success or failure). Always provide
outcome_summary — this text is what parent nodes will see when they call
get_plan_context after this node completes.

Write outcome_summary in one to two sentences describing what was found
or accomplished, not the process used to find it. The parent needs
conclusions, not methods.
```

---

### `start_fold`

```
Opens a new trajectory fold for a sub-task. Returns a fold_id used to
append REPL turns as the sub-task executes.

CALL WHEN: Starting any sub-task that will involve multiple REPL turns or
sub-LLM calls and whose intermediate results you want to be able to
retrieve later. Always call write_plan_node first.

A fold is the durable equivalent of a REPL scope. Everything appended to
a fold is stored and retrievable by future sessions via
retrieve_fold_context.
```

---

### `append_to_fold`

```
Appends a REPL turn (code + output pair) to an active fold. Returns
current token_count.

CALL WHEN: After each REPL execution within an active fold.

MONITOR token_count: If token_count exceeds ~80,000, consider opening a
nested fold (start_fold with the current fold_id as parent_fold_id) for
the next phase rather than continuing in the same fold. This prevents
single folds from becoming unwieldy.
```

---

### `complete_fold`

```
Seals a fold and writes a summary. Creates FOLDED_INTO graph edge to
parent fold. Queues raw trajectory for background compression.

CALL WHEN: When a sub-task is fully complete. Always call before
returning from a recursive level.

Write summary as a dense NL capsule: the key findings, state changes,
and answers discovered during this fold. Do not summarize the process —
summarize the outcomes. The summary is what future retrieval will return
by default. The raw trajectory is archived but accessible via
include_raw=true if needed.

Cost: ~10ms. Returns compression_ratio for monitoring.
```

---

### `retrieve_fold_context`

```
ANN vector search over prior fold summaries. Returns the k most
semantically similar fold summaries to the current query.

CALL WHEN: Starting a new task or sub-task where prior work from earlier
sessions might be relevant. Also call when stuck — prior fold summaries
often contain relevant evidence that was not in the original query.

USE include_raw=true only when the summary is insufficient and you need
the full trajectory. This incurs a cold S3 read and should be rare.

RETRIEVAL LOOP (MemR³): If the returned summaries partially answer your
query but leave gaps, call retrieve_fold_context again with a more
specific query embedding targeting the gap. Two to three retrieval rounds
is normal for complex queries; more than five suggests a retrieval
strategy problem.

Cost: ~10ms (HNSW). include_raw adds ~200–2000ms (S3 read).
```

---

### `upsert_entity`

```
Writes a discovered named entity to the entity store. Deduplicates
against existing entities via phonetic matching — returns the existing
entity_id if a variant of this name is already stored.

CALL WHEN: Any time you identify a named entity (person, place,
organization, event, concept) from retrieved or processed content. Always
link to the source_fold_id that produced it.

The phonetic deduplication prevents duplicate nodes from variant
spellings and transliterations. Check is_new in the response: if false,
the entity already exists and you can use the returned entity_id to
attach new facts without creating a duplicate.
```

---

### `retrieve_entities`

```
Retrieves named entities by name (phonetic fuzzy match), by semantic
similarity (ANN), or both.

CALL WHEN: Need to find entities related to the current query. Use
strategy='phonetic' for known entity names that might have spelling
variants. Use strategy='ann' for semantic search when the exact name is
unknown. Use strategy='both' for maximum recall.

The phonetic strategy is especially valuable for entity names from OCR
output, transliterations, or noisy corpora where exact string matching
fails.

Cost: phonetic ~5ms, ann ~10ms, both ~15ms.
```

---

### `record_outcome`

```
Records the result of a retrieval or memo operation for offline routing
improvement. Used to train better retrieval strategies over time.

CALL WHEN: After every retrieval operation (retrieve_fold_context,
retrieve_entities, check_memo_cache). Provide:
  - program_type: the strategy that was used
  - task_complexity: 'simple' (O(1) lookup), 'linear' (scan corpus),
    or 'quadratic' (pairwise reasoning)
  - succeeded: whether the retrieval produced useful results
  - latency_ms and token_cost: for cost-accuracy tracking

This call is write-only and takes ~1ms. It has no effect on the current
task but improves routing quality for all future sessions.
```

---

## Part 3: Usage Pattern Examples

Embed these examples in the server manifest under an `examples` key. They shape model behavior through demonstration (THREAD finding: few-shot examples in descriptions are as effective as fine-tuning for tool usage patterns).

### Example 1: Simple memo cache check before a sub-call

```
# Before processing a 500-document corpus chunk:
hit = check_memo_cache(prompt="classify document type", context_slice=doc_chunk)
if hit.hit:
    result = hit.result
    record_outcome(program_type="memo_hit", succeeded=True, ...)
else:
    result = llm_query(f"Classify this document: {doc_chunk}")
    store_memo_result(prompt="classify document type", context_slice=doc_chunk, result=result)
    record_outcome(program_type="llm_call", succeeded=True, ...)
```

### Example 2: Full fold lifecycle for a sub-task

```
# Starting a research sub-task at depth=1:
write_plan_node(session_id=sid, depth=1, subtask_id="find_founders",
                goal_text="Identify founders of Acme Corp from the document corpus")
fold_id = start_fold(session_id=sid, depth=1, initial_context=query)

# During execution, append REPL turns:
for chunk in corpus_chunks:
    result = process_chunk(chunk)
    append_to_fold(fold_id=fold_id, repl_turn=f"processed {chunk.id}: {result}")

# On completion:
complete_fold(fold_id=fold_id,
              summary="Found three founders: Alice (CEO), Bob (CTO), Carol (CFO). "
                      "Alice joined 2018, Bob and Carol joined 2019.",
              embedding=embed(summary))
update_plan_node(session_id=sid, depth=1, subtask_id="find_founders",
                 status="complete",
                 outcome_summary="Identified three founders with joining dates.")
```

### Example 3: MemR³ retrieval loop with gap tracking

```
# Initial retrieval:
results = retrieve_fold_context(session_id=sid, query_embedding=embed(query), k=5)
# Results partially answer: know Alice and Bob, missing Carol.

# Gap-targeted second retrieval:
gap_query = "founders joined 2019 CTO CFO Acme Corp"
results2 = retrieve_fold_context(session_id=sid, query_embedding=embed(gap_query), k=3)
# Now have sufficient evidence to answer.
```

---

## Part 4: What Not to Do

These anti-patterns produce measurable degradation based on the research:

| Anti-pattern | Why it fails | Fix |
|---|---|---|
| Single-sentence tool descriptions | MCPShield: vague descriptions trigger distrust; models skip the tool | Use four-part structure: what, when to call, when to skip, what to do with result |
| No bypass conditions | Think, Don't Overthink: models over-call on simple tasks, causing blowup | Always include explicit "DO NOT CALL" condition |
| Retrieval without loop pattern | MemR³: single-shot retrieve-then-answer is measurably inferior | Include RETRIEVAL LOOP example with explicit gap-targeting step |
| No cost signals | SRLM: program selection requires cost information to be useful | Include `Cost:` line on every tool description |
| Server description without usage loop | Models learn procedures from descriptions, not just capabilities | Always include the numbered 5-step loop in the server description |
| Tool descriptions that omit output field semantics | Models don't know what to do with `is_new`, `compression_ratio`, etc. | Document each output field's usage in the description |
| Describing what the tool does to storage | LLMs don't need to know about Ferrosa internals | Describe the tool's effect on the *agent's task*, not on the database |

---

## Part 5: Manifest JSON Structure

```json
{
  "name": "ferrosa-memory",
  "version": "1.0.0",
  "description": "[SERVER DESCRIPTION FROM PART 1]",
  "capabilities": {
    "tools": true,
    "resources": false,
    "prompts": false
  },
  "tools": [
    {
      "name": "check_memo_cache",
      "description": "[TOOL DESCRIPTION FROM PART 2]",
      "inputSchema": {
        "type": "object",
        "properties": {
          "prompt": { "type": "string", "maxLength": 4096 },
          "context_slice": { "type": "string", "maxLength": 131072 },
          "model_version": { "type": "string", "maxLength": 64 }
        },
        "required": ["prompt", "context_slice", "model_version"]
      }
    }
  ],
  "examples": [
    "[EXAMPLE 1 FROM PART 3]",
    "[EXAMPLE 2 FROM PART 3]",
    "[EXAMPLE 3 FROM PART 3]"
  ]
}
```

### Schema Design Notes for MCPShield Compliance

- **All string inputs have explicit `maxLength` constraints.** MCPShield validates that tool parameters have bounded types. Unbounded strings are a red flag.
- **`strategy` fields use `enum` not `string`.** `{"type": "string", "enum": ["ann", "phonetic", "both"]}` — this constrains the parameter space to known-safe values and prevents parameter injection.
- **`depth` and `k` are `integer` with `minimum` and `maximum`.** Prevents pathological values (negative depth, k=10000) at the schema level.
- **Boolean fields are `boolean`, not `string`.** `include_raw` should be `{"type": "boolean"}`, not `{"type": "string", "enum": ["true", "false"]}`. Type precision matters for MCPShield's parameter validation.

---

## Summary

The server advertisement is a first-class engineering artifact, not an afterthought. The research shows that model behavior on tool use is shaped as strongly by description quality as by the underlying tool implementation. A good manifest:

1. Names the memory hierarchy explicitly so the model knows what exists
2. Provides numbered procedure loops so the model knows the intended sequence
3. Includes explicit bypass conditions so the model knows when to skip storage entirely
4. Surfaces cost signals so the model can make informed routing tradeoffs
5. Embeds usage examples that demonstrate the retrieve-reflect-answer loop
6. Uses precise schema types (enum, maxLength, integer bounds) for MCPShield compliance and parameter safety
