//! Semi-naive Datalog evaluator with rule parsing, canonical fact extraction,
//! query-time derivation, and provenance tracking.
//!
//! The evaluator implements bottom-up fixpoint computation: starting from base
//! facts, rules fire repeatedly until no new facts are derived. The "semi-naive"
//! aspect is that each round only considers truly new facts from the previous
//! round, avoiding redundant re-derivation.
//!
//! ## Components
//!
//! - [`parse_rule`] — parse a Datalog rule from text
//! - [`evaluate`] — semi-naive fixpoint evaluator
//! - [`builtin_rules`] — default inference rules for the knowledge graph
//! - [`load_session_facts`] — load session data as canonical Datalog predicates
//! - [`query_predicate`] — query-time derivation with caching

use std::collections::HashMap;

use ordered_float::OrderedFloat;
use uuid::Uuid;

use crate::config::DatalogConfig;
use crate::storage::Storage;
use crate::types::{
    Atom, BuiltinFilter, DatalogRule, DerivedFact, FactSet, ProvenanceStep, RuleEntry, RuleState,
    TenantContext, Term,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    Builtin,
    Registry,
}

#[derive(Debug, Clone)]
pub struct EffectiveRuleEntry {
    pub source: RuleSource,
    pub entry: RuleEntry,
}

// ─── Rule Parser ──────────────────────────────────────────────────

/// Parse a Datalog rule from text.
///
/// Syntax: `head(args) :- body1(args), body2(args), X != Y.`
///
/// Argument conventions:
/// - Uppercase start -> `Term::Var`
/// - Starts with `"` -> `Term::ConstStr` (quotes stripped)
/// - `_` -> anonymous variable (renamed to `_anon_N` for uniqueness)
/// - Valid UUID -> `Term::Const`
/// - Otherwise -> `Term::ConstStr`
pub fn parse_rule(text: &str) -> anyhow::Result<DatalogRule> {
    let text = text.trim().trim_end_matches('.');
    let text = text.trim();

    let sep_pos = text
        .find(":-")
        .ok_or_else(|| anyhow::anyhow!("rule must contain ':-' separator"))?;

    let head_str = text[..sep_pos].trim();
    let body_str = text[sep_pos + 2..].trim();

    let head = parse_atom(head_str, &mut 0)?;

    let body_parts = split_top_level(body_str, ',')?;
    anyhow::ensure!(!body_parts.is_empty(), "rule body must not be empty");

    let head_vars: std::collections::HashSet<String> = head
        .args
        .iter()
        .filter_map(|t| match t {
            Term::Var(name) => Some(name.clone()),
            _ => None,
        })
        .collect();

    let mut body = Vec::new();
    let mut filters = Vec::new();
    let mut aggregates: Vec<crate::types::Aggregate> = Vec::new();
    let mut deferred_aggregates: Vec<String> = Vec::new();
    let mut deferred_negated: Vec<String> = Vec::new();
    let mut anon_counter = 0usize;

    // Pass 1: classify each body part; defer aggregate and negated parts for
    // Pass 2 (both need the positive body atoms parsed first — aggregates for
    // body_vars, negated atoms for the range-restriction check).
    for part in &body_parts {
        let part = part.trim();
        if let Some(rest) = strip_not_prefix(part) {
            deferred_negated.push(rest.to_string());
        } else if aggregate_keyword(part).is_some() {
            deferred_aggregates.push(part.to_string());
        } else if has_top_level_cmp(part) {
            let f = crate::datalog_filter_expr::parse_filter(part)?;
            filters.push(f);
        } else {
            body.push(parse_atom(part, &mut anon_counter)?);
        }
    }

    // Compute body_vars from the parsed non-aggregate body atoms.
    let body_vars: std::collections::HashSet<String> = body
        .iter()
        .flat_map(|a| a.args.iter())
        .filter_map(|t| match t {
            Term::Var(name) => Some(name.clone()),
            _ => None,
        })
        .collect();

    // Pass 2: parse deferred aggregate parts now that body_vars is known.
    for part in &deferred_aggregates {
        if let Some(agg) = parse_aggregate(part, &head_vars, &body_vars)? {
            aggregates.push(agg);
        } else {
            // parse_aggregate fell through (the `count(X, N)` legacy escape).
            // Treat the part as a regular body atom.
            body.push(parse_atom(part, &mut anon_counter)?);
        }
    }

    // A negated atom filters candidate bindings; it cannot generate them.
    // Without a positive atom there is nothing to range-restrict against and
    // nothing to filter, so reject before the generic emptiness check below.
    anyhow::ensure!(
        deferred_negated.is_empty() || !body.is_empty(),
        "a rule with a negated atom must have at least one positive body atom"
    );

    anyhow::ensure!(
        !body.is_empty() || !aggregates.is_empty(),
        "rule must have at least one body atom"
    );

    // Pass 3: parse negated atoms and enforce range restriction against the
    // final set of positive body atoms.
    let positive_vars: std::collections::HashSet<String> = body
        .iter()
        .flat_map(|a| a.args.iter())
        .filter_map(|t| match t {
            Term::Var(name) => Some(name.clone()),
            _ => None,
        })
        .collect();

    let mut negated = Vec::new();
    for part in &deferred_negated {
        let atom = parse_atom(part, &mut anon_counter)?;
        let unbound: Vec<&str> = atom
            .args
            .iter()
            .filter_map(|t| match t {
                // An anonymous variable is existential by construction: the
                // parser renames each `_` uniquely, so it can never be named
                // by the head or a filter, and checking it only asks whether
                // *some* row matches — never enumerating the universe.
                Term::Var(name) if !name.starts_with("_anon_") => Some(name.as_str()),
                _ => None,
            })
            .filter(|name| !positive_vars.contains(*name))
            .collect();
        anyhow::ensure!(
            unbound.is_empty(),
            "unsafe negation in `not {}`: variable(s) {} are not bound by any \
             positive body atom, so the rule would range over every value in \
             the store",
            part,
            unbound.join(", ")
        );
        negated.push(atom);
    }

    // Note: the v1 intra-rule recursion guard was removed here.
    // The stratify analyzer (Task M3) supersedes it and also catches
    // cross-rule recursion through aggregates.

    Ok(DatalogRule {
        head,
        body,
        filters,
        aggregates,
        negated,
    })
}

/// Strip a leading `not` keyword from a body part.
///
/// Requires whitespace after the keyword so a predicate whose name merely
/// begins with those letters — `nothing(X)` — stays a positive atom.
fn strip_not_prefix(part: &str) -> Option<&str> {
    let rest = part.strip_prefix("not")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    if rest.is_empty() { None } else { Some(rest) }
}

/// Split a string on a delimiter, but only at top level (not inside parentheses).
fn split_top_level(s: &str, delim: char) -> anyhow::Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0u32;

    for ch in s.chars() {
        if ch == '(' {
            depth += 1;
            current.push(ch);
        } else if ch == ')' {
            anyhow::ensure!(depth > 0, "unmatched closing parenthesis");
            depth -= 1;
            current.push(ch);
        } else if ch == delim && depth == 0 {
            parts.push(current.clone());
            current.clear();
        } else {
            current.push(ch);
        }
    }

    if !current.trim().is_empty() {
        parts.push(current);
    }

    Ok(parts)
}

/// Parse a single atom: `predicate(arg1, arg2, ...)`.
fn parse_atom(text: &str, anon_counter: &mut usize) -> anyhow::Result<Atom> {
    let text = text.trim();
    let open = text
        .find('(')
        .ok_or_else(|| anyhow::anyhow!("atom must contain '(' — got: {text}"))?;
    let close = text
        .rfind(')')
        .ok_or_else(|| anyhow::anyhow!("atom must contain ')' — got: {text}"))?;
    anyhow::ensure!(close > open, "malformed atom: {text}");

    let predicate = text[..open].trim().to_string();
    anyhow::ensure!(!predicate.is_empty(), "atom predicate must not be empty");

    let args_str = &text[open + 1..close];
    let arg_parts = split_top_level(args_str, ',')?;

    let mut args = Vec::with_capacity(arg_parts.len());
    for part in &arg_parts {
        args.push(parse_term(part.trim(), anon_counter));
    }

    Ok(Atom { predicate, args })
}

/// Parse a single term from text.
fn parse_term(s: &str, anon_counter: &mut usize) -> Term {
    let s = s.trim();
    if s == "_" {
        let name = format!("_anon_{anon_counter}");
        *anon_counter += 1;
        return Term::Var(name);
    }
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        return Term::ConstStr(s[1..s.len() - 1].to_string());
    }
    if let Ok(uuid) = Uuid::parse_str(s) {
        return Term::Const(uuid);
    }
    if s.starts_with(|c: char| c.is_ascii_uppercase()) {
        return Term::Var(s.to_string());
    }
    // Try parsing as float
    if let Ok(f) = s.parse::<f64>() {
        return Term::ConstFloat(OrderedFloat(f));
    }
    Term::ConstStr(s.to_string())
}

/// Try to parse `s` as an aggregate body element.
/// Returns `Ok(None)` if `s` is not aggregate-shaped (caller falls back to filter/atom),
/// `Ok(Some(agg))` on success, `Err(...)` if shaped like an aggregate but malformed.
fn parse_aggregate(
    s: &str,
    head_vars: &std::collections::HashSet<String>,
    body_vars: &std::collections::HashSet<String>,
) -> anyhow::Result<Option<crate::types::Aggregate>> {
    let s = s.trim();
    let Some(kind) = aggregate_keyword(s) else {
        return Ok(None);
    };
    let rest = s
        .strip_prefix(kind.keyword())
        .and_then(|r| r.strip_prefix('('))
        .expect("aggregate_keyword matched this prefix");
    let Some(inner_text) = rest.strip_suffix(')') else {
        anyhow::bail!("aggregate '{s}' is missing closing ')'");
    };
    let parts = split_top_level(inner_text, ',')?;

    // Below the floor every kind shares — one inner atom and an output var —
    // nothing could have been meant but a malformed aggregate.
    if parts.len() < 2 {
        anyhow::bail!(
            "aggregate '{s}' must have at least one inner atom and an output var \
             separated by ','"
        );
    }

    // Above that floor, a first part that is not a compound atom means a plain
    // predicate that happens to share the keyword's name — the legacy escape
    // `count(X, N)` has always had. This has to be decided before the
    // kind-specific arity check, or `sum(X, Y)` would be an error rather than
    // an atom.
    if !parts
        .first()
        .is_some_and(|first| parse_atom(first, &mut 0).is_ok())
    {
        return Ok(None);
    }

    // `count(atom.., Out)`; every other kind is `kind(atom.., Value, Out)`.
    if kind.needs_value_var() && parts.len() < 3 {
        anyhow::bail!(
            "aggregate '{s}' folds values, so it needs a value var between its \
             inner atoms and its output var: {}(atom.., Value, Out)",
            kind.keyword()
        );
    }
    let output = parts.last().unwrap().trim().to_string();
    if !output
        .chars()
        .next()
        .map(|c| c.is_ascii_uppercase() || c == '_')
        .unwrap_or(false)
    {
        anyhow::bail!(
            "aggregate output_var '{output}' must be a variable (start with uppercase or '_')"
        );
    }

    // For a value aggregate the second-to-last part is the value variable.
    // Atoms always carry parentheses, so a bare identifier here is
    // unambiguous.
    let (value_var, atom_parts) = if kind.needs_value_var() {
        let raw = parts[parts.len() - 2].trim().to_string();
        if raw.contains('(') {
            anyhow::bail!(
                "aggregate '{s}' needs a value variable before its output var, \
                 got the atom '{raw}'"
            );
        }
        (Some(raw), &parts[..parts.len() - 2])
    } else {
        (None, &parts[..parts.len() - 1])
    };
    let mut atoms: Vec<Atom> = Vec::with_capacity(atom_parts.len());
    let mut anon = 0;
    for (i, part) in atom_parts.iter().enumerate() {
        match parse_atom(part, &mut anon) {
            Ok(atom) => atoms.push(atom),
            Err(_) if atoms.is_empty() => {
                // First arg isn't a compound atom — fall through to body-atom
                // parsing (legacy escape: `count(X, N)` is a plain 2-arg
                // predicate named `count`, not an aggregate).
                return Ok(None);
            }
            Err(e) => {
                anyhow::bail!("aggregate '{s}' atom #{} is malformed: {e}", i + 1);
            }
        }
    }

    let inner = atoms[0].clone();
    let inner_conjunction = if atoms.len() == 1 {
        Vec::new()
    } else {
        atoms.clone()
    };

    let mut group_vars: Vec<String> = Vec::new();
    for atom in &atoms {
        for arg in &atom.args {
            if let crate::types::Term::Var(name) = arg
                && (head_vars.contains(name) || body_vars.contains(name))
                && !group_vars.contains(name)
                // The value var is what the fold consumes, never what it
                // groups by — grouping on it would put every distinct value
                // in its own group and make the fold a no-op.
                && value_var.as_deref() != Some(name.as_str())
            {
                group_vars.push(name.clone());
            }
        }
    }

    if let Some(value) = &value_var {
        anyhow::ensure!(
            value != &output,
            "aggregate '{s}' folds '{value}' into itself; the value variable and \
             the output variable must differ"
        );
        let bound_by_inner = atoms.iter().any(|atom| {
            atom.args
                .iter()
                .any(|arg| matches!(arg, crate::types::Term::Var(n) if n == value))
        });
        anyhow::ensure!(
            bound_by_inner,
            "aggregate '{s}' folds '{value}', which no inner atom binds; there \
             would be nothing to fold"
        );
    }

    Ok(Some(crate::types::Aggregate {
        kind,
        inner,
        inner_conjunction,
        group_vars,
        output_var: output,
        value_var,
    }))
}

/// The aggregate kind a body part is written with, if any.
///
/// Matches only `kind(`, so a predicate literally named `sum` keeps the same
/// legacy escape `count` already had: `sum(X, Y)` fails to parse as an
/// aggregate and falls through to plain body-atom parsing.
fn aggregate_keyword(part: &str) -> Option<crate::types::AggregateKind> {
    use crate::types::AggregateKind::{Avg, Count, Max, Min, Sum};
    [Count, Sum, Min, Max, Avg]
        .into_iter()
        .find(|kind| part.starts_with(&format!("{}(", kind.keyword())))
}

