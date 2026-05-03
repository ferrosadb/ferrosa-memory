# Datalog Multi-Predicate Aggregation — `count` over a Conjunction

**Status:** Design
**Date:** 2026-05-02
**Component:** `ferrosa-memory-core` / `datalog`
**Predecessor specs:**
- `2026-05-02-datalog-filter-grammar-design.md`
- `2026-05-02-datalog-aggregation-design.md`
**Work item:** `specs/in-process/feat-multi-predicate-preaggregation.md`

## Problem

The v1 `count(...)` aggregate accepts a single inner atom, so rules that need to count over a *join* of multiple predicates have no clean expression. The user's target rule:

```datalog
preferred_tool(Ctx, Tool) :-
    count(worked_well(S, Tool), session_context(S, Ctx), N),
    N >= 3.
```

…is unwritable today. The current workaround in `datalog_learning_hook.py::prefer_working_tools` duplicates the `worked_well` atom and uses `Session != Session2` inequality as an ad-hoc threshold of 2 — brittle, doesn't scale to K≥3, and breaks when the codomain shifts.

## Goals

1. Allow `count(<atom_1>, <atom_2>, ..., <atom_n>, <OutVar>)` for any n ≥ 1.
2. Allow rule-derived predicates inside the aggregate's inner conjunction (the inner predicate doesn't have to be a base fact-set predicate).
3. Allow bidirectional binding flow: outer body bindings (head vars + non-aggregate body vars) flow into the inner conjunction as filters/group keys; the inner's `output_var` flows out.
4. Detect recursion through aggregation (intra-rule and cross-rule) at load time and refuse to derive — no infinite loop, no silent partial result.
5. Stay backward-compatible with v1 single-atom aggregates already persisted to CQL.

## Non-goals

- `sum`, `min`, `max`, `avg` — same machinery, different reducer; defer.
- Aggregation in head positions (e.g. `count(...) > X` directly in head) — not standard Datalog.
- Negation (`not p(X)`) — orthogonal feature; if/when added, stratification will subsume both.
- Multi-rule recursion through aggregation. v1 blocked intra-rule head recursion only; v2 lifts and generalizes the check via stratification.

## AST changes (`crates/ferrosa-memory-core/src/types.rs`)

`Aggregate` grows one field; existing fields stay byte-compatible with v1 serialized form:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub kind: AggregateKind,
    /// v1 single-atom inner. Continues to be set by the parser for
    /// every aggregate (set to the FIRST atom of the conjunction in
    /// v2) so that already-persisted readers see a coherent shape.
    pub inner: Atom,
    /// v2 multi-atom inner (the conjunction). Empty for v1-shaped
    /// rules that have only one inner atom; the evaluator falls back
    /// to `[&inner]` in that case.
    #[serde(default)]
    pub inner_conjunction: Vec<Atom>,
    pub group_vars: Vec<String>,
    pub output_var: String,
}
```

`StratifyError` added next to `Aggregate`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StratifyError {
    /// The rule set has a cycle that passes through an aggregate's
    /// inner conjunction, which would mean computing an aggregate over
    /// a predicate whose own derivation depends on the aggregate's
    /// result. We refuse to evaluate; this is a fail-loud error.
    RecursionThroughAggregate { cycle: Vec<String> },
}
```

## Parser changes (`datalog.rs::parse_aggregate`)

Generalize from `count(atom, OutVar)` to `count(atom_1, atom_2, ..., atom_n, OutVar)`:

1. Strip `count(` prefix and trailing `)`.
2. `split_top_level(inner_text, ',')` — already correctly handles nested parens (e.g. `foo(X, Y)` is one atom, not two).
3. The last comma-separated piece is `output_var`; require it to start with `[A-Z_]`.
4. The remaining pieces (≥ 1) are the atoms; parse each via `parse_atom` and collect into a `Vec<Atom>`.
5. If exactly one atom: legacy v1 shape — set `inner = atom[0]`, `inner_conjunction = vec![]`.
6. If two or more atoms: v2 shape — set `inner = atom[0].clone()` (for backward-compat readers), `inner_conjunction = atoms` (full list including atom[0]).

