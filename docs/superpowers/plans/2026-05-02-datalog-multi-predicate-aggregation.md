# Datalog Multi-Predicate Aggregation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Extend the v1 `count(...)` aggregate to accept N≥2 inner atoms (a conjunction) with bidirectional binding flow, plus a stratification analyzer that catches recursion through aggregation across the full rule set.

**Architecture:** Add `inner_conjunction: Vec<Atom>` to `Aggregate` (legacy `inner: Atom` field stays populated to the first atom for backward-compat). Add `pub fn stratify` (Tarjan SCC over a predicate dep graph with Plain/Aggregate edge labels) and rework `evaluate` into stratum-by-stratum fixpoint. Generalize `count_inner_matches` to backtracking unification over the atom list.

**Tech Stack:** Rust 2024, no new deps. Target crate: `ferrosa-memory-core`.

**Spec:** `docs/superpowers/specs/2026-05-02-datalog-multi-predicate-aggregation-design.md`

**Branch:** `feat/datalog-filter-grammar` (continue; same branch holds filter-grammar + v1 aggregation + this v2 work).

---

## File Structure

| Action | Path | Responsibility |
|--------|------|----------------|
| Modify | `crates/ferrosa-memory-core/src/types.rs` | Add `inner_conjunction: Vec<Atom>` (with `#[serde(default)]`) to `Aggregate`; add `StratifyError` enum. |
| Modify | `crates/ferrosa-memory-core/src/datalog.rs` | Generalize `parse_aggregate`; add `stratify`; rewrite `evaluate` to stratum-by-stratum; extend `count_inner_matches`/conjunction backtracking. |

Non-goals for this plan: no new module files, no public API additions beyond `pub fn stratify` + `pub enum StratifyError`.

---

## Task M1: Extend `Aggregate` AST + `StratifyError`

**Files:** `crates/ferrosa-memory-core/src/types.rs`

- [ ] **Step 1: Failing serde tests**

Append to `types::tests`:

```rust
#[test]
fn aggregate_v2_round_trips_through_json() {
    let a = Aggregate {
        kind: AggregateKind::Count,
        inner: Atom {
            predicate: "worked_well".into(),
            args: vec![Term::Var("S".into()), Term::Var("Tool".into())],
        },
        inner_conjunction: vec![
            Atom {
                predicate: "worked_well".into(),
                args: vec![Term::Var("S".into()), Term::Var("Tool".into())],
            },
            Atom {
                predicate: "session_context".into(),
                args: vec![Term::Var("S".into()), Term::Var("Ctx".into())],
            },
        ],
        group_vars: vec!["Ctx".into(), "Tool".into()],
        output_var: "N".into(),
    };
    let json = serde_json::to_string(&a).unwrap();
    let back: Aggregate = serde_json::from_str(&json).unwrap();
    assert_eq!(back, a);
}

#[test]
fn aggregate_v1_legacy_deserializes_with_empty_conjunction() {
    // JSON shaped without `inner_conjunction` (the v1 wire format) must
    // deserialize cleanly with the field defaulted to vec![].
    let json = r#"{
        "kind": "Count",
        "inner": {"predicate": "user_corrected", "args": [{"type":"Var","value":"S"},{"type":"Var","value":"X"}]},
        "group_vars": ["X"],
        "output_var": "N"
    }"#;
    let agg: Aggregate = serde_json::from_str(json).unwrap();
    assert!(agg.inner_conjunction.is_empty());
    assert_eq!(agg.inner.predicate, "user_corrected");
}

#[test]
fn stratify_error_round_trips() {
    let e = StratifyError::RecursionThroughAggregate { cycle: vec!["a".into(), "b".into()] };
    let json = serde_json::to_string(&e).unwrap();
    let back: StratifyError = serde_json::from_str(&json).unwrap();
    assert_eq!(back, e);
}
```

(If the existing test JSON for `Term::Var` uses a different shape than `{"type":"Var","value":"S"}`, look at the existing `datalog_rule_without_aggregates_field_deserializes_with_default` test in the same module — copy its `Term::Var` wire shape verbatim. The exact tag is not load-bearing; what matters is the new field defaults to `vec![]`.)

Run: `cargo test --package ferrosa-memory-core --lib types::tests::aggregate_v2_round_trips_through_json types::tests::aggregate_v1_legacy_deserializes_with_empty_conjunction types::tests::stratify_error_round_trips`
Expected: FAIL — `inner_conjunction` and `StratifyError` don't exist.

- [ ] **Step 2: Add the field and the error**

In `types.rs`, replace the existing `Aggregate` struct with:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub kind: AggregateKind,
    pub inner: Atom,
    #[serde(default)]
    pub inner_conjunction: Vec<Atom>,
    pub group_vars: Vec<String>,
    pub output_var: String,
}
```

Add next to `Aggregate`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StratifyError {
    RecursionThroughAggregate { cycle: Vec<String> },
}
```

- [ ] **Step 3: Update existing constructors**

`cargo build --package ferrosa-memory-core --lib` — find and fix any `Aggregate { ... }` literal that's missing `inner_conjunction: Vec::new(),`. The likely sites are `parse_aggregate` and any v1 tests that build `Aggregate` directly.

- [ ] **Step 4: Run new tests, must pass**

