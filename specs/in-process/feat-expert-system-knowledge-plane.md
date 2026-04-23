---
type: feat
priority: P1
status: in-process
created: 2026-04-19
updated: 2026-04-20
reported-by: blueprint update for expert-system knowledge plane (2026-04-19)
implemented-by: codex
---

# Expert-system knowledge plane in `ferrosa-memory-core`

## Goal

Turn the current Datalog/rule/provenance substrate into a reviewable symbolic knowledge plane with:

- one effective runtime rule surface
- scoped claims and approvals
- exact alias persistence
- explanation queries for derived facts

## Why now

The codebase now ships a converged expert-system backend with a shared effective-rule loader and governance planes, while operator-facing workbench/query routes are the explicit remaining rollout scope.

## Deliverables

1. Shared `EffectiveRuleSet` loader used by `manage_rules`, `query_derived`, `recursive_explore`, and `promotion`
2. Claim and approval storage model with auth-derived reviewer identity
3. Alias persistence with exact scoped lookup
4. Explanation API for derived facts, rule provenance, and approval context

## Acceptance

- No inference path bypasses the effective rule loader.
- Unapproved claims/rules are excluded from default runtime loading.
- Alias execution uses exact scoped lookup, not fuzzy retrieval.
- Derived facts can be explained with bounded support chains.

## References

- [expert-system-knowledge-plane.md](../expert-system-knowledge-plane.md)
- [project-plan.md](../project-plan.md)
- [threat-model.md](../threat-model.md)
- [fmea.md](../fmea.md)

## Status

- Backend convergence and governance workstreams (effective rule set, scoped claim/approval/alias storage, `explain_derived`, `get_effective_rule_set`) are implemented.
- Operator workbench root, CQL explorer, Datalog explorer, and rules-management UI remain in progress and should be validated before shared-query rollout.