`group_vars` computation expands. For each var that appears in the effective atom list (`inner_conjunction` if non-empty, else `[&inner]`):

- Include it in `group_vars` iff it also appears in `head_vars` OR in any non-aggregate body atom of the same rule.
- Otherwise it is existentially quantified (counted over but not exposed).

`head_vars` and `body_vars` are computed before the body-element loop. The intra-rule head-recursion guard added in v1's Task A2 is **removed** — the new `stratify` check at evaluation time supersedes it (and catches more cases).

## Stratification analyzer (`datalog.rs::stratify`)

```rust
pub fn stratify(rules: &[DatalogRule]) -> Result<Vec<Vec<usize>>, StratifyError>
```

Algorithm:

1. **Build the predicate dependency graph** with two edge kinds (`Plain`, `Aggregate`):
   - For each rule, for each atom `a` in `body`: add edge `head.predicate --Plain--> a.predicate`.
   - For each rule, for each atom in `inner_conjunction` (or `inner` if empty): add edge `head.predicate --Aggregate--> a.predicate`.

2. **Find SCCs** with Tarjan's algorithm.

3. **Reject unstratifiable cycles**: for each SCC of size > 1, scan all edges within the SCC. If any edge has kind `Aggregate`, return `Err(StratifyError::RecursionThroughAggregate { cycle: <predicate names in SCC> })`. Self-edges (size-1 SCCs that link back to themselves) get the same check.

4. **Assign strata**: condense the SCC graph (DAG of SCCs) and topologically sort. Each SCC's stratum = max stratum of its predecessor SCCs, plus 1 if the inbound edge was `Aggregate`, else 0.

5. **Return rule indices grouped by stratum** (ascending). Predicates that are only base facts (never appear as a rule head) sit at stratum 0 and contribute nothing to the rule grouping.

Failure mode: if the analyzer rejects, `evaluate` immediately returns `(initial_facts.clone(), Vec::new())` and emits one `tracing::warn!(?error, "datalog: rule set is unstratifiable; deriving nothing")`. Per the project's fail-loud rules, this is the visible-failure path — never a silent partial derivation.

## Evaluator changes (`datalog.rs::evaluate`)

Replace the current single-pass body with stratum-by-stratum evaluation:

```rust
pub fn evaluate(
    rules: &[DatalogRule],
    initial_facts: &FactSet,
    max_iterations: usize,
    max_facts: usize,
) -> (FactSet, Vec<DerivedFact>) {
    let strata = match stratify(rules) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = ?e, "datalog: rule set is unstratifiable; deriving nothing");
            return (initial_facts.clone(), Vec::new());
        }
    };

    let mut all_facts = initial_facts.clone();
    let mut derived = Vec::new();
    let mut budget_iter = max_iterations;
    let mut budget_facts = max_facts;

    for stratum_idxs in strata {
        let stratum_rules: Vec<DatalogRule> =
            stratum_idxs.iter().map(|i| rules[*i].clone()).collect();
        let (next_facts, next_derived) = evaluate_stratum(
            &stratum_rules,
            &all_facts,
            &mut budget_iter,
            &mut budget_facts,
        );
        all_facts = next_facts;
        derived.extend(next_derived);
        if budget_iter == 0 || budget_facts == 0 {
            break;
        }
    }
    (all_facts, derived)
}
```

`evaluate_stratum(rules, facts, budget_iter, budget_facts)` is the previous `evaluate` body, factored out and threading mutable budget refs. Within a stratum, the existing semi-naive loop works unchanged because the stratification guarantee means no rule in the stratum aggregates over a head produced by another rule in this or any later stratum.

`count_inner_matches` extends to handle the conjunction by recursive backtracking unification:

