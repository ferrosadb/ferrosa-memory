# ADR-004: Expert-System Knowledge Plane Default Implementation Choices

## Status

Accepted

## Context

The expert-system knowledge-plane blueprint introduced four open questions:

1. Should built-in rules be mirrored into the registry as synthetic versioned entries?
2. Should claims remain entity-backed or get dedicated storage immediately?
3. Should approvals be entity-backed, table-backed, or dual-written?
4. Which explanation queries need precomputed support indexes versus on-demand reconstruction?

These questions matter to implementation detail, but they should not block the blueprint pipeline. The project needs default choices that are safe enough to support test planning, harness generation, and compiled execution planning.

## Decision

Adopt the following implementation choices for Sprint 8 and the remaining blueprint phases:

1. **All rules live in the database, including built-ins.**
   Built-in rules are represented as synthetic registry entries so operators can inspect, diff, activate, deprecate, and audit them through the same rule-management surface as other rules. The runtime still uses a shared `EffectiveRuleSet` loader, but that loader reads the canonical rule set from storage rather than treating code-defined built-ins as a privileged separate class.

2. **Claims are entity-backed initially.**
   Claims start as entity-store-backed artifacts plus typed edges and provenance metadata. Move to dedicated tables only if ergonomics, indexing, or performance prove insufficient.

3. **Approvals are dual-written.**
   Approval decisions are authoritative in a dedicated append-only table for audit/replay, and they are mirrored into the entity/graph layer for retrieval, browsing, and workbench context. The table is the source of truth when the two representations disagree.

4. **Explanation queries are on-demand first, with statistics collection.**
   Build explanation reads on top of existing provenance and support-chain storage with strict bounds on depth and cardinality. Capture latency, fan-out, and common-query statistics from day one so the team can justify precomputed explanation indexes if real workloads demand them.

5. **The operator UI is an integrated workbench.**
   The UI grows from `/viz` into a single authenticated workbench rooted at `/`, with shared navigation, scope/session filters, and linked views for graph exploration, CQL inspection, Datalog inspection, explanations, approvals, and rule management. Viz remains a mode inside the workbench rather than a separate top-level mini-app.

## Consequences

- The blueprint can proceed through test-spec, test-gen, test-harness, and compile-project without waiting for further human decisions.
- The rule registry becomes the single source of truth for all rule artifacts, including built-ins, which improves operator visibility and removes a major source of rule-surface ambiguity.
- The implementation path is incremental: claims reuse existing entity infrastructure, while approvals get both strong audit semantics and first-class workbench visibility.
- Explanation queries may be slower at first, but the cost is bounded and measurable; statistics are required so precomputation decisions are evidence-based rather than speculative.
- The operator UI has a clear shape early, which allows test planning and harness generation for a unified workbench rather than a collection of disconnected pages.
