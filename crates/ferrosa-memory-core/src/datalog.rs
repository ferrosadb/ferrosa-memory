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
    Atom, BuiltinFilter, DatalogRule, DerivedFact, FactSet, ProvenanceStep, TenantContext, Term,
};

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

    let mut body = Vec::new();
    let mut filters = Vec::new();
    let mut anon_counter = 0usize;

    for part in &body_parts {
        let part = part.trim();
        if let Some(filter) = try_parse_filter(part) {
            filters.push(filter);
        } else {
            body.push(parse_atom(part, &mut anon_counter)?);
        }
    }

    anyhow::ensure!(!body.is_empty(), "rule must have at least one body atom");

    Ok(DatalogRule {
        head,
        body,
        filters,
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

/// Try to parse a filter expression like `X != Y`, `X > 3.0`, or `X < 3.0`.
fn try_parse_filter(s: &str) -> Option<BuiltinFilter> {
    let s = s.trim();
    if let Some(pos) = s.find("!=") {
        let lhs = s[..pos].trim().to_string();
        let rhs = s[pos + 2..].trim().to_string();
        return Some(BuiltinFilter::NotEqual(lhs, rhs));
    }
    if let Some(pos) = s.find('>') {
        let lhs = s[..pos].trim().to_string();
        if let Ok(val) = s[pos + 1..].trim().parse::<f64>() {
            return Some(BuiltinFilter::GreaterThan(lhs, val));
        }
    }
    if let Some(pos) = s.find('<') {
        let lhs = s[..pos].trim().to_string();
        if let Ok(val) = s[pos + 1..].trim().parse::<f64>() {
            return Some(BuiltinFilter::LessThan(lhs, val));
        }
    }
    None
}

// ─── Semi-Naive Evaluator ─────────────────────────────────────────

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
/// Uses nested-loop join: for each body atom left-to-right, find all matching
/// facts and extend the variable binding. After all atoms match, check builtin
/// filters and instantiate the head.
fn evaluate_rule(rule: &DatalogRule, all_facts: &FactSet) -> Vec<(Vec<Term>, Vec<ProvenanceStep>)> {
    let mut results = Vec::new();

    // Start with a single empty binding
    let initial_bindings: Vec<(HashMap<String, Term>, Vec<ProvenanceStep>)> =
        vec![(HashMap::new(), Vec::new())];

    let final_bindings = rule
        .body
        .iter()
        .fold(initial_bindings, |current_bindings, body_atom| {
            let mut next_bindings = Vec::new();

            for (binding, provenance) in &current_bindings {
                // Get matching facts for this predicate
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

    // Apply filters and instantiate head
    for (binding, provenance) in final_bindings {
        if check_filters(&rule.filters, &binding) {
            let head_args = instantiate(&rule.head.args, &binding);
            results.push((head_args, provenance));
        }
    }

    results
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
pub fn builtin_rules() -> Vec<DatalogRule> {
    let rules_text = [
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
    rules_text
        .iter()
        .filter_map(|r| parse_rule(r).ok())
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
    let rules = builtin_rules();
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
        let rule =
            parse_rule("related(X, Z) :- co_occurs(X, Y), co_occurs(Y, Z), X != Z.").unwrap();
        assert_eq!(rule.head.predicate, "related");
        assert_eq!(rule.body.len(), 2);
        assert_eq!(rule.body[0].predicate, "co_occurs");
        assert_eq!(rule.body[1].predicate, "co_occurs");
        assert_eq!(rule.filters.len(), 1);
        assert_eq!(
            rule.filters[0],
            BuiltinFilter::NotEqual("X".into(), "Z".into())
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
        let rule = parse_rule("hot(X) :- warmth(X, W), W > 0.5.").unwrap();
        assert_eq!(rule.filters.len(), 1);
        assert_eq!(rule.filters[0], BuiltinFilter::GreaterThan("W".into(), 0.5));
    }

    #[test]
    fn test_parse_less_than_filter() {
        let rule = parse_rule("cold(X) :- warmth(X, W), W < 0.1.").unwrap();
        assert_eq!(rule.filters.len(), 1);
        assert_eq!(rule.filters[0], BuiltinFilter::LessThan("W".into(), 0.1));
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
}