Run: `cargo test --package ferrosa-memory-core --lib types::tests::aggregate_v2_round_trips_through_json types::tests::aggregate_v1_legacy_deserializes_with_empty_conjunction types::tests::stratify_error_round_trips`
Expected: PASS.

- [ ] **Step 5: Full library suite, no regressions**

Run: `cargo test --package ferrosa-memory-core --lib`
Expected: 700 + 3 new = 703 passed, 0 failed, 0 ignored.

- [ ] **Step 6: Commit**

```bash
git add crates/ferrosa-memory-core/src/types.rs crates/ferrosa-memory-core/src/datalog.rs
git commit -m "types(datalog): add Aggregate.inner_conjunction + StratifyError"
```

---

## Task M2: `parse_aggregate` accepts N≥2 atoms

**Files:** `crates/ferrosa-memory-core/src/datalog.rs`

- [ ] **Step 1: Failing parser tests**

Append to `datalog::tests`:

```rust
#[test]
fn parse_rule_supports_two_atom_conjunction() {
    use crate::types::AggregateKind;
    let rule = parse_rule(
        "preferred_tool(Ctx, Tool) :- count(worked_well(S, Tool), session_context(S, Ctx), N), N >= 3."
    ).unwrap();
    assert_eq!(rule.aggregates.len(), 1);
    let agg = &rule.aggregates[0];
    assert_eq!(agg.kind, AggregateKind::Count);
    assert_eq!(agg.inner_conjunction.len(), 2);
    assert_eq!(agg.inner_conjunction[0].predicate, "worked_well");
    assert_eq!(agg.inner_conjunction[1].predicate, "session_context");
    // For backward-compat, `inner` is set to the first atom of the conjunction.
    assert_eq!(agg.inner.predicate, "worked_well");
    // group_vars: vars in the conjunction that also appear in the head.
    // Ctx and Tool both appear in head and in the conjunction; S only in conjunction.
    let mut sorted_groups = agg.group_vars.clone();
    sorted_groups.sort();
    assert_eq!(sorted_groups, vec!["Ctx".to_string(), "Tool".to_string()]);
    assert_eq!(agg.output_var, "N");
}

#[test]
fn parse_rule_rejects_aggregate_with_no_atoms() {
    // count(N) — only the output var, no atoms.
    let err = parse_rule("foo(X) :- count(N).").unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("inner atom") || msg.contains("at least"),
        "expected at-least-one-atom error, got: {msg}"
    );
}

#[test]
fn parse_rule_single_atom_aggregate_keeps_v1_shape() {
    // v1 single-atom path: inner_conjunction must stay empty so v1
    // deserializers see the original shape.
    let rule = parse_rule("foo(X) :- count(bar(X), N), N > 0.").unwrap();
    let agg = &rule.aggregates[0];
    assert!(agg.inner_conjunction.is_empty());
    assert_eq!(agg.inner.predicate, "bar");
}
```

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::parse_rule_supports_two_atom_conjunction datalog::tests::parse_rule_rejects_aggregate_with_no_atoms datalog::tests::parse_rule_single_atom_aggregate_keeps_v1_shape`
Expected: FAIL.

- [ ] **Step 2: Generalize `parse_aggregate`**

Open `datalog.rs`, find the existing `parse_aggregate` function (it currently splits into 2 parts: 1 atom + output_var). Replace its body with:

```rust
fn parse_aggregate(
    s: &str,
    head_vars: &std::collections::HashSet<String>,
    body_vars: &std::collections::HashSet<String>,
) -> anyhow::Result<Option<crate::types::Aggregate>> {
    let s = s.trim();
    let Some(rest) = s.strip_prefix("count(") else {
        return Ok(None);
    };
    let Some(inner_text) = rest.strip_suffix(')') else {
        anyhow::bail!("aggregate '{s}' is missing closing ')'");
    };
    let parts = split_top_level(inner_text, ',')?;
    if parts.len() < 2 {
        anyhow::bail!("aggregate '{s}' must have at least one inner atom and an output var separated by ','");
    }
    let output = parts.last().unwrap().trim().to_string();
    if !output
        .chars()
        .next()
        .map(|c| c.is_ascii_uppercase() || c == '_')
        .unwrap_or(false)
    {
        anyhow::bail!("aggregate output_var '{output}' must be a variable (start with uppercase or '_')");
    }

    let atom_parts = &parts[..parts.len() - 1];
    let mut atoms: Vec<Atom> = Vec::with_capacity(atom_parts.len());
    let mut anon = 0;
    for (i, part) in atom_parts.iter().enumerate() {
        match parse_atom(part, &mut anon) {
            Ok(atom) => atoms.push(atom),
            Err(_) if atoms.is_empty() => {
                // First arg isn't a compound atom — fall through to body-atom
                // parsing (preserves the v1 escape hatch where `count(X, N)`
                // is a plain 2-arg predicate named `count`, not an aggregate).
                return Ok(None);
            }
            Err(e) => {
                anyhow::bail!("aggregate '{s}' atom #{} is malformed: {e}", i + 1);
            }
        }
    }

    let inner = atoms[0].clone();
    let inner_conjunction = if atoms.len() == 1 { Vec::new() } else { atoms.clone() };

    // group_vars: variables in the conjunction that ALSO appear in the
    // head OR in any non-aggregate body atom of the same rule.
    let mut group_vars: Vec<String> = Vec::new();
    let scope = atoms.as_slice();
    for atom in scope {
        for arg in &atom.args {
            if let crate::types::Term::Var(name) = arg {
                if (head_vars.contains(name) || body_vars.contains(name))
                    && !group_vars.contains(name)
                {
                    group_vars.push(name.clone());
                }
            }
        }
    }

    Ok(Some(crate::types::Aggregate {
        kind: crate::types::AggregateKind::Count,
        inner,
        inner_conjunction,
        group_vars,
        output_var: output,
    }))
}
```

- [ ] **Step 3: Wire `body_vars` into `parse_rule`**

`parse_aggregate` now takes `body_vars` as well as `head_vars`. The existing `parse_rule` only computes `head_vars` and runs the body loop in one pass — but `body_vars` requires having already parsed the non-aggregate body atoms.

Refactor `parse_rule` into a two-pass body loop:

1. **Pass 1**: split body parts; classify each as `aggregate-shape` (starts with `count(` and the inner first-arg looks like a compound atom — defer detection to `parse_aggregate` Step 2's heuristic), `filter-shape` (`has_top_level_cmp`), or `atom-shape`. Parse only the filter and atom shapes now; collect aggregate parts as `Vec<&str>` for Pass 2.
2. **Compute `body_vars`** by walking the parsed body atoms and collecting `Term::Var` names.
3. **Pass 2**: parse each aggregate part with both `head_vars` and `body_vars`.

This is a structural change in `parse_rule`. The existing dispatch order (aggregate before filter before atom) is preserved at the *output* level, but the *parsing order* now does atoms+filters first, then aggregates.

Implementation sketch:

```rust
// before the existing loop
let head_vars: std::collections::HashSet<String> = head
    .args
    .iter()
    .filter_map(|t| match t {
        Term::Var(name) => Some(name.clone()),
        _ => None,
    })
    .collect();

let mut deferred_aggregates: Vec<String> = Vec::new();
for part in &body_parts {
    let part = part.trim();
    if part.trim_start().starts_with("count(") {
        deferred_aggregates.push(part.to_string());
    } else if has_top_level_cmp(part) {
        let f = crate::datalog_filter_expr::parse_filter(part)?;
        filters.push(f);
    } else {
        body.push(parse_atom(part, &mut anon_counter)?);
    }
}

let body_vars: std::collections::HashSet<String> = body
    .iter()
    .flat_map(|a| a.args.iter())
    .filter_map(|t| match t {
        Term::Var(name) => Some(name.clone()),
        _ => None,
    })
    .collect();

for part in &deferred_aggregates {
    if let Some(agg) = parse_aggregate(part, &head_vars, &body_vars)? {
        aggregates.push(agg);
    } else {
        // parse_aggregate fell through (the `count(X, N)` legacy escape).
        // Treat the part as a regular atom.
        body.push(parse_atom(part, &mut anon_counter)?);
    }
}
```

The existing intra-rule head-recursion guard (the `for agg in &aggregates { if agg.inner.predicate == head.predicate ... }` loop) must be **deleted** here — Task M3's stratification analyzer subsumes it.

- [ ] **Step 4: Update v1 test that asserted parse-time recursion error**

Find `parse_rule_rejects_intra_rule_recursion_through_count`. Either delete it or rewrite to assert the new behavior:

```rust
#[test]
fn intra_rule_recursion_through_count_now_rejected_at_evaluate_time() {
    // The v1 parse-time guard was removed in favour of the stratify
    // analyzer (which catches cross-rule recursion too). The rule now
    // parses cleanly; `evaluate` will reject it.
    let rule = parse_rule("loop(X) :- count(loop(Y), N), N > 0.").unwrap();
    assert_eq!(rule.aggregates.len(), 1);
    // The actual reject-at-evaluate-time test lives in
    // `acceptance_recursion_rejected_at_load_time` — see Task M5.
}
```

Keep it as a marker so the contract is documented in the parser tests too.

- [ ] **Step 5: Run new tests + full datalog suite**

`cargo test --package ferrosa-memory-core --lib datalog::tests::parse_rule_supports_two_atom_conjunction datalog::tests::parse_rule_rejects_aggregate_with_no_atoms datalog::tests::parse_rule_single_atom_aggregate_keeps_v1_shape datalog::tests::intra_rule_recursion_through_count_now_rejected_at_evaluate_time`
All four must PASS.

`cargo test --package ferrosa-memory-core --lib datalog::tests`
All previously-passing tests + the four new ones. No regressions; old aggregation tests still green.

- [ ] **Step 6: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs
git commit -m "feat(datalog): parse N-atom count(...) conjunction; remove v1 recursion guard"
```

---

## Task M3: Stratification analyzer

**Files:** `crates/ferrosa-memory-core/src/datalog.rs`

- [ ] **Step 1: Failing tests**

Append to `datalog::tests`:

```rust
#[test]
fn stratify_simple_chain_assigns_ascending_strata() {
    let r1 = parse_rule("b(X) :- a(X).").unwrap();
    let r2 = parse_rule("c(X) :- b(X).").unwrap();
    let strata = stratify(&[r1, r2]).unwrap();
    // Expect at least 2 strata; r1 (head=b) before r2 (head=c).
    assert!(strata.len() >= 2);
    let r1_stratum = strata.iter().position(|s| s.contains(&0)).unwrap();
    let r2_stratum = strata.iter().position(|s| s.contains(&1)).unwrap();
    assert!(r1_stratum < r2_stratum, "b's rule must come before c's rule");
}

#[test]
fn stratify_aggregate_lifts_one_level() {
    let r = parse_rule("b(X) :- count(a(X), N), N > 0.").unwrap();
    let strata = stratify(&[r]).unwrap();
    // The single rule must end up at stratum >= 1 (because its only
    // dependency is via an Aggregate edge to `a`, which forces a lift).
    // We don't observe `a` directly because it has no rule; we just
    // verify the rule index 0 is in some stratum.
    assert!(strata.iter().any(|s| s.contains(&0)));
}

#[test]
fn stratify_rejects_intra_rule_recursion_through_aggregate() {
    let r = parse_rule("loop(X) :- count(loop(Y), N), N > 0.").unwrap();
    let err = stratify(&[r]).unwrap_err();
    match err {
        crate::types::StratifyError::RecursionThroughAggregate { cycle } => {
            assert!(cycle.contains(&"loop".to_string()));
        }
    }
}

#[test]
fn stratify_rejects_cross_rule_recursion_through_aggregate() {
    let r1 = parse_rule("a(X) :- b(X).").unwrap();
    let r2 = parse_rule("b(X) :- count(a(Y), N), N > 0.").unwrap();
    let err = stratify(&[r1, r2]).unwrap_err();
    match err {
        crate::types::StratifyError::RecursionThroughAggregate { cycle } => {
            assert!(cycle.iter().any(|c| c == "a"));
            assert!(cycle.iter().any(|c| c == "b"));
        }
    }
}

#[test]
fn stratify_allows_plain_recursion() {
    // path(X, Z) :- edge(X, Y), path(Y, Z). is recursive but not through
    // any aggregate; must accept.
    let r = parse_rule("path(X, Z) :- edge(X, Y), path(Y, Z).").unwrap();
    let strata = stratify(&[r]).unwrap();
    assert!(strata.iter().any(|s| s.contains(&0)));
}
```

Run: all five must FAIL because `stratify` doesn't exist.

- [ ] **Step 2: Implement `stratify`**

Add at the bottom of `datalog.rs` (or near `evaluate`):

```rust
/// Compute strata over a rule set.
///
/// Returns `Err(StratifyError::RecursionThroughAggregate { cycle })` iff
/// the predicate dependency graph has a strongly-connected component
/// that contains an Aggregate-labelled edge — i.e. some predicate's
/// derivation transitively requires aggregating over its own (or a
/// peer's) result.
///
/// On success, returns rule indices grouped by ascending stratum. Rules
/// in the same stratum can be evaluated together; later strata can read
/// earlier strata's derived facts as if they were base facts.
pub fn stratify(
    rules: &[crate::types::DatalogRule],
) -> Result<Vec<Vec<usize>>, crate::types::StratifyError> {
    use std::collections::{HashMap, HashSet};

    // Edge label: each edge is either Plain (plain body atom) or
    // Aggregate (atom inside an aggregate's inner_conjunction).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Edge {
        Plain,
        Aggregate,
    }

    // Predicate dependency graph: head_predicate -> Vec<(target_predicate, Edge)>.
    let mut graph: HashMap<String, Vec<(String, Edge)>> = HashMap::new();
    let mut all_preds: HashSet<String> = HashSet::new();

    for rule in rules {
        let head = rule.head.predicate.clone();
        all_preds.insert(head.clone());
        let entry = graph.entry(head.clone()).or_default();
        for atom in &rule.body {
            entry.push((atom.predicate.clone(), Edge::Plain));
            all_preds.insert(atom.predicate.clone());
        }
        for agg in &rule.aggregates {
            let atoms: Vec<&Atom> = if agg.inner_conjunction.is_empty() {
                vec![&agg.inner]
            } else {
                agg.inner_conjunction.iter().collect()
            };
            for atom in atoms {
                entry.push((atom.predicate.clone(), Edge::Aggregate));
                all_preds.insert(atom.predicate.clone());
            }
        }
    }

    // Tarjan's SCC over the predicate graph.
    // Standard implementation; nodes are predicate names.
    let mut index_counter: usize = 0;
    let mut stack: Vec<String> = Vec::new();
    let mut on_stack: HashSet<String> = HashSet::new();
    let mut indices: HashMap<String, usize> = HashMap::new();
    let mut lowlinks: HashMap<String, usize> = HashMap::new();
    let mut sccs: Vec<Vec<String>> = Vec::new();

    fn strongconnect(
        node: &str,
        graph: &HashMap<String, Vec<(String, Edge)>>,
        index_counter: &mut usize,
        stack: &mut Vec<String>,
        on_stack: &mut HashSet<String>,
        indices: &mut HashMap<String, usize>,
        lowlinks: &mut HashMap<String, usize>,
        sccs: &mut Vec<Vec<String>>,
    ) {
        indices.insert(node.to_string(), *index_counter);
        lowlinks.insert(node.to_string(), *index_counter);
        *index_counter += 1;
        stack.push(node.to_string());
        on_stack.insert(node.to_string());

        if let Some(succs) = graph.get(node) {
            for (succ, _) in succs {
                if !indices.contains_key(succ) {
                    strongconnect(succ, graph, index_counter, stack, on_stack, indices, lowlinks, sccs);
                    let succ_low = *lowlinks.get(succ).unwrap();
                    let cur = lowlinks.get_mut(node).unwrap();
                    if succ_low < *cur {
                        *cur = succ_low;
                    }
                } else if on_stack.contains(succ) {
                    let succ_idx = *indices.get(succ).unwrap();
                    let cur = lowlinks.get_mut(node).unwrap();
                    if succ_idx < *cur {
                        *cur = succ_idx;
                    }
                }
            }
        }

        if lowlinks.get(node) == indices.get(node) {
            let mut scc = Vec::new();
            while let Some(w) = stack.pop() {
                on_stack.remove(&w);
                let done = w == node;
                scc.push(w);
                if done {
                    break;
                }
            }
            sccs.push(scc);
        }
    }

    for pred in all_preds.iter().cloned().collect::<Vec<_>>() {
        if !indices.contains_key(&pred) {
            strongconnect(
                &pred,
                &graph,
                &mut index_counter,
                &mut stack,
                &mut on_stack,
                &mut indices,
                &mut lowlinks,
                &mut sccs,
            );
        }
    }

    // Reject any SCC that contains an Aggregate edge.
    for scc in &sccs {
        let scc_set: HashSet<&str> = scc.iter().map(String::as_str).collect();
        // Self-edges through aggregate (size-1 SCC linking back to itself
        // via Aggregate) and multi-node aggregate cycles are both errors.
        for node in scc {
            if let Some(succs) = graph.get(node) {
                for (succ, edge) in succs {
                    if scc_set.contains(succ.as_str()) && *edge == Edge::Aggregate {
                        return Err(crate::types::StratifyError::RecursionThroughAggregate {
                            cycle: scc.clone(),
                        });
                    }
                }
            }
        }
    }

    // Assign strata: condense to a DAG of SCC indices, topological sort.
    let mut node_to_scc: HashMap<String, usize> = HashMap::new();
    for (i, scc) in sccs.iter().enumerate() {
        for node in scc {
            node_to_scc.insert(node.clone(), i);
        }
    }

    // Stratum per SCC.
    let mut scc_stratum: HashMap<usize, usize> = HashMap::new();
    // Iterate in reverse-topological order produced by Tarjan (already
    // ordered: leaves first, then ancestors). Tarjan emits SCCs in
    // reverse topological order, so iterate sccs.iter().enumerate() to
    // get leaves first.
    for (i, scc) in sccs.iter().enumerate() {
        let mut max_dep_stratum: i64 = -1;
        let mut had_aggregate_edge = false;
        for node in scc {
            if let Some(succs) = graph.get(node) {
                for (succ, edge) in succs {
                    let succ_scc = *node_to_scc.get(succ).unwrap();
                    if succ_scc != i {
                        let s = *scc_stratum.get(&succ_scc).unwrap_or(&0) as i64;
                        if s > max_dep_stratum {
                            max_dep_stratum = s;
                        }
                        if *edge == Edge::Aggregate {
                            had_aggregate_edge = true;
                        }
                    }
                }
            }
        }
        let lift = if had_aggregate_edge { 1 } else { 0 };
        let stratum = if max_dep_stratum < 0 {
            0
        } else {
            (max_dep_stratum as usize) + lift
        };
        scc_stratum.insert(i, stratum);
    }

    // Group rule indices by their head predicate's stratum.
    let mut by_stratum: std::collections::BTreeMap<usize, Vec<usize>> = std::collections::BTreeMap::new();
    for (rule_idx, rule) in rules.iter().enumerate() {
        let scc_idx = *node_to_scc.get(&rule.head.predicate).unwrap_or(&0);
        let stratum = *scc_stratum.get(&scc_idx).unwrap_or(&0);
        by_stratum.entry(stratum).or_default().push(rule_idx);
    }

    Ok(by_stratum.into_values().collect())
}
```

> **Implementer note:** Rust's borrow checker may require splitting the recursive `strongconnect` into an iterative version or using `unsafe`-free shared references. If recursion proves awkward with the mutable HashMaps, convert to the iterative-stack form of Tarjan's algorithm (well-documented online). The semantics are what matters; the test cases are agnostic to the implementation strategy.

- [ ] **Step 3: Run stratify tests**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::stratify_simple_chain_assigns_ascending_strata datalog::tests::stratify_aggregate_lifts_one_level datalog::tests::stratify_rejects_intra_rule_recursion_through_aggregate datalog::tests::stratify_rejects_cross_rule_recursion_through_aggregate datalog::tests::stratify_allows_plain_recursion`
Expected: PASS.

- [ ] **Step 4: Full datalog suite (regression check)**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests`
Expected: all previously-passing tests remain green.

- [ ] **Step 5: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs
git commit -m "feat(datalog): add stratify analyzer (Tarjan SCC + Plain/Aggregate edge labels)"
```

---

## Task M4: Stratum-by-stratum evaluator + conjunction backtracking

**Files:** `crates/ferrosa-memory-core/src/datalog.rs`

- [ ] **Step 1: Failing tests for the conjunction evaluator**

Append to `datalog::tests`:

```rust
#[test]
fn evaluator_two_atom_conjunction_groups_correctly() {
    // Three (Ctx, Tool) groupings:
    //   (cA, t1): sessions s1, s2, s3 -> count = 3 -> avoid_action fires
    //   (cA, t2): sessions s1, s2     -> count = 2 -> does NOT fire
    //   (cB, t1): sessions s1, s4     -> count = 2 -> does NOT fire
    let s1 = Uuid::new_v4();
    let s2 = Uuid::new_v4();
    let s3 = Uuid::new_v4();
    let s4 = Uuid::new_v4();
    let ca = Uuid::new_v4();
    let cb = Uuid::new_v4();
    let t1 = Uuid::new_v4();
    let t2 = Uuid::new_v4();

    let mut facts = FactSet::new();
    // worked_well(Session, Tool)
    facts.insert("worked_well", vec![Term::Const(s1), Term::Const(t1)]);
    facts.insert("worked_well", vec![Term::Const(s2), Term::Const(t1)]);
    facts.insert("worked_well", vec![Term::Const(s3), Term::Const(t1)]);
    facts.insert("worked_well", vec![Term::Const(s1), Term::Const(t2)]);
    facts.insert("worked_well", vec![Term::Const(s2), Term::Const(t2)]);
    facts.insert("worked_well", vec![Term::Const(s4), Term::Const(t1)]);
    // session_context(Session, Ctx)
    facts.insert("session_context", vec![Term::Const(s1), Term::Const(ca)]);
    facts.insert("session_context", vec![Term::Const(s2), Term::Const(ca)]);
    facts.insert("session_context", vec![Term::Const(s3), Term::Const(ca)]);
    facts.insert("session_context", vec![Term::Const(s1), Term::Const(cb)]);
    facts.insert("session_context", vec![Term::Const(s4), Term::Const(cb)]);

    let rule = parse_rule(
        "preferred_tool(Ctx, Tool) :- count(worked_well(S, Tool), session_context(S, Ctx), N), N >= 3."
    ).unwrap();
    let (derived, _) = evaluate(&[rule], &facts, 100, 1000);

    let pairs: std::collections::HashSet<(Uuid, Uuid)> = derived
        .get("preferred_tool")
        .into_iter()
        .flatten()
        .filter_map(|args| {
            let (Term::Const(c), Term::Const(t)) = (args.first()?, args.get(1)?) else { return None; };
            Some((*c, *t))
        })
        .collect();

    assert!(pairs.contains(&(ca, t1)), "(cA, t1) with 3 distinct sessions must fire");
    assert!(!pairs.contains(&(ca, t2)), "(cA, t2) with 2 sessions must NOT fire");
    assert!(!pairs.contains(&(cb, t1)), "(cB, t1) with 2 sessions must NOT fire");
}

#[test]
fn evaluator_existential_var_aggregated_over() {
    // S only appears in the conjunction (not in head, not in non-aggregate
    // body). Distinct S values per group key are enumerated; S itself
    // does not appear in the derived `preferred_tool` facts.
    let s1 = Uuid::new_v4();
    let s2 = Uuid::new_v4();
    let s3 = Uuid::new_v4();
    let ca = Uuid::new_v4();
    let t1 = Uuid::new_v4();

    let mut facts = FactSet::new();
    for s in [s1, s2, s3] {
        facts.insert("worked_well", vec![Term::Const(s), Term::Const(t1)]);
        facts.insert("session_context", vec![Term::Const(s), Term::Const(ca)]);
    }

    let rule = parse_rule(
        "preferred_tool(Ctx, Tool) :- count(worked_well(S, Tool), session_context(S, Ctx), N), N >= 3."
    ).unwrap();
    let (derived, _) = evaluate(&[rule], &facts, 100, 1000);

    let row = derived.get("preferred_tool").into_iter().flatten().next();
    let row = row.expect("expected one preferred_tool fact");
    // Head has 2 args (Ctx, Tool); S must NOT appear.
    assert_eq!(row.len(), 2);
}

#[test]
fn evaluator_recursion_through_aggregate_emits_warn_and_no_facts() {
    let rule = parse_rule("loop(X) :- count(loop(Y), N), N > 0.").unwrap();
    let mut facts = FactSet::new();
    let x = Uuid::new_v4();
    facts.insert("loop", vec![Term::Const(x)]);

    let (derived, _) = evaluate(&[rule], &facts, 100, 1000);
    // The rule must derive nothing; loop(x) is preserved as a base fact.
    // (Verifying the warn fires would require a tracing-test fixture;
    // for now, verifying the no-derivation property is sufficient.)
    let derived_loop_count = derived.get("loop").map(|v| v.len()).unwrap_or(0);
    assert_eq!(derived_loop_count, 1, "stratification rejection must leave only the base fact");
}
```

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::evaluator_two_atom_conjunction_groups_correctly datalog::tests::evaluator_existential_var_aggregated_over datalog::tests::evaluator_recursion_through_aggregate_emits_warn_and_no_facts`
Expected: FAIL — `count_inner_matches` only handles single-atom; `evaluate` doesn't yet call `stratify`.

- [ ] **Step 2: Generalize `count_inner_matches`**

In `datalog.rs`, locate the existing `count_inner_matches`. Replace it with:

```rust
fn count_inner_matches(
    agg: &crate::types::Aggregate,
    binding: &std::collections::HashMap<String, Term>,
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
    binding: std::collections::HashMap<String, Term>,
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

Update callers if any (`apply_aggregate` already calls `count_inner_matches`; signature unchanged).

- [ ] **Step 3: Update `seed_bindings_from_inner` for the conjunction case**

Find `seed_bindings_from_inner`. Today it enumerates rows of `agg.inner` to seed group_var bindings. Update to use the first atom of the conjunction (which is also `agg.inner` per Task M2's parser invariant):

```rust
// At the top of seed_bindings_from_inner:
let first_atom = if agg.inner_conjunction.is_empty() {
    &agg.inner
} else {
    &agg.inner_conjunction[0]
};
// (Replace any existing reference to `agg.inner` in this function with `first_atom`.)
```

- [ ] **Step 4: Replace `evaluate` with stratum-by-stratum loop**

Find `pub fn evaluate(...)`. Replace its body with:

```rust
pub fn evaluate(
    rules: &[crate::types::DatalogRule],
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
        let stratum_rules: Vec<crate::types::DatalogRule> =
            stratum_idxs.iter().map(|i| rules[*i].clone()).collect();
        let (next_facts, next_derived) =
            evaluate_stratum(&stratum_rules, &all_facts, &mut budget_iter, &mut budget_facts);
        all_facts = next_facts;
        derived.extend(next_derived);
        if budget_iter == 0 || budget_facts == 0 {
            break;
        }
    }
    (all_facts, derived)
}
```

Add `evaluate_stratum`. The simplest path is: rename the previous `evaluate` body to `evaluate_stratum`, change its parameters to take `&mut max_iterations, &mut max_facts`, and decrement them as the existing fixpoint loop runs (the inner loop already counts iterations; surface that count to the parent via `*max_iterations -= consumed_iter` on each call).

- [ ] **Step 5: Run new tests**

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::evaluator_two_atom_conjunction_groups_correctly datalog::tests::evaluator_existential_var_aggregated_over datalog::tests::evaluator_recursion_through_aggregate_emits_warn_and_no_facts`
Expected: PASS.