```rust
fn count_inner_matches(
    agg: &Aggregate,
    binding: &HashMap<String, Term>,
    all_facts: &FactSet,
) -> usize {
    let atoms: Vec<&Atom> = if agg.inner_conjunction.is_empty() {
        vec![&agg.inner]
    } else {
        agg.inner_conjunction.iter().collect()
    };
    let mut count = 0;
    count_conjunction(&atoms, 0, binding.clone(), all_facts, &mut count);
    count
}

fn count_conjunction(
    atoms: &[&Atom],
    i: usize,
    binding: HashMap<String, Term>,
    all_facts: &FactSet,
    count: &mut usize,
) {
    if i == atoms.len() {
        *count += 1;
        return;
    }
    let atom = atoms[i];
    let Some(rows) = all_facts.get(&atom.predicate) else { return };
    for row in rows {
        if let Some(extended) = try_unify(&atom.args, row, &binding) {
            count_conjunction(atoms, i + 1, extended, all_facts, count);
        }
    }
}
```

`apply_aggregate` and `seed_bindings_from_inner` adapt:

- `apply_aggregate` calls the new `count_inner_matches` per group; nothing else changes.
- `seed_bindings_from_inner` (the v1 helper that enumerates from the inner predicate when there's no body) now seeds from the **first** atom of the conjunction — its rows generate the candidate `group_vars` values; the rest of the conjunction is then enforced by `count_inner_matches`.

## Bindings flow

To answer the question "what does the inner conjunction see?" — concretely, when evaluating `count(...)` for a binding at the rule's outer scope:

- **Visible inside the aggregate:** values bound by the rule head's vars (when the head is constructed) and by any non-aggregate body atom in the same rule.
- **Not visible inside the aggregate:** other aggregates' output vars (the partition in `evaluate_rule_with_aggregates` already enforces this — pre-aggregate filters can't reference aggregate outputs).

This is enforced naturally by the two-phase evaluator: Phase 1 produces candidate bindings from the non-aggregate body, then each aggregate runs against those bindings — so `binding` already contains what the conjunction needs as constants.

## Backward compatibility

- **Persisted rules:** `inner: Atom` field is unchanged. `inner_conjunction: Vec<Atom>` has `#[serde(default)]`, so old rows deserialize with `inner_conjunction: vec![]`. Old readers that don't know the new field skip it. New readers prefer `inner_conjunction` when non-empty and fall back to `[&inner]`.
- **Behavior of v1 rules:** Single-atom aggregates from v1 keep the v1 evaluator path verbatim (the `inner_conjunction.is_empty()` branch). All v1 tests pass without changes.
- **Recursion-through-head intra-rule check:** The v1 explicit `agg.inner.predicate == head.predicate` test in `parse_rule` is **removed**. The new `stratify` analyzer subsumes it and catches cross-rule cycles too. Behavior change: rules with intra-rule head recursion now parse cleanly but fail at `evaluate` time with a clearer error. Tests that asserted the old parse-time error (`parse_rule_rejects_intra_rule_recursion_through_count`) need updating.

## Testing

Six test groups; ~14 new tests.

1. **Type/serde** (`types.rs::tests`):
   - `aggregate_v2_round_trips_through_json` — 2-atom conjunction round-trips JSON.
   - `aggregate_v1_legacy_deserializes_with_empty_conjunction` — JSON without `inner_conjunction` key deserializes with empty vec.

2. **Stratification analyzer** (`datalog.rs::tests`):
   - `stratify_simple_chain_assigns_ascending_strata` — `b :- a.` and `c :- b.` produces `[[a-rules], [b-rules], [c-rules]]`.
   - `stratify_aggregate_lifts_one_level` — `b :- count(a, N), N > 0.` puts `b` strictly above `a`.
   - `stratify_rejects_recursion_through_aggregate` — `b :- count(b, N).` returns `Err(RecursionThroughAggregate { cycle: ["b"] })`.
   - `stratify_rejects_cross_rule_recursion_through_aggregate` — mutual recursion `a :- b.` + `b :- count(a, N).` rejects with both names in the cycle.
   - `stratify_allows_plain_recursion` — `path(X, Z) :- edge(X, Y), path(Y, Z).` accepts (cycle exists but only via Plain edges).

3. **Parser** (`datalog.rs::tests`):
   - `parse_rule_supports_two_atom_conjunction` — FR's exact rule parses; `inner_conjunction.len() == 2`; `inner` is the first atom; `group_vars` includes shared head vars.
   - `parse_rule_intra_rule_recursion_now_at_evaluate_time` — `count(loop(Y), N)` in `loop` head's rule parses cleanly; `evaluate` returns empty + warn (the v1 parse-time check is removed).
   - `parse_rule_rejects_aggregate_with_no_atoms` — `count(N)` (just an output var) fails at parse with "must have at least one inner atom and an output var".

