# ferrosa-memory-mcp

Ferrosa Memory MCP Server — a Rust MCP server exposing Ferrosa DB as typed memory tools for LLM agent trajectories.

## Stack

- Rust (Tokio async runtime)
- No Python anywhere — all algorithms ported to Rust (see ADR-001, ADR-002)

## Project Plan

See [specs/project-plan.md](specs/project-plan.md) for the current project plan with sprint breakdown, risk register, and task details.

## Architecture

See [specs/](specs/) for full architecture specs, DSM analysis, threat model, and FMEA.