- [ ] **Step 6: Full datalog suite + clippy**

`cargo test --package ferrosa-memory-core --lib datalog::tests` — all green.
`cargo clippy --package ferrosa-memory-core --lib --message-format=short -- -D warnings` — clean.

- [ ] **Step 7: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs
git commit -m "feat(datalog): stratum-by-stratum evaluator + conjunction backtracking"
```

---

## Task M5: Acceptance tests + work-item promotion

**Files:** `crates/ferrosa-memory-core/src/datalog.rs`, `specs/in-process/feat-multi-predicate-preaggregation.md`

- [ ] **Step 1: Acceptance tests matching the work item's criteria**

Append to `datalog::tests`:

```rust
#[test]
fn acceptance_threshold_K_eq_3() {
    // From specs/in-process/feat-multi-predicate-preaggregation.md:
    // "A learning or user rule can express: count(atom1, atom2, ..., N), N >= K"
    let rule = parse_rule(
        "preferred_tool(Ctx, Tool) :- count(worked_well(S, Tool), session_context(S, Ctx), N), N >= 3."
    ).unwrap();
    assert_eq!(rule.aggregates.len(), 1);
    assert_eq!(rule.aggregates[0].inner_conjunction.len(), 2);
}

#[test]
fn acceptance_existential_quantification() {
    // The aggregate groups by variables shared between the outer rule
    // head and the inner conjunction atoms. S is only in conjunction;
    // it must not appear in group_vars.
    let rule = parse_rule(
        "preferred_tool(Ctx, Tool) :- count(worked_well(S, Tool), session_context(S, Ctx), N), N >= 3."
    ).unwrap();
    let agg = &rule.aggregates[0];
    assert!(!agg.group_vars.contains(&"S".to_string()), "S must be existentially quantified");
    assert!(agg.group_vars.contains(&"Ctx".to_string()));
    assert!(agg.group_vars.contains(&"Tool".to_string()));
}

