# Datalog Aggregation — `count` Aggregate

**Status:** Design
**Date:** 2026-05-02
**Component:** `ferrosa-memory-core` / `datalog`
**Related code:**
- `crates/ferrosa-memory-core/src/datalog.rs` (parser, evaluator)
- `crates/ferrosa-memory-core/src/types.rs` (`DatalogRule`, `Atom`)
**Predecessor spec:** `2026-05-02-datalog-filter-grammar-design.md`

## Problem

After the filter-grammar change, rules can express `N >= 3`. They still cannot bind `N` from a count over an inner goal. The user's target rule:

```datalog
avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.
```

needs an aggregate body element that runs the inner atom `user_corrected(S, X)` to fixpoint over the rule's other bindings, counts distinct matching rows for each `X`, binds `N`, then continues with the regular filter `N >= 3`.

The previous spec deliberately deferred this; the test `user_example_count_aggregate_with_ge` is `#[ignore]`d as a placeholder.

## Goals

- Parse `count(<atom>, <output_var>)` body elements into a new `Aggregate` AST node.
- Stratified evaluation: aggregates compute after the non-aggregate parts of the body bind `group_vars`, then bind `output_var` for use in subsequent filters.
- Reject rules that recurse through an aggregate (the head predicate appearing inside any `inner` atom of an aggregate in the same rule) with a clear parse-time error.

## Non-goals (deferred)

- `sum`, `min`, `max`, `avg` — same machinery, different reducer; defer to a follow-up spec.
- Multi-rule recursion through aggregation (stratified negation/aggregation across rules). v1 only blocks intra-rule recursion.
- Aggregation over rule heads (e.g. `count(my_rule(X), N)` where `my_rule` is itself derived). v1 supports aggregation over predicates already in the fact set when the aggregate runs.

## Grammar

A body element is now one of three shapes (current implementation distinguishes filter vs atom; aggregation adds a third):

```
body_elem ::= aggregate | filter | atom
aggregate ::= aggregate_fn "(" atom "," output_var ")"
aggregate_fn ::= "count"
output_var ::= identifier         (Datalog variable; same identifier rules as elsewhere)
```

The dispatcher in `parse_rule` checks shape in this order:

1. **Aggregate first**: if the trimmed body element starts with one of the aggregate function names followed by `(`, attempt aggregate parsing.
2. **Filter**: existing `has_top_level_cmp` pre-scan.
3. **Atom**: fallback.

The aggregate test runs before `has_top_level_cmp` so that, e.g., `count(foo(X, Y), N)` (which contains a `,` but no top-level comparison) is recognized as an aggregate before the filter dispatcher sees it.

## Type changes

`crates/ferrosa-memory-core/src/types.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AggregateKind {
    Count,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub kind: AggregateKind,
    pub inner: Atom,
    /// Variables that appear in `inner` AND in the rule head (or in
    /// other body atoms) — the aggregate groups by these. Computed at
    /// parse time so the evaluator does not have to re-derive them.
    pub group_vars: Vec<String>,
    /// The variable bound by the aggregate's output, e.g. the `N` in
    /// `count(foo(X), N)`.
    pub output_var: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatalogRule {
    pub head: Atom,
    pub body: Vec<Atom>,
    pub filters: Vec<BuiltinFilter>,
    /// New. Empty for non-aggregating rules — preserves CQL-stored
    /// rule deserialization (serde defaults the field).
    #[serde(default)]
    pub aggregates: Vec<Aggregate>,
}
```

`#[serde(default)]` ensures already-persisted `RuleEntry` rows that lack the field still deserialize. Existing constructors of `DatalogRule` need to fill in `aggregates: Vec::new()` — there are only a handful (parse_rule + tests).

## Parser changes (`datalog.rs`)

New private function:

```rust
fn parse_aggregate(s: &str, head_vars: &HashSet<String>, body_vars: &HashSet<String>) -> Option<anyhow::Result<Aggregate>>
```

Returns:
- `None` if `s` is not shaped like an aggregate (not aggregate dispatch).
- `Some(Ok(Aggregate))` on success.
- `Some(Err(...))` if it looks like an aggregate but is malformed (clear error message).

`parse_rule` flow becomes (for each body element after splitting on top-level `,`):

