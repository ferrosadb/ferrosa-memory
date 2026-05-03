# Feature Request: Multi-Predicate Pre-Aggregation Materialization

**Status:** Proposed / Feature Request
**Date:** 2026-05-02
**Component:** `ferrosa-memory-core` / `datalog`
**Blocked by:** `feat/datalog-aggregation` (count aggregate implementation)
**Priority:** Medium — needed to replace remaining `Session != Session2` workarounds

## Problem

The count aggregate (v1) supports aggregation over a single predicate fact:

```datalog
avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.
```

This works because `user_corrected/2` is a single base predicate in the fact set. But rules that need to JOIN multiple predicates before counting have no clean expression:

**Current workaround** (in `datalog_learning_hook.py` rule `prefer_working_tools`):
```datalog
preferred_tool(Ctx, Tool) :-
    worked_well(Session, Tool),
    session_context(Session, Ctx),
    worked_well(Session2, Tool),
    Session != Session2.
```

This duplicates the `worked_well` atom and uses cross-session inequality — a brittle workaround that:
- Breaks if 3+ sessions use the same tool
- Hardcodes the threshold in the number of atoms (2 atoms = 2 sessions)
- Cannot express "3 distinct sessions in the same context"

The desired rule:
```datalog
preferred_tool(Ctx, Tool) :-
    count(worked_in_context(Session, Ctx, Tool), N),
    N >= 3.
```

…but `worked_in_context/3` doesn't exist in the fact set — it's a JOIN of `worked_well(Session, Tool)` + `session_context(Session, Ctx)`.

## Proposed Solution: Pre-Aggregation Materialization

Add support for **rule-derived predicates as aggregate inner atoms**. When the inner atom's predicate is not in the base fact set, the evaluator should:

1. Derive the inner predicate first (materialize it into a temporary fact set)
2. Run the aggregate over the derived facts
3. Clean up the temporary materialization

### Grammar Extension (v2 — after this FR)

```datalog
preferred_tool(Ctx, Tool) :-
    count(
        worked_well(Session, Tool),
        session_context(Session, Ctx),
        N
    ),
    N >= 3.
```

The aggregate inner becomes a ** conjunction of atoms** instead of a single atom. The count computes distinct `Session` values per `(Ctx, Tool)` group that satisfy both atoms.

### Implementation Plan (v2 — not in this FR)

1. Extend `Aggregate.inner` from `Atom` to `Vec<Atom>` (or add `inner_conjunction: Vec<Atom>`)
2. Modify `count_inner_matches` to evaluate a conjunction of atoms per binding
3. Add stratification: the aggregate's inner predicates must be from a lower stratum than the rule head
4. Add parse-time recursion check for any predicate in the inner conjunction
5. Add test: `count(foo(A, B), bar(B, C), N)` — counts distinct tuples where both atoms unify

This is a larger change than v1 aggregation. This FR documents the need; actual implementation should be a follow-up spec.

## Workaround Until v2

Keep the current `worked_well` + `session_context` + `S != S2` duplication pattern in the learning hook. Include a comment referencing this FR:

```python
# FR: Multi-predicate pre-aggregation materialization
# Until v2, we duplicate atoms + use inequality as a threshold workaround.
```

## Acceptance Criteria

- [ ] A learning or user rule can express: `count(atom1, atom2, ..., N), N >= K`
- [ ] The aggregate groups by variables shared between the outer rule head and the inner conjunction atoms
- [ ] Variables that appear in the inner conjunction but NOT in the head or other body atoms are existentially quantified (counted over)
- [ ] Recursion through the aggregate is rejected at parse time
- [ ] No regression to existing single-predicate aggregate behavior

## Risk

| Risk | Mitigation |
|------|-----------|
| Stratification complexity with multi-predicate conjunctions | v2 limits to base predicates only; rule-derived predicates in inner conjunction deferred to v3 |
| Performance: temporary materialization for each aggregate group | Benchmark with 10k facts; if too slow, add memoization of inner conjunction bindings |
| Breaking change to `Aggregate` struct shape | Add `inner_conjunction: Vec<Atom>` alongside existing `inner: Atom`; deprecate `inner` in v3 |

## Related

- `datalog_learning_hook.py` — `prefer_working_tools` rule uses the workaround this FR would eliminate
- `2026-05-02-datalog-aggregation-design.md` — the v1 spec that this FR extends