#[test]
fn acceptance_recursion_rejected_at_load_time() {
    // "Recursion through the aggregate is rejected at parse time" —
    // updated for v2: rejected at evaluate time via stratification.
    // The failure mode is: no derivations + warn emitted.
    let rule = parse_rule("loop(X) :- count(loop(Y), N), N > 0.").unwrap();
    let mut facts = FactSet::new();
    let x = Uuid::new_v4();
    facts.insert("loop", vec![Term::Const(x)]);
    let (derived, derived_log) = evaluate(&[rule], &facts, 100, 1000);
    // No new derivations beyond the seeded base fact.
    assert_eq!(derived.get("loop").map(|v| v.len()).unwrap_or(0), 1);
    assert!(derived_log.is_empty(), "no facts should be derived from an unstratifiable rule set");
}

#[test]
fn acceptance_no_regression_on_v1_aggregation() {
    // From the work item: "No regression to existing single-predicate
    // aggregate behavior". The v1 user-supplied test is the canonical
    // assertion — re-run it inline as a fresh isolation check.
    let s1 = Uuid::new_v4();
    let s2 = Uuid::new_v4();
    let s3 = Uuid::new_v4();
    let target = Uuid::new_v4();
    let mut facts = FactSet::new();
    facts.insert("user_corrected", vec![Term::Const(s1), Term::Const(target)]);
    facts.insert("user_corrected", vec![Term::Const(s2), Term::Const(target)]);
    facts.insert("user_corrected", vec![Term::Const(s3), Term::Const(target)]);
    let rule = parse_rule("avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.").unwrap();
    assert!(rule.aggregates[0].inner_conjunction.is_empty(), "v1 single-atom path");
    let (derived, _) = evaluate(&[rule], &facts, 100, 1000);
    let any = derived.get("avoid_action").map(|v| v.len()).unwrap_or(0);
    assert_eq!(any, 1, "v1 single-atom aggregate must still fire");
}
```

Run: `cargo test --package ferrosa-memory-core --lib datalog::tests::acceptance_threshold_K_eq_3 datalog::tests::acceptance_existential_quantification datalog::tests::acceptance_recursion_rejected_at_load_time datalog::tests::acceptance_no_regression_on_v1_aggregation`
All four must PASS.

- [ ] **Step 2: Promote the work item**

Edit `specs/in-process/feat-multi-predicate-preaggregation.md` and add an `## Implementation Notes` section at the bottom with:

