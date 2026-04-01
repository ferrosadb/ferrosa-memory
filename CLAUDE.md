# ferrosa-memory-mcp

Ferrosa Memory MCP Server — a Rust MCP server exposing Ferrosa DB as typed memory tools for LLM agent trajectories.

## Stack

- Rust (Tokio async runtime)
- No Python anywhere — all algorithms ported to Rust (see ADR-001, ADR-002)

## Project Plan

See [specs/project-plan.md](specs/project-plan.md) for the current project plan with sprint breakdown, risk register, and task details.

## Architecture

See [specs/](specs/) for full architecture specs, DSM analysis, threat model, and FMEA.

## Related Projects

- `../ferrosa/` — Ferrosa DB engine. Architecture specs at `../ferrosa/specs/` (CQL protocol, SUBSCRIBE semantics, storage engine, graph engine, consensus).
- `../research/tools/skilltools/` — CLI/MCP companion tool. Provides `ingest` for codebase/docs→memory ingestion, plus code analysis tools (DSM, digest, smell-detect, etc.).
