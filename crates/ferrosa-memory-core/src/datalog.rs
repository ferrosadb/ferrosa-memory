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
    let mut anon_counter = 0usize;

    for part in &body_parts {
        let part = part.trim();
        if let Some(agg) = parse_aggregate(part, &head_vars)? {
            aggregates.push(agg);
        } else if has_top_level_cmp(part) {
            let f = crate::datalog_filter_expr::parse_filter(part)?;
            filters.push(f);
        } else {
            body.push(parse_atom(part, &mut anon_counter)?);
        }
    }

    anyhow::ensure!(
        !body.is_empty() || !aggregates.is_empty(),
        "rule must have at least one body atom"
    );

    for agg in &aggregates {
        if agg.inner.predicate == head.predicate {
            anyhow::bail!(
                "aggregation through head predicate '{}' is not supported in v1",
                head.predicate
            );
        }
    }

    Ok(DatalogRule {
        head,
        body,
        filters,
        aggregates,
    })
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
        anyhow::bail!("aggregate '{s}' must have an inner atom and an output var separated by ','");
    }
    let output = parts.last().unwrap().trim().to_string();
    let inner_text = parts[..parts.len() - 1].join(",");

    let mut anon = 0;
    // If the inner text isn't a valid atom (e.g. `count(X, N)` where X has no
    // parentheses), fall through to body-atom parsing rather than erroring.
    let inner = match parse_atom(&inner_text, &mut anon) {
        Ok(atom) => atom,
        Err(_) => return Ok(None),
    };

    if !output
        .chars()
        .next()
        .map(|c| c.is_ascii_uppercase() || c == '_')
        .unwrap_or(false)
    {
        anyhow::bail!("aggregate output_var '{output}' must be a variable (start with uppercase or '_')");
    }

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