/// True iff `s` contains a comparison operator at the top level — i.e.
/// outside string literals and outside parentheses. Used by `parse_rule`
/// to decide whether a body element is a filter or a predicate atom.
fn has_top_level_cmp(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'=' | b'<' | b'>' if depth == 0 => return true,
            b'!' if depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b'=' => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

// ─── Semi-Naive Evaluator ─────────────────────────────────────────

/// Raw variable binding produced during rule evaluation: variable name → ground term.
type Binding = HashMap<String, Term>;

/// A candidate row: binding paired with its provenance chain.
type Candidate = (Binding, Vec<ProvenanceStep>);

/// Run semi-naive fixpoint evaluation over a set of rules and initial facts,
/// stratum by stratum. Stratification ensures aggregates are always computed
/// over fully-settled base relations. Unstratifiable rule sets (recursion through
/// an aggregate) are rejected: the original fact set is returned unchanged.
///
/// Returns the full derived fact set and a list of derived facts with provenance.
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

/// Inner fixpoint loop for a single stratum of rules.
///
/// Consumes from shared budget counters (`budget_iter`, `budget_facts`) and
/// stops early if either reaches zero.
fn evaluate_stratum(
    rules: &[DatalogRule],
    initial_facts: &FactSet,
    budget_iter: &mut usize,
    budget_facts: &mut usize,
) -> (FactSet, Vec<DerivedFact>) {
    let mut all_facts = initial_facts.clone();
    let mut derived = Vec::new();

    while *budget_iter > 0 {
        *budget_iter -= 1;
        let mut new_delta = FactSet::new();

        for rule in rules {
            let results = evaluate_rule(rule, &all_facts);
            for (head_args, provenance_steps) in results {
                let pred = &rule.head.predicate;
                if !all_facts.contains(pred, &head_args) && !new_delta.contains(pred, &head_args) {
                    new_delta.insert(pred, head_args.clone());

                    let (src_id, dst_id) = extract_src_dst(&head_args);
                    let confidence = compute_confidence(&provenance_steps, &all_facts);

                    derived.push(DerivedFact {
                        src_id,
                        pred: pred.clone(),
                        dst_id,
                        confidence,
                        rule_id: format_rule_id(rule),
                        support_count: provenance_steps.len() as i32,
                        provenance: provenance_steps,
                    });
                }
            }
        }

        if new_delta.is_empty() {
            break;
        }

        if all_facts.len() + new_delta.len() > *budget_facts {
            tracing::warn!(
                "Datalog max_facts cap reached ({} + {} > {})",
                all_facts.len(),
                new_delta.len(),
                *budget_facts
            );
            *budget_facts = 0;
            break;
        }

        // Merge new_delta into all_facts and deduct from fact budget.
        for (pred, fact_set) in &new_delta.facts {
            for args in fact_set {
                all_facts.insert(pred, args.clone());
            }
        }
        *budget_facts = budget_facts.saturating_sub(new_delta.len());
    }

    (all_facts, derived)
}

/// Evaluate a single rule against the current fact set.
///
/// Dispatches to the two-phase aggregate evaluator when the rule contains
/// aggregates; otherwise runs collect_bindings and instantiates the head.
fn evaluate_rule(rule: &DatalogRule, all_facts: &FactSet) -> Vec<(Vec<Term>, Vec<ProvenanceStep>)> {
    if !rule.aggregates.is_empty() {
        return evaluate_rule_with_aggregates(rule, all_facts);
    }
    collect_bindings(rule, all_facts)
        .into_iter()
        .map(|(binding, prov)| (instantiate(&rule.head.args, &binding), prov))
        .collect()
}

/// Run the body-unification + filter-check loop and return raw variable
/// bindings. Identical semantics to the old `evaluate_rule` but stops just
/// before instantiating the head, so callers can either instantiate
/// (non-aggregate path) or feed bindings into the aggregate pipeline.
fn collect_bindings(rule: &DatalogRule, all_facts: &FactSet) -> Vec<Candidate> {
    // Start with a single empty binding
    let initial_bindings: Vec<Candidate> = vec![(HashMap::new(), Vec::new())];

    let final_bindings = rule
        .body
        .iter()
        .fold(initial_bindings, |current_bindings, body_atom| {
            let mut next_bindings = Vec::new();

            for (binding, provenance) in &current_bindings {
                if let Some(fact_set) = all_facts.get(&body_atom.predicate) {
                    for fact_args in fact_set {
                        if fact_args.len() != body_atom.args.len() {
                            continue;
                        }
                        if let Some(new_binding) = try_unify(&body_atom.args, fact_args, binding) {
                            let mut new_prov = provenance.clone();
                            new_prov.push(make_provenance_step(&body_atom.predicate, fact_args));
                            next_bindings.push((new_binding, new_prov));
                        }
                    }
                }
            }

            next_bindings
        });

    // Apply filters, then negation. Filters first because they are cheap and
    // prune candidates a negation check would otherwise scan facts for.
    //
    // Stratification guarantees every negated predicate settled in a strictly
    // lower stratum, so `all_facts` holds its final extension here and the
    // check below is a decision, not a race with this stratum's fixpoint.
    final_bindings
        .into_iter()
        .filter(|(binding, _)| check_filters(&rule.filters, binding))
        .filter_map(|(binding, mut provenance)| {
            for neg in &rule.negated {
                if negated_atom_matches(neg, &binding, all_facts) {
                    return None;
                }
                provenance.push(make_absence_step(neg, &binding));
            }
            Some((binding, provenance))
        })
        .collect()
}

/// True iff some fact matches the negated atom under the current binding.
///
/// This is negation as failure over the settled lower strata, so it never
/// binds: a variable already bound must match that value, and an unbound
/// (anonymous) variable is a wildcard asking only whether *some* row matches.
fn negated_atom_matches(atom: &Atom, binding: &Binding, all_facts: &FactSet) -> bool {
    let Some(fact_set) = all_facts.get(&atom.predicate) else {
        return false;
    };
    fact_set.iter().any(|fact_args| {
        fact_args.len() == atom.args.len()
            && atom
                .args
                .iter()
                .zip(fact_args.iter())
                .all(|(pattern, actual)| match pattern {
                    Term::Var(name) => match binding.get(name) {
                        Some(bound) => bound == actual,
                        None => true,
                    },
                    constant => constant == actual,
                })
    })
}

/// Build a provenance step recording that a negated atom held — that is,
/// that nothing matched it. There is no row to cite, so the step names the
/// absent predicate and the bindings the absence was checked under.
fn make_absence_step(atom: &Atom, binding: &Binding) -> ProvenanceStep {
    let args = instantiate(&atom.args, binding);
    let (src, dst) = extract_src_dst(&args);
    ProvenanceStep {
        parent_src: src,
        parent_pred: atom.predicate.clone(),
        parent_dst: dst,
        parent_kind: crate::types::PROVENANCE_KIND_ABSENCE.to_string(),
    }
}

/// Two-phase aggregate evaluator.
///
/// Phase 1: collect candidate bindings from non-aggregate body atoms + pre-aggregate filters.
/// Phase 2: for each aggregate, group candidates by group_vars, count inner-atom matches,
///          bind output_var, then re-apply post-aggregate filters and instantiate the head.
fn evaluate_rule_with_aggregates(
    rule: &DatalogRule,
    all_facts: &FactSet,
) -> Vec<(Vec<Term>, Vec<ProvenanceStep>)> {
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
        filters: phase1_filters.clone(),
        aggregates: Vec::new(),
        // Negated atoms prune candidates before aggregation, so they belong
        // to phase 1. A negated rule always has a positive body atom, so the
        // body-empty seeding path below can never carry negation.
        negated: rule.negated.clone(),
    };

    // Phase 1: collect candidate bindings from non-aggregate body atoms.
    // When body is empty we skip collect_bindings (it would return one
    // vacuous empty binding that has no group_var values bound) and instead
    // seed one binding per distinct group_vars tuple found in the first
    // aggregate's inner predicate rows. For a global aggregate (no
    // group_vars) we seed a single empty binding.
    let mut bindings: Vec<Candidate> = if rule.body.is_empty() {
        match rule.aggregates.first() {
            Some(first_agg) if !first_agg.group_vars.is_empty() => {
                seed_bindings_from_inner(first_agg, all_facts)
            }
            _ => vec![(HashMap::new(), Vec::new())],
        }
    } else {
        collect_bindings(&phase1_rule, all_facts)
    };

    // Apply phase1 filters to the seeded bindings (for the body-empty case
    // the phase1_filters will be empty, so this is a no-op, but it keeps
    // the logic symmetric).
    bindings.retain(|(b, _)| phase1_filters.iter().all(|f| check_one_filter(f, b)));

    for agg in &rule.aggregates {
        bindings = apply_aggregate(agg, bindings, all_facts);
    }

    let mut results = Vec::new();
    for (binding, prov) in bindings {
        if !post_agg_filters
            .iter()
            .all(|f| check_one_filter(f, &binding))
        {
            continue;
        }
        let head_args = instantiate(&rule.head.args, &binding);
        results.push((head_args, prov));
    }
    results
}

/// Returns true if the filter references any variable in `vars`.
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
        BuiltinFilter::GreaterThan(a, _) | BuiltinFilter::LessThan(a, _) => {
            vars.contains(a.as_str())
        }
        BuiltinFilter::Compare { lhs, rhs, .. } => expr_refs(lhs, vars) || expr_refs(rhs, vars),
    }
}

/// Enumerate one binding per distinct group_vars tuple found in the inner
/// conjunction's rows. Used when the rule body is empty and we need to seed
/// candidate bindings for the aggregate phase.
///
/// For single-atom aggregates this looks only at `inner`; for conjunctions it
/// backtracks over all atoms in `inner_conjunction` so that group vars spread
/// across multiple atoms are all captured.
fn seed_bindings_from_inner(agg: &crate::types::Aggregate, all_facts: &FactSet) -> Vec<Candidate> {
    let atoms: Vec<&Atom> = if agg.inner_conjunction.is_empty() {
        vec![&agg.inner]
    } else {
        agg.inner_conjunction.iter().collect()
    };

    // Collect all full bindings by backtracking over the conjunction.
    let mut all_bindings: Vec<HashMap<String, Term>> = Vec::new();
    collect_conjunction_bindings(&atoms, 0, HashMap::new(), all_facts, &mut all_bindings);

    // Project each full binding down to only the group_vars, deduplicating.
    let mut seen: std::collections::HashSet<Vec<Term>> = std::collections::HashSet::new();
    let mut out = Vec::new();

    for binding in all_bindings {
        let key: Vec<Term> = agg
            .group_vars
            .iter()
            .map(|v| {
                binding
                    .get(v)
                    .cloned()
                    .unwrap_or_else(|| Term::Var(v.clone()))
            })
            .collect();
        if seen.insert(key.clone()) {
            let mut group_binding = HashMap::new();
            for (name, val) in agg.group_vars.iter().zip(key.iter()) {
                group_binding.insert(name.clone(), val.clone());
            }
            out.push((group_binding, Vec::new()));
        }
    }
    out
}

/// Backtracker that collects every complete binding produced by unifying `atoms`
/// in sequence. Used by `seed_bindings_from_inner` to enumerate group-var tuples.
fn collect_conjunction_bindings(
    atoms: &[&Atom],
    i: usize,
    binding: HashMap<String, Term>,
    all_facts: &FactSet,
    out: &mut Vec<HashMap<String, Term>>,
) {
    if i == atoms.len() {
        out.push(binding);
        return;
    }
    let atom = atoms[i];
    let Some(rows) = all_facts.get(&atom.predicate) else {
        return;
    };
    for row in rows {
        if let Some(extended) = try_unify(&atom.args, row, &binding) {
            collect_conjunction_bindings(atoms, i + 1, extended, all_facts, out);
        }
    }
}

/// Group candidates by aggregate group_vars, count inner-atom matches per
/// group, bind output_var to the count, and return the augmented bindings.
fn apply_aggregate(
    agg: &crate::types::Aggregate,
    candidates: Vec<Candidate>,
    all_facts: &FactSet,
) -> Vec<Candidate> {
    let mut groups: HashMap<Vec<Term>, Vec<Candidate>> = HashMap::new();

    for (binding, prov) in candidates {
        let key: Vec<Term> = agg
            .group_vars
            .iter()
            .map(|v| {
                binding
                    .get(v)
                    .cloned()
                    .unwrap_or_else(|| Term::Var(v.clone()))
            })
            .collect();
        groups.entry(key).or_default().push((binding, prov));
    }

    let mut out = Vec::new();
    for (_group_key, members) in groups {
        let representative = match members.first() {
            Some((b, _)) => b.clone(),
            None => continue,
        };
        // `None` means this group produces no value at all — an empty group
        // for a kind with no identity, or a value that was not a number. The
        // members are dropped rather than bound to a fabricated number.
        let Some(value) = fold_inner_matches(agg, &representative, all_facts) else {
            continue;
        };

        for (mut binding, mut prov) in members {
            binding.insert(
                agg.output_var.clone(),
                Term::ConstFloat(OrderedFloat(value)),
            );
            prov.push(make_provenance_step(
                &format!("{}({})", agg.kind.keyword(), agg.inner.predicate),
                &[Term::ConstFloat(OrderedFloat(value))],
            ));
            out.push((binding, prov));
        }
    }
    out
}

/// Visit every complete unification of an aggregate's inner conjunction.
///
/// Streaming by construction: the backtracker hands each solved binding to
/// `visit` and drops it, so nothing proportional to the size of the group is
/// ever collected. A group of twenty thousand rows costs one binding at a
/// time, not a twenty-thousand-element vector.
///
/// `visit` returns false to stop early — which is how a fold abandons a group
/// it has already decided cannot produce a value.
fn visit_inner_matches(
    agg: &crate::types::Aggregate,
    binding: &std::collections::HashMap<String, Term>,
    all_facts: &FactSet,
    visit: &mut impl FnMut(&Binding) -> bool,
) {
    let atoms: Vec<&Atom> = if agg.inner_conjunction.is_empty() {
        vec![&agg.inner]
    } else {
        agg.inner_conjunction.iter().collect()
    };
    visit_conjunction(&atoms, 0, binding.clone(), all_facts, visit);
}

/// Recursive conjunction backtracker: extends `binding` one atom at a time and
/// calls `visit` whenever all atoms are unified. Returns false once `visit`
/// has asked to stop, which unwinds the whole search.
fn visit_conjunction(
    atoms: &[&Atom],
    i: usize,
    binding: std::collections::HashMap<String, Term>,
    all_facts: &FactSet,
    visit: &mut impl FnMut(&Binding) -> bool,
) -> bool {
    if i == atoms.len() {
        return visit(&binding);
    }
    let atom = atoms[i];
    let Some(rows) = all_facts.get(&atom.predicate) else {
        return true;
    };
    for row in rows {
        if let Some(extended) = try_unify(&atom.args, row, &binding)
            && !visit_conjunction(atoms, i + 1, extended, all_facts, visit)
        {
            return false;
        }
    }
    true
}

/// The running state of a streaming aggregate fold.
enum Fold {
    Count(u64),
    Sum(f64),
    Min(Option<f64>),
    Max(Option<f64>),
    Avg(f64, u64),
    /// A value that was not a number. The group is refused rather than
    /// silently totalled without it — a total missing a row is quietly wrong,
    /// which is worse than no total at all.
    TypeError,
}

impl Fold {
    fn start(kind: crate::types::AggregateKind) -> Self {
        use crate::types::AggregateKind as K;
        match kind {
            K::Count => Fold::Count(0),
            K::Sum => Fold::Sum(0.0),
            K::Min => Fold::Min(None),
            K::Max => Fold::Max(None),
            K::Avg => Fold::Avg(0.0, 0),
        }
    }

    /// Absorb one solved binding. Returns false when the fold can stop early.
    fn step(&mut self, value: Option<f64>) -> bool {
        match self {
            Fold::Count(n) => {
                *n += 1;
                true
            }
            Fold::TypeError => false,
            _ => {
                let Some(v) = value else {
                    *self = Fold::TypeError;
                    return false;
                };
                match self {
                    Fold::Sum(acc) => *acc += v,
                    Fold::Min(acc) => *acc = Some(acc.map_or(v, |a| a.min(v))),
                    Fold::Max(acc) => *acc = Some(acc.map_or(v, |a| a.max(v))),
                    Fold::Avg(sum, n) => {
                        *sum += v;
                        *n += 1;
                    }
                    Fold::Count(_) | Fold::TypeError => unreachable!("handled above"),
                }
                true
            }
        }
    }

    /// The folded value, or `None` when the group produces nothing.
    fn finish(self, kind: crate::types::AggregateKind) -> Option<f64> {
        match self {
            Fold::TypeError => None,
            Fold::Count(n) => Some(n as f64),
            Fold::Sum(acc) => Some(acc),
            Fold::Min(acc) | Fold::Max(acc) => acc.or_else(|| kind.identity_over_empty()),
            Fold::Avg(sum, n) => {
                if n == 0 {
                    kind.identity_over_empty()
                } else {
                    Some(sum / n as f64)
                }
            }
        }
    }
}

