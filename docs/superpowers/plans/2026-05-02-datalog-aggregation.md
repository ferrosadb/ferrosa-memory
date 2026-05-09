# Datalog Aggregation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Add `count(<atom>, <var>)` aggregate support to the Datalog parser and evaluator in `ferrosa-memory-core`. Unignore the `user_example_count_aggregate_with_ge` test and make it pass.

**Architecture:** Add `Aggregate` struct + `AggregateKind` enum and an `aggregates: Vec<Aggregate>` field to `DatalogRule` (with `#[serde(default)]` for backward compat). Extend `parse_rule` body-element dispatch with an aggregate-shape check. Two-phase evaluator: regular body+filters first, then aggregate phase that groups candidate bindings, counts inner-atom matches per group, binds `output_var`, then re-applies filters that reference aggregate outputs. Reject intra-rule recursion through aggregates at parse time.

**Tech Stack:** Rust 2024, no new deps. Target crate: `ferrosa-memory-core`.

**Spec:** `docs/superpowers/specs/2026-05-02-datalog-aggregation-design.md`

**Branch:** `feat/datalog-filter-grammar` (continue from head 29a0195)

---

## File Structure

| Action | Path | Responsibility |
|--------|------|----------------|
| Modify | `crates/ferrosa-memory-core/src/types.rs` | Add `AggregateKind`, `Aggregate`; add `aggregates` field to `DatalogRule` (with `#[serde(default)]`). |
| Modify | `crates/ferrosa-memory-core/src/datalog.rs` | Add `parse_aggregate`, recursion check; extend `parse_rule` dispatch; rewrite `evaluate_rule` for two-phase aggregate handling. |

---

## Task A1: Add Aggregate types

**Files:** `crates/ferrosa-memory-core/src/types.rs`

- [ ] **Step 1: Failing test (serde + struct shape)**

Append to the test module in `types.rs`:

```rust
#[test]
fn aggregate_round_trips_through_json() {
    let a = Aggregate {
        kind: AggregateKind::Count,
        inner: Atom {
            predicate: "user_corrected".into(),
            args: vec![
                Term::Var("S".into()),
                Term::Var("X".into()),
            ],
        },
        group_vars: vec!["X".into()],
        output_var: "N".into(),
    };
    let json = serde_json::to_string(&a).unwrap();
    let back: Aggregate = serde_json::from_str(&json).unwrap();
    assert_eq!(back, a);
}

#[test]
fn datalog_rule_without_aggregates_field_deserializes_with_default() {
    // Old RuleEntry rows in CQL serialize without an `aggregates` field;
    // serde(default) must fill it in.
    let json = r#"{
        "head": {"predicate": "foo", "args": [{"Var": "X"}]},
        "body": [{"predicate": "bar", "args": [{"Var": "X"}]}],
        "filters": []
    }"#;
    let rule: DatalogRule = serde_json::from_str(json).unwrap();
    assert!(rule.aggregates.is_empty());
}
```

Run: `cargo test --package ferrosa-memory-core --lib types::tests::aggregate_round_trips_through_json types::tests::datalog_rule_without_aggregates_field_deserializes_with_default`
Expected: FAIL — `Aggregate` and `AggregateKind` don't exist; `DatalogRule` has no `aggregates` field.

- [ ] **Step 2: Add the types**

In `types.rs`, near `BuiltinFilter`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AggregateKind {
    Count,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub kind: AggregateKind,
    pub inner: Atom,
    pub group_vars: Vec<String>,
    pub output_var: String,
}
```

Modify `DatalogRule` to add the field with serde default:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatalogRule {
    pub head: Atom,
    pub body: Vec<Atom>,
    pub filters: Vec<BuiltinFilter>,
    #[serde(default)]
    pub aggregates: Vec<Aggregate>,
}
```

- [ ] **Step 3: Update existing constructors of `DatalogRule`** so the file compiles. Search the workspace for `DatalogRule {` and add `aggregates: Vec::new(),` to every struct literal that doesn't already have it. Use `cargo build --package ferrosa-memory-core --lib` to find all sites; iterate until clean.

- [ ] **Step 4: Run tests, expect green**