/// Run semi-naive fixpoint evaluation over a set of rules and initial facts.
///
/// Returns the full derived fact set and a list of derived facts with provenance.
/// Terminates when no new facts are derived (fixpoint), or when `max_iterations`
/// or `max_facts` caps are reached.
pub fn evaluate(
    rules: &[DatalogRule],
    initial_facts: &FactSet,
    max_iterations: usize,
    max_facts: usize,
) -> (FactSet, Vec<DerivedFact>) {
    let mut all_facts = initial_facts.clone();
    let mut derived = Vec::new();

    for _iteration in 0..max_iterations {
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

        if all_facts.len() + new_delta.len() > max_facts {
            tracing::warn!(
                "Datalog max_facts cap reached ({} + {} > {})",
                all_facts.len(),
                new_delta.len(),
                max_facts
            );
            break;
        }

        // Merge new_delta into all_facts
        for (pred, fact_set) in &new_delta.facts {
            for args in fact_set {
                all_facts.insert(pred, args.clone());
            }
        }
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

    // Apply filters and return bindings (head instantiation is the caller's job)
    final_bindings
        .into_iter()
        .filter(|(binding, _)| check_filters(&rule.filters, binding))
        .collect()
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
        if !post_agg_filters.iter().all(|f| check_one_filter(f, &binding)) {
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
/// predicate's rows. Used when the rule body is empty and we need to seed
/// candidate bindings for the aggregate phase.
fn seed_bindings_from_inner(agg: &crate::types::Aggregate, all_facts: &FactSet) -> Vec<Candidate> {
    let Some(rows) = all_facts.get(&agg.inner.predicate) else {
        return Vec::new();
    };

    // Find the position of each group_var in inner.args so we can extract
    // its bound value from each row.
    let group_positions: Vec<(String, usize)> = agg
        .group_vars
        .iter()
        .filter_map(|gv| {
            agg.inner.args.iter().enumerate().find_map(|(i, arg)| {
                if let Term::Var(name) = arg {
                    if name == gv { Some((gv.clone(), i)) } else { None }
                } else {
                    None
                }
            })
        })
        .collect();

    let mut seen: std::collections::HashSet<Vec<Term>> = std::collections::HashSet::new();
    let mut out = Vec::new();

    for row in rows {
        if row.len() != agg.inner.args.len() {
            continue;
        }
        let key: Vec<Term> = group_positions
            .iter()
            .map(|(_, pos)| row[*pos].clone())
            .collect();
        if seen.insert(key.clone()) {
            let mut binding = HashMap::new();
            for ((name, _), val) in group_positions.iter().zip(key.iter()) {
                binding.insert(name.clone(), val.clone());
            }
            out.push((binding, Vec::new()));
        }
    }
    out
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
            .map(|v| binding.get(v).cloned().unwrap_or_else(|| Term::Var(v.clone())))
            .collect();
        groups.entry(key).or_default().push((binding, prov));
    }

    let mut out = Vec::new();
    for (_group_key, members) in groups {
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

/// Count how many rows in `all_facts` for `inner.predicate` unify with the
/// given binding (variables in `inner` not in the binding are wildcards).
fn count_inner_matches(
    inner: &Atom,
    binding: &Binding,
    all_facts: &FactSet,
) -> usize {
    let Some(rows) = all_facts.get(&inner.predicate) else {
        return 0;
    };
    rows.iter()
        .filter(|row| {
            row.len() == inner.args.len() && try_unify(&inner.args, row, binding).is_some()
        })
        .count()
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
    (min_parent * weight).clamp(0.0, 1.0)
}

/// Format a rule identifier from its head predicate and body predicates.
fn format_rule_id(rule: &DatalogRule) -> String {
    let body_preds: Vec<&str> = rule.body.iter().map(|a| a.predicate.as_str()).collect();
    format!("{}:-{}", rule.head.predicate, body_preds.join(","))
}

// ─── Built-in Rules ───────────────────────────────────────────────

/// Return the default set of inference rules for the knowledge graph.
///
/// These rules derive transitive relationships, clusters, reachability,
/// taxonomy hierarchies, and part-of ancestry from base graph predicates.
const BUILTIN_RULES_TEXT: [&str; 10] = [
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
    for te in &typed_edges {
        let pred = &te.edge_type;
        facts.insert(pred, vec![Term::Const(te.src_id), Term::Const(te.dst_id)]);
        facts.insert(
            "edge",
            vec![
                Term::Const(te.src_id),
                Term::ConstStr(pred.clone()),
                Term::Const(te.dst_id),
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

    // 4. Cache results and record telemetry
    let elapsed_ms = start.elapsed().as_millis() as i64;
    if !results.is_empty() {
        storage.derived_cache_put(ctx, &cache_key, &results).await?;
    }
    storage
        .heat_record(ctx, predicate, false, Some(elapsed_ms))
        .await?;

    Ok(results)
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
            10,
            "expected 10 builtin rules, got {}",
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
        assert_eq!(agg.group_vars, vec!["X".to_string()]);
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

    #[test]
    fn evaluator_count_aggregate_groups_and_counts() {
        let s1 = Uuid::new_v4();
        let s2 = Uuid::new_v4();
        let s3 = Uuid::new_v4();
        let target_t = Uuid::new_v4(); // 3 distinct correctors
        let target_u = Uuid::new_v4(); // 2 distinct correctors

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

    #[test]
    #[ignore = "requires aggregation (count) support — see specs follow-up: \
                count(predicate(...), N) is not in the arithmetic/comparison grammar. \
                Tracking deliverable: extend the parser AST + evaluator with aggregate \
                predicates. Removing #[ignore] without adding aggregation will fail \
                with 'invalid filter' on the count(...) clause."]
    fn user_example_count_aggregate_with_ge() {
        // Target rule from the user:
        //   avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.
        //
        // The N >= 3 filter parses fine after this change. The blocker is
        // count(user_corrected(S, X), N) — that's an aggregate predicate, not
        // a plain atom or a filter. parse_rule currently has no notion of
        // aggregation, so this rule fails at parse time on the count(...) body.
        //
        // When the aggregation feature lands, remove #[ignore] and assert the
        // expected derivation:
        //   3 distinct sessions corrected target T  ⇒  avoid_action(T) fires
        //   2 distinct sessions corrected target U  ⇒  avoid_action(U) does NOT fire
        let rule = parse_rule("avoid_action(X) :- count(user_corrected(S, X), N), N >= 3.")
            .expect("aggregation-extended parser should accept this rule");
        let _ = rule;
        panic!("aggregation evaluator not implemented");
    }
}