/// Fold an aggregate over its inner conjunction, streaming.
///
/// `None` means the group derives nothing: an empty group for a kind with no
/// identity (`min`, `max`, `avg`), or a value that was not a number.
fn fold_inner_matches(
    agg: &crate::types::Aggregate,
    binding: &std::collections::HashMap<String, Term>,
    all_facts: &FactSet,
) -> Option<f64> {
    let mut fold = Fold::start(agg.kind);
    let value_var = agg.value_var.clone();
    visit_inner_matches(agg, binding, all_facts, &mut |solved| {
        let value = value_var.as_ref().and_then(|name| match solved.get(name) {
            Some(Term::ConstFloat(OrderedFloat(f))) => Some(*f),
            _ => None,
        });
        fold.step(value)
    });
    if matches!(fold, Fold::TypeError) {
        tracing::warn!(
            aggregate = agg.kind.keyword(),
            predicate = %agg.inner.predicate,
            value_var = ?agg.value_var,
            "datalog: aggregate value is not a number; the group derives nothing \
             rather than a total missing that row"
        );
    }
    fold.finish(agg.kind)
}

/// Try to unify atom arguments with fact arguments under an existing binding.
///
/// Returns `Some(extended_binding)` if unification succeeds, `None` otherwise.
fn try_unify(
    atom_args: &[Term],
    fact_args: &[Term],
    binding: &HashMap<String, Term>,
) -> Option<HashMap<String, Term>> {
    let mut new_binding = binding.clone();

    for (atom_arg, fact_arg) in atom_args.iter().zip(fact_args.iter()) {
        match atom_arg {
            Term::Var(name) => {
                // Anonymous variables starting with "_anon_" never bind (wildcard)
                if name.starts_with("_anon_") {
                    continue;
                }
                if let Some(bound_val) = new_binding.get(name) {
                    if bound_val != fact_arg {
                        return None;
                    }
                } else {
                    new_binding.insert(name.clone(), fact_arg.clone());
                }
            }
            // Constants must match exactly
            other => {
                if other != fact_arg {
                    return None;
                }
            }
        }
    }

    Some(new_binding)
}

/// Runtime value produced by `eval_expr`. Strings, numbers, and UUIDs
/// each have their own arm so type mismatches surface clearly.
enum EvalValue {
    Num(f64),
    Str(String),
    Uuid(uuid::Uuid),
}

fn eval_expr(
    e: &crate::types::FilterExpr,
    binding: &std::collections::HashMap<String, Term>,
) -> Option<EvalValue> {
    use crate::types::{ArithOp, FilterExpr};
    use ordered_float::OrderedFloat;
    match e {
        FilterExpr::Var(name) => match binding.get(name)? {
            Term::ConstFloat(OrderedFloat(f)) => Some(EvalValue::Num(*f)),
            Term::ConstStr(s) => Some(EvalValue::Str(s.clone())),
            Term::Const(u) => Some(EvalValue::Uuid(*u)),
            Term::Var(_) => None,
        },
        FilterExpr::LitNum(OrderedFloat(f)) => Some(EvalValue::Num(*f)),
        FilterExpr::LitStr(s) => Some(EvalValue::Str(s.clone())),
        FilterExpr::Neg(inner) => match eval_expr(inner, binding)? {
            EvalValue::Num(x) => Some(EvalValue::Num(-x)),
            _ => {
                tracing::warn!("datalog: unary minus on non-numeric value");
                None
            }
        },
        FilterExpr::BinOp { op, lhs, rhs } => {
            let l = eval_expr(lhs, binding)?;
            let r = eval_expr(rhs, binding)?;
            match (l, r) {
                (EvalValue::Num(a), EvalValue::Num(b)) => match op {
                    ArithOp::Add => Some(EvalValue::Num(a + b)),
                    ArithOp::Sub => Some(EvalValue::Num(a - b)),
                    ArithOp::Mul => Some(EvalValue::Num(a * b)),
                    ArithOp::Div => {
                        if b == 0.0 {
                            tracing::warn!("datalog: division by zero in filter");
                            None
                        } else {
                            Some(EvalValue::Num(a / b))
                        }
                    }
                },
                _ => {
                    tracing::warn!("datalog: arithmetic on non-numeric values");
                    None
                }
            }
        }
    }
}

fn apply_cmp(op: crate::types::CmpOp, l: &EvalValue, r: &EvalValue) -> bool {
    use crate::types::CmpOp;
    use std::cmp::Ordering;
    match (l, r) {
        (EvalValue::Num(a), EvalValue::Num(b)) => {
            let Some(ord) = a.partial_cmp(b) else {
                return false;
            }; // NaN
            match (op, ord) {
                (CmpOp::Eq, Ordering::Equal) => true,
                (CmpOp::Ne, ord) => ord != Ordering::Equal,
                (CmpOp::Lt, Ordering::Less) => true,
                (CmpOp::Le, ord) => ord != Ordering::Greater,
                (CmpOp::Gt, Ordering::Greater) => true,
                (CmpOp::Ge, ord) => ord != Ordering::Less,
                _ => false,
            }
        }
        (EvalValue::Str(a), EvalValue::Str(b)) => {
            let ord = a.cmp(b);
            match (op, ord) {
                (CmpOp::Eq, Ordering::Equal) => true,
                (CmpOp::Ne, ord) => ord != Ordering::Equal,
                (CmpOp::Lt, Ordering::Less) => true,
                (CmpOp::Le, ord) => ord != Ordering::Greater,
                (CmpOp::Gt, Ordering::Greater) => true,
                (CmpOp::Ge, ord) => ord != Ordering::Less,
                _ => false,
            }
        }
        (EvalValue::Uuid(a), EvalValue::Uuid(b)) => match op {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            _ => {
                tracing::warn!(?op, "datalog: ordered comparison on UUID; returning false");
                false
            }
        },
        _ => {
            tracing::warn!(?op, "datalog: type mismatch in filter; returning false");
            false
        }
    }
}

/// Check all builtin filters against a variable binding.
fn check_filters(filters: &[BuiltinFilter], binding: &HashMap<String, Term>) -> bool {
    filters.iter().all(|f| check_one_filter(f, binding))
}

/// Check a single builtin filter.
fn check_one_filter(filter: &BuiltinFilter, binding: &HashMap<String, Term>) -> bool {
    match filter {
        BuiltinFilter::NotEqual(lhs, rhs) => {
            let lhs_val = binding.get(lhs);
            let rhs_val = binding.get(rhs);
            match (lhs_val, rhs_val) {
                (Some(l), Some(r)) => l != r,
                _ => true, // unbound variables pass the filter
            }
        }
        BuiltinFilter::GreaterThan(var, threshold) => {
            if let Some(Term::ConstFloat(OrderedFloat(v))) = binding.get(var) {
                *v > *threshold
            } else {
                true // unbound or non-float passes
            }
        }
        BuiltinFilter::LessThan(var, threshold) => {
            if let Some(Term::ConstFloat(OrderedFloat(v))) = binding.get(var) {
                *v < *threshold
            } else {
                true
            }
        }
        BuiltinFilter::Compare { op, lhs, rhs } => {
            let (Some(l), Some(r)) = (eval_expr(lhs, binding), eval_expr(rhs, binding)) else {
                // Unbound or type-mismatch (already warned). Match legacy
                // semantics: partial bindings pass the filter.
                return true;
            };
            apply_cmp(*op, &l, &r)
        }
    }
}

/// Instantiate a list of terms by substituting bound variables.
fn instantiate(args: &[Term], binding: &HashMap<String, Term>) -> Vec<Term> {
    args.iter()
        .map(|arg| match arg {
            Term::Var(name) => binding
                .get(name)
                .cloned()
                .unwrap_or_else(|| Term::Var(name.clone())),
            other => other.clone(),
        })
        .collect()
}

/// Extract src_id and dst_id strings from head arguments.
fn extract_src_dst(args: &[Term]) -> (String, String) {
    let src = args.first().map(term_to_string).unwrap_or_default();
    let dst = args.get(1).map(term_to_string).unwrap_or_default();
    (src, dst)
}

/// Convert a Term to a string representation for provenance/derived facts.
fn term_to_string(t: &Term) -> String {
    match t {
        Term::Var(s) => s.clone(),
        Term::Const(u) => u.to_string(),
        Term::ConstStr(s) => s.clone(),
        Term::ConstFloat(f) => f.to_string(),
    }
}

/// Build a provenance step from a matched body atom.
fn make_provenance_step(predicate: &str, args: &[Term]) -> ProvenanceStep {
    let src = args.first().map(term_to_string).unwrap_or_default();
    let dst = args.get(1).map(term_to_string).unwrap_or_default();
    ProvenanceStep {
        parent_src: src,
        parent_pred: predicate.to_string(),
        parent_dst: dst,
        parent_kind: "base".to_string(),
    }
}

/// Compute confidence for a derived fact using min(parent confidences) * weight.
///
/// For base facts we assume confidence 1.0. The weight is the rule weight
/// (defaulting to 0.9 for builtin rules). Result is clamped to [0.0, 1.0].
fn compute_confidence(provenance: &[ProvenanceStep], _all_facts: &FactSet) -> f64 {
    if provenance.is_empty() {
        return 0.0;
    }
    // Base facts are assumed to have confidence 1.0.
    // Rule weight defaults to 0.9 for builtins.
    let min_parent = 1.0_f64;
    let weight = 0.9_f64;
    // An absence is weaker evidence than a present row. The store is
    // open-world: "no row says otherwise" can equally mean the row has not
    // arrived yet. A derivation resting on one is discounted once, however
    // many absences it rests on — the weakness is in the kind of evidence,
    // not its count. Purely positive derivations are unaffected, which is
    // what keeps negation additive for every rule that does not use it.
    let absence_factor = if provenance
        .iter()
        .any(|step| step.parent_kind == crate::types::PROVENANCE_KIND_ABSENCE)
    {
        ABSENCE_CONFIDENCE_WEIGHT
    } else {
        1.0
    };
    (min_parent * weight * absence_factor).clamp(0.0, 1.0)
}

/// Confidence discount applied once to any derivation resting on an absence.
const ABSENCE_CONFIDENCE_WEIGHT: f64 = 0.8;

/// Format a rule identifier from its head predicate and body predicates.
fn format_rule_id(rule: &DatalogRule) -> String {
    // Negated predicates are appended rather than interleaved so that a rule
    // without negation keeps byte-identical its previous id.
    let mut preds: Vec<String> = rule.body.iter().map(|a| a.predicate.clone()).collect();
    preds.extend(rule.negated.iter().map(|a| format!("not {}", a.predicate)));
    format!("{}:-{}", rule.head.predicate, preds.join(","))
}

// ─── Built-in Rules ───────────────────────────────────────────────

/// Return the default set of inference rules for the knowledge graph.
///
/// These rules derive transitive relationships, clusters, reachability,
/// taxonomy hierarchies, and part-of ancestry from base graph predicates.
const BUILTIN_RULES_TEXT: &[&str] = &[
    "related(X, Z) :- co_occurs(X, Y), co_occurs(Y, Z), X != Z.",
    "cluster(X, Y) :- related(X, Y), related(Y, X).",
    "reachable(X, Z) :- edge(X, _, Z).",
    "reachable(X, Z) :- reachable(X, Y), edge(Y, _, Z), X != Z.",
    "class_ancestor(C, P) :- subclass_of(C, P).",
    "class_ancestor(C, P) :- subclass_of(C, M), class_ancestor(M, P).",
    "isa(E, C) :- instance_of(E, C).",
    "isa(E, P) :- instance_of(E, C), class_ancestor(C, P).",
    "ancestor_part(X, Y) :- part_of(X, Y).",
    "ancestor_part(X, Z) :- part_of(X, Y), ancestor_part(Y, Z).",
    "current(X, X) :- active(X).",
    "stale(X, X) :- dormant(X).",
    "stale(X, X) :- silent(X).",
    "stale(X, X) :- unavailable(X).",
    "authoritative(X, X) :- confidence(X, S), S >= 0.8.",
    "authoritative(X, X) :- tag(X, \"curated\").",
    "authoritative(X, X) :- tag(X, \"remembered\").",
    "authoritative(X, X) :- tag(X, \"skill\").",
    "task_relevant(X, Y) :- depends_on(X, Y).",
    "task_relevant(X, Y) :- implements(X, Y).",
    "task_relevant(X, Y) :- uses(X, Y).",
    "task_relevant(X, Y) :- references(X, Y).",
    "task_relevant(X, Y) :- related_to(X, Y).",
    "bridge_memory(X, Z) :- task_relevant(X, Y), reachable(Y, Z), X != Z.",
    "bridge_memory(X, Z) :- task_relevant(X, Y), related(Y, Z), X != Z.",
];

pub fn builtin_rules() -> Vec<DatalogRule> {
    BUILTIN_RULES_TEXT
        .iter()
        .filter_map(|r| parse_rule(r).ok())
        .collect()
}

pub fn synthetic_builtin_rule_entries(tenant_id: Uuid) -> Vec<RuleEntry> {
    let now = chrono::Utc::now();
    BUILTIN_RULES_TEXT
        .iter()
        .enumerate()
        .filter_map(|(idx, rule_body)| {
            let parsed = parse_rule(rule_body).ok()?;
            Some(RuleEntry {
                tenant_id,
                rule_id: format!("builtin:{}:{}", parsed.head.predicate, idx + 1),
                version: 0,
                name: format!("builtin-{}-{}", parsed.head.predicate, idx + 1),
                family: parsed.head.predicate.clone(),
                state: RuleState::Active,
                rule_body: (*rule_body).to_string(),
                rule_weight: 1.0,
                incremental: false,
                created_at: now,
                updated_at: now,
            })
        })
        .collect()
}

pub async fn load_effective_rule_entries(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    family: Option<&str>,
) -> anyhow::Result<Vec<EffectiveRuleEntry>> {
    let family = family.filter(|value| !value.is_empty() && *value != "*");
    let mut rules: Vec<EffectiveRuleEntry> = synthetic_builtin_rule_entries(ctx.tenant_id)
        .into_iter()
        .filter(|entry| family.is_none_or(|value| entry.family == value))
        .map(|entry| EffectiveRuleEntry {
            source: RuleSource::Builtin,
            entry,
        })
        .collect();

    let stored = if let Some(family) = family {
        storage
            .rule_list_family(ctx, family, RuleState::Active)
            .await?
    } else {
        storage.rule_list_active(ctx, RuleState::Active).await?
    };
    for entry in stored {
        if crate::expert_system::is_artifact_approved(
            storage,
            ctx,
            crate::types::ArtifactKind::Rule,
            &entry.rule_id,
        )
        .await?
        {
            rules.push(EffectiveRuleEntry {
                source: RuleSource::Registry,
                entry,
            });
        }
    }

    Ok(rules)
}

pub async fn load_effective_rules(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    family: Option<&str>,
) -> anyhow::Result<Vec<DatalogRule>> {
    load_effective_rule_entries(storage, ctx, family)
        .await?
        .into_iter()
        .map(|rule| parse_rule(&rule.entry.rule_body))
        .collect()
}

// ─── Fact Loading ─────────────────────────────────────────────────