```markdown
## Implementation Notes

- Implemented on branch `feat/datalog-filter-grammar` as part of the
  multi-predicate aggregation rollout.
- Spec: `docs/superpowers/specs/2026-05-02-datalog-multi-predicate-aggregation-design.md`
- Plan: `docs/superpowers/plans/2026-05-02-datalog-multi-predicate-aggregation.md`
- Acceptance tests: `crates/ferrosa-memory-core/src/datalog.rs::tests::acceptance_*`
- Single-atom v1 path preserved verbatim; multi-atom path activates
  when `inner_conjunction.len() >= 2`.
- Recursion-through-aggregate enforcement moved from parse time (v1
  intra-rule guard) to evaluate time via `stratify`. Cross-rule cycles
  are now also caught.
- `worked_in_context` (the FR's hypothesised join predicate) does not
  need to exist as a base predicate — the conjunction inside `count(...)`
  expresses the join inline.

implemented-by: subagent-driven-development on feat/datalog-filter-grammar
```

Then `git mv` the file:

```bash
git mv specs/in-process/feat-multi-predicate-preaggregation.md specs/implemented/
```

(The file moves to `implemented/` awaiting verification by a separate agent — per the work-item-pipeline rule, the implementer cannot self-verify.)

- [ ] **Step 3: Final verification**

