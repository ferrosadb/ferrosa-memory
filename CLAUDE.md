# ferrosa-memory-mcp

Ferrosa Memory MCP Server — a Rust MCP server exposing Ferrosa DB as typed memory tools for LLM agent trajectories.

## Stack

- Rust (Tokio async runtime)
- No Python anywhere — all algorithms ported to Rust (see ADR-001, ADR-002)

## Project Plan

See [specs/project-plan.md](specs/project-plan.md) for the current project plan with sprint breakdown, risk register, and task details.

## Agent Rules

Follow [AGENTS.md](AGENTS.md). In particular, every schema change must include an ordered, versioned, automatic, data-preserving migration. Migrations must support upgrading from version `N` to `M` by applying `N+1`, `N+2`, ... `M` in sequence, and must never silently drop, damage, or orphan legacy data.

## Architecture

See [specs/](specs/) for full architecture specs, DSM analysis, threat model, and FMEA.

## Session Restore

On every session start (including after `/clear`), immediately restore context from ferrosa-memory before doing anything else:

1. Call `check_intentions` with the current git branch and recent commit subjects as context
2. Call `hybrid_search` with the current branch name and any relevant keywords from recent commits
3. Briefly summarize what was being worked on so the user knows you have context

Do not wait for the user to ask — this is automatic.

## No Workarounds for Ferrosa Bugs

This project is a test program for the Ferrosa database. Never build workarounds, fallback logic, or compatibility shims in this repo for missing or broken Ferrosa functionality. If the database has a bug, file a report in `../ferrosa/specs/` and fix it upstream. Working around database bugs here hides them and defeats the purpose of this project.

## Related Projects

- `../ferrosa/` — Ferrosa DB engine. Architecture specs at `../ferrosa/specs/` (CQL protocol, SUBSCRIBE semantics, storage engine, graph engine, consensus).
- `../research/tools/forge/` — CLI/MCP companion tool. Provides `ingest` for codebase/docs→memory ingestion, plus code analysis tools (DSM, digest, smell-detect, etc.).

## Using forge `cargo` Tool

Use `mcp__forge__cargo` instead of bash for all Rust/Cargo commands. Supports: `build`, `check`, `test`, `clippy`, `fmt_check`.

**Example:**
```
mcp__forge__cargo(command="test", path="./ferrosa-memory", args="--package ferrosa-memory-core")
```

Returns structured JSON with parsed failures (`{test_name, error}` pairs), 16KB raw output for test failures. See `.claude/skills/cargo.skill.md` for full documentation.
