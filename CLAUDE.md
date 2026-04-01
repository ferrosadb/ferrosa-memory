# ferrosa-memory-mcp

Ferrosa Memory MCP Server — a Rust MCP server exposing Ferrosa DB as typed memory tools for LLM agent trajectories.

## Stack

- Rust (Tokio async runtime)
- No Python anywhere — all algorithms ported to Rust (see ADR-001, ADR-002)

## Project Plan

See [specs/project-plan.md](specs/project-plan.md) for the current project plan with sprint breakdown, risk register, and task details.

## Architecture

See [specs/](specs/) for full architecture specs, DSM analysis, threat model, and FMEA.

## Session Restore

On every session start (including after `/clear`), immediately restore context from ferrosa-memory before doing anything else:

1. Call `check_intentions` with the current git branch and recent commit subjects as context
2. Call `hybrid_search` with the current branch name and any relevant keywords from recent commits
3. Briefly summarize what was being worked on so the user knows you have context

Do not wait for the user to ask — this is automatic.

## Related Projects

- `../ferrosa/` — Ferrosa DB engine. Architecture specs at `../ferrosa/specs/` (CQL protocol, SUBSCRIBE semantics, storage engine, graph engine, consensus).
- `../research/tools/skilltools/` — CLI/MCP companion tool. Provides `ingest` for codebase/docs→memory ingestion, plus code analysis tools (DSM, digest, smell-detect, etc.).