`cargo test --package ferrosa-memory-core --lib` — print total counts.
`cargo clippy --package ferrosa-memory-core --lib -- -D warnings` — clean.
`cargo fmt --check` — clean.

If fmt fails, run `cargo fmt` and stage with the next commit.

- [ ] **Step 4: Commit**

```bash
git add crates/ferrosa-memory-core/src/datalog.rs specs/in-process/ specs/implemented/
git commit -m "test(datalog): acceptance tests for multi-predicate aggregation; promote work item"
```

---

## Self-Review

**Spec coverage:**

| Spec section | Implementing task |
|---|---|
| `Aggregate.inner_conjunction` field with `#[serde(default)]` | M1 |
| `StratifyError` enum | M1 |
| `parse_aggregate` accepts N≥2 atoms | M2 |
| Backward-compat `inner` populated to first atom | M2 |
| `group_vars` from head ∪ body, not just head | M2 |
| Removal of v1 intra-rule recursion guard | M2 |
| `parse_rule` two-pass body loop (atoms first, then aggregates) | M2 |
| `pub fn stratify` (Tarjan SCC + Plain/Aggregate edges) | M3 |
| Stratum lift on Aggregate edges | M3 |
| Cross-rule recursion rejection | M3 |
| `count_inner_matches` recursive backtracking over conjunction | M4 |
| `seed_bindings_from_inner` uses first atom of conjunction | M4 |
| Stratum-by-stratum `evaluate` | M4 |
| Fail-loud unstratifiable: warn + return without derivations | M4 |
| All four work-item acceptance criteria | M5 |
| Work item moved to `implemented/` with Implementation Notes | M5 |
| 700-test baseline holds (minus one rewritten parser test) | M2 step 4, verified end of M5 |

**Placeholder scan:** No "TBD"/"implement later" outside the explicitly-flagged Tarjan-iterative-vs-recursive implementer freedom in M3 step 2. Every other step has full code.

**Type consistency:** `Aggregate { kind, inner, inner_conjunction, group_vars, output_var }` used identically in types.rs, parse_aggregate, evaluate, stratify, and tests. `StratifyError::RecursionThroughAggregate { cycle: Vec<String> }` used identically in stratify and tests.

**Out-of-scope (per spec):** `sum`/`min`/`max`/`avg`, negation, multi-version-deploy gating flag, conjunction-binding memoization.
