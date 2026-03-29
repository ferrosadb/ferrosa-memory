# Three-Tier Entity Extraction

**Date**: 2026-03-26
**Branch**: feature/entity-extraction
**Status**: Approved

## Problem

`smart_ingest` derives entity names by truncating content to 8 words:
```rust
entity_name: content.split_whitespace().take(8).collect::<Vec<_>>().join(" ")
```
This produces names like "Ben Kearns is the developer of ferrosa-memory-mcp. Ops" instead of "Ben Kearns". NER classifiers (heuristic and LLM) can't classify sentence fragments, so most entities end up typed as "concept".

## Design

Add an optional `entity_name` field to the `smart_ingest` MCP tool. When absent, extract the name (and optionally the type) through a three-tier fallback chain.

### Extraction Chain

```
entity_name provided? ──yes──> use it directly
       │ no
       ▼
LLM available? ──yes──> extract (name, type) from content
       │ no              type only overridden if caller said "concept"
       ▼
heuristic extraction ──> extract_entity_candidates(content)
       │                  + infer_entity_type(name)
       ▼ (nothing found)
first 8 words of content (current behavior, last resort)
```

### Tier 1: Explicit Name

The calling agent provides `entity_name` directly. Zero cost, most accurate. The agent has full conversational context and already knows what the entity is.

### Tier 2: LLM Extraction

Single call to the configured NER model (default: `qwen3.5:27b` via Ollama) extracts both name and type from content. ~200ms latency.

**Prompt**:
```
/no_think
Extract the primary named entity from this text.
Return JSON: {"name": "...", "type": "person|org|tool|project|place|event|concept|decision|pattern|preference"}
Text: {content}
Reply with ONLY the JSON, nothing else.
```

**Type override rule**: The extracted type only replaces the caller's type when the caller passed "concept". Explicit types (person, org, tool, etc.) are trusted.

### Tier 3: Heuristic Extraction

If the LLM is unavailable or fails:
1. Run `extract_entity_candidates(content)` to find capitalized phrases
2. If candidates found, use the first one with `infer_entity_type()` for classification
3. If no candidates, fall back to `content.split_whitespace().take(8)` (current behavior)

## Components Changed

### 1. Tool Schema (`dispatch.rs`)

Add optional `entity_name` property to the `smart_ingest` tool definition:
```json
{
  "entity_name": {
    "type": "string",
    "maxLength": 256,
    "description": "Clean entity name (e.g. 'Ben Kearns', 'Ferrosa'). If omitted, extracted automatically from content."
  }
}
```

### 2. `smart_ingest` Function (`smart_ingest.rs`)

- Add `entity_name: Option<&str>` parameter
- When `Some(name)`: use directly as `entity_name`
- When `None`: call `ner::extract_entity_from_content()` to get `(name, type)`
- Apply type override rule (only override "concept")

### 3. Entity Extraction (`ner.rs`)

Add `extract_entity_from_content()`:
- Accepts `http_client`, `ollama_url`, `model`, `content`, `caller_type`
- Returns `(String, String)` — extracted `(name, type)`
- Implements the three-tier chain internally
- Reuses existing `infer_entity_type()` for tier 3

### 4. Config (`config.rs`)

Add `ner_model` field to `EmbeddingConfig` (reuse the embeddings section since it already has Ollama config):
```toml
[embeddings]
ner_model = "qwen3.5:27b"  # default
```

### 5. Dispatch Handler (`dispatch.rs`)

- Parse optional `entity_name` from args
- Pass to `smart_ingest()`
- When extraction runs, needs access to an HTTP client and the Ollama URL from config
- Add `EmbeddingConfig` reference to `SessionState` or pass through dispatch

### 6. Batch Tool (`ferrosa-memory-batch`)

Add `rename-entities` subcommand:
- Reads all entities for the tenant
- Filters to entities where `entity_name` has >5 words (sentence fragments)
- Runs each through `ner::extract_entity_from_content()`
- Updates via `entity_put()` with the clean name and corrected type
- Logs each rename with old/new name and type
- Reports totals: renamed, skipped, failed

## Backwards Compatibility

Fully backwards compatible:
- `entity_name` is optional — existing callers don't need to change
- When omitted, the extraction chain produces better names than the current 8-word truncation
- Existing explicit types are never overridden (only "concept" gets reclassified)
- The heuristic fallback ensures the system works even without Ollama running

## Testing

- Unit tests for `extract_entity_from_content()` with mock HTTP responses
- Unit tests for the type override rule (concept → override, explicit → preserve)
- Unit tests for the heuristic fallback path
- Unit tests for the 8-word truncation last resort
- Integration: `smart_ingest` with and without `entity_name` parameter
- Batch: `rename-entities` against mock storage