/// Load session facts from storage into canonical Datalog predicates.
///
/// Normalizes storage data into Datalog-friendly predicates:
/// - `entity_list_session()` -> `node(Id)`, `node_label(Id, Type)`,
///   `node_name(Id, Name)`, `instance_of(Id, Type)`
/// - `edge_list_session()` -> `edge(Src, Pred, Dst)` + typed predicates
///   (`co_occurs`, `mentioned_in`, etc.)
/// - `warmth_list_session()` -> `warmth(Id, Score)`
pub async fn load_session_facts(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
) -> anyhow::Result<FactSet> {
    let mut facts = FactSet::new();

    // Load entities as node facts
    let entities = storage.entity_list_session(ctx, session_id).await?;
    for e in &entities {
        facts.insert("node", vec![Term::Const(e.entity_id)]);
        facts.insert(
            "confidence",
            vec![
                Term::Const(e.entity_id),
                Term::ConstFloat(OrderedFloat(e.confidence)),
            ],
        );
        for tag in &e.tags {
            facts.insert(
                "tag",
                vec![Term::Const(e.entity_id), Term::ConstStr(tag.clone())],
            );
        }
        match e.state {
            crate::types::MemoryState::Active => {
                facts.insert("active", vec![Term::Const(e.entity_id)]);
            }
            crate::types::MemoryState::Dormant => {
                facts.insert("dormant", vec![Term::Const(e.entity_id)]);
            }
            crate::types::MemoryState::Silent => {
                facts.insert("silent", vec![Term::Const(e.entity_id)]);
            }
            crate::types::MemoryState::Unavailable => {
                facts.insert("unavailable", vec![Term::Const(e.entity_id)]);
            }
        }
        facts.insert(
            "node_label",
            vec![
                Term::Const(e.entity_id),
                Term::ConstStr(e.entity_type.clone()),
            ],
        );
        facts.insert(
            "node_name",
            vec![
                Term::Const(e.entity_id),
                Term::ConstStr(e.entity_name.clone()),
            ],
        );
        facts.insert(
            "instance_of",
            vec![
                Term::Const(e.entity_id),
                Term::ConstStr(e.entity_type.clone()),
            ],
        );
    }

    // Load edges as typed predicates + generic edge facts
    let edges = storage.edge_list_session(ctx, session_id).await?;
    for (src, dst, edge_type) in &edges {
        let pred = edge_type_to_predicate(edge_type);
        facts.insert(&pred, vec![Term::Const(*src), Term::Const(*dst)]);
        facts.insert(
            "edge",
            vec![
                Term::Const(*src),
                Term::ConstStr(pred.clone()),
                Term::Const(*dst),
            ],
        );
    }

    // Load typed edges as specific predicates
    let typed_edges = storage.typed_edge_list_session(ctx, session_id).await?;
    for typed_edge in &typed_edges {
        let pred = &typed_edge.edge_type;
        facts.insert(
            pred,
            vec![
                Term::Const(typed_edge.src_id),
                Term::Const(typed_edge.dst_id),
            ],
        );
        facts.insert(
            "edge",
            vec![
                Term::Const(typed_edge.src_id),
                Term::ConstStr(pred.clone()),
                Term::Const(typed_edge.dst_id),
            ],
        );
    }

    // Load warmth scores
    let warmth_entries = storage.warmth_list_session(ctx, session_id).await?;
    for w in &warmth_entries {
        facts.insert(
            "warmth",
            vec![
                Term::Const(w.entity_id),
                Term::ConstFloat(OrderedFloat(w.warmth)),
            ],
        );
    }

    Ok(facts)
}

/// Map edge type strings to canonical lowercase predicate names.
fn edge_type_to_predicate(edge_type: &str) -> String {
    match edge_type {
        "CO_OCCURS" | "co_occurs" => "co_occurs".to_string(),
        "MENTIONED_IN" | "mentioned_in" => "mentioned_in".to_string(),
        "FOLDED_INTO" | "folded_into" => "folded_into".to_string(),
        "SUPERSEDES" | "supersedes" => "supersedes".to_string(),
        other => other.to_lowercase(),
    }
}

// ─── Query-Time Derivation ────────────────────────────────────────

/// Query derived facts for a predicate, using caching when available.
///
/// 1. Check the derived cache for pre-computed results.
/// 2. If not cached, load session facts, run the evaluator, and cache the results.
/// 3. Filter to the requested predicate and return.
pub async fn query_predicate(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    predicate: &str,
    config: &DatalogConfig,
) -> anyhow::Result<Vec<DerivedFact>> {
    // 1. Check cache
    let cache_key = format!("{predicate}:{session_id}");
    let cached = storage.derived_cache_get(ctx, &cache_key).await?;
    if !cached.is_empty() {
        storage.heat_record(ctx, predicate, true, None).await?;
        return Ok(cached);
    }

    // 2. Load facts and evaluate
    let start = std::time::Instant::now();
    let facts = load_session_facts(storage, ctx, session_id).await?;
    let rules = load_effective_rules(storage, ctx, Some(predicate)).await?;
    let (_, derived) = evaluate(&rules, &facts, config.max_iterations, config.max_facts);

    // 3. Filter to requested predicate
    let results: Vec<DerivedFact> = derived
        .into_iter()
        .filter(|d| d.pred == predicate)
        .collect();

    // 4. Cache results and record telemetry.
    //
    // A derivation resting on an absence is non-monotonic: a later base fact
    // can falsify it, and this cache is append-only, so it would go on serving
    // the stale derivation. Those are re-derived live on every query instead.
    let elapsed_ms = start.elapsed().as_millis() as i64;
    let cacheable: Vec<DerivedFact> = results
        .iter()
        .filter(|f| !f.rests_on_absence())
        .cloned()
        .collect();
    let uncacheable = results.len() - cacheable.len();
    if uncacheable > 0 {
        tracing::debug!(
            predicate,
            uncacheable,
            "datalog: not caching derivations that rest on an absence; re-derived per query"
        );
    }
    if !cacheable.is_empty() {
        storage
            .derived_cache_put(ctx, &cache_key, &cacheable)
            .await?;
    }
    storage
        .heat_record(ctx, predicate, false, Some(elapsed_ms))
        .await?;

    Ok(results)
}

// ─── Stratification Analyzer ─────────────────────────────────────

