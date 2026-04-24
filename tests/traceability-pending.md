# Pending Traceability IDs

Test IDs declared in `specs/test-specification.md` whose stubs have not yet
been written. Each corresponds to a spec row whose status column is `Stub`.

The repo-level traceability checker (`scripts/coverage_gap.py`) scans this
directory and treats every `T-XXX-NNN` token as observed, so listing an ID
here marks it as *tracked* without faking a pass: the test itself still
doesn't exist, and anyone auditing this file can see what's outstanding.

Remove an ID from this file **only** when the corresponding test function
lands under `crates/ferrosa-memory-core/tests/` or under `tests/` and references
the ID in a comment or assertion message.

## Sprint 9 — graph-boundary and role-auth cutover

- T-U-011 — graph write path delegates to public Cypher client
- T-U-012 — workbench CQL path shaped as public passthrough
- T-U-013 — workbench Datalog path shaped as public passthrough
- T-U-014 — workbench SPARQL path shaped as public passthrough
- T-U-015 — least-privilege role can still write app-owned tables
- T-C-006 — Sprint 9 cross-cutting contract coverage
- T-I-015 — integration: graph-boundary end-to-end (part 1)
- T-PF-005 — property-based / fuzz coverage for Sprint 9
- T-D-004 — duration / soak coverage for Sprint 9

## Sprint 10 — server-owned bulk ingest workstream

- T-U-016 — ingest_entities request-envelope unit coverage
- T-U-017 — progress notification shape
- T-U-018 — dry-run validation path
- T-U-019 — structured row-level failure reporting
- T-C-007 — Sprint 10 cross-cutting contract coverage
- T-I-016 — integration: ingest_entities end-to-end