Run: `cargo test --package ferrosa-memory-core --lib types::tests::aggregate_round_trips_through_json types::tests::datalog_rule_without_aggregates_field_deserializes_with_default`
Expected: PASS.

- [ ] **Step 5: Run the full library test suite — must not regress**

Run: `cargo test --package ferrosa-memory-core --lib`
Expected: 693 passed, 1 ignored (same as baseline). The new tests bring the count up by 2.

- [ ] **Step 6: Commit**

```bash
git add crates/ferrosa-memory-core/src/types.rs crates/ferrosa-memory-core/src/datalog.rs
git commit -m "types(datalog): add Aggregate + AggregateKind; add DatalogRule.aggregates"
```

(`datalog.rs` may have changed if its test fixtures or `parse_rule` constructed `DatalogRule` literally — both go in the same commit since they're a coordinated type change.)

---

## Task A2: Parse `count(atom, var)` body elements

**Files:** `crates/ferrosa-memory-core/src/datalog.rs`

- [ ] **Step 1: Failing test for parser**

Append to `datalog::tests`:

```rust
#[test]
fn parse_rule_supports_count_aggregate() {
    use crate::types::{Aggregate, AggregateKind};
    let rule = parse_rule(
        "avoid_action(X) :- count(user_corrected(S, X), N), N >= 3."
    )
    .unwrap();
    assert_eq!(rule.aggregates.len(), 1);
    let agg = &rule.aggregates[0];
    assert_eq!(agg.kind, AggregateKind::Count);
    assert_eq!(agg.inner.predicate, "user_corrected");
    assert_eq!(agg.inner.args.len(), 2);
    assert_eq!(agg.output_var, "N");
    // X appears in head, S only in inner — group by X, aggregate over S.
    assert_eq!(agg.group_vars, vec!["X".to_string()]);
    // The N >= 3 filter is preserved.
    assert_eq!(rule.filters.len(), 1);
}

#[test]
fn parse_rule_rejects_intra_rule_recursion_through_count() {
    let err = parse_rule(
        "loop(X) :- count(loop(Y), N), N > 0."
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("aggregation through head predicate") || msg.contains("recursion"),
        "expected recursion-through-aggregate error, got: {msg}"
    );
}

#[test]
fn parse_rule_rejects_count_with_non_var_output() {
    let err = parse_rule("avoid_action(X) :- count(user_corrected(S, X), 3).").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("output_var") || msg.contains("variable") || msg.contains("Var"),
        "expected output-var-must-be-Var error, got: {msg}"
    );
}
```

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::parse_rule_supports_count_aggregate datalog::tests::parse_rule_rejects_intra_rule_recursion_through_count datalog::tests::parse_rule_rejects_count_with_non_var_output`
Expected: FAIL — current `parse_rule` doesn't recognize aggregates.

- [ ] **Step 2: Implement `parse_aggregate`**

In `crates/ferrosa-memory-core/src/datalog.rs`, just below the existing `parse_atom` function (or wherever helpers live), add:

```rust
/// Try to parse `s` as an aggregate body element. Returns:
/// * `Ok(None)` — `s` is not aggregate-shaped; caller should try filter/atom.
/// * `Ok(Some(agg))` — `s` parses cleanly.
/// * `Err(...)` — `s` looks like an aggregate (starts with `count(`) but
///   is malformed.
///
/// `head_vars` is the set of variables in the rule head; used to compute
/// `group_vars`. Variables in `inner` that are also in `head_vars` (or
/// will be in `body_vars` once non-aggregate body parses complete — see
/// `parse_rule` for the second pass) become group vars.
fn parse_aggregate(
    s: &str,
    head_vars: &std::collections::HashSet<String>,
) -> anyhow::Result<Option<crate::types::Aggregate>> {
    let s = s.trim();
    let Some(rest) = s.strip_prefix("count(") else {
        return Ok(None);
    };
    let Some(inner_text) = rest.strip_suffix(')') else {
        anyhow::bail!("aggregate '{s}' is missing closing ')'");
    };
    // Split on the LAST top-level comma — `count(foo(X, Y), N)` has the
    // output var after the last top-level comma.
    let parts = split_top_level(inner_text, ',')?;
    if parts.len() < 2 {
        anyhow::bail!("aggregate '{s}' must have an inner atom and an output var separated by ','");
    }
    let output = parts.last().unwrap().trim().to_string();
    let inner_text = parts[..parts.len() - 1].join(",");

    let mut anon = 0;
    let inner = parse_atom(&inner_text, &mut anon)?;

    // Output must be a variable (uppercase-leading or underscore).
    if !output
        .chars()
        .next()
        .map(|c| c.is_ascii_uppercase() || c == '_')
        .unwrap_or(false)
    {
        anyhow::bail!("aggregate output_var '{output}' must be a variable (start with uppercase or '_')");
    }

    // group_vars: variables in inner.args that also appear in head_vars.
    // (Vars that appear only in inner are aggregated over.)
    let mut group_vars = Vec::new();
    for arg in &inner.args {
        if let crate::types::Term::Var(name) = arg
            && head_vars.contains(name)
            && !group_vars.contains(name)
        {
            group_vars.push(name.clone());
        }
    }

    Ok(Some(crate::types::Aggregate {
        kind: crate::types::AggregateKind::Count,
        inner,
        group_vars,
        output_var: output,
    }))
}
```

- [ ] **Step 3: Wire `parse_aggregate` into `parse_rule`**

Locate the body-parts loop in `parse_rule`. Before the loop, compute `head_vars`:

```rust
let head_vars: std::collections::HashSet<String> = head
    .args
    .iter()
    .filter_map(|t| match t {
        Term::Var(name) => Some(name.clone()),
        _ => None,
    })
    .collect();
```

Replace the existing if-else:

```rust
if has_top_level_cmp(part) {
    let f = crate::datalog_filter_expr::parse_filter(part)?;
    filters.push(f);
} else {
    body.push(parse_atom(part, &mut anon_counter)?);
}
```

with:

```rust
if let Some(agg) = parse_aggregate(part, &head_vars)? {
    aggregates.push(agg);
} else if has_top_level_cmp(part) {
    let f = crate::datalog_filter_expr::parse_filter(part)?;
    filters.push(f);
} else {
    body.push(parse_atom(part, &mut anon_counter)?);
}
```

Initialize `let mut aggregates: Vec<crate::types::Aggregate> = Vec::new();` next to the existing `let mut body = Vec::new();` and `let mut filters = Vec::new();`.

After the loop, add the recursion check:

```rust
for agg in &aggregates {
    if agg.inner.predicate == head.predicate {
        anyhow::bail!(
            "aggregation through head predicate '{}' is not supported in v1",
            head.predicate
        );
    }
}
```

Then build the `DatalogRule { head, body, filters, aggregates }`.

- [ ] **Step 4: Run the new parser tests**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::parse_rule_supports_count_aggregate datalog::tests::parse_rule_rejects_intra_rule_recursion_through_count datalog::tests::parse_rule_rejects_count_with_non_var_output`
Expected: PASS.

- [ ] **Step 5: Full datalog test suite (no regression)**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests`
Expected: same number passing as before this task plus 3, with 1 still ignored.

- [ ] **Step 6: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs
git commit -m "feat(datalog): parse count(atom, var) aggregates with intra-rule recursion check"
```

---

## Task A3: Two-phase evaluator with aggregate computation

**Files:** `crates/ferrosa-memory-core/src/datalog.rs`

- [ ] **Step 1: Failing evaluator unit test**

Append to `datalog::tests`:

```rust
#[test]
fn evaluator_count_aggregate_groups_and_counts() {
    let s1 = Uuid::new_v4();
    let s2 = Uuid::new_v4();
    let s3 = Uuid::new_v4();
    let target_t = Uuid::new_v4();  // 3 distinct correctors
    let target_u = Uuid::new_v4();  // 2 distinct correctors

    let mut facts = FactSet::new();
    facts.insert("user_corrected", vec![Term::Const(s1), Term::Const(target_t)]);
    facts.insert("user_corrected", vec![Term::Const(s2), Term::Const(target_t)]);
    facts.insert("user_corrected", vec![Term::Const(s3), Term::Const(target_t)]);
    facts.insert("user_corrected", vec![Term::Const(s1), Term::Const(target_u)]);
    facts.insert("user_corrected", vec![Term::Const(s2), Term::Const(target_u)]);

    let rule = parse_rule("avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.").unwrap();
    let (derived, _) = evaluate(&[rule], &facts, 100, 1000);

    let avoided: std::collections::HashSet<Uuid> = derived
        .get("avoid_action")
        .into_iter()
        .flatten()
        .filter_map(|args| match args.first()? {
            Term::Const(u) => Some(*u),
            _ => None,
        })
        .collect();

    assert!(avoided.contains(&target_t), "3 distinct correctors should fire avoid_action");
    assert!(!avoided.contains(&target_u), "2 distinct correctors should NOT fire avoid_action");
}
```

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::evaluator_count_aggregate_groups_and_counts`
Expected: FAIL — evaluator does not handle aggregates yet (rule parses but body has empty `body`/`filters` only references `N` which is never bound, and aggregates Vec is non-empty but ignored).

- [ ] **Step 2: Extend `evaluate_rule` with the two-phase logic**

Locate `fn evaluate_rule(rule: &DatalogRule, ...)`. It currently iterates body atoms, unifies, applies filters, and emits head bindings.

The fix is delicate. Add this logic to the start of `evaluate_rule`:

```rust
if !rule.aggregates.is_empty() {
    return evaluate_rule_with_aggregates(rule, all_facts);
}
```

Then add the new function below:

```rust
fn evaluate_rule_with_aggregates(
    rule: &DatalogRule,
    all_facts: &FactSet,
) -> Vec<(Vec<Term>, Vec<ProvenanceStep>)> {
    use std::collections::HashMap;

    // Phase 1: produce candidate bindings from non-aggregate body atoms.
    // Build a temporary rule with empty aggregates and only filters that
    // do NOT reference any aggregate output_var. Those filters are
    // re-applied in Phase 2.
    let agg_output_vars: std::collections::HashSet<&str> = rule
        .aggregates
        .iter()
        .map(|a| a.output_var.as_str())
        .collect();
    let (phase1_filters, post_agg_filters): (Vec<_>, Vec<_>) = rule
        .filters
        .iter()
        .cloned()
        .partition(|f| !filter_references_any(f, &agg_output_vars));

    let phase1_rule = DatalogRule {
        head: rule.head.clone(),
        body: rule.body.clone(),
        filters: phase1_filters,
        aggregates: Vec::new(),
    };

    // Run Phase 1 by collecting bindings, but we need the BINDINGS (a
    // HashMap<String, Term>), not the head substitution. Refactor: extract
    // the inner unification loop into a helper that returns
    // Vec<(HashMap<String, Term>, Vec<ProvenanceStep>)>.
    let candidate_bindings = collect_bindings(&phase1_rule, all_facts);

    // Phase 2: for each aggregate, group candidates by group_vars and
    // count matching inner-atom rows in all_facts per group.
    let mut augmented = candidate_bindings;
    for agg in &rule.aggregates {
        augmented = apply_aggregate(agg, augmented, all_facts);
    }

    // Phase 3: re-apply filters that reference aggregate output vars,
    // then emit head bindings.
    let mut results = Vec::new();
    for (binding, prov) in augmented {
        if !post_agg_filters.iter().all(|f| check_one_filter(f, &binding)) {
            continue;
        }
        let head_args = instantiate(&rule.head.args, &binding);
        results.push((head_args, prov));
    }
    results
}

fn filter_references_any(f: &BuiltinFilter, vars: &std::collections::HashSet<&str>) -> bool {
    use crate::types::FilterExpr;
    fn expr_refs(e: &FilterExpr, vars: &std::collections::HashSet<&str>) -> bool {
        match e {
            FilterExpr::Var(name) => vars.contains(name.as_str()),
            FilterExpr::LitNum(_) | FilterExpr::LitStr(_) => false,
            FilterExpr::Neg(inner) => expr_refs(inner, vars),
            FilterExpr::BinOp { lhs, rhs, .. } => expr_refs(lhs, vars) || expr_refs(rhs, vars),
        }
    }
    match f {
        BuiltinFilter::NotEqual(a, b) => vars.contains(a.as_str()) || vars.contains(b.as_str()),
        BuiltinFilter::GreaterThan(a, _) | BuiltinFilter::LessThan(a, _) => vars.contains(a.as_str()),
        BuiltinFilter::Compare { lhs, rhs, .. } => expr_refs(lhs, vars) || expr_refs(rhs, vars),
    }
}

fn apply_aggregate(
    agg: &crate::types::Aggregate,
    candidates: Vec<(std::collections::HashMap<String, Term>, Vec<ProvenanceStep>)>,
    all_facts: &FactSet,
) -> Vec<(std::collections::HashMap<String, Term>, Vec<ProvenanceStep>)> {
    use std::collections::HashMap;
    use ordered_float::OrderedFloat;

    // Group candidates by group_vars values.
    let mut groups: HashMap<Vec<Term>, Vec<(HashMap<String, Term>, Vec<ProvenanceStep>)>> =
        HashMap::new();
    for (binding, prov) in candidates {
        let key: Vec<Term> = agg
            .group_vars
            .iter()
            .map(|v| binding.get(v).cloned().unwrap_or(Term::Var(v.clone())))
            .collect();
        groups.entry(key).or_default().push((binding, prov));
    }

    // For each group, count distinct inner-atom rows in all_facts that
    // unify with the group's binding for the group_vars (other vars in
    // inner are existentially quantified).
    let mut out = Vec::new();
    for (_group_key, members) in groups {
        // Pick any binding from the group to compute the count — group
        // vars are by construction equal across all members.
        let representative = match members.first() {
            Some((b, _)) => b.clone(),
            None => continue,
        };
        let count = count_inner_matches(&agg.inner, &representative, all_facts);

        for (mut binding, mut prov) in members {
            binding.insert(
                agg.output_var.clone(),
                Term::ConstFloat(OrderedFloat(count as f64)),
            );
            prov.push(make_provenance_step(
                &format!("count({})", agg.inner.predicate),
                &[Term::ConstFloat(OrderedFloat(count as f64))],
            ));
            out.push((binding, prov));
        }
    }
    out
}

fn count_inner_matches(
    inner: &Atom,
    binding: &std::collections::HashMap<String, Term>,
    all_facts: &FactSet,
) -> usize {
    let Some(rows) = all_facts.get(&inner.predicate) else {
        return 0;
    };
    let mut count = 0;
    for row in rows {
        if try_unify(&inner.args, row, binding).is_some() {
            count += 1;
        }
    }
    count
}

/// Helper: refactor of the current evaluate_rule body to return raw
/// bindings instead of head args. Used by Phase 1 of aggregation.
fn collect_bindings(
    rule: &DatalogRule,
    all_facts: &FactSet,
) -> Vec<(std::collections::HashMap<String, Term>, Vec<ProvenanceStep>)> {
    // Mirror the body of the existing evaluate_rule, but return bindings
    // instead of head_args at the end. See implementation note: factor
    // the existing evaluate_rule body into a private helper that yields
    // bindings; have evaluate_rule call this helper and then run
    // instantiate(head.args). Expected diff: ~30-line refactor.
    todo!("factor existing evaluate_rule into collect_bindings + head instantiation")
}
```

> **Implementer note for the refactor in Step 2:** Look at the existing `evaluate_rule` function. It does roughly: for each body atom, fold candidate bindings; for each candidate, run `check_filters`; if pass, instantiate `head.args` and emit. Refactor it to (a) build `Vec<(HashMap<String, Term>, Vec<ProvenanceStep>)>` and stop just before the final instantiation, expose that as `collect_bindings`. Then make the existing `evaluate_rule` (no aggregates) be:
> ```rust
> fn evaluate_rule(rule: &DatalogRule, all_facts: &FactSet) -> Vec<(Vec<Term>, Vec<ProvenanceStep>)> {
>     if !rule.aggregates.is_empty() {
>         return evaluate_rule_with_aggregates(rule, all_facts);
>     }
>     collect_bindings(rule, all_facts)
>         .into_iter()
>         .map(|(binding, prov)| (instantiate(&rule.head.args, &binding), prov))
>         .collect()
> }
> ```
> This keeps the non-aggregate path unchanged behaviorally.

- [ ] **Step 3: Run the new evaluator test**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::evaluator_count_aggregate_groups_and_counts`
Expected: PASS.

- [ ] **Step 4: Run the full datalog test suite — no regressions**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests`
Expected: existing tests still pass; new test passes; the still-`#[ignore]`d aggregation test will be unignored in Task A4.

- [ ] **Step 5: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs
git commit -m "feat(datalog): two-phase evaluator with count aggregate computation"
```

---

## Task A4: Unignore `user_example_count_aggregate_with_ge` and turn it into a real test

**Files:** `crates/ferrosa-memory-core/src/datalog.rs`

- [ ] **Step 1: Replace the ignored test with the real assertion**

Find `user_example_count_aggregate_with_ge` in the test module and replace the entire test (including the `#[ignore = "..."]` attribute) with:

```rust
#[test]
fn user_example_count_aggregate_with_ge() {
    // The user's target rule:
    //   avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.
    //
    // 3 distinct sessions corrected target T  ⇒  avoid_action(T) fires
    // 2 distinct sessions corrected target U  ⇒  avoid_action(U) does NOT fire
    let s1 = Uuid::new_v4();
    let s2 = Uuid::new_v4();
    let s3 = Uuid::new_v4();
    let target_t = Uuid::new_v4();
    let target_u = Uuid::new_v4();

    let mut facts = FactSet::new();
    facts.insert("user_corrected", vec![Term::Const(s1), Term::Const(target_t)]);
    facts.insert("user_corrected", vec![Term::Const(s2), Term::Const(target_t)]);
    facts.insert("user_corrected", vec![Term::Const(s3), Term::Const(target_t)]);
    facts.insert("user_corrected", vec![Term::Const(s1), Term::Const(target_u)]);
    facts.insert("user_corrected", vec![Term::Const(s2), Term::Const(target_u)]);

    let rule = parse_rule("avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.").unwrap();
    let (derived, _) = evaluate(&[rule], &facts, 100, 1000);

    let avoided: std::collections::HashSet<Uuid> = derived
        .get("avoid_action")
        .into_iter()
        .flatten()
        .filter_map(|args| match args.first()? {
            Term::Const(u) => Some(*u),
            _ => None,
        })
        .collect();

    assert!(avoided.contains(&target_t), "3 distinct correctors should derive avoid_action");
    assert!(!avoided.contains(&target_u), "2 distinct correctors should NOT derive avoid_action");
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::user_example_count_aggregate_with_ge`
Expected: PASS.

- [ ] **Step 3: Run the entire ferrosa-memory-core suite**

Run: `cargo test --package ferrosa-memory-core --lib`
Expected: total passed up by 1 (the test moved from ignored to passing); ignored count down by 1.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy --package ferrosa-memory-core --lib --message-format=short -- -D warnings`
Expected: clean. Fix any new warnings (commit them with the same message extended `style:`).

- [ ] **Step 5: Run fmt**

Run: `cargo fmt --check`. If not clean, run `cargo fmt` and fold into the commit.

- [ ] **Step 6: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs
git commit -m "test(datalog): un-ignore count aggregation regression — now passing"
```

---

## Self-Review

**Spec coverage:**
| Spec section | Implementing task |
|---|---|
| `Aggregate`, `AggregateKind` types + serde default for backward compat | Task A1 |
| Parser dispatch order (aggregate → filter → atom) | Task A2 |
| `parse_aggregate` recognition of `count(...)` | Task A2 |
| `group_vars` computed at parse time from head_vars ∩ inner.args | Task A2 |
| Recursion-through-aggregate parse error | Task A2 |
| Output-var must be a Var error | Task A2 |
| Two-phase evaluator | Task A3 |
| Filter partitioning on aggregate output vars | Task A3 (`filter_references_any`) |
| `count_inner_matches` over `all_facts` | Task A3 |
| User regression test passing | Task A4 |
| `cargo clippy -D warnings` + `cargo fmt --check` | Task A4 |

**Placeholder check:** No "TBD"/"implement later" outside the explicitly-flagged refactor note in Task A3 Step 2 (which is the only intentional implementer freedom). Every other step has full code.

**Type consistency:** `Aggregate { kind, inner, group_vars, output_var }` used identically in types.rs, parse_aggregate, evaluate_rule_with_aggregates, and tests. `AggregateKind::Count` only.

**Out-of-scope (per spec):** sum/min/max/avg, multi-rule recursion through aggregation, aggregation over derived predicates whose derivations haven't completed yet.