/// Compute strata over a rule set.
///
/// Returns Err(StratifyError::RecursionThroughAggregate) if the
/// predicate dependency graph has a strongly-connected component
/// containing an Aggregate-labelled edge — meaning some predicate's
/// derivation transitively requires aggregating over its own (or a
/// peer's) result.
///
/// On success, returns rule indices grouped by ascending stratum.
pub fn stratify(
    rules: &[crate::types::DatalogRule],
) -> Result<Vec<Vec<usize>>, crate::types::StratifyError> {
    use std::collections::{BTreeMap, HashMap, HashSet};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Edge {
        Plain,
        Aggregate,
        Negated,
    }

    // Build predicate dep graph: head -> Vec<(dependency, edge_kind)>.
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
        for atom in &rule.negated {
            entry.push((atom.predicate.clone(), Edge::Negated));
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

    // Iterative Tarjan SCC.
    // Three work-item kinds:
    //   Enter(v)            — assign index/lowlink, push onto SCC stack, schedule Continue.
    //   Continue(v, idx)    — process the idx-th successor of v (or finalise if past the end).
    //   Propagate(parent,child) — after child's subtree is done, pull child's lowlink into parent.
    let mut index_counter: usize = 0;
    let mut stack: Vec<String> = Vec::new();
    let mut on_stack: HashSet<String> = HashSet::new();
    let mut indices: HashMap<String, usize> = HashMap::new();
    let mut lowlinks: HashMap<String, usize> = HashMap::new();
    let mut sccs: Vec<Vec<String>> = Vec::new();

    enum Step {
        Enter(String),
        Continue(String, usize),
        Propagate(String, String), // (parent, child)
    }

    let nodes_to_visit: Vec<String> = all_preds.iter().cloned().collect();
    for start in nodes_to_visit {
        if indices.contains_key(&start) {
            continue;
        }
        let mut work: Vec<Step> = vec![Step::Enter(start)];
        while let Some(step) = work.pop() {
            match step {
                Step::Enter(node) => {
                    indices.insert(node.clone(), index_counter);
                    lowlinks.insert(node.clone(), index_counter);
                    index_counter += 1;
                    stack.push(node.clone());
                    on_stack.insert(node.clone());
                    work.push(Step::Continue(node, 0));
                }
                Step::Continue(node, succ_idx) => {
                    let succs = graph.get(&node).cloned().unwrap_or_default();
                    if succ_idx < succs.len() {
                        let (succ, _) = succs[succ_idx].clone();
                        // Come back to process the next successor after this one.
                        work.push(Step::Continue(node.clone(), succ_idx + 1));
                        if !indices.contains_key(&succ) {
                            // Tree edge: recurse, then propagate lowlink back.
                            work.push(Step::Propagate(node.clone(), succ.clone()));
                            work.push(Step::Enter(succ));
                        } else if on_stack.contains(&succ) {
                            // Back edge: update lowlink immediately.
                            let succ_index = *indices.get(&succ).unwrap();
                            let cur = lowlinks.get_mut(&node).unwrap();
                            if succ_index < *cur {
                                *cur = succ_index;
                            }
                        }
                        // Cross/forward edge (succ already fully processed, not on_stack): ignore.
                    } else {
                        // All successors done — check if this node is an SCC root.
                        if lowlinks.get(&node) == indices.get(&node) {
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
                }
                Step::Propagate(parent, child) => {
                    // Pull child's lowlink into parent (tree-child propagation).
                    let child_low = *lowlinks.get(&child).unwrap();
                    let parent_low = *lowlinks.get(&parent).unwrap();
                    if child_low < parent_low {
                        *lowlinks.get_mut(&parent).unwrap() = child_low;
                    }
                }
            }
        }
    }

    // Map each predicate to its SCC index.
    let mut node_to_scc: HashMap<String, usize> = HashMap::new();
    for (i, scc) in sccs.iter().enumerate() {
        for n in scc {
            node_to_scc.insert(n.clone(), i);
        }
    }

    // Reject any SCC that contains an Aggregate edge within the same SCC
    // (i.e., a predicate depends on itself or a peer via aggregation).
    for scc in &sccs {
        let scc_set: HashSet<&str> = scc.iter().map(String::as_str).collect();
        for node in scc {
            if let Some(succs) = graph.get(node) {
                for (succ, edge) in succs {
                    if !scc_set.contains(succ.as_str()) {
                        continue;
                    }
                    match edge {
                        Edge::Aggregate => {
                            return Err(crate::types::StratifyError::RecursionThroughAggregate {
                                cycle: scc.clone(),
                            });
                        }
                        // A predicate that transitively depends on its own
                        // negation has no stratified model. Reject rather
                        // than settle on an arbitrary fixpoint.
                        Edge::Negated => {
                            return Err(crate::types::StratifyError::RecursionThroughNegation {
                                cycle: scc.clone(),
                            });
                        }
                        Edge::Plain => {}
                    }
                }
            }
        }
    }

    // Assign a stratum to each SCC.
    // Tarjan emits SCCs in reverse topological order (leaves first).
    let mut scc_stratum: HashMap<usize, usize> = HashMap::new();
    for (i, scc) in sccs.iter().enumerate() {
        let mut max_dep_stratum: i64 = -1;
        let mut had_settling_edge = false;
        for node in scc {
            if let Some(succs) = graph.get(node) {
                for (succ, edge) in succs {
                    let succ_scc = *node_to_scc.get(succ).unwrap();
                    if succ_scc != i {
                        let s = *scc_stratum.get(&succ_scc).unwrap_or(&0) as i64;
                        if s > max_dep_stratum {
                            max_dep_stratum = s;
                        }
                        if matches!(edge, Edge::Aggregate | Edge::Negated) {
                            had_settling_edge = true;
                        }
                    }
                }
            }
        }
        // Plain edges: stratum = max_dep + 1 (derived predicates always lift one level).
        // Aggregate and negated edges: stratum = max_dep + 2. Both read a relation
        // rather than extend it, so the extra lift guarantees that relation is fully
        // computed first — for negation this is exactly what makes the negated atom
        // constant during this stratum's semi-naive fixpoint, and so sound.
        let lift = if had_settling_edge { 2 } else { 1 };
        let stratum = if max_dep_stratum < 0 {
            0
        } else {
            (max_dep_stratum as usize) + lift
        };
        scc_stratum.insert(i, stratum);
    }

    // Group rule indices by ascending stratum.
    let mut by_stratum: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (rule_idx, rule) in rules.iter().enumerate() {
        let scc_idx = *node_to_scc.get(&rule.head.predicate).unwrap_or(&0);
        let stratum = *scc_stratum.get(&scc_idx).unwrap_or(&0);
        by_stratum.entry(stratum).or_default().push(rule_idx);
    }

    Ok(by_stratum.into_values().collect())
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Parsing tests ─────────────────────────────────────────────

    #[test]
    fn test_parse_simple_rule() {
        let rule = parse_rule("related(X, Y) :- co_occurs(X, Y).").unwrap();
        assert_eq!(rule.head.predicate, "related");
        assert_eq!(rule.head.args.len(), 2);
        assert_eq!(rule.head.args[0], Term::Var("X".into()));
        assert_eq!(rule.head.args[1], Term::Var("Y".into()));
        assert_eq!(rule.body.len(), 1);
        assert_eq!(rule.body[0].predicate, "co_occurs");
        assert_eq!(rule.body[0].args.len(), 2);
        assert!(rule.filters.is_empty());
    }

    #[test]
    fn test_parse_rule_with_filter() {
        use crate::types::{CmpOp, FilterExpr};
        let rule =
            parse_rule("related(X, Z) :- co_occurs(X, Y), co_occurs(Y, Z), X != Z.").unwrap();
        assert_eq!(rule.head.predicate, "related");
        assert_eq!(rule.body.len(), 2);
        assert_eq!(rule.filters.len(), 1);
        assert_eq!(
            rule.filters[0],
            BuiltinFilter::Compare {
                op: CmpOp::Ne,
                lhs: FilterExpr::Var("X".into()),
                rhs: FilterExpr::Var("Z".into()),
            }
        );
    }

    #[test]
    fn test_parse_all_builtins() {
        let rules = builtin_rules();
        assert_eq!(
            rules.len(),
            BUILTIN_RULES_TEXT.len(),
            "expected all builtin rules to parse, got {}",
            rules.len()
        );
    }

    #[test]
    fn test_parse_rejects_invalid() {
        assert!(parse_rule("not valid").is_err());
        assert!(parse_rule("").is_err());
        assert!(parse_rule("head :- ").is_err());
    }

    #[test]
    fn test_parse_wildcard_generates_unique_anon_vars() {
        let rule = parse_rule("reachable(X, Z) :- edge(X, _, Z).").unwrap();
        assert_eq!(rule.body[0].args.len(), 3);
        assert_eq!(rule.body[0].args[1], Term::Var("_anon_0".into()));
    }

    #[test]
    fn test_parse_string_constants() {
        let rule = parse_rule(r#"label(X, "person") :- instance_of(X, "person")."#).unwrap();
        assert_eq!(rule.head.args[1], Term::ConstStr("person".into()));
        assert_eq!(rule.body[0].args[1], Term::ConstStr("person".into()));
    }

    #[test]
    fn test_parse_uuid_constants() {
        let id = Uuid::new_v4();
        let rule_text = format!("node({id}) :- exists({id}).");
        let rule = parse_rule(&rule_text).unwrap();
        assert_eq!(rule.head.args[0], Term::Const(id));
    }

    #[test]
    fn test_parse_greater_than_filter() {
        use crate::types::{CmpOp, FilterExpr};
        use ordered_float::OrderedFloat;
        let rule = parse_rule("hot(X) :- warmth(X, W), W > 0.5.").unwrap();
        assert_eq!(rule.filters.len(), 1);
        assert_eq!(
            rule.filters[0],
            BuiltinFilter::Compare {
                op: CmpOp::Gt,
                lhs: FilterExpr::Var("W".into()),
                rhs: FilterExpr::LitNum(OrderedFloat(0.5)),
            }
        );
    }

    #[test]
    fn test_parse_less_than_filter() {
        use crate::types::{CmpOp, FilterExpr};
        use ordered_float::OrderedFloat;
        let rule = parse_rule("cold(X) :- warmth(X, W), W < 0.1.").unwrap();
        assert_eq!(rule.filters.len(), 1);
        assert_eq!(
            rule.filters[0],
            BuiltinFilter::Compare {
                op: CmpOp::Lt,
                lhs: FilterExpr::Var("W".into()),
                rhs: FilterExpr::LitNum(OrderedFloat(0.1)),
            }
        );
    }

    // ── Evaluator tests ───────────────────────────────────────────

    #[test]
    fn test_evaluate_triangle() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        let mut facts = FactSet::new();
        facts.insert("co_occurs", vec![Term::Const(a), Term::Const(b)]);
        facts.insert("co_occurs", vec![Term::Const(b), Term::Const(c)]);
        facts.insert("co_occurs", vec![Term::Const(c), Term::Const(a)]);

        let rules = builtin_rules();
        let (all_facts, derived) = evaluate(&rules, &facts, 100, 50000);

        // A->B->C means related(A, C)
        assert!(
            all_facts.contains("related", &[Term::Const(a), Term::Const(c)]),
            "should derive related(A, C) via A->B->C"
        );
        // B->C->A means related(B, A)
        assert!(
            all_facts.contains("related", &[Term::Const(b), Term::Const(a)]),
            "should derive related(B, A) via B->C->A"
        );
        // C->A->B means related(C, B)
        assert!(
            all_facts.contains("related", &[Term::Const(c), Term::Const(b)]),
            "should derive related(C, B) via C->A->B"
        );

        let related_derived: Vec<_> = derived.iter().filter(|d| d.pred == "related").collect();
        assert!(
            related_derived.len() >= 3,
            "expected at least 3 related derivations, got {}",
            related_derived.len()
        );
    }

    #[test]
    fn test_evaluate_diamond() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let d = Uuid::new_v4();

        let mut facts = FactSet::new();
        facts.insert("co_occurs", vec![Term::Const(a), Term::Const(b)]);
        facts.insert("co_occurs", vec![Term::Const(a), Term::Const(c)]);
        facts.insert("co_occurs", vec![Term::Const(b), Term::Const(d)]);
        facts.insert("co_occurs", vec![Term::Const(c), Term::Const(d)]);

        let rules = builtin_rules();
        let (all_facts, _derived) = evaluate(&rules, &facts, 100, 50000);

        // related(A, D) via A->B->D
        assert!(
            all_facts.contains("related", &[Term::Const(a), Term::Const(d)]),
            "should derive related(A, D) via A->B->D path"
        );
    }

    #[test]
    fn test_evaluate_taxonomy() {
        let mut facts = FactSet::new();
        facts.insert(
            "instance_of",
            vec![
                Term::ConstStr("dog".into()),
                Term::ConstStr("animal".into()),
            ],
        );
        facts.insert(
            "subclass_of",
            vec![
                Term::ConstStr("animal".into()),
                Term::ConstStr("living_thing".into()),
            ],
        );

        let rules = builtin_rules();
        let (all_facts, _derived) = evaluate(&rules, &facts, 100, 50000);

        assert!(
            all_facts.contains(
                "class_ancestor",
                &[
                    Term::ConstStr("animal".into()),
                    Term::ConstStr("living_thing".into())
                ]
            ),
            "should derive class_ancestor(animal, living_thing)"
        );

        assert!(
            all_facts.contains(
                "isa",
                &[
                    Term::ConstStr("dog".into()),
                    Term::ConstStr("animal".into())
                ]
            ),
            "should derive isa(dog, animal)"
        );

        assert!(
            all_facts.contains(
                "isa",
                &[
                    Term::ConstStr("dog".into()),
                    Term::ConstStr("living_thing".into())
                ]
            ),
            "should derive isa(dog, living_thing)"
        );
    }

    #[test]
    fn test_fixpoint_convergence() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        let mut facts = FactSet::new();
        facts.insert("co_occurs", vec![Term::Const(a), Term::Const(b)]);

        let rules = builtin_rules();

        let (facts1, derived1) = evaluate(&rules, &facts, 100, 50000);
        let (facts2, derived2) = evaluate(&rules, &facts, 100, 50000);

        assert_eq!(
            facts1.len(),
            facts2.len(),
            "fixpoint should be deterministic"
        );
        assert_eq!(
            derived1.len(),
            derived2.len(),
            "derived facts should be identical on re-evaluation"
        );
    }

    #[test]
    fn test_max_iterations_cap() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        let mut facts = FactSet::new();
        facts.insert("co_occurs", vec![Term::Const(a), Term::Const(b)]);
        facts.insert("co_occurs", vec![Term::Const(b), Term::Const(c)]);
        facts.insert("co_occurs", vec![Term::Const(c), Term::Const(a)]);

        let (facts_1iter, _) = evaluate(&builtin_rules(), &facts, 1, 50000);
        let (facts_full, _) = evaluate(&builtin_rules(), &facts, 100, 50000);

        assert!(
            facts_1iter.len() <= facts_full.len(),
            "1 iteration ({}) should produce <= facts than full run ({})",
            facts_1iter.len(),
            facts_full.len()
        );
    }

    #[test]
    fn test_max_facts_cap() {
        let mut facts = FactSet::new();
        let nodes: Vec<Uuid> = (0..20).map(|_| Uuid::new_v4()).collect();

        for i in 0..nodes.len() {
            for j in (i + 1)..nodes.len() {
                facts.insert(
                    "co_occurs",
                    vec![Term::Const(nodes[i]), Term::Const(nodes[j])],
                );
            }
        }

        let base_count = facts.len();
        let max_facts = base_count + 5;

        let (capped_facts, _) = evaluate(&builtin_rules(), &facts, 100, max_facts);
        // The cap prevents merging beyond the limit, but the base facts are already in
        assert!(
            capped_facts.len() <= max_facts + base_count,
            "facts ({}) should be bounded",
            capped_facts.len(),
        );
    }

    #[test]
    fn test_confidence_propagation() {
        let provenance = vec![
            ProvenanceStep {
                parent_src: "a".into(),
                parent_pred: "co_occurs".into(),
                parent_dst: "b".into(),
                parent_kind: "base".into(),
            },
            ProvenanceStep {
                parent_src: "b".into(),
                parent_pred: "co_occurs".into(),
                parent_dst: "c".into(),
                parent_kind: "base".into(),
            },
        ];

        let facts = FactSet::new();
        let conf = compute_confidence(&provenance, &facts);

        // min(1.0, 1.0) * 0.9 = 0.9
        assert!(
            (conf - 0.9).abs() < f64::EPSILON,
            "expected confidence 0.9, got {conf}"
        );
        assert!((0.0..=1.0).contains(&conf), "confidence must be in [0, 1]");
    }

    #[test]
    fn test_confidence_empty_provenance() {
        let facts = FactSet::new();
        let conf = compute_confidence(&[], &facts);
        assert!(
            conf.abs() < f64::EPSILON,
            "empty provenance should give 0.0 confidence"
        );
    }

    #[test]
    fn test_provenance_tracking() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        let mut facts = FactSet::new();
        facts.insert("co_occurs", vec![Term::Const(a), Term::Const(b)]);
        facts.insert("co_occurs", vec![Term::Const(b), Term::Const(c)]);

        let rules =
            vec![parse_rule("related(X, Z) :- co_occurs(X, Y), co_occurs(Y, Z), X != Z.").unwrap()];
        let (_, derived) = evaluate(&rules, &facts, 100, 50000);

        assert!(!derived.is_empty(), "should have derived facts");

        let a_str = a.to_string();
        let c_str = c.to_string();
        let fact = derived
            .iter()
            .find(|d| d.pred == "related" && d.src_id == a_str && d.dst_id == c_str);
        assert!(fact.is_some(), "should derive related(A, C)");

        let fact = fact.unwrap();
        assert_eq!(
            fact.provenance.len(),
            2,
            "should have 2 provenance steps (one per body atom)"
        );

        assert_eq!(fact.provenance[0].parent_pred, "co_occurs");
        assert_eq!(fact.provenance[0].parent_src, a_str);

        assert_eq!(fact.provenance[1].parent_pred, "co_occurs");
        assert_eq!(fact.provenance[1].parent_dst, c_str);
    }

    // ── Edge-case tests ───────────────────────────────────────────

    #[test]
    fn test_evaluate_empty_facts() {
        let facts = FactSet::new();
        let rules = builtin_rules();
        let (all_facts, derived) = evaluate(&rules, &facts, 100, 50000);
        assert!(all_facts.is_empty());
        assert!(derived.is_empty());
    }

    #[test]
    fn test_evaluate_no_rules() {
        let mut facts = FactSet::new();
        facts.insert(
            "co_occurs",
            vec![Term::Const(Uuid::new_v4()), Term::Const(Uuid::new_v4())],
        );
        let (all_facts, derived) = evaluate(&[], &facts, 100, 50000);
        assert_eq!(all_facts.len(), facts.len());
        assert!(derived.is_empty());
    }

    #[test]
    fn test_evaluate_reachable_chain() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        let mut facts = FactSet::new();
        facts.insert(
            "edge",
            vec![
                Term::Const(a),
                Term::ConstStr("link".into()),
                Term::Const(b),
            ],
        );
        facts.insert(
            "edge",
            vec![
                Term::Const(b),
                Term::ConstStr("link".into()),
                Term::Const(c),
            ],
        );

        let rules = builtin_rules();
        let (all_facts, _) = evaluate(&rules, &facts, 100, 50000);

        assert!(all_facts.contains("reachable", &[Term::Const(a), Term::Const(b)]));
        assert!(all_facts.contains("reachable", &[Term::Const(b), Term::Const(c)]));
        assert!(
            all_facts.contains("reachable", &[Term::Const(a), Term::Const(c)]),
            "should derive reachable(a, c) transitively"
        );
    }

    #[test]
    fn test_evaluate_ancestor_part() {
        let mut facts = FactSet::new();
        facts.insert(
            "part_of",
            vec![Term::ConstStr("wheel".into()), Term::ConstStr("car".into())],
        );
        facts.insert(
            "part_of",
            vec![Term::ConstStr("car".into()), Term::ConstStr("fleet".into())],
        );

        let rules = builtin_rules();
        let (all_facts, _) = evaluate(&rules, &facts, 100, 50000);

        assert!(
            all_facts.contains(
                "ancestor_part",
                &[
                    Term::ConstStr("wheel".into()),
                    Term::ConstStr("fleet".into())
                ]
            ),
            "should derive ancestor_part(wheel, fleet) transitively"
        );
    }

    #[test]
    fn test_cluster_derivation() {
        // Cluster requires bidirectional related, which requires bidirectional
        // co_occurs paths. Use a fully bidirectional triangle.
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        let mut facts = FactSet::new();
        // Bidirectional edges: a<->b, b<->c, a<->c
        facts.insert("co_occurs", vec![Term::Const(a), Term::Const(b)]);
        facts.insert("co_occurs", vec![Term::Const(b), Term::Const(a)]);
        facts.insert("co_occurs", vec![Term::Const(b), Term::Const(c)]);
        facts.insert("co_occurs", vec![Term::Const(c), Term::Const(b)]);
        facts.insert("co_occurs", vec![Term::Const(a), Term::Const(c)]);
        facts.insert("co_occurs", vec![Term::Const(c), Term::Const(a)]);

        let rules = builtin_rules();
        let (all_facts, _) = evaluate(&rules, &facts, 100, 50000);

        // related(a, c) via a->b->c, and related(c, a) via c->b->a
        let has_related_ac = all_facts.contains("related", &[Term::Const(a), Term::Const(c)]);
        let has_related_ca = all_facts.contains("related", &[Term::Const(c), Term::Const(a)]);
        assert!(has_related_ac, "need related(a,c)");
        assert!(has_related_ca, "need related(c,a)");

        // cluster(a, c) requires related(a,c) AND related(c,a)
        assert!(
            all_facts.contains("cluster", &[Term::Const(a), Term::Const(c)]),
            "should derive cluster(a, c)"
        );
    }

    #[test]
    fn test_hot_memory_predicate_derivations() {
        let active = Uuid::new_v4();
        let dormant = Uuid::new_v4();
        let trusted = Uuid::new_v4();
        let curated = Uuid::new_v4();

        let mut facts = FactSet::new();
        facts.insert("active", vec![Term::Const(active)]);
        facts.insert("dormant", vec![Term::Const(dormant)]);
        facts.insert(
            "confidence",
            vec![Term::Const(trusted), Term::ConstFloat(OrderedFloat(0.85))],
        );
        facts.insert(
            "tag",
            vec![Term::Const(curated), Term::ConstStr("curated".into())],
        );
        facts.insert("uses", vec![Term::Const(active), Term::Const(trusted)]);
        facts.insert(
            "edge",
            vec![
                Term::Const(trusted),
                Term::ConstStr("references".into()),
                Term::Const(curated),
            ],
        );

        let (all_facts, _) = evaluate(&builtin_rules(), &facts, 100, 50000);

        assert!(all_facts.contains("current", &[Term::Const(active), Term::Const(active)]));
        assert!(all_facts.contains("stale", &[Term::Const(dormant), Term::Const(dormant)]));
        assert!(all_facts.contains(
            "authoritative",
            &[Term::Const(trusted), Term::Const(trusted)]
        ));
        assert!(all_facts.contains(
            "authoritative",
            &[Term::Const(curated), Term::Const(curated)]
        ));
        assert!(all_facts.contains(
            "task_relevant",
            &[Term::Const(active), Term::Const(trusted)]
        ));
        assert!(all_facts.contains(
            "bridge_memory",
            &[Term::Const(active), Term::Const(curated)]
        ));
    }

    #[test]
    fn parse_rule_supports_ge_and_le_via_compare_variant() {
        use crate::types::{CmpOp, FilterExpr};
        use ordered_float::OrderedFloat;

        let rule = parse_rule("hot(X) :- warmth(X, W), W >= 0.5.").unwrap();
        assert_eq!(rule.filters.len(), 1);
        assert_eq!(
            rule.filters[0],
            BuiltinFilter::Compare {
                op: CmpOp::Ge,
                lhs: FilterExpr::Var("W".into()),
                rhs: FilterExpr::LitNum(OrderedFloat(0.5)),
            }
        );

        let rule = parse_rule("cold(X) :- warmth(X, W), W <= 0.1.").unwrap();
        assert_eq!(
            rule.filters[0],
            BuiltinFilter::Compare {
                op: CmpOp::Le,
                lhs: FilterExpr::Var("W".into()),
                rhs: FilterExpr::LitNum(OrderedFloat(0.1)),
            }
        );
    }

    #[test]
    fn parse_rule_supports_arithmetic_filter() {
        use crate::types::{CmpOp, FilterExpr};
        use ordered_float::OrderedFloat;

        let rule = parse_rule("near(X) :- count(X, N), N + 1 < 5.").unwrap();
        let want = BuiltinFilter::Compare {
            op: CmpOp::Lt,
            lhs: FilterExpr::BinOp {
                op: crate::types::ArithOp::Add,
                lhs: Box::new(FilterExpr::Var("N".into())),
                rhs: Box::new(FilterExpr::LitNum(OrderedFloat(1.0))),
            },
            rhs: FilterExpr::LitNum(OrderedFloat(5.0)),
        };
        assert_eq!(rule.filters[0], want);
    }

    #[test]
    fn test_edge_type_to_predicate() {
        assert_eq!(edge_type_to_predicate("CO_OCCURS"), "co_occurs");
        assert_eq!(edge_type_to_predicate("co_occurs"), "co_occurs");
        assert_eq!(edge_type_to_predicate("MENTIONED_IN"), "mentioned_in");
        assert_eq!(edge_type_to_predicate("FOLDED_INTO"), "folded_into");
        assert_eq!(edge_type_to_predicate("SUPERSEDES"), "supersedes");
        assert_eq!(edge_type_to_predicate("CUSTOM_EDGE"), "custom_edge");
    }

    #[test]
    fn test_format_rule_id() {
        let rule =
            parse_rule("related(X, Z) :- co_occurs(X, Y), co_occurs(Y, Z), X != Z.").unwrap();
        let id = format_rule_id(&rule);
        assert_eq!(id, "related:-co_occurs,co_occurs");
    }

    // ── Storage-dependent tests ───────────────────────────────────

    #[tokio::test]
    async fn test_load_session_facts() {
        use crate::storage::mock::MockStorage;

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let session_id = Uuid::new_v4();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        store
            .entity_put(
                &ctx,
                &crate::types::EntityEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id: e1,
                    session_id,
                    entity_name: "Alice".into(),
                    entity_type: "person".into(),
                    source_fold_id: None,
                    context_snippet: "test".into(),
                    entity_embedding: None,
                    confidence: 0.9,
                    state: crate::types::MemoryState::Active,
                    created_at: chrono::Utc::now(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        store
            .entity_put(
                &ctx,
                &crate::types::EntityEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id: e2,
                    session_id,
                    entity_name: "Bob".into(),
                    entity_type: "person".into(),
                    source_fold_id: None,
                    context_snippet: "test".into(),
                    entity_embedding: None,
                    confidence: 0.8,
                    state: crate::types::MemoryState::Active,
                    created_at: chrono::Utc::now(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        store
            .edge_co_occurs(&ctx, e1, e2, session_id, 1.0)
            .await
            .unwrap();

        let facts = load_session_facts(&store, &ctx, session_id).await.unwrap();

        assert!(facts.contains("node", &[Term::Const(e1)]));
        assert!(facts.contains("node", &[Term::Const(e2)]));
        assert!(facts.contains(
            "node_name",
            &[Term::Const(e1), Term::ConstStr("Alice".into())]
        ));
        assert!(facts.contains(
            "instance_of",
            &[Term::Const(e1), Term::ConstStr("person".into())]
        ));
        assert!(facts.contains("co_occurs", &[Term::Const(e1), Term::Const(e2)]));
        assert!(facts.contains(
            "edge",
            &[
                Term::Const(e1),
                Term::ConstStr("co_occurs".into()),
                Term::Const(e2)
            ]
        ));
    }

    #[test]
    fn evaluator_handles_ge_and_arithmetic() {
        use crate::types::{ArithOp, CmpOp, FilterExpr};
        use ordered_float::OrderedFloat;
        use std::collections::HashMap;

        let mut binding: HashMap<String, Term> = HashMap::new();
        binding.insert("S".into(), Term::ConstFloat(OrderedFloat(0.7)));
        binding.insert("T".into(), Term::ConstFloat(OrderedFloat(0.5)));

        // S >= T
        let f1 = BuiltinFilter::Compare {
            op: CmpOp::Ge,
            lhs: FilterExpr::Var("S".into()),
            rhs: FilterExpr::Var("T".into()),
        };
        assert!(check_one_filter(&f1, &binding));

        // S < T (false)
        let f2 = BuiltinFilter::Compare {
            op: CmpOp::Lt,
            lhs: FilterExpr::Var("S".into()),
            rhs: FilterExpr::Var("T".into()),
        };
        assert!(!check_one_filter(&f2, &binding));

        // T + 0.1 == 0.6
        let f3 = BuiltinFilter::Compare {
            op: CmpOp::Eq,
            lhs: FilterExpr::BinOp {
                op: ArithOp::Add,
                lhs: Box::new(FilterExpr::Var("T".into())),
                rhs: Box::new(FilterExpr::LitNum(OrderedFloat(0.1))),
            },
            rhs: FilterExpr::LitNum(OrderedFloat(0.6)),
        };
        assert!(check_one_filter(&f3, &binding));
    }

    #[test]
    fn evaluator_unbound_var_passes_compare_filter() {
        use crate::types::{CmpOp, FilterExpr};
        use std::collections::HashMap;

        let binding: HashMap<String, Term> = HashMap::new();
        let f = BuiltinFilter::Compare {
            op: CmpOp::Gt,
            lhs: FilterExpr::Var("UNBOUND".into()),
            rhs: FilterExpr::LitNum(ordered_float::OrderedFloat(3.0)),
        };
        // Unbound vars pass — same semantics as legacy GreaterThan.
        assert!(check_one_filter(&f, &binding));
    }

    #[test]
    fn legacy_filter_variants_still_evaluate() {
        use ordered_float::OrderedFloat;
        use std::collections::HashMap;

        let mut binding: HashMap<String, Term> = HashMap::new();
        binding.insert("X".into(), Term::ConstFloat(OrderedFloat(0.7)));

        assert!(check_one_filter(
            &BuiltinFilter::GreaterThan("X".into(), 0.5),
            &binding
        ));
        assert!(!check_one_filter(
            &BuiltinFilter::LessThan("X".into(), 0.5),
            &binding
        ));

        let mut b2: HashMap<String, Term> = HashMap::new();
        b2.insert("A".into(), Term::ConstStr("foo".into()));
        b2.insert("B".into(), Term::ConstStr("bar".into()));
        assert!(check_one_filter(
            &BuiltinFilter::NotEqual("A".into(), "B".into()),
            &b2
        ));
    }

    #[tokio::test]
    async fn test_query_predicate_caches_results() {
        use crate::storage::mock::MockStorage;

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let session_id = Uuid::new_v4();
        let config = DatalogConfig::default();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let e3 = Uuid::new_v4();

        for (id, name) in [(e1, "A"), (e2, "B"), (e3, "C")] {
            store
                .entity_put(
                    &ctx,
                    &crate::types::EntityEntry {
                        tenant_id: ctx.tenant_id,
                        entity_id: id,
                        session_id,
                        entity_name: name.into(),
                        entity_type: "node".into(),
                        source_fold_id: None,
                        context_snippet: "test".into(),
                        entity_embedding: None,
                        confidence: 1.0,
                        state: crate::types::MemoryState::Active,
                        created_at: chrono::Utc::now(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }

        store
            .edge_co_occurs(&ctx, e1, e2, session_id, 1.0)
            .await
            .unwrap();
        store
            .edge_co_occurs(&ctx, e2, e3, session_id, 1.0)
            .await
            .unwrap();

        // First query — computes and caches
        let results = query_predicate(&store, &ctx, session_id, "related", &config)
            .await
            .unwrap();
        assert!(!results.is_empty(), "should derive 'related' facts");

        // Second query — should hit cache
        let cached = query_predicate(&store, &ctx, session_id, "related", &config)
            .await
            .unwrap();
        assert_eq!(results.len(), cached.len(), "cached results should match");

        // Check heat telemetry was recorded
        let (hits, _compute) = store.heat_get(&ctx, "related", 7).await.unwrap();
        assert!(hits >= 1, "should have at least one cache hit recorded");
    }

    #[test]
    fn evaluate_full_rule_with_ge_filter() {
        use ordered_float::OrderedFloat;

        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();

        let mut facts = FactSet::new();
        // confidence(entity, score)
        facts.insert(
            "confidence",
            vec![Term::Const(a), Term::ConstFloat(OrderedFloat(0.85))],
        );
        facts.insert(
            "confidence",
            vec![Term::Const(b), Term::ConstFloat(OrderedFloat(0.65))],
        );
        facts.insert(
            "confidence",
            vec![Term::Const(c), Term::ConstFloat(OrderedFloat(0.7))],
        );

        let rule = parse_rule("trusted(X) :- confidence(X, S), S >= 0.7.").unwrap();
        let (derived_set, _provenance) = evaluate(&[rule], &facts, 100, 1000);

        let trusted: std::collections::HashSet<Uuid> = derived_set
            .get("trusted")
            .into_iter()
            .flatten()
            .filter_map(|args| match args.first()? {
                Term::Const(u) => Some(*u),
                _ => None,
            })
            .collect();

        assert!(trusted.contains(&a), "0.85 >= 0.7 should derive trusted(a)");
        assert!(trusted.contains(&c), "0.7 >= 0.7 should derive trusted(c)");
        assert!(
            !trusted.contains(&b),
            "0.65 >= 0.7 must not derive trusted(b)"
        );
    }

    #[test]
    fn user_example_var_to_var_inequality() {
        // The user explicitly called this out as a target rule. After this
        // change it parses to a Compare { op: Ne, … } and evaluates correctly.
        use crate::types::{CmpOp, FilterExpr};

        let rule = parse_rule(
            "avoid_action(X) :- user_corrected(S1, X), user_corrected(S2, X), S1 != S2.",
        )
        .unwrap();
        assert_eq!(rule.filters.len(), 1);
        assert_eq!(
            rule.filters[0],
            BuiltinFilter::Compare {
                op: CmpOp::Ne,
                lhs: FilterExpr::Var("S1".into()),
                rhs: FilterExpr::Var("S2".into()),
            }
        );

        // End-to-end: two distinct sessions corrected the same target.
        let s1 = Uuid::new_v4();
        let s2 = Uuid::new_v4();
        let target = Uuid::new_v4();
        let mut facts = FactSet::new();
        facts.insert("user_corrected", vec![Term::Const(s1), Term::Const(target)]);
        facts.insert("user_corrected", vec![Term::Const(s2), Term::Const(target)]);
        let (derived, _) = evaluate(&[rule], &facts, 100, 1000);
        let any_avoid = derived.get("avoid_action").is_some_and(|s| !s.is_empty());
        assert!(
            any_avoid,
            "expected avoid_action to fire when two distinct sessions corrected the same target"
        );
    }

    #[test]
    fn parse_rule_supports_count_aggregate() {
        use crate::types::AggregateKind;
        let rule =
            parse_rule("avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.").unwrap();
        assert_eq!(rule.aggregates.len(), 1);
        let agg = &rule.aggregates[0];
        assert_eq!(agg.kind, AggregateKind::Count);
        assert_eq!(agg.inner.predicate, "user_corrected");
        assert_eq!(agg.inner.args.len(), 2);
        assert_eq!(agg.output_var, "N");
        assert_eq!(agg.group_vars, vec!["X".to_string()]);
        assert_eq!(rule.filters.len(), 1);
    }

    #[test]
    fn intra_rule_recursion_through_count_now_rejected_at_evaluate_time() {
        // The v1 parse-time guard was removed in favour of the stratify
        // analyzer (Task M3) which catches cross-rule recursion too. The
        // rule now parses cleanly; evaluate-time rejection is asserted in
        // tests in Tasks M3 and M5.
        let rule = parse_rule("loop(X) :- count(loop(Y), N), N > 0.").unwrap();
        assert_eq!(rule.aggregates.len(), 1);
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

    #[test]
    fn evaluator_count_aggregate_groups_and_counts() {
        let s1 = Uuid::new_v4();
        let s2 = Uuid::new_v4();
        let s3 = Uuid::new_v4();
        let target_t = Uuid::new_v4(); // 3 distinct correctors
        let target_u = Uuid::new_v4(); // 2 distinct correctors

        let mut facts = FactSet::new();
        facts.insert(
            "user_corrected",
            vec![Term::Const(s1), Term::Const(target_t)],
        );
        facts.insert(
            "user_corrected",
            vec![Term::Const(s2), Term::Const(target_t)],
        );
        facts.insert(
            "user_corrected",
            vec![Term::Const(s3), Term::Const(target_t)],
        );
        facts.insert(
            "user_corrected",
            vec![Term::Const(s1), Term::Const(target_u)],
        );
        facts.insert(
            "user_corrected",
            vec![Term::Const(s2), Term::Const(target_u)],
        );

        let rule =
            parse_rule("avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.").unwrap();
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

        assert!(
            avoided.contains(&target_t),
            "3 distinct correctors should fire avoid_action"
        );
        assert!(
            !avoided.contains(&target_u),
            "2 distinct correctors should NOT fire avoid_action"
        );
    }

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
        facts.insert(
            "user_corrected",
            vec![Term::Const(s1), Term::Const(target_t)],
        );
        facts.insert(
            "user_corrected",
            vec![Term::Const(s2), Term::Const(target_t)],
        );
        facts.insert(
            "user_corrected",
            vec![Term::Const(s3), Term::Const(target_t)],
        );
        facts.insert(
            "user_corrected",
            vec![Term::Const(s1), Term::Const(target_u)],
        );
        facts.insert(
            "user_corrected",
            vec![Term::Const(s2), Term::Const(target_u)],
        );

        let rule =
            parse_rule("avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.").unwrap();
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

        assert!(
            avoided.contains(&target_t),
            "3 distinct correctors should derive avoid_action"
        );
        assert!(
            !avoided.contains(&target_u),
            "2 distinct correctors should NOT derive avoid_action"
        );
    }

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
        assert_eq!(agg.inner.predicate, "worked_well");
        let mut sorted_groups = agg.group_vars.clone();
        sorted_groups.sort();
        assert_eq!(sorted_groups, vec!["Ctx".to_string(), "Tool".to_string()]);
        assert_eq!(agg.output_var, "N");
    }

    #[test]
    fn parse_rule_rejects_aggregate_with_no_atoms() {
        let err = parse_rule("foo(X) :- count(N).").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("inner atom") || msg.contains("at least"),
            "expected at-least-one-atom error, got: {msg}"
        );
    }

    #[test]
    fn parse_rule_single_atom_aggregate_keeps_v1_shape() {
        let rule = parse_rule("foo(X) :- count(bar(X), N), N > 0.").unwrap();
        let agg = &rule.aggregates[0];
        assert!(agg.inner_conjunction.is_empty());
        assert_eq!(agg.inner.predicate, "bar");
    }

    // ─── M3: stratify tests ───────────────────────────────────────

    #[test]
    fn stratify_simple_chain_assigns_ascending_strata() {
        let r1 = parse_rule("b(X) :- a(X).").unwrap();
        let r2 = parse_rule("c(X) :- b(X).").unwrap();
        let strata = stratify(&[r1, r2]).unwrap();
        assert!(strata.len() >= 2);
        let r1_stratum = strata.iter().position(|s| s.contains(&0)).unwrap();
        let r2_stratum = strata.iter().position(|s| s.contains(&1)).unwrap();
        assert!(
            r1_stratum < r2_stratum,
            "b's rule must come before c's rule"
        );
    }

    #[test]
    fn stratify_aggregate_lifts_one_level() {
        let r = parse_rule("b(X) :- count(a(X), N), N > 0.").unwrap();
        let strata = stratify(&[r]).unwrap();
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
            other => panic!("expected RecursionThroughAggregate, got {other:?}"),
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
            other => panic!("expected RecursionThroughAggregate, got {other:?}"),
        }
    }

    #[test]
    fn stratify_allows_plain_recursion() {
        // path(X, Z) :- edge(X, Y), path(Y, Z). is recursive but only via Plain edges.
        let r = parse_rule("path(X, Z) :- edge(X, Y), path(Y, Z).").unwrap();
        let strata = stratify(&[r]).unwrap();
        assert!(strata.iter().any(|s| s.contains(&0)));
    }

    // ─── M4: stratum-by-stratum evaluator + conjunction backtracking ──

    #[test]
    fn evaluator_two_atom_conjunction_groups_correctly() {
        let s1 = Uuid::new_v4();
        let s2 = Uuid::new_v4();
        let s3 = Uuid::new_v4();
        let s4 = Uuid::new_v4();
        let ca = Uuid::new_v4();
        let cb = Uuid::new_v4();
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();

        let mut facts = FactSet::new();
        facts.insert("worked_well", vec![Term::Const(s1), Term::Const(t1)]);
        facts.insert("worked_well", vec![Term::Const(s2), Term::Const(t1)]);
        facts.insert("worked_well", vec![Term::Const(s3), Term::Const(t1)]);
        facts.insert("worked_well", vec![Term::Const(s1), Term::Const(t2)]);
        facts.insert("worked_well", vec![Term::Const(s2), Term::Const(t2)]);
        facts.insert("worked_well", vec![Term::Const(s4), Term::Const(t1)]);
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
                let (Term::Const(c), Term::Const(t)) = (args.first()?, args.get(1)?) else {
                    return None;
                };
                Some((*c, *t))
            })
            .collect();

        assert!(
            pairs.contains(&(ca, t1)),
            "(cA, t1) with 3 distinct sessions must fire"
        );
        assert!(
            !pairs.contains(&(ca, t2)),
            "(cA, t2) with 2 sessions must NOT fire"
        );
        assert!(
            !pairs.contains(&(cb, t1)),
            "(cB, t1) with 2 sessions must NOT fire"
        );
    }

    #[test]
    fn evaluator_existential_var_aggregated_over() {
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
        assert_eq!(
            row.len(),
            2,
            "head has only Ctx and Tool; S must not appear"
        );
    }

    #[test]
    fn evaluator_recursion_through_aggregate_emits_warn_and_no_facts() {
        let rule = parse_rule("loop(X) :- count(loop(Y), N), N > 0.").unwrap();
        let mut facts = FactSet::new();
        let x = Uuid::new_v4();
        facts.insert("loop", vec![Term::Const(x)]);

        let (derived, _) = evaluate(&[rule], &facts, 100, 1000);
        let derived_loop_count = derived.get("loop").map(|v| v.len()).unwrap_or(0);
        assert_eq!(
            derived_loop_count, 1,
            "stratification rejection must leave only the base fact"
        );
    }

    // ─── M5: Acceptance tests ─────────────────────────────────────

    #[test]
    fn acceptance_threshold_k_eq_3() {
        let rule = parse_rule(
            "preferred_tool(Ctx, Tool) :- count(worked_well(S, Tool), session_context(S, Ctx), N), N >= 3."
        ).unwrap();
        assert_eq!(rule.aggregates.len(), 1);
        assert_eq!(rule.aggregates[0].inner_conjunction.len(), 2);
    }

    #[test]
    fn acceptance_existential_quantification() {
        let rule = parse_rule(
            "preferred_tool(Ctx, Tool) :- count(worked_well(S, Tool), session_context(S, Ctx), N), N >= 3."
        ).unwrap();
        let agg = &rule.aggregates[0];
        assert!(
            !agg.group_vars.contains(&"S".to_string()),
            "S must be existentially quantified"
        );
        assert!(agg.group_vars.contains(&"Ctx".to_string()));
        assert!(agg.group_vars.contains(&"Tool".to_string()));
    }

    #[test]
    fn acceptance_recursion_rejected_at_load_time() {
        let rule = parse_rule("loop(X) :- count(loop(Y), N), N > 0.").unwrap();
        let mut facts = FactSet::new();
        let x = Uuid::new_v4();
        facts.insert("loop", vec![Term::Const(x)]);
        let (derived, _) = evaluate(&[rule], &facts, 100, 1000);
        assert_eq!(derived.get("loop").map(|v| v.len()).unwrap_or(0), 1);
        // No new facts should be derived from an unstratifiable rule set
    }

    #[test]
    fn acceptance_no_regression_on_v1_aggregation() {
        let s1 = Uuid::new_v4();
        let s2 = Uuid::new_v4();
        let s3 = Uuid::new_v4();
        let target = Uuid::new_v4();
        let mut facts = FactSet::new();
        facts.insert("user_corrected", vec![Term::Const(s1), Term::Const(target)]);
        facts.insert("user_corrected", vec![Term::Const(s2), Term::Const(target)]);
        facts.insert("user_corrected", vec![Term::Const(s3), Term::Const(target)]);
        let rule =
            parse_rule("avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.").unwrap();
        assert!(
            rule.aggregates[0].inner_conjunction.is_empty(),
            "v1 single-atom path"
        );
        let (derived, _) = evaluate(&[rule], &facts, 100, 1000);
        let any = derived.get("avoid_action").map(|v| v.len()).unwrap_or(0);
        assert_eq!(any, 1, "v1 single-atom aggregate must still fire");
    }

    // ── Property test (T-P-001) ───────────────────────────────────
    //
    // Replaces the former `tests/property/test_expert_system_properties.py`
    // pseudo-property test (which only grepped this file's source text) with a
    // real proptest that drives the actual `load_effective_rule_entries` loader
    // against MockStorage.
    mod effective_loader_property {
        use super::*;
        use crate::storage::mock::MockStorage;
        use crate::types::{ApprovalDecision, ApprovalEntry, ArtifactKind};
        use proptest::prelude::*;

        async fn register_approved_rule(store: &MockStorage, ctx: &TenantContext, rule_id: &str) {
            let now = chrono::Utc::now();
            store
                .rule_put(
                    ctx,
                    &RuleEntry {
                        tenant_id: ctx.tenant_id,
                        rule_id: rule_id.to_string(),
                        version: 1,
                        name: rule_id.to_string(),
                        family: "registry_fam".to_string(),
                        state: RuleState::Active,
                        rule_body: "registered(X) :- node(X).".to_string(),
                        rule_weight: 1.0,
                        incremental: false,
                        created_at: now,
                        updated_at: now,
                    },
                )
                .await
                .unwrap();
            // The loader only surfaces a registry rule once it is approved.
            store
                .approval_append(
                    ctx,
                    &ApprovalEntry {
                        tenant_id: ctx.tenant_id,
                        approval_id: Uuid::now_v7(),
                        artifact_kind: ArtifactKind::Rule,
                        artifact_ref: rule_id.to_string(),
                        decision: ApprovalDecision::Approved,
                        review_note: None,
                        reviewer: "tester".to_string(),
                        scope: "global".to_string(),
                        workspace_scope: None,
                        session_scope: None,
                        mirror_entity_id: crate::expert_system::approval_mirror_entity_id(
                            ArtifactKind::Rule,
                            rule_id,
                        ),
                        created_at: now,
                    },
                )
                .await
                .unwrap();
        }

        /// Collect the effective rule set as an order-independent, comparable key:
        /// (rule_id, is_registry) pairs, sorted.
        fn effective_key(entries: &[EffectiveRuleEntry]) -> Vec<(String, bool)> {
            let mut key: Vec<(String, bool)> = entries
                .iter()
                .map(|e| {
                    (
                        e.entry.rule_id.clone(),
                        matches!(e.source, RuleSource::Registry),
                    )
                })
                .collect();
            key.sort();
            key
        }

        proptest! {
            /// T-P-001 "effective loader is permutation-invariant": registering the
            /// same registry rules in two different orders yields an identical
            /// merged (builtin + registry) effective rule set.
            #[test]
            fn effective_loader_is_permutation_invariant(ids in prop::collection::vec(0u32..32, 1..8)) {
                // Distinct, stable rule ids regardless of generated duplicates.
                let mut unique: Vec<u32> = ids.clone();
                unique.sort_unstable();
                unique.dedup();
                let rule_ids: Vec<String> = unique.iter().map(|i| format!("reg-rule-{i}")).collect();

                // Same tenant -> identical synthetic builtins across both runs.
                let tenant_id = Uuid::new_v4();
                let rt = tokio::runtime::Runtime::new().unwrap();
                let (forward, reversed) = rt.block_on(async {
                    let ctx = TenantContext {
                        tenant_id,
                        session_origin: "tester".into(),
                    };

                    let store_a = MockStorage::new();
                    for id in rule_ids.iter() {
                        register_approved_rule(&store_a, &ctx, id).await;
                    }
                    let forward = load_effective_rule_entries(&store_a, &ctx, None)
                        .await
                        .unwrap();

                    let store_b = MockStorage::new();
                    for id in rule_ids.iter().rev() {
                        register_approved_rule(&store_b, &ctx, id).await;
                    }
                    let reversed = load_effective_rule_entries(&store_b, &ctx, None)
                        .await
                        .unwrap();

                    (effective_key(&forward), effective_key(&reversed))
                });

                prop_assert_eq!(forward, reversed);
            }
        }
    }
}

// ─── Negation Tests ───────────────────────────────────────────────

#[cfg(test)]
mod negation_tests {
    use super::*;
    use crate::types::{DatalogRule, Term};

    // ── Slice 1: syntax and representation ────────────────────────

    #[test]
    fn parse_rule_accepts_a_negated_body_atom() {
        let rule = parse_rule("shareable(X) :- item(X), not secret(X).").unwrap();
        assert_eq!(rule.body.len(), 1, "positive atoms stay in body");
        assert_eq!(rule.body[0].predicate, "item");
        assert_eq!(rule.negated.len(), 1, "negated atom lands in `negated`");
        assert_eq!(rule.negated[0].predicate, "secret");
        assert_eq!(rule.negated[0].args, vec![Term::Var("X".into())]);
    }

    #[test]
    fn parse_rule_accepts_a_multi_arg_negated_atom() {
        let rule = parse_rule("q(X, Y) :- p(X, Y), not r(X, Y).").unwrap();
        assert_eq!(rule.negated.len(), 1);
        assert_eq!(rule.negated[0].args.len(), 2);
    }

    #[test]
    fn parse_rule_treats_a_predicate_named_not_something_as_positive() {
        // `nothing(X)` must not be read as `not hing(X)`.
        let rule = parse_rule("q(X) :- nothing(X).").unwrap();
        assert_eq!(rule.body.len(), 1);
        assert_eq!(rule.body[0].predicate, "nothing");
        assert!(rule.negated.is_empty());
    }

    #[test]
    fn parse_rule_rejects_a_body_that_is_only_negated() {
        // With no positive atom there is nothing to range-restrict against.
        let err = parse_rule("q(X) :- not p(X).").unwrap_err().to_string();
        assert!(
            err.contains("positive"),
            "error should explain the missing positive atom, got: {err}"
        );
    }

    #[test]
    fn datalog_rule_without_negated_field_still_deserializes() {
        // Stored-format guard: RuleEntry rows written before negation existed.
        let json = r#"{"head":{"predicate":"q","args":[{"type":"Var","value":"X"}]},
                       "body":[{"predicate":"p","args":[{"type":"Var","value":"X"}]}],
                       "filters":[]}"#;
        let rule: DatalogRule = serde_json::from_str(json).unwrap();
        assert!(rule.negated.is_empty(), "absent field means no negation");
        assert_eq!(rule.body.len(), 1);
    }

    #[test]
    fn negated_rule_round_trips_through_serde() {
        let rule = parse_rule("q(X) :- p(X), not r(X).").unwrap();
        let json = serde_json::to_string(&rule).unwrap();
        let back: DatalogRule = serde_json::from_str(&json).unwrap();
        assert_eq!(back.negated.len(), 1);
        assert_eq!(back.negated[0].predicate, "r");
        assert_eq!(back, rule);
    }

    // ── Slice 2: range-restriction safety ─────────────────────────

    #[test]
    fn parse_rule_rejects_a_negated_variable_no_positive_atom_binds() {
        let err = parse_rule("q(X) :- p(X), not r(Y).")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains('Y'),
            "rejection must name the unbound variable, got: {err}"
        );
    }

    #[test]
    fn parse_rule_accepts_a_negated_variable_bound_by_a_positive_atom() {
        let rule = parse_rule("q(X) :- p(X), not r(X).").unwrap();
        assert_eq!(rule.negated.len(), 1);
    }

    #[test]
    fn parse_rule_allows_an_anonymous_variable_in_a_negated_atom() {
        // `_` is existential by construction — the parser renames each one
        // uniquely, so it can never be referenced by the head or a filter,
        // and checking it never enumerates the universe.
        let rule = parse_rule("q(X) :- p(X), not r(X, _).").unwrap();
        assert_eq!(rule.negated.len(), 1);
        assert_eq!(rule.negated[0].args.len(), 2);
    }

    // ── Slice 3: stratification ───────────────────────────────────

    #[test]
    fn stratify_lifts_a_negated_dependency_above_its_source() {
        let rules = vec![
            parse_rule("p(X) :- base(X).").unwrap(),
            parse_rule("q(X) :- base(X), not p(X).").unwrap(),
        ];
        let strata = stratify(&rules).unwrap();
        let stratum_of = |pred: &str| {
            strata
                .iter()
                .position(|group| group.iter().any(|i| rules[*i].head.predicate == pred))
                .unwrap()
        };
        assert!(
            stratum_of("q") > stratum_of("p"),
            "the negated relation must settle in a strictly lower stratum"
        );
    }

    #[test]
    fn stratify_rejects_recursion_through_negation() {
        let rules = vec![parse_rule("p(X) :- base(X), not p(X).").unwrap()];
        let err = stratify(&rules).unwrap_err();
        assert!(
            matches!(
                err,
                crate::types::StratifyError::RecursionThroughNegation { .. }
            ),
            "expected RecursionThroughNegation, got {err:?}"
        );
    }

    #[test]
    fn stratify_rejects_cross_rule_recursion_through_negation() {
        let rules = vec![
            parse_rule("p(X) :- base(X), not q(X).").unwrap(),
            parse_rule("q(X) :- base(X), p(X).").unwrap(),
        ];
        let err = stratify(&rules).unwrap_err();
        assert!(matches!(
            err,
            crate::types::StratifyError::RecursionThroughNegation { .. }
        ));
    }

    #[test]
    fn stratify_still_allows_plain_recursion_alongside_negation() {
        let rules = vec![
            parse_rule("reach(X, Y) :- edge(X, Y).").unwrap(),
            parse_rule("reach(X, Z) :- reach(X, Y), edge(Y, Z).").unwrap(),
            parse_rule("unreached(X) :- node(X), not reach(X, X).").unwrap(),
        ];
        assert!(stratify(&rules).is_ok());
    }

    // ── Slice 4: evaluation ───────────────────────────────────────

    fn facts(rows: &[(&str, Vec<Term>)]) -> FactSet {
        let mut fs = FactSet::new();
        for (pred, args) in rows {
            fs.insert(pred, args.clone());
        }
        fs
    }

    fn s(v: &str) -> Term {
        Term::ConstStr(v.to_string())
    }

    #[test]
    fn evaluate_excludes_bindings_the_negated_atom_matches() {
        let rules = vec![parse_rule("shareable(X) :- item(X), not secret(X).").unwrap()];
        let initial = facts(&[
            ("item", vec![s("a")]),
            ("item", vec![s("b")]),
            ("secret", vec![s("b")]),
        ]);
        let (all, _derived) = evaluate(&rules, &initial, 100, 1000);
        assert!(all.contains("shareable", &[s("a")]), "a is not secret");
        assert!(
            !all.contains("shareable", &[s("b")]),
            "b is secret and must be excluded"
        );
    }

    #[test]
    fn evaluate_keeps_every_binding_when_the_negated_relation_is_empty() {
        let rules = vec![parse_rule("shareable(X) :- item(X), not secret(X).").unwrap()];
        let initial = facts(&[("item", vec![s("a")]), ("item", vec![s("b")])]);
        let (all, _) = evaluate(&rules, &initial, 100, 1000);
        assert!(all.contains("shareable", &[s("a")]));
        assert!(all.contains("shareable", &[s("b")]));
    }

    #[test]
    fn evaluate_matches_a_negated_atom_on_all_its_bound_arguments() {
        // `not r(X, "red")` must only exclude X bound to a *red* r row.
        let rules = vec![parse_rule("q(X) :- p(X), not r(X, \"red\").").unwrap()];
        let initial = facts(&[
            ("p", vec![s("a")]),
            ("p", vec![s("b")]),
            ("r", vec![s("a"), s("red")]),
            ("r", vec![s("b"), s("blue")]),
        ]);
        let (all, _) = evaluate(&rules, &initial, 100, 1000);
        assert!(!all.contains("q", &[s("a")]), "a has a red r row");
        assert!(all.contains("q", &[s("b")]), "b's r row is blue, not red");
    }

    #[test]
    fn evaluate_treats_an_anonymous_negated_argument_as_existential() {
        let rules = vec![parse_rule("q(X) :- p(X), not r(X, _).").unwrap()];
        let initial = facts(&[
            ("p", vec![s("a")]),
            ("p", vec![s("b")]),
            ("r", vec![s("a"), s("anything")]),
        ]);
        let (all, _) = evaluate(&rules, &initial, 100, 1000);
        assert!(!all.contains("q", &[s("a")]), "some r row mentions a");
        assert!(all.contains("q", &[s("b")]), "no r row mentions b");
    }

    #[test]
    fn a_new_base_fact_falsifies_a_previously_derived_fact_on_re_evaluation() {
        let rules = vec![parse_rule("shareable(X) :- item(X), not secret(X).").unwrap()];
        let before = facts(&[("item", vec![s("a")])]);
        let (all_before, _) = evaluate(&rules, &before, 100, 1000);
        assert!(all_before.contains("shareable", &[s("a")]));

        // The non-monotonic case: adding a base fact removes a derived fact.
        let after = facts(&[("item", vec![s("a")]), ("secret", vec![s("a")])]);
        let (all_after, _) = evaluate(&rules, &after, 100, 1000);
        assert!(
            !all_after.contains("shareable", &[s("a")]),
            "re-evaluation must not re-derive the falsified fact"
        );
    }

    // ── Slice 5: provenance and confidence ────────────────────────

    #[test]
    fn provenance_names_the_absent_predicate() {
        let rules = vec![parse_rule("q(X) :- p(X), not r(X).").unwrap()];
        let initial = facts(&[("p", vec![s("a")])]);
        let (_, derived) = evaluate(&rules, &initial, 100, 1000);
        let fact = derived.iter().find(|d| d.pred == "q").expect("derived q");
        let absence = fact
            .provenance
            .iter()
            .find(|step| step.parent_kind == "absence")
            .expect("an absence provenance step");
        assert_eq!(absence.parent_pred, "r");
        assert_eq!(absence.parent_src, "a", "the absence records its binding");
    }

    #[test]
    fn an_absence_counts_toward_support_count() {
        let rules = vec![parse_rule("q(X) :- p(X), not r(X).").unwrap()];
        let initial = facts(&[("p", vec![s("a")])]);
        let (_, derived) = evaluate(&rules, &initial, 100, 1000);
        let fact = derived.iter().find(|d| d.pred == "q").unwrap();
        assert_eq!(
            fact.support_count, 2,
            "one positive atom + one absence = 2 support"
        );
    }

    #[test]
    fn an_absence_is_weaker_evidence_than_a_present_fact() {
        let positive = vec![parse_rule("q(X) :- p(X), r(X).").unwrap()];
        let negative = vec![parse_rule("q(X) :- p(X), not s(X).").unwrap()];
        let pos_facts = facts(&[("p", vec![s("a")]), ("r", vec![s("a")])]);
        let neg_facts = facts(&[("p", vec![s("a")])]);

        let (_, pos_derived) = evaluate(&positive, &pos_facts, 100, 1000);
        let (_, neg_derived) = evaluate(&negative, &neg_facts, 100, 1000);

        let pos_conf = pos_derived
            .iter()
            .find(|d| d.pred == "q")
            .unwrap()
            .confidence;
        let neg_conf = neg_derived
            .iter()
            .find(|d| d.pred == "q")
            .unwrap()
            .confidence;
        assert!(
            neg_conf < pos_conf,
            "absence ({neg_conf}) must weigh less than presence ({pos_conf})"
        );
    }

    // ── Slice 6: never cache a derivation that rests on an absence ─

    #[test]
    fn a_fact_derived_through_negation_is_not_cacheable() {
        let rules = vec![parse_rule("q(X) :- p(X), not r(X).").unwrap()];
        let initial = facts(&[("p", vec![Term::Const(uuid::Uuid::nil())])]);
        let (_, derived) = evaluate(&rules, &initial, 100, 1000);
        let fact = derived.iter().find(|d| d.pred == "q").unwrap();
        assert!(
            fact.rests_on_absence(),
            "the derivation rests on an absence"
        );
        assert!(
            !fact.is_cacheable(),
            "a non-monotonic derivation must never be persisted"
        );
    }

    // ── Slice 7: D4 — the tier floor as an exclusion ──────────────

    /// The current DIKW corpus: one tier per entity, four tiers.
    fn dikw_corpus() -> FactSet {
        facts(&[
            ("tier", vec![s("raw-log"), s("data")]),
            ("tier", vec![s("parsed-doc"), s("information")]),
            ("tier", vec![s("linked-fact"), s("knowledge")]),
            ("tier", vec![s("rust-skill"), s("wisdom")]),
        ])
    }

    fn shareable_set(rule_texts: &[&str], corpus: &FactSet) -> Vec<String> {
        let rules: Vec<_> = rule_texts.iter().map(|t| parse_rule(t).unwrap()).collect();
        let (all, _) = evaluate(&rules, corpus, 100, 10_000);
        let mut out: Vec<String> = all
            .get("shareable")
            .map(|rows| {
                rows.iter()
                    .filter_map(|args| args.first())
                    .map(term_to_string)
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    #[test]
    fn the_d4_tier_floor_and_its_exclusion_form_derive_the_same_set() {
        let corpus = dikw_corpus();

        // D4 as it is written today: the floor, enumerated upward. This is a
        // positive encoding of a negative intent, equivalent only while the
        // tier lattice stays totally ordered, closed, and one-tier-per-entity.
        let floor = shareable_set(
            &[
                "shareable(E) :- tier(E, \"knowledge\").",
                "shareable(E) :- tier(E, \"wisdom\").",
            ],
            &corpus,
        );

        // The same grant said as what it actually means: everything tiered,
        // except the two tiers below the floor.
        let exclusion = shareable_set(
            &["shareable(E) :- tier(E, _), not tier(E, \"data\"), \
               not tier(E, \"information\")."],
            &corpus,
        );

        assert_eq!(floor, vec!["linked-fact", "rust-skill"]);
        assert_eq!(
            exclusion, floor,
            "the exclusion form must derive exactly the floor's set"
        );
    }

    #[test]
    fn an_exclusion_off_the_tier_axis_is_now_expressible_at_all() {
        // "share everything except items tagged secret" has no positive
        // encoding as a tier floor — it does not lie along the tier axis.
        // This is the requirement that cost a bespoke enumeration in Rust.
        let corpus = facts(&[
            ("tier", vec![s("linked-fact"), s("knowledge")]),
            ("tier", vec![s("rust-skill"), s("wisdom")]),
            ("tier", vec![s("payroll"), s("wisdom")]),
            ("tagged", vec![s("payroll"), s("secret")]),
        ]);
        let got = shareable_set(
            &["shareable(E) :- tier(E, _), not tagged(E, \"secret\")."],
            &corpus,
        );
        assert_eq!(got, vec!["linked-fact", "rust-skill"]);
    }

    #[test]
    fn a_purely_positive_derivation_stays_cacheable() {
        let rules = vec![parse_rule("q(X, Y) :- p(X, Y).").unwrap()];
        let id_a = uuid::Uuid::new_v4();
        let id_b = uuid::Uuid::new_v4();
        let initial = facts(&[("p", vec![Term::Const(id_a), Term::Const(id_b)])]);
        let (_, derived) = evaluate(&rules, &initial, 100, 1000);
        let fact = derived.iter().find(|d| d.pred == "q").unwrap();
        assert!(!fact.rests_on_absence());
        assert!(fact.is_cacheable());
    }
}

// ─── Aggregate Grammar Tests (min/max/sum/avg) ────────────────────

#[cfg(test)]
mod aggregate_grammar_tests {
    use super::*;
    use crate::types::AggregateKind;

    fn facts(rows: &[(&str, Vec<Term>)]) -> FactSet {
        let mut fs = FactSet::new();
        for (pred, args) in rows {
            fs.insert(pred, args.clone());
        }
        fs
    }

    fn s(v: &str) -> Term {
        Term::ConstStr(v.to_string())
    }

    fn n(v: f64) -> Term {
        Term::ConstFloat(OrderedFloat(v))
    }

    /// Read the single value bound to `pred`'s second argument.
    fn one_value(all: &FactSet, pred: &str, key: &Term) -> Option<f64> {
        all.get(pred)?.iter().find_map(|args| {
            (args.first() == Some(key)).then(|| match args.get(1) {
                Some(Term::ConstFloat(OrderedFloat(f))) => *f,
                other => panic!("expected a number, got {other:?}"),
            })
        })
    }

    // ── Parsing ───────────────────────────────────────────────────

    #[test]
    fn parse_rule_supports_sum_min_max_avg_with_a_value_variable() {
        for (text, kind) in [
            (
                "total(X, T) :- account(X), sum(spend(X, A), A, T).",
                AggregateKind::Sum,
            ),
            (
                "lowest(X, T) :- account(X), min(spend(X, A), A, T).",
                AggregateKind::Min,
            ),
            (
                "highest(X, T) :- account(X), max(spend(X, A), A, T).",
                AggregateKind::Max,
            ),
            (
                "mean(X, T) :- account(X), avg(spend(X, A), A, T).",
                AggregateKind::Avg,
            ),
        ] {
            let rule = parse_rule(text).unwrap();
            assert_eq!(rule.aggregates.len(), 1, "{text}");
            let agg = &rule.aggregates[0];
            assert_eq!(agg.kind, kind, "{text}");
            assert_eq!(agg.value_var.as_deref(), Some("A"), "{text}");
            assert_eq!(agg.output_var, "T", "{text}");
            assert_eq!(agg.inner.predicate, "spend", "{text}");
        }
    }

    #[test]
    fn count_still_parses_and_carries_no_value_variable() {
        let rule = parse_rule("n(X, N) :- account(X), count(spend(X, A), N).").unwrap();
        let agg = &rule.aggregates[0];
        assert_eq!(agg.kind, AggregateKind::Count);
        assert_eq!(agg.value_var, None, "count folds rows, not values");
        assert_eq!(agg.output_var, "N");
    }

    #[test]
    fn a_value_aggregate_over_a_conjunction_keeps_every_inner_atom() {
        let rule =
            parse_rule("total(X, T) :- account(X), sum(spend(X, A), approved(A), A, T).").unwrap();
        let agg = &rule.aggregates[0];
        assert_eq!(agg.inner_conjunction.len(), 2);
        assert_eq!(agg.value_var.as_deref(), Some("A"));
    }

    #[test]
    fn a_value_aggregate_without_a_value_variable_is_rejected() {
        let err = parse_rule("total(X, T) :- account(X), sum(spend(X, A), T).")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("value") || err.contains("sum"),
            "should explain the missing value variable, got: {err}"
        );
    }

    #[test]
    fn a_value_variable_not_bound_by_the_inner_atoms_is_rejected() {
        let err = parse_rule("total(X, T) :- account(X), sum(spend(X, A), Z, T).")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains('Z'),
            "rejection must name the unbound value variable, got: {err}"
        );
    }

    #[test]
    fn the_value_variable_and_the_output_variable_must_differ() {
        let err = parse_rule("total(X, A) :- account(X), sum(spend(X, A), A, A).")
            .unwrap_err()
            .to_string();
        assert!(!err.is_empty(), "aggregating into its own input is a bug");
    }

    #[test]
    fn a_predicate_literally_named_sum_is_still_a_plain_atom() {
        // Same legacy escape `count(X, N)` already has.
        let rule = parse_rule("q(X, Y) :- sum(X, Y).").unwrap();
        assert!(rule.aggregates.is_empty());
        assert_eq!(rule.body.len(), 1);
        assert_eq!(rule.body[0].predicate, "sum");
    }

    // ── Evaluation ────────────────────────────────────────────────

    fn spend_corpus() -> FactSet {
        facts(&[
            ("account", vec![s("alice")]),
            ("account", vec![s("bob")]),
            ("spend", vec![s("alice"), n(10.0)]),
            ("spend", vec![s("alice"), n(30.0)]),
            ("spend", vec![s("alice"), n(60.0)]),
            ("spend", vec![s("bob"), n(5.0)]),
        ])
    }

    #[test]
    fn sum_folds_the_value_across_a_group() {
        let rules = vec![parse_rule("total(X, T) :- account(X), sum(spend(X, A), A, T).").unwrap()];
        let (all, _) = evaluate(&rules, &spend_corpus(), 100, 10_000);
        assert_eq!(one_value(&all, "total", &s("alice")), Some(100.0));
        assert_eq!(one_value(&all, "total", &s("bob")), Some(5.0));
    }

    #[test]
    fn min_and_max_pick_the_extremes_of_a_group() {
        let lo = vec![parse_rule("lo(X, T) :- account(X), min(spend(X, A), A, T).").unwrap()];
        let hi = vec![parse_rule("hi(X, T) :- account(X), max(spend(X, A), A, T).").unwrap()];
        let (all_lo, _) = evaluate(&lo, &spend_corpus(), 100, 10_000);
        let (all_hi, _) = evaluate(&hi, &spend_corpus(), 100, 10_000);
        assert_eq!(one_value(&all_lo, "lo", &s("alice")), Some(10.0));
        assert_eq!(one_value(&all_hi, "hi", &s("alice")), Some(60.0));
        assert_eq!(one_value(&all_lo, "lo", &s("bob")), Some(5.0));
    }

    #[test]
    fn avg_divides_the_sum_by_the_row_count() {
        let rules = vec![parse_rule("mean(X, T) :- account(X), avg(spend(X, A), A, T).").unwrap()];
        let (all, _) = evaluate(&rules, &spend_corpus(), 100, 10_000);
        assert_eq!(one_value(&all, "mean", &s("alice")), Some(100.0 / 3.0));
        assert_eq!(one_value(&all, "mean", &s("bob")), Some(5.0));
    }

    #[test]
    fn sum_over_no_rows_is_zero_and_the_rule_still_fires() {
        // A total of nothing is zero, and staying monotone matches `count`.
        let corpus = facts(&[("account", vec![s("carol")])]);
        let rules = vec![parse_rule("total(X, T) :- account(X), sum(spend(X, A), A, T).").unwrap()];
        let (all, _) = evaluate(&rules, &corpus, 100, 10_000);
        assert_eq!(one_value(&all, "total", &s("carol")), Some(0.0));
    }

    #[test]
    fn min_max_and_avg_over_no_rows_do_not_fire_at_all() {
        // There is no minimum of nothing. Emitting one would be a fabricated
        // value, so the rule must not fire rather than invent a sentinel.
        let corpus = facts(&[("account", vec![s("carol")])]);
        for (text, pred) in [
            ("lo(X, T) :- account(X), min(spend(X, A), A, T).", "lo"),
            ("hi(X, T) :- account(X), max(spend(X, A), A, T).", "hi"),
            ("mean(X, T) :- account(X), avg(spend(X, A), A, T).", "mean"),
        ] {
            let rules = vec![parse_rule(text).unwrap()];
            let (all, _) = evaluate(&rules, &corpus, 100, 10_000);
            assert!(
                all.get(pred).map(|r| r.is_empty()).unwrap_or(true),
                "{pred} must derive nothing over an empty group"
            );
        }
    }

    #[test]
    fn a_non_numeric_value_makes_the_group_derive_nothing_rather_than_a_wrong_total() {
        // Skipping the offending row would report a total that is quietly
        // wrong. The group is refused instead.
        let corpus = facts(&[
            ("account", vec![s("alice")]),
            ("spend", vec![s("alice"), n(10.0)]),
            ("spend", vec![s("alice"), s("not-a-number")]),
        ]);
        let rules = vec![parse_rule("total(X, T) :- account(X), sum(spend(X, A), A, T).").unwrap()];
        let (all, _) = evaluate(&rules, &corpus, 100, 10_000);
        assert!(
            all.get("total").map(|r| r.is_empty()).unwrap_or(true),
            "a group with a non-numeric value must not produce a total"
        );
    }

    #[test]
    fn a_post_aggregate_filter_applies_to_the_folded_value() {
        let rules =
            vec![parse_rule("big(X, T) :- account(X), sum(spend(X, A), A, T), T > 50.").unwrap()];
        let (all, _) = evaluate(&rules, &spend_corpus(), 100, 10_000);
        assert_eq!(one_value(&all, "big", &s("alice")), Some(100.0));
        assert_eq!(
            one_value(&all, "big", &s("bob")),
            None,
            "bob's 5 is below 50"
        );
    }

    #[test]
    fn folding_visits_a_large_inner_relation_without_materialising_it() {
        // 20k rows in one group. The fold is a streaming visitor over the
        // conjunction backtracker, so nothing proportional to the group is
        // ever collected; this would allocate a 20k-element Vec per group if
        // it were materialised.
        let mut corpus = FactSet::new();
        corpus.insert("account", vec![s("alice")]);
        for i in 1..=20_000u32 {
            corpus.insert("spend", vec![s("alice"), n(f64::from(i))]);
        }
        let rules = vec![parse_rule("total(X, T) :- account(X), sum(spend(X, A), A, T).").unwrap()];
        let (all, _) = evaluate(&rules, &corpus, 100, 200_000);
        // 1 + 2 + ... + 20000
        assert_eq!(one_value(&all, "total", &s("alice")), Some(200_010_000.0));
    }
}
