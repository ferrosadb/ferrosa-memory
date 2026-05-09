---
type: feature
priority: P2
status: draft
created: 2026-05-04
reported-by: Hermes Agent (post-debugging session)
---

# Automatic Session Entity Extraction into fmem

## Problem

Key facts discovered during debugging sessions are currently saved manually via `smart_ingest` at the end of the session. If the agent forgets, the knowledge is lost. The per-turn `memory` tool is limited to ~2,200 chars and silently rejects entries that would exceed the limit.

Examples from recent sessions:
- FRSA-BUG-025 (ANN PREPARE bind-marker bug) was only preserved because the user explicitly asked.
- `search_arxiv.py` double-encoding bug was diagnosed but could have been lost if not manually ingested.
- Hippo Health `brokerage_domains` localhost seed bug lives in file-based memory, not fmem, until explicitly migrated.

## Desired Behavior

After every session (or every N turns), a background process scans the transcript for durable facts and auto-ingests them into ferrosa-memory as entities + temporal facts + edges. The process should:

1. Extract candidate entities from user corrections, bug diagnoses, config discoveries, and convention changes.
2. Run `hybrid_search` or `retrieve_entities` to check for near-duplicates before creating new ones.
3. Write temporal facts for state changes ("migration 31 authored but not applied", "bug filed upstream").
4. Create edges (`related_to`, `depends_on`, `supersedes`) between extracted entities.
5. Summarize what was ingested and surface it to the user.

## Scope & Boundaries

- **In scope:** Bug root causes, environment configs, project conventions, user preferences, tool quirks.
- **Out of scope:** Raw code (derivable from files), ephemeral task state, completed-work logs, temporary TODO items.
- **Do not auto-ingest:** Anything the user explicitly marks as temporary.

## Proposed Implementation

### Option A: Cron job + transcript parsing
- A cron job runs every 15m, reads the latest session transcript from Hermes logs.
- Uses an LLM call (lightweight model, e.g. spark) to extract facts in structured JSON.
- Calls fmem tools via MCP to ingest.
- Pros: Simple, doesn't complicate the main agent loop.
- Cons: Delayed, may miss context that only makes sense during the session.

### Option B: In-agent hook after tool calls
- After any `terminal`, `read_file`, `search_files`, or `patch` call, the agent analyzes whether the result contains a durable fact.
- If so, calls `smart_ingest` immediately.
- Pros: Real-time, context-fresh.
- Cons: Adds latency to every tool call, risk of over-ingestion.

### Option C: Session-close trigger
- When the session ends (user says "wrap up", "done", or `/new`), a final pass extracts all durable facts.
- This is what humans do manually today — just automate it.
- Pros: Controlled, one-shot per session, low overhead.
- Cons: Requires detecting session end, which is platform-dependent.

**Recommendation: Start with Option C**, then add Option A as a safety net.

## Acceptance Criteria

- [ ] A session ending with "save what we learned" produces at least one new entity in fmem.
- [ ] Running `hybrid_search` for a fact mentioned in the previous session returns it.
- [ ] Duplicate suppression: re-running extraction on the same session does not create duplicate entities.
- [ ] User can review and reject auto-ingested facts (e.g., via a "dismiss" temporal fact).

## Related

- `feat-ingest-entities.md` — bulk ingest pipeline
- `skill-ingest-support.md` — skill catalog ingestion
- `memory-sync.md` — memory lifecycle and eviction