4. **Evaluator over conjunctions** (`datalog.rs::tests`):
   - `evaluator_two_atom_conjunction_groups_correctly` — full FR rule. Seed 4 sessions × 2 contexts × 3 tools with carefully chosen overlap; assert `(Ctx, Tool)` pairs with ≥ 3 distinct shared sessions appear, < 3 do not.
   - `evaluator_existential_var_aggregated_over` — vars only in `inner_conjunction` (not head, not body) are quantified over and not exposed.
   - `evaluator_outer_body_var_flows_into_aggregate` — a non-aggregate body atom binds a var; that binding is visible inside the conjunction during enumeration.

5. **Backward compat** (`datalog.rs::tests`):
   - `legacy_v1_single_atom_rule_still_works` — `count(a(X), N)` parses with `inner_conjunction.is_empty()` and evaluates identically to v1.
   - All v1 aggregation tests (`user_example_count_aggregate_with_ge`, `evaluator_count_aggregate_groups_and_counts`) keep passing without modification.
   - **Update needed:** `parse_rule_rejects_intra_rule_recursion_through_count` no longer asserts at parse time. Either delete it or rewrite to assert the equivalent at `evaluate` time.

6. **Integration / acceptance** (`datalog.rs::tests`):
   - `acceptance_threshold_K_eq_3` — fires on K=3, doesn't on K=2.
   - `acceptance_existential_quantification` — vars only in inner conjunction don't appear in derived head facts.
   - `acceptance_recursion_rejected_at_load_time` — calls `evaluate` on a recursion-through-aggregate rule set; asserts no derivations + warn fired (use a `tracing-subscriber` test fixture).

The existing 700-test suite must remain green except for the one parser test noted in group 5 that gets updated/removed.

## Risk

| Risk | Mitigation |
|------|-----------|
| Old persisted rules fail to deserialize | `#[serde(default)]` on `inner_conjunction`; `inner` field shape unchanged. Round-trip test exercises both shapes. |
| Old binary reads v2 rules and computes wrong counts (mixed-version deploy) | Real issue: an old reader sees only `inner` (the first atom) and would treat a v2 rule as if it had only one inner atom, undercounting. In a single-local-cluster setup the version-skew window is the redeploy moment only. For multi-replica deployments, gate v2 emission behind a server-config flag (`datalog.allow_multi_atom_aggregates`) and roll out: enable readers first, enable writers second. Not implemented in v2; documented as a roll-out constraint for any future mixed-version environment. |
| Stratification analyzer has bugs at scale | Pure-function with a small (predicate-name) graph; Tarjan is well-trodden. Add property test (optional) generating random rule graphs and checking that any cycle through an Aggregate edge is caught. |
| Performance: backtracking conjunction evaluation can blow up combinatorially | The user's expected scale is ≤ 1000 rows per predicate × ≤ 4 atoms per conjunction = ≤ 10^12 worst case but typically << 10^6 with bound vars pruning early. If hot, add memoization keyed on `(atoms, binding-restriction-to-conjunction-vars)`. Not in v2. |
| Cross-rule cycle detection error message is too cryptic | `cycle: Vec<String>` carries the predicate names in source-code order; the warn log includes them. |
| Stratum-by-stratum evaluation diverges from current semi-naive guarantees for plain rules | Identical: each stratum runs the same fixpoint loop; only the rule grouping is new. v1 plain-recursion tests confirm parity. |

## Out-of-scope follow-ups

- `sum`/`min`/`max`/`avg` aggregates — same conjunction machinery, different reducer.
- Aggregate over rule-derived predicate where the rule itself uses an aggregate (multi-stratum aggregation) — requires verifying no transitive cycle; current analyzer already handles this.
- Negation (`not p(X)`) — would integrate with stratification (negation also forces a stratum boundary).
- Memoization of conjunction unification keyed on bound prefix.
