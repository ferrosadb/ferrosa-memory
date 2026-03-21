# ADR-002: Nightly Batch Job in Rust

## Status

Accepted

## Context

The spec (Section 6.3) describes a "nightly batch job (implemented as a WASM UDF or external script)" for exporting failure pairs from `feedback_outcomes` and generating updated routing guidelines. The spec left the implementation language open.

## Decision

Implement the batch job as a Rust binary (separate `[[bin]]` target in the same Cargo workspace) rather than a Python script or WASM UDF.

## Approach

- `ferrosa-memory-batch` binary in the same workspace
- Reads `feedback_outcomes` via CQL
- Computes strategy success rates, identifies failure patterns
- Writes updated routing guidelines to `routing_guidelines` config table
- Triggered by cron or systemd timer (not WASM UDF — needs network access for CQL)

## Consequences

- Single language for the entire project (Rust)
- Shares CQL client code with the MCP server via workspace crate
- No Python dependency for any part of the system
- WASM UDF path remains available for simpler in-database computations (e.g., expiry sweep)