```
1. if let Some(result) = parse_aggregate(part, &head_vars, &body_vars) { aggregates.push(result?) }
2. else if has_top_level_cmp(part) { filters.push(parse_filter(part)?) }
3. else { body.push(parse_atom(part, &mut anon_counter)?) }
```

`group_vars` is computed at parse time as: variables in `inner.args` that also appear in either the head or any other (non-aggregate) body atom of the same rule. Variables that appear only inside the aggregate's `inner` are aggregated over (their values are not exposed; they're enumerated to compute the count).

**Recursion check** at the end of `parse_rule`: if any `aggregate.inner.predicate == head.predicate`, return `Err("aggregation through head predicate '<name>' is not supported in v1")`. Multi-rule mutual recursion is harder to detect at parse time; document as a known limitation.

## Evaluator changes (`datalog.rs::evaluate_rule`)

Today `evaluate_rule` returns `Vec<(Vec<Term>, Vec<ProvenanceStep>)>` — a candidate set of head bindings with provenance. Two-phase rewrite when `rule.aggregates` is non-empty:

**Phase 1 — non-aggregate evaluation:**
Run the existing body+filter unifier ignoring aggregate filters that depend on aggregate output vars. Produce candidate bindings over `body_vars` (vars in `body` atoms only).

**Phase 2 — aggregate computation:**
For each `Aggregate` in `rule.aggregates`, in source order:
1. Group the candidate bindings by the values of `aggregate.group_vars`.
2. For each group, count the number of distinct rows in `all_facts` whose predicate matches `aggregate.inner.predicate` and that unify with `aggregate.inner.args` under the group's binding (variables in `inner.args` not in `group_vars` are existentially quantified — any binding works).
3. Bind `aggregate.output_var` to the count for that group, and emit a fresh per-group binding into the candidate set.

Filters that reference an aggregate output var were skipped in Phase 1; re-apply them now using the augmented bindings.

Provenance for derived facts that came from aggregates includes the inner atom name and the count: `ProvenanceStep::Aggregate { predicate, group_vars, count }`.

## Backward compatibility

- `aggregates: Vec<Aggregate>` is a new field with `#[serde(default)]` — old `RuleEntry` rows deserialize with `aggregates: vec![]`.
- Rules that have no aggregates take the existing single-phase evaluation path; performance unchanged.
- Existing tests assert on `rule.head`, `rule.body`, `rule.filters` — no test pokes at `rule.aggregates`, so adding the field is non-breaking.

## Testing

1. Unignore `user_example_count_aggregate_with_ge` and turn it into a real assertion: 3 distinct sessions correcting target T → `avoid_action(T)` fires; 2 sessions correcting U → does not fire.
2. Parser unit tests:
   - `count(foo(X, Y), N)` parses with `group_vars = ["X"]` (assuming X appears in head, Y does not).
   - `count(foo(X), N)` with X in head → group_vars = ["X"].
   - Malformed: `count(N)` → parse error.
   - Malformed: `count(foo(X), 3)` (output var must be a Var) → parse error.
   - Recursion: `loop(X) :- count(loop(Y), N), N > 0.` → parse error citing recursion.
3. Evaluator unit test for the count itself: seed N rows, parse a count rule, assert N is bound and the head fires/doesn't fire per a `>=` filter.
4. Backward-compat: parse all 10 rules in `builtin_rules()` — they have no aggregates, must still work.
5. Serde: `DatalogRule` with empty `aggregates` round-trips; with one aggregate also round-trips.

## Risk

| Risk | Mitigation |
|------|-----------|
| Old persisted rules fail to deserialize | `#[serde(default)]` fills empty `aggregates`. Round-trip test exercises both shapes. |
| Aggregate over rule-derived predicates produces wrong count when ordering | v1 supports aggregation only over fact-set predicates that have already been derived when the aggregate runs. Document as a known limitation; revisit when stratified-negation work begins. |
| Recursion through aggregate produces non-terminating evaluation | Parse-time check rejects head-predicate recurrence in any `aggregate.inner`. |
| `group_vars` mis-computed for variables that appear only in inner | Test case: `count(foo(X, Y), N)` with `Y` only in `inner` — assert `Y ∉ group_vars`. |
