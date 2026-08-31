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

/// The most distinct values one `count_distinct` group may hold.
///
/// Every other fold streams: it keeps one accumulator no matter how large the
/// group. Distinctness cannot — it has to remember what it has already seen,
/// and that set grows with the answer.
///
/// So it is bounded, and the bound is loud. Past it the group derives nothing
/// rather than a truncated count, because a count that silently omits values
/// is a number the caller cannot tell from a real one. Deliberately
/// conservative; raise it if a real group needs more.
pub const DISTINCT_VALUE_CAP: usize = 10_000;

/// The most values one whole-group fold may retain.
///
/// `median`, `percentile` and `group_concat` cannot stream: an answer does not
/// exist until the group is ordered, so the values have to be kept. Lower than
/// `DISTINCT_VALUE_CAP` because these retain every value, not only the
/// distinct ones. Past it the group derives nothing rather than a statistic
/// computed from a truncated sample.
pub const RETAINED_VALUE_CAP: usize = 10_000;

/// The most rules one disjunctive rule may expand to.
///
/// Alternatives multiply: N binary groups is 2^N rules. The cap turns a
/// runaway into an error naming it, rather than one rule quietly becoming
/// hundreds that all have to be evaluated.
const MAX_RULE_ALTERNATIVES: usize = 64;

/// Parse a rule, expanding `;` alternatives into one rule each.
///
/// Disjunction is handled here rather than in the evaluator: expanding to
/// disjunctive normal form at load costs nothing at evaluation time and means
/// the evaluator never learns a new body shape.
pub fn parse_rules(text: &str) -> anyhow::Result<Vec<DatalogRule>> {
    let text = text.trim().trim_end_matches('.').trim();
    let sep = text
        .find(":-")
        .ok_or_else(|| anyhow::anyhow!("rule must contain ':-' separator"))?;
    let head_str = &text[..sep];
    let bodies = expand_disjunction(text[sep + 2..].trim())?;
    anyhow::ensure!(
        bodies.len() <= MAX_RULE_ALTERNATIVES,
        "rule expands to {} alternatives, more than the {MAX_RULE_ALTERNATIVES} allowed; \
         split it into separate rules so the cost is visible",
        bodies.len()
    );
    bodies
        .iter()
        .map(|body| parse_rule(&format!("{head_str} :- {body}.")))
        .collect()
}

/// Expand a body into one conjunction per alternative.
///
/// `,` binds tighter than `;`, as in Prolog, so `a, b ; c` is `(a, b) ; c`.
/// A parenthesised group holding alternatives distributes over the rest of the
/// conjunction, which is what makes `a, (b ; c)` two rules rather than one.
fn expand_disjunction(body: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for alternative in split_top_level(body, ';')? {
        let parts = split_top_level(&alternative, ',')?;
        // Each part expands to one or more conjunct strings; the alternatives
        // of the whole are the cartesian product of the parts' expansions.
        let mut combos: Vec<Vec<String>> = vec![Vec::new()];
        for part in &parts {
            let trimmed = part.trim();
            let expansions = match strip_group(trimmed) {
                Some(inner) => expand_disjunction(inner)?,
                None => vec![trimmed.to_string()],
            };
            anyhow::ensure!(
                combos.len() * expansions.len() <= MAX_RULE_ALTERNATIVES,
                "rule expands past the {MAX_RULE_ALTERNATIVES}-alternative cap; \
                 split it into separate rules so the cost is visible"
            );
            combos = combos
                .into_iter()
                .flat_map(|prefix| {
                    expansions.iter().map(move |e| {
                        let mut next = prefix.clone();
                        next.push(e.clone());
                        next
                    })
                })
                .collect();
        }
        out.extend(combos.into_iter().map(|c| c.join(", ")));
    }
    Ok(out)
}

/// The inside of a parenthesised group that holds alternatives, if this part
/// is one. A group without a `;` is left alone — it may be a filter.
fn strip_group(part: &str) -> Option<&str> {
    let inner = part.strip_prefix('(')?.strip_suffix(')')?;
    // Only a *balanced* outer pair counts: `(a) ; (b)` is not one group.
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    split_top_level(inner, ';')
        .ok()
        .filter(|alts| alts.len() > 1)
        .map(|_| inner)
}

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

    // `parse_rule` is used where exactly one rule is expected. Returning the
    // first alternative and dropping the rest would be silently wrong, so a
    // disjunctive body is refused here and callers that want it use
    // `parse_rules`.
    if let Ok(alternatives) = expand_disjunction(body_str)
        && alternatives.len() > 1
    {
        anyhow::bail!(
            "rule body has {} alternatives; use parse_rules to expand them",
            alternatives.len()
        );
    }

    let (head, head_exprs) = parse_head(head_str)?;

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
    let mut deferred_bindings: Vec<String> = Vec::new();
    let mut anon_counter = 0usize;

    // Pass 1: classify each body part; defer aggregate and negated parts for
    // Pass 2 (both need the positive body atoms parsed first — aggregates for
    // body_vars, negated atoms for the range-restriction check).
    for part in &body_parts {
        let part = part.trim();
        if let Some(rest) = strip_not_prefix(part) {
            deferred_negated.push(rest.to_string());
        } else if split_assignment(part).is_some() {
            deferred_bindings.push(part.to_string());
        } else if is_boolean_filter_part(part) || is_reserved_str_part(part) {
            filters.push(crate::datalog_filter_expr::parse_filter(part)?);
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

    // Bindings run in order after the positive atoms, so each may use what the
    // body bound and what an earlier binding named — but nothing later.
    let mut bindings: Vec<crate::types::Binding> = Vec::new();
    let mut bound_so_far: std::collections::HashSet<String> = positive_vars.clone();
    bound_so_far.extend(aggregates.iter().map(|a| a.output_var.clone()));
    for part in &deferred_bindings {
        let (var, rhs) = split_assignment(part).expect("classified as an assignment");
        let var = var.trim().to_string();
        anyhow::ensure!(
            var.starts_with(|c: char| c.is_ascii_uppercase() || c == '_'),
            "'{var}' is not a variable, so ':=' has nothing to bind"
        );
        anyhow::ensure!(
            !bound_so_far.contains(&var),
            "'{var}' is already bound; ':=' names a NEW value, and rebinding \
             would make the rule read two ways depending on evaluation order"
        );
        let expr = crate::datalog_filter_expr::parse_head_expr(rhs)?;
        let mut used = Vec::new();
        collect_expr_vars(&expr, &mut used);
        let unbound: Vec<String> = used
            .into_iter()
            .filter(|v| !bound_so_far.contains(v))
            .collect();
        anyhow::ensure!(
            unbound.is_empty(),
            "binding '{var} := {}' uses variable(s) {} that nothing has bound yet",
            rhs.trim(),
            unbound.join(", ")
        );
        bound_so_far.insert(var.clone());
        bindings.push(crate::types::Binding { var, expr });
    }

    // A computed head argument may only use variables the body binds; there is
    // nothing else to compute from.
    let agg_outputs: std::collections::HashSet<String> = bound_so_far.clone();
    for he in &head_exprs {
        let mut unbound: Vec<String> = Vec::new();
        collect_expr_vars(&he.expr, &mut unbound);
        let unbound: Vec<String> = unbound
            .into_iter()
            .filter(|v| !agg_outputs.contains(v))
            .collect();
        anyhow::ensure!(
            unbound.is_empty(),
            "head expression at argument {} uses variable(s) {} that no body atom binds",
            he.index,
            unbound.join(", ")
        );
    }

    Ok(DatalogRule {
        head,
        body,
        filters,
        aggregates,
        negated,
        head_exprs,
        bindings,
    })
}

/// Split `NAME := expr` at a top-level `:=`.
///
/// `=` is deliberately not accepted: it already parses to `CmpOp::Eq`, and
/// redefining it would silently change the meaning of rules already stored.
fn split_assignment(part: &str) -> Option<(&str, &str)> {
    let bytes = part.as_bytes();
    let (mut depth, mut in_str, mut i) = (0i32, false, 0usize);
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
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b':' if depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == b'=' => {
                return Some((&part[..i], &part[i + 2..]));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Collect every variable referenced by a filter expression.
fn collect_expr_vars(e: &crate::types::FilterExpr, out: &mut Vec<String>) {
    use crate::types::FilterExpr;
    match e {
        FilterExpr::Var(name) => out.push(name.clone()),
        FilterExpr::LitNum(_)
        | FilterExpr::LitStr(_)
        | FilterExpr::Null
        | FilterExpr::LitTime(_) => {}
        FilterExpr::Neg(inner) => collect_expr_vars(inner, out),
        FilterExpr::BinOp { lhs, rhs, .. } => {
            collect_expr_vars(lhs, out);
            collect_expr_vars(rhs, out);
        }
        FilterExpr::Call { args, .. } => {
            for a in args {
                collect_expr_vars(a, out);
            }
        }
    }
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

/// Parse a rule head, splitting computed arguments out of the atom.
///
/// A head argument holding a top-level arithmetic operator is an expression;
/// anything else is an ordinary term. The atom keeps a placeholder variable at
/// each computed position so arities and downstream code are unchanged, and
/// the expression is carried alongside.
fn parse_head(head_str: &str) -> anyhow::Result<(Atom, Vec<crate::types::HeadExpr>)> {
    let Some(open) = head_str.find('(') else {
        return Ok((parse_atom(head_str, &mut 0)?, Vec::new()));
    };
    let Some(close) = head_str.rfind(')') else {
        return Ok((parse_atom(head_str, &mut 0)?, Vec::new()));
    };
    let predicate = head_str[..open].trim();
    let arg_parts = split_top_level(&head_str[open + 1..close], ',')?;

    let mut rewritten: Vec<String> = Vec::with_capacity(arg_parts.len());
    let mut head_exprs = Vec::new();
    for (index, part) in arg_parts.iter().enumerate() {
        if has_top_level_arith(part) || looks_like_call(part) {
            head_exprs.push(crate::types::HeadExpr {
                index,
                expr: crate::datalog_filter_expr::parse_head_expr(part)?,
            });
            rewritten.push(format!("_headexpr_{index}"));
        } else {
            rewritten.push(part.trim().to_string());
        }
    }

    let atom = parse_atom(&format!("{predicate}({})", rewritten.join(", ")), &mut 0)?;
    Ok((atom, head_exprs))
}

/// True when a head argument is shaped like a function call.
///
/// The name is not checked here. Routing it to the expression parser is what
/// makes an unknown name an error naming it, rather than a term that quietly
/// becomes the string "frobnicate(V)".
fn looks_like_call(part: &str) -> bool {
    let part = part.trim();
    let Some(open) = part.find('(') else {
        return false;
    };
    if !part.ends_with(')') {
        return false;
    }
    let name = &part[..open];
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// True when a head argument carries an arithmetic operator at the top level.
///
/// A leading `-` is a negative literal, not an operator, so it does not count.
fn has_top_level_arith(part: &str) -> bool {
    let part = part.trim();
    let bytes = part.as_bytes();
    let (mut depth, mut in_str, mut i) = (0i32, false, 0usize);
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
            // `*`, `/` and `%` never appear inside a uuid, a date or an
            // identifier, so they are unambiguous on sight.
            b'*' | b'/' | b'%' if depth == 0 => return true,
            // `+` and `-` do appear inside uuids (550e8400-e29b-…) and dates
            // (2026-01-15), so they only count as operators when written with
            // space around them, which is how arithmetic is actually written.
            b'+' | b'-'
                if depth == 0
                    && i > 0
                    && bytes[i - 1].is_ascii_whitespace()
                    && i + 1 < bytes.len()
                    && bytes[i + 1].is_ascii_whitespace() =>
            {
                return true;
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// Split a string on a delimiter, but only at top level (not inside parentheses).
fn split_top_level(s: &str, delim: char) -> anyhow::Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0u32;

    let mut in_str = false;
    let mut escaped = false;

    for ch in s.chars() {
        // A comma inside a string literal is text, not a separator. Without
        // this, `p(X, "a,b")` splits in the middle of its own argument.
        if in_str {
            current.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if ch == '"' {
            in_str = true;
            current.push(ch);
            continue;
        }
        // Brackets nest exactly like parentheses here: a set literal's commas
        // separate its elements, not the rule's body parts.
        if ch == '(' || ch == '[' {
            depth += 1;
            current.push(ch);
        } else if ch == ')' || ch == ']' {
            anyhow::ensure!(depth > 0, "unmatched closing parenthesis or bracket");
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
        // Only a head computes. Left alone, `warmth(X, W + 1)` would become a
        // *variable named* "W + 1" — unbound, therefore matching every row, so
        // the rule would fire on everything and look like it worked.
        anyhow::ensure!(
            !has_top_level_arith(part),
            "'{}' in atom '{}' looks like arithmetic, but a body atom is a \
             pattern to match, not an expression to compute. Only a rule head \
             may compute an argument.",
            part.trim(),
            text
        );
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
    // Exactly `null`, lowercase. A variable is uppercase-initial, so
    // `Nullable` is untouched, and a quoted "null" is still the string.
    if s == "null" {
        return Term::ConstNull;
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

    if kind.needs_param() && parts.len() < 4 {
        anyhow::bail!(
            "aggregate '{s}' needs a literal between its value var and its output: \
             {}(atom.., Value, {}, Out)",
            kind.keyword(),
            if kind == crate::types::AggregateKind::Percentile {
                "Fraction"
            } else {
                "Separator"
            }
        );
    }

    // The literal parameter, when the kind takes one, sits just before the
    // output var.
    let param = if kind.needs_param() {
        let raw = parts[parts.len() - 2].trim();
        let term = parse_term(raw, &mut 0);
        match kind {
            crate::types::AggregateKind::Percentile => {
                let Term::ConstFloat(OrderedFloat(p)) = term else {
                    anyhow::bail!("percentile fraction '{raw}' must be a number");
                };
                anyhow::ensure!(
                    (0.0..=1.0).contains(&p),
                    "percentile fraction {p} is outside 0..=1, so it names no \
                     position in the group"
                );
                Some(Term::ConstFloat(OrderedFloat(p)))
            }
            _ => {
                let Term::ConstStr(sep) = term else {
                    anyhow::bail!("group_concat separator '{raw}' must be a quoted string");
                };
                Some(Term::ConstStr(sep))
            }
        }
    } else {
        None
    };

    // For a value aggregate the second-to-last part is the value variable.
    // Atoms always carry parentheses, so a bare identifier here is
    // unambiguous.
    let tail = if kind.needs_param() { 3 } else { 2 };
    let (value_var, atom_parts) = if kind.needs_value_var() {
        let raw = parts[parts.len() - tail].trim().to_string();
        if raw.contains('(') {
            anyhow::bail!(
                "aggregate '{s}' needs a value variable before its output var, \
                 got the atom '{raw}'"
            );
        }
        (Some(raw), &parts[..parts.len() - tail])
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
        param,
        inner,
        inner_conjunction,
        group_vars,
        output_var: output,
        value_var,
    }))
}

/// True for a body part that opens with `!`, joins with `||` / `&&`, or holds
/// a set literal at the top level — each of which makes it a filter even when
/// it carries no comparison `has_top_level_cmp` can see.
fn is_boolean_filter_part(part: &str) -> bool {
    let part = part.trim();
    if part.starts_with('!') {
        return true;
    }
    let bytes = part.as_bytes();
    let (mut depth, mut in_str, mut i) = (0i32, false, 0usize);
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
            b'|' | b'&' if depth == 0 && i + 1 < bytes.len() && bytes[i + 1] == c => {
                return true;
            }
            // A set literal. Atoms never use brackets, so a top-level `[`
            // means `expr in [...]` and nothing else.
            b'[' if depth == 0 => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// True for a body part that claims the reserved `str_` prefix.
///
/// The prefix is reserved so a typo cannot quietly become a relation over
/// stored facts — `str_startswith(N, "a")` would then derive nothing and look
/// exactly like "no rows matched". Claiming the prefix means the filter parser
/// gets the part and rejects it by name if it is not a real builtin.
///
/// The bare names were not taken: `contains` is already an edge type here, so
/// `contains(X, Y)` is a legitimate stored relation.
fn is_reserved_str_part(part: &str) -> bool {
    let bare = part.trim_start_matches('!').trim_start();
    bare.starts_with("str_") || bare.starts_with("is_null(") || bare.starts_with("geo_")
}

/// The aggregate kind a body part is written with, if any.
///
/// Matches only `kind(`, so a predicate literally named `sum` keeps the same
/// legacy escape `count` already had: `sum(X, Y)` fails to parse as an
/// aggregate and falls through to plain body-atom parsing.
fn aggregate_keyword(part: &str) -> Option<crate::types::AggregateKind> {
    use crate::types::AggregateKind::{
        Avg, Count, CountDistinct, GroupConcat, Max, Median, Min, Percentile, StdDev, Sum,
    };
    // `count_distinct` before `count`: prefix matching includes the `(`, so
    // they cannot actually collide, but keeping the longer name first makes
    // that independent of how the match is written.
    [
        CountDistinct,
        Count,
        Sum,
        Min,
        Max,
        Avg,
        StdDev,
        Median,
        Percentile,
        GroupConcat,
    ]
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
    evaluate_at(
        rules,
        initial_facts,
        max_iterations,
        max_facts,
        chrono::Utc::now(),
    )
}

/// Evaluate against a given instant.
///
/// The clock is read ONCE, here, and stamped into the rules before the
/// fixpoint runs. Reading it per row would let two rows in one evaluation
/// disagree about what "now" is, and a rule near a boundary would then include
/// one and exclude the other for no reason a person could see.
///
/// Taking the instant as an argument is also what makes any of it testable.
pub fn evaluate_at(
    rules: &[DatalogRule],
    initial_facts: &FactSet,
    max_iterations: usize,
    max_facts: usize,
    now: chrono::DateTime<chrono::Utc>,
) -> (FactSet, Vec<DerivedFact>) {
    let stamped: Vec<DatalogRule> = rules.iter().map(|r| stamp_clock(r, now)).collect();
    let rules = &stamped[..];
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
                    // A clock-dependent derivation is non-monotonic in the same
                    // way a negated one is: it stops being true with no base
                    // fact changing. Recording that in provenance is what makes
                    // `is_cacheable` refuse it, by the mechanism absence
                    // already uses.
                    let mut provenance_steps = provenance_steps;
                    if rule_reads_the_clock(rule) {
                        provenance_steps.push(ProvenanceStep {
                            parent_src: String::new(),
                            parent_pred: "now".to_string(),
                            parent_dst: String::new(),
                            parent_kind: crate::types::PROVENANCE_KIND_CLOCK.to_string(),
                        });
                    }
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
        .filter_map(|(binding, prov)| {
            let args = instantiate_head(rule, &binding)?;
            Some((args, prov))
        })
        .collect()
}

/// Instantiate a head, computing any expression arguments.
///
/// `None` when an expression has no value — a non-number, or a zero divisor.
/// Deriving the fact anyway would leave an argument the caller cannot tell
/// from a real one, so the rule does not fire for that binding.
fn instantiate_head(rule: &DatalogRule, binding: &Binding) -> Option<Vec<Term>> {
    let mut args = instantiate(&rule.head.args, binding);
    for he in &rule.head_exprs {
        let value = match eval_expr(&he.expr, binding) {
            Eval::Value(EvalValue::Num(f)) => Term::ConstFloat(OrderedFloat(f)),
            Eval::Value(EvalValue::Str(s)) => Term::ConstStr(s),
            Eval::Value(EvalValue::Uuid(u)) => Term::Const(u),
            Eval::Value(EvalValue::Time(t)) => Term::ConstTime(t),
            // A geometry is a computed intermediate, not something a fact can
            // hold — there is no geometry term, on purpose. Keep the GeoJSON
            // text and call `geo()` where the question is asked.
            Eval::Value(EvalValue::Geo(_)) => {
                tracing::warn!(
                    predicate = %rule.head.predicate,
                    "datalog: a geometry cannot be a head argument; store its GeoJSON text"
                );
                return None;
            }
            // Null is a value, so the rule fires carrying it. Only an error or
            // an undecidable expression stops the head being built.
            Eval::Null => Term::ConstNull,
            Eval::Undefined | Eval::Unbound => {
                tracing::warn!(
                    predicate = %rule.head.predicate,
                    index = he.index,
                    "datalog: head expression has no value; the rule does not fire"
                );
                return None;
            }
        };
        *args.get_mut(he.index)? = value;
    }
    Some(args)
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
        // Bindings come before filters, because a filter may test what a
        // binding named. A binding with no value drops the candidate: firing
        // with a missing value would derive a row the caller cannot tell from
        // a real one.
        .filter_map(|(mut binding, prov)| {
            for b in &rule.bindings {
                let value = match eval_expr(&b.expr, &binding) {
                    Eval::Value(EvalValue::Num(f)) => Term::ConstFloat(OrderedFloat(f)),
                    Eval::Value(EvalValue::Str(s)) => Term::ConstStr(s),
                    Eval::Value(EvalValue::Uuid(u)) => Term::Const(u),
                    Eval::Value(EvalValue::Time(t)) => Term::ConstTime(t),
                    Eval::Value(EvalValue::Geo(_)) => {
                        tracing::warn!(
                            var = %b.var,
                            "datalog: a geometry cannot be bound; keep its GeoJSON text"
                        );
                        return None;
                    }
                    Eval::Null => Term::ConstNull,
                    Eval::Undefined | Eval::Unbound => {
                        tracing::warn!(
                            var = %b.var,
                            "datalog: binding has no value; the rule does not fire"
                        );
                        return None;
                    }
                };
                binding.insert(b.var.clone(), value);
            }
            Some((binding, prov))
        })
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
        // Phase 1 collects bindings only; the head is instantiated by the
        // caller, which is where head expressions are applied.
        head_exprs: Vec::new(),
        bindings: rule.bindings.clone(),
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
            FilterExpr::LitNum(_)
            | FilterExpr::LitStr(_)
            | FilterExpr::Null
            | FilterExpr::LitTime(_) => false,
            FilterExpr::Neg(inner) => expr_refs(inner, vars),
            FilterExpr::BinOp { lhs, rhs, .. } => expr_refs(lhs, vars) || expr_refs(rhs, vars),
            FilterExpr::Call { args, .. } => args.iter().any(|a| expr_refs(a, vars)),
        }
    }
    match f {
        BuiltinFilter::NotEqual(a, b) => vars.contains(a.as_str()) || vars.contains(b.as_str()),
        BuiltinFilter::GreaterThan(a, _) | BuiltinFilter::LessThan(a, _) => {
            vars.contains(a.as_str())
        }
        BuiltinFilter::Compare { lhs, rhs, .. } => expr_refs(lhs, vars) || expr_refs(rhs, vars),
        BuiltinFilter::StrPred { subject, arg, .. } => {
            expr_refs(subject, vars) || expr_refs(arg, vars)
        }
        BuiltinFilter::IsNull(e) => expr_refs(e, vars),
        BuiltinFilter::GeoPred { left, right, .. } => {
            expr_refs(left, vars) || expr_refs(right, vars)
        }
        BuiltinFilter::Any(branches) | BuiltinFilter::All(branches) => {
            branches.iter().any(|b| filter_references_any(b, vars))
        }
        BuiltinFilter::Not(inner) => filter_references_any(inner, vars),
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
            binding.insert(agg.output_var.clone(), value.clone());
            prov.push(make_provenance_step(
                &format!("{}({})", agg.kind.keyword(), agg.inner.predicate),
                std::slice::from_ref(&value),
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

/// Evaluate a whitelisted function call.
///
/// A wrong-typed argument is Undefined rather than false, matching the rest of
/// the evaluator: false is a value `!` would flip into a spurious pass.
fn eval_call(
    func: crate::types::Func,
    args: &[crate::types::FilterExpr],
    binding: &std::collections::HashMap<String, Term>,
) -> Eval {
    use crate::types::Func;

    let mut values = Vec::with_capacity(args.len());
    for a in args {
        match eval_expr(a, binding) {
            Eval::Value(v) => values.push(v),
            // A function of null is null, as in SQL.
            other => return other,
        }
    }

    let wrong_type = || {
        tracing::warn!(
            function = func.keyword(),
            "datalog: function applied to the wrong type; deriving nothing"
        );
        Eval::Undefined
    };

    match (func, values.as_slice()) {
        // `now()` never reaches here: `stamp_clock` replaces it with a literal
        // before evaluation, so every row in one run sees one instant.
        (Func::Now, []) => {
            tracing::warn!("datalog: now() was not stamped before evaluation");
            Eval::Undefined
        }
        (Func::Geo, [EvalValue::Str(text)]) => match crate::geojson::Geometry::parse(text) {
            Some(g) => Eval::Value(EvalValue::Geo(g)),
            None => {
                tracing::warn!("datalog: geo() could not read this as GeoJSON");
                Eval::Undefined
            }
        },
        // Already a geometry — accepted so a rule need not know how a column
        // is stored.
        (Func::Geo, [EvalValue::Geo(g)]) => Eval::Value(EvalValue::Geo(g.clone())),
        (Func::GeoDistance, [EvalValue::Geo(a), EvalValue::Geo(b)]) => match a.distance_to(b) {
            Some(m) => Eval::Value(EvalValue::Num(m)),
            None => Eval::Undefined,
        },
        (Func::Date, [EvalValue::Str(text)]) => match parse_iso8601(text) {
            Some(t) => Eval::Value(EvalValue::Time(t)),
            None => {
                tracing::warn!(
                    value = %text,
                    "datalog: date() could not read this as a timestamp"
                );
                Eval::Undefined
            }
        },
        // Already a time — accepted so a rule need not know how a column is
        // stored.
        (Func::Date, [EvalValue::Time(t)]) => Eval::Value(EvalValue::Time(*t)),
        (Func::Weeks, [EvalValue::Num(n)]) => Eval::Value(EvalValue::Num(n * 604_800_000.0)),
        (Func::Days, [EvalValue::Num(n)]) => Eval::Value(EvalValue::Num(n * 86_400_000.0)),
        (Func::Hours, [EvalValue::Num(n)]) => Eval::Value(EvalValue::Num(n * 3_600_000.0)),
        (Func::Minutes, [EvalValue::Num(n)]) => Eval::Value(EvalValue::Num(n * 60_000.0)),
        (Func::Abs, [EvalValue::Num(x)]) => Eval::Value(EvalValue::Num(x.abs())),
        (Func::Floor, [EvalValue::Num(x)]) => Eval::Value(EvalValue::Num(x.floor())),
        (Func::Ceil, [EvalValue::Num(x)]) => Eval::Value(EvalValue::Num(x.ceil())),
        (Func::Round, [EvalValue::Num(x)]) => Eval::Value(EvalValue::Num(x.round())),
        // Characters, not bytes: a length in bytes would differ for the same
        // text depending on the alphabet it is written in.
        (Func::Len, [EvalValue::Str(s)]) => Eval::Value(EvalValue::Num(s.chars().count() as f64)),
        (Func::Lower, [EvalValue::Str(s)]) => Eval::Value(EvalValue::Str(s.to_lowercase())),
        (Func::Upper, [EvalValue::Str(s)]) => Eval::Value(EvalValue::Str(s.to_uppercase())),
        (Func::Concat, [EvalValue::Str(a), EvalValue::Str(b)]) => {
            Eval::Value(EvalValue::Str(format!("{a}{b}")))
        }
        _ => wrong_type(),
    }
}

/// Reduce a retained group to its answer.
///
/// `median` is `percentile` at 0.5 rather than a separate calculation, so the
/// two can never disagree about an even-sized group.
fn finish_retained(
    kind: crate::types::AggregateKind,
    param: Option<&Term>,
    values: Vec<Term>,
) -> Option<Term> {
    use crate::types::AggregateKind as K;

    if values.is_empty() {
        return kind.identity_over_empty();
    }

    if kind == K::GroupConcat {
        let Some(Term::ConstStr(sep)) = param else {
            return None;
        };
        // Sorted so the answer does not depend on fact-set iteration order,
        // which is a HashSet's and therefore arbitrary.
        let mut rendered: Vec<String> = values.iter().map(term_to_string).collect();
        rendered.sort();
        return Some(Term::ConstStr(rendered.join(sep)));
    }

    // Order statistics are numeric: ordering a string against a number has no
    // meaningful answer, the same reason min/max refuses a mixed group.
    let mut nums: Vec<f64> = Vec::with_capacity(values.len());
    for v in &values {
        let Term::ConstFloat(OrderedFloat(f)) = v else {
            tracing::warn!(
                aggregate = kind.keyword(),
                "datalog: order statistic over a non-numeric value; deriving nothing"
            );
            return None;
        };
        nums.push(*f);
    }
    nums.sort_by(|a, b| a.partial_cmp(b).expect("no NaN: eval rejects it"));

    let fraction = match kind {
        K::Median => 0.5,
        _ => match param {
            Some(Term::ConstFloat(OrderedFloat(p))) => *p,
            _ => return None,
        },
    };

    // Linear interpolation between the two neighbouring ranks. This is what
    // makes percentile(0.5) equal median on an even-sized group.
    let idx = fraction * (nums.len() - 1) as f64;
    let lo = idx.floor() as usize;
    let hi = idx.ceil() as usize;
    let frac = idx - lo as f64;
    Some(Term::ConstFloat(OrderedFloat(
        nums[lo] + frac * (nums[hi] - nums[lo]),
    )))
}

/// Replace every `now()` in a rule with the instant this evaluation is for.
///
/// Done once up front rather than per row, so the whole run agrees about when
/// it is. A rule with no `now()` is returned unchanged, which is what keeps
/// this free for every rule that does not ask.
fn stamp_clock(rule: &DatalogRule, now: chrono::DateTime<chrono::Utc>) -> DatalogRule {
    if !rule_reads_the_clock(rule) {
        return rule.clone();
    }
    let mut out = rule.clone();
    for f in &mut out.filters {
        stamp_filter(f, now);
    }
    for he in &mut out.head_exprs {
        stamp_expr(&mut he.expr, now);
    }
    for b in &mut out.bindings {
        stamp_expr(&mut b.expr, now);
    }
    out
}

fn stamp_filter(f: &mut BuiltinFilter, now: chrono::DateTime<chrono::Utc>) {
    match f {
        BuiltinFilter::Compare { lhs, rhs, .. } => {
            stamp_expr(lhs, now);
            stamp_expr(rhs, now);
        }
        BuiltinFilter::StrPred { subject, arg, .. } => {
            stamp_expr(subject, now);
            stamp_expr(arg, now);
        }
        BuiltinFilter::IsNull(e) => stamp_expr(e, now),
        BuiltinFilter::GeoPred { left, right, .. } => {
            stamp_expr(left, now);
            stamp_expr(right, now);
        }
        BuiltinFilter::Any(bs) | BuiltinFilter::All(bs) => {
            for b in bs {
                stamp_filter(b, now);
            }
        }
        BuiltinFilter::Not(inner) => stamp_filter(inner, now),
        BuiltinFilter::NotEqual(_, _)
        | BuiltinFilter::GreaterThan(_, _)
        | BuiltinFilter::LessThan(_, _) => {}
    }
}

fn stamp_expr(e: &mut crate::types::FilterExpr, now: chrono::DateTime<chrono::Utc>) {
    use crate::types::FilterExpr;
    match e {
        FilterExpr::Call { func, args } => {
            if func.reads_the_clock() {
                *e = FilterExpr::LitTime(now);
                return;
            }
            for a in args {
                stamp_expr(a, now);
            }
        }
        FilterExpr::Neg(inner) => stamp_expr(inner, now),
        FilterExpr::BinOp { lhs, rhs, .. } => {
            stamp_expr(lhs, now);
            stamp_expr(rhs, now);
        }
        FilterExpr::Var(_)
        | FilterExpr::LitNum(_)
        | FilterExpr::LitStr(_)
        | FilterExpr::Null
        | FilterExpr::LitTime(_) => {}
    }
}

/// True if the rule's answer depends on when it is asked.
///
/// Matches both forms deliberately: `now()` before stamping, and the
/// `LitTime` it becomes after. A `LitTime` cannot be written by hand — it only
/// arises from stamping — so its presence is proof the clock was read, which
/// is what lets one function serve both the stamper and the cache guard.
fn rule_reads_the_clock(rule: &DatalogRule) -> bool {
    fn expr_reads(e: &crate::types::FilterExpr) -> bool {
        use crate::types::FilterExpr;
        match e {
            FilterExpr::LitTime(_) => true,
            FilterExpr::Call { func, args } => {
                func.reads_the_clock() || args.iter().any(expr_reads)
            }
            FilterExpr::Neg(inner) => expr_reads(inner),
            FilterExpr::BinOp { lhs, rhs, .. } => expr_reads(lhs) || expr_reads(rhs),
            _ => false,
        }
    }
    fn filter_reads(f: &BuiltinFilter) -> bool {
        match f {
            BuiltinFilter::Compare { lhs, rhs, .. } => expr_reads(lhs) || expr_reads(rhs),
            BuiltinFilter::StrPred { subject, arg, .. } => expr_reads(subject) || expr_reads(arg),
            BuiltinFilter::IsNull(e) => expr_reads(e),
            BuiltinFilter::GeoPred { left, right, .. } => expr_reads(left) || expr_reads(right),
            BuiltinFilter::Any(bs) | BuiltinFilter::All(bs) => bs.iter().any(filter_reads),
            BuiltinFilter::Not(inner) => filter_reads(inner),
            _ => false,
        }
    }
    rule.filters.iter().any(filter_reads)
        || rule.head_exprs.iter().any(|h| expr_reads(&h.expr))
        || rule.bindings.iter().any(|b| expr_reads(&b.expr))
}

/// Shift a time by a duration in milliseconds.
fn shift_time(t: chrono::DateTime<chrono::Utc>, ms: f64) -> Eval {
    if !ms.is_finite() {
        return Eval::Undefined;
    }
    match chrono::Duration::try_milliseconds(ms as i64) {
        Some(d) => match t.checked_add_signed(d) {
            Some(shifted) => Eval::Value(EvalValue::Time(shifted)),
            // Beyond what a timestamp can represent. A wrapped or clamped
            // instant would be a value the caller cannot tell from a real one.
            None => {
                tracing::warn!("datalog: shifting this time overflows the calendar");
                Eval::Undefined
            }
        },
        None => Eval::Undefined,
    }
}

/// Read an ISO-8601 timestamp, or a bare `YYYY-MM-DD` as midnight UTC.
///
/// Both because a corpus holds both: a full RFC-3339 stamp from a machine, and
/// a plain day from a person.
fn parse_iso8601(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::TimeZone;
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    let day = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    let midnight = day.and_hms_opt(0, 0, 0)?;
    chrono::Utc.from_local_datetime(&midnight).single()
}

/// Compare two ground terms of the same kind.
///
/// `None` for two different kinds: a string against a number has no meaningful
/// order, and picking one arbitrarily would answer a question nobody asked.
fn compare_terms(a: &Term, b: &Term) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Term::ConstFloat(x), Term::ConstFloat(y)) => Some(x.cmp(y)),
        (Term::ConstStr(x), Term::ConstStr(y)) => Some(x.cmp(y)),
        (Term::Const(x), Term::Const(y)) => Some(x.cmp(y)),
        (Term::ConstTime(x), Term::ConstTime(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// The running state of a streaming aggregate fold.
enum Fold {
    Count(u64),
    Sum(f64),
    /// `min`/`max` need only a total order, so they carry the term itself
    /// rather than a number — that is what lets a rule take the earliest
    /// timestamp or the first name alphabetically.
    Extreme {
        want: std::cmp::Ordering,
        best: Option<Term>,
    },
    Avg(f64, u64),
    /// The non-streaming fold. Holds every distinct value seen so far, capped
    /// at `DISTINCT_VALUE_CAP`.
    Distinct(std::collections::HashSet<Term>),
    /// Welford's online variance: count, running mean, running sum of squared
    /// deviations. Constant memory, one pass — `stddev` streams.
    Spread {
        n: u64,
        mean: f64,
        m2: f64,
    },
    /// The whole group, retained because an answer does not exist until it is
    /// ordered. Capped at `RETAINED_VALUE_CAP`.
    Retained(Vec<Term>),
    /// A value the fold cannot use — not a number where one is required, or a
    /// group whose values are not all the same kind. The group is refused
    /// rather than silently folded over the subset that happened to fit.
    TypeError,
}

impl Fold {
    fn start(kind: crate::types::AggregateKind) -> Self {
        use crate::types::AggregateKind as K;
        match kind {
            K::Count => Fold::Count(0),
            K::Sum => Fold::Sum(0.0),
            K::Min => Fold::Extreme {
                want: std::cmp::Ordering::Less,
                best: None,
            },
            K::Max => Fold::Extreme {
                want: std::cmp::Ordering::Greater,
                best: None,
            },
            K::Avg => Fold::Avg(0.0, 0),
            K::CountDistinct => Fold::Distinct(std::collections::HashSet::new()),
            K::StdDev => Fold::Spread {
                n: 0,
                mean: 0.0,
                m2: 0.0,
            },
            K::Median | K::Percentile | K::GroupConcat => Fold::Retained(Vec::new()),
        }
    }

    /// Absorb one solved binding. Returns false when the fold can stop early.
    fn step(&mut self, value: Option<&Term>) -> bool {
        if let Fold::Count(n) = self {
            *n += 1;
            return true;
        }
        if matches!(self, Fold::TypeError) {
            return false;
        }
        let Some(value) = value else {
            *self = Fold::TypeError;
            return false;
        };
        match self {
            Fold::Distinct(seen) => {
                seen.insert(value.clone());
                if seen.len() > DISTINCT_VALUE_CAP {
                    tracing::warn!(
                        cap = DISTINCT_VALUE_CAP,
                        "datalog: count_distinct group exceeded its cap; deriving \
                         nothing rather than a truncated count"
                    );
                    *self = Fold::TypeError;
                    return false;
                }
            }
            Fold::Retained(values) => {
                values.push(value.clone());
                if values.len() > RETAINED_VALUE_CAP {
                    tracing::warn!(
                        cap = RETAINED_VALUE_CAP,
                        "datalog: whole-group aggregate exceeded its cap; deriving \
                         nothing rather than a statistic over a truncated sample"
                    );
                    *self = Fold::TypeError;
                    return false;
                }
            }
            Fold::Spread { n, mean, m2 } => {
                let Term::ConstFloat(OrderedFloat(v)) = value else {
                    *self = Fold::TypeError;
                    return false;
                };
                // Welford: update the mean, then accumulate the squared
                // deviation against both the old and new mean. Numerically
                // stabler than summing squares and subtracting.
                *n += 1;
                let delta = *v - *mean;
                *mean += delta / *n as f64;
                *m2 += delta * (*v - *mean);
            }
            Fold::Extreme { want, best } => match best {
                None => *best = Some(value.clone()),
                Some(current) => match compare_terms(value, current) {
                    Some(ord) if ord == *want => *best = Some(value.clone()),
                    Some(_) => {}
                    // Mixed kinds within one group.
                    None => {
                        *self = Fold::TypeError;
                        return false;
                    }
                },
            },
            Fold::Sum(_) | Fold::Avg(_, _) => {
                let Term::ConstFloat(OrderedFloat(v)) = value else {
                    *self = Fold::TypeError;
                    return false;
                };
                let v = *v;
                match self {
                    Fold::Sum(acc) => *acc += v,
                    Fold::Avg(sum, n) => {
                        *sum += v;
                        *n += 1;
                    }
                    _ => unreachable!("matched above"),
                }
            }
            Fold::Count(_) | Fold::TypeError => unreachable!("handled above"),
        }
        true
    }

    /// The folded value, or `None` when the group produces nothing.
    fn finish(self, kind: crate::types::AggregateKind, param: Option<&Term>) -> Option<Term> {
        let num = |f: f64| Term::ConstFloat(OrderedFloat(f));
        match self {
            Fold::TypeError => None,
            Fold::Count(n) => Some(num(n as f64)),
            Fold::Distinct(seen) => Some(num(seen.len() as f64)),
            Fold::Spread { n, m2, .. } => {
                if n == 0 {
                    kind.identity_over_empty()
                } else {
                    // Population, not sample: the group IS the population the
                    // rule asked about, not a draw from a larger one.
                    Some(num((m2 / n as f64).sqrt()))
                }
            }
            Fold::Retained(values) => finish_retained(kind, param, values),
            Fold::Sum(acc) => Some(num(acc)),
            // An extreme of nothing does not exist, and neither min nor max has
            // an identity to fall back on.
            Fold::Extreme { best, .. } => best.or_else(|| kind.identity_over_empty()),
            Fold::Avg(sum, n) => {
                if n == 0 {
                    kind.identity_over_empty()
                } else {
                    Some(num(sum / n as f64))
                }
            }
        }
    }
}

/// Fold an aggregate over its inner conjunction, streaming.
///
/// `None` means the group derives nothing: an empty group for a kind with no
/// identity (`min`, `max`, `avg`), a value of the wrong kind, or a group whose
/// values are not all the same kind.
fn fold_inner_matches(
    agg: &crate::types::Aggregate,
    binding: &std::collections::HashMap<String, Term>,
    all_facts: &FactSet,
) -> Option<Term> {
    let mut fold = Fold::start(agg.kind);
    let value_var = agg.value_var.clone();
    visit_inner_matches(agg, binding, all_facts, &mut |solved| {
        let value = value_var.as_ref().and_then(|name| solved.get(name));
        // SQL's rule: a value fold ignores nulls, so `avg` divides by the
        // count of non-null values and `min` never returns one. `count` looks
        // at no value at all, so it still counts the row — it is `count(*)`,
        // not `count(col)`.
        if matches!(value, Some(Term::ConstNull)) {
            return true;
        }
        fold.step(value)
    });
    if matches!(fold, Fold::TypeError) {
        tracing::warn!(
            aggregate = agg.kind.keyword(),
            predicate = %agg.inner.predicate,
            value_var = ?agg.value_var,
            "datalog: aggregate value is unusable (wrong kind, or a group mixing \
             kinds); the group derives nothing rather than a partial answer"
        );
    }
    fold.finish(agg.kind, agg.param.as_ref())
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
    Time(chrono::DateTime<chrono::Utc>),
    Geo(crate::geojson::Geometry),
}

/// The outcome of evaluating a filter expression.
///
/// `None` used to mean both "no value yet" and "no value ever", and the filter
/// passed either way. Those are not the same thing: an unbound variable means
/// the comparison cannot be decided *yet*, while a zero divisor means it has
/// no answer *at all*. Collapsing them let `V / 0 == 0` derive a fact.
enum Eval {
    Value(EvalValue),
    /// A known-absent value. Propagates through arithmetic and function calls
    /// the way SQL's null does, and reaches a comparison as `Unknown` — which
    /// is Kleene, unlike `Undefined`, which poisons.
    Null,
    /// A variable the binding does not bind. The filter passes, which is the
    /// legacy semantics partial bindings have always relied on.
    Unbound,
    /// The expression is fully bound but has no value — a zero divisor or
    /// modulus, or arithmetic on something that is not a number. The filter
    /// must not pass: firing anyway would derive a fact off an error.
    Undefined,
}

fn eval_expr(
    e: &crate::types::FilterExpr,
    binding: &std::collections::HashMap<String, Term>,
) -> Eval {
    use crate::types::{ArithOp, FilterExpr};
    use ordered_float::OrderedFloat;
    match e {
        FilterExpr::Var(name) => match binding.get(name) {
            Some(Term::ConstFloat(OrderedFloat(f))) => Eval::Value(EvalValue::Num(*f)),
            Some(Term::ConstStr(s)) => Eval::Value(EvalValue::Str(s.clone())),
            Some(Term::Const(u)) => Eval::Value(EvalValue::Uuid(*u)),
            Some(Term::ConstTime(t)) => Eval::Value(EvalValue::Time(*t)),
            Some(Term::ConstNull) => Eval::Null,
            Some(Term::Var(_)) | None => Eval::Unbound,
        },
        FilterExpr::LitNum(OrderedFloat(f)) => Eval::Value(EvalValue::Num(*f)),
        FilterExpr::LitStr(s) => Eval::Value(EvalValue::Str(s.clone())),
        FilterExpr::Null => Eval::Null,
        FilterExpr::LitTime(t) => Eval::Value(EvalValue::Time(*t)),
        FilterExpr::Call { func, args } => eval_call(*func, args, binding),
        FilterExpr::Neg(inner) => match eval_expr(inner, binding) {
            Eval::Null => Eval::Null,
            Eval::Value(EvalValue::Num(x)) => Eval::Value(EvalValue::Num(-x)),
            Eval::Value(_) => {
                tracing::warn!("datalog: unary minus on non-numeric value");
                Eval::Undefined
            }
            other => other,
        },
        FilterExpr::BinOp { op, lhs, rhs } => {
            let l = eval_expr(lhs, binding);
            let r = eval_expr(rhs, binding);
            // An error anywhere in the tree poisons the whole tree; only when
            // nothing is undefined does an unbound operand mean "not yet".
            if matches!(l, Eval::Undefined) || matches!(r, Eval::Undefined) {
                return Eval::Undefined;
            }
            // Null before Unbound: `null + 1` is null, not "not yet decided",
            // and it must NOT be a type error — if it poisoned, the whole
            // Kleene design would collapse back to refusing.
            if matches!(l, Eval::Null) || matches!(r, Eval::Null) {
                return Eval::Null;
            }
            let (Eval::Value(l), Eval::Value(r)) = (l, r) else {
                return Eval::Unbound;
            };
            match (l, r) {
                // A time plus or minus a duration is a time; the duration is
                // milliseconds, which is why `days(7)` is just a number.
                (EvalValue::Time(t), EvalValue::Num(ms)) => match op {
                    ArithOp::Add => shift_time(t, ms),
                    ArithOp::Sub => shift_time(t, -ms),
                    _ => {
                        tracing::warn!("datalog: a time may only be shifted by a duration");
                        Eval::Undefined
                    }
                },
                // A duration before a time reads oddly but is the same thing.
                (EvalValue::Num(ms), EvalValue::Time(t)) if matches!(op, ArithOp::Add) => {
                    shift_time(t, ms)
                }
                // Two times subtract to the duration between them.
                (EvalValue::Time(a), EvalValue::Time(b)) => match op {
                    ArithOp::Sub => Eval::Value(EvalValue::Num((a - b).num_milliseconds() as f64)),
                    _ => {
                        tracing::warn!("datalog: two times may only be subtracted");
                        Eval::Undefined
                    }
                },
                (EvalValue::Num(a), EvalValue::Num(b)) => match op {
                    ArithOp::Add => Eval::Value(EvalValue::Num(a + b)),
                    ArithOp::Sub => Eval::Value(EvalValue::Num(a - b)),
                    ArithOp::Mul => Eval::Value(EvalValue::Num(a * b)),
                    ArithOp::Div => {
                        if b == 0.0 {
                            tracing::warn!("datalog: division by zero in filter");
                            Eval::Undefined
                        } else {
                            Eval::Value(EvalValue::Num(a / b))
                        }
                    }
                    // A zero modulus is as undefined as a zero divisor, and
                    // f64 would answer NaN — a value the caller cannot tell
                    // from a real one. Refuse it the same way.
                    // f64::powf answers NaN for a negative base with a
                    // fractional exponent, and infinity on overflow. Both are
                    // values the caller cannot tell from a real one.
                    ArithOp::Pow => {
                        let r = a.powf(b);
                        if r.is_finite() {
                            Eval::Value(EvalValue::Num(r))
                        } else {
                            tracing::warn!(
                                base = a,
                                exponent = b,
                                "datalog: exponentiation has no finite real answer"
                            );
                            Eval::Undefined
                        }
                    }
                    ArithOp::Rem => {
                        if b == 0.0 {
                            tracing::warn!("datalog: modulo by zero in filter");
                            Eval::Undefined
                        } else {
                            Eval::Value(EvalValue::Num(a % b))
                        }
                    }
                },
                _ => {
                    tracing::warn!("datalog: arithmetic on non-numeric values");
                    Eval::Undefined
                }
            }
        }
    }
}

/// Apply a comparison operator to an ordering.
///
/// Extracted because three kinds order the same way and only differ in how
/// they produce the ordering; keeping one copy is what stops a fourth kind
/// getting a subtly different `Le`.
fn ordered(op: crate::types::CmpOp, ord: std::cmp::Ordering) -> bool {
    use crate::types::CmpOp;
    use std::cmp::Ordering;
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

fn apply_cmp(op: crate::types::CmpOp, l: &EvalValue, r: &EvalValue) -> bool {
    use crate::types::CmpOp;
    match (l, r) {
        (EvalValue::Num(a), EvalValue::Num(b)) => match a.partial_cmp(b) {
            Some(ord) => ordered(op, ord),
            None => false, // NaN
        },
        (EvalValue::Str(a), EvalValue::Str(b)) => ordered(op, a.cmp(b)),
        // Instants are totally ordered, which is the whole point of having a
        // time value rather than comparing timestamp strings and hoping the
        // format sorts.
        (EvalValue::Time(a), EvalValue::Time(b)) => ordered(op, a.cmp(b)),
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

/// A filter's verdict.
///
/// Five states, because "no answer" has three different causes that deserve
/// three different propagations. Collapsing any pair of them has been a bug
/// each time it was tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    True,
    False,
    /// No answer because a value is null.
    ///
    /// Propagates by KLEENE — `true || unknown` is true, `false && unknown` is
    /// false — because a null is ordinary data and should not spread through a
    /// rule that has already been decided by another branch.
    ///
    /// Does not pass, matching SQL: `WHERE x = NULL` returns no rows.
    Unknown,
    /// No answer because something went wrong — a zero divisor or modulus,
    /// arithmetic on a non-number, a non-finite power, a string question asked
    /// of a number.
    ///
    /// POISONS, unlike `Unknown`: it refuses the whole tree even beside a
    /// branch that is true on its own, because a rule containing a mistake
    /// should say so rather than fire on a coincidence. Negating it still
    /// refuses, so `!` cannot turn an error into a pass.
    Undefined,
    /// Not decidable yet because something is unbound. Passes, which is the
    /// legacy semantics partial bindings rely on.
    Unbound,
}

impl Verdict {
    fn of(b: bool) -> Self {
        if b { Verdict::True } else { Verdict::False }
    }

    /// Whether the binding survives this filter.
    fn passes(self) -> bool {
        matches!(self, Verdict::True | Verdict::Unbound)
    }
}

/// Check a single builtin filter.
fn check_one_filter(filter: &BuiltinFilter, binding: &HashMap<String, Term>) -> bool {
    eval_filter(filter, binding).passes()
}

fn eval_filter(filter: &BuiltinFilter, binding: &HashMap<String, Term>) -> Verdict {
    match filter {
        BuiltinFilter::NotEqual(lhs, rhs) => match (binding.get(lhs), binding.get(rhs)) {
            (Some(a), Some(b)) => Verdict::of(a != b),
            _ => Verdict::Unbound,
        },
        BuiltinFilter::GreaterThan(var, threshold) => {
            match binding.get(var) {
                Some(Term::ConstFloat(OrderedFloat(v))) => Verdict::of(*v > *threshold),
                // Legacy: unbound or non-float passes.
                _ => Verdict::Unbound,
            }
        }
        BuiltinFilter::LessThan(var, threshold) => match binding.get(var) {
            Some(Term::ConstFloat(OrderedFloat(v))) => Verdict::of(*v < *threshold),
            _ => Verdict::Unbound,
        },
        BuiltinFilter::Compare { op, lhs, rhs } => {
            match (eval_expr(lhs, binding), eval_expr(rhs, binding)) {
                (Eval::Value(l), Eval::Value(r)) => Verdict::of(apply_cmp(*op, &l, &r)),
                // An error outranks a null: a rule that divides by zero AND
                // touches a null is a rule with a mistake in it.
                (Eval::Undefined, _) | (_, Eval::Undefined) => Verdict::Undefined,
                // Comparing to null has no answer, and that is ordinary data
                // rather than a mistake — so Unknown, which is Kleene.
                (Eval::Null, _) | (_, Eval::Null) => Verdict::Unknown,
                _ => Verdict::Unbound,
            }
        }
        BuiltinFilter::GeoPred {
            relation,
            left,
            right,
        } => match (eval_expr(left, binding), eval_expr(right, binding)) {
            (Eval::Value(EvalValue::Geo(a)), Eval::Value(EvalValue::Geo(b))) => {
                match a.relates(*relation, &b) {
                    Some(answer) => Verdict::of(answer),
                    // The relate engine could not answer for this pair. "No"
                    // and "cannot say" are different, and a rule must not fire
                    // on the second thinking it got the first.
                    None => {
                        tracing::warn!(
                            relation = relation.keyword(),
                            "datalog: this pair of shapes has no answer for that relation"
                        );
                        Verdict::Undefined
                    }
                }
            }
            (Eval::Null, _) | (_, Eval::Null) => Verdict::Unknown,
            // A geometry question asked of something that is not a geometry —
            // usually text that failed to parse, which is a mistake in the
            // data rather than an absence.
            (Eval::Value(_), _) | (_, Eval::Value(_)) => {
                tracing::warn!(
                    relation = relation.keyword(),
                    "datalog: spatial relation applied to a non-geometry; deriving nothing"
                );
                Verdict::Undefined
            }
            _ => Verdict::Unbound,
        },
        // The only question about a null with a definite answer, which is why
        // it has to exist: `V == null` is Unknown and so never fires.
        BuiltinFilter::IsNull(e) => match eval_expr(e, binding) {
            Eval::Null => Verdict::True,
            Eval::Value(_) => Verdict::False,
            Eval::Undefined => Verdict::Undefined,
            Eval::Unbound => Verdict::Unbound,
        },
        BuiltinFilter::StrPred { op, subject, arg } => {
            match (eval_expr(subject, binding), eval_expr(arg, binding)) {
                (Eval::Value(EvalValue::Str(s)), Eval::Value(EvalValue::Str(a))) => {
                    Verdict::of(op.apply(&s, &a))
                }
                (Eval::Null, _) | (_, Eval::Null) => Verdict::Unknown,
                // Asking whether a number starts with a string has no answer.
                (Eval::Value(_), _) | (_, Eval::Value(_)) => {
                    tracing::warn!(
                        predicate = op.keyword(),
                        "datalog: string predicate applied to a non-string; deriving nothing"
                    );
                    Verdict::Undefined
                }
                _ => Verdict::Unbound,
            }
        }
        // `Undefined` is tested first in both, which is the poisoning rule: an
        // error refuses the whole tree regardless of what its siblings say.
        // Below that these are Kleene's tables — a decisive branch settles the
        // answer, and only an inconclusive set is Unknown.
        BuiltinFilter::All(branches) => {
            let verdicts: Vec<Verdict> = branches.iter().map(|b| eval_filter(b, binding)).collect();
            if verdicts.contains(&Verdict::Undefined) {
                Verdict::Undefined
            } else if verdicts.contains(&Verdict::False) {
                // `false && unknown` is false: one false branch settles it.
                Verdict::False
            } else if verdicts.contains(&Verdict::Unknown) {
                Verdict::Unknown
            } else {
                Verdict::True
            }
        }
        BuiltinFilter::Any(branches) => {
            let verdicts: Vec<Verdict> = branches.iter().map(|b| eval_filter(b, binding)).collect();
            if verdicts.contains(&Verdict::Undefined) {
                Verdict::Undefined
            } else if verdicts.contains(&Verdict::True) || verdicts.contains(&Verdict::Unbound) {
                // `true || unknown` is true: one true branch settles it.
                Verdict::True
            } else if verdicts.contains(&Verdict::Unknown) {
                Verdict::Unknown
            } else {
                Verdict::False
            }
        }
        BuiltinFilter::Not(inner) => match eval_filter(inner, binding) {
            Verdict::True => Verdict::False,
            Verdict::False => Verdict::True,
            other => other,
        },
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
        // RFC-3339, so a rendered time reads back through `date()`.
        Term::ConstTime(t) => t.to_rfc3339(),
        Term::ConstNull => "null".to_string(),
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
    // `parse_rules`, not `parse_rule`: a stored rule may use `;`, and each
    // alternative becomes a rule the evaluator runs.
    let entries = load_effective_rule_entries(storage, ctx, family).await?;
    let mut rules = Vec::with_capacity(entries.len());
    for entry in entries {
        rules.extend(parse_rules(&entry.entry.rule_body)?);
    }
    Ok(rules)
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
        /// A plain dependency, but from a head that computes an argument.
        /// Harmless across strata; inside one it never reaches a fixpoint.
        HeadExpr,
    }

    // Build predicate dep graph: head -> Vec<(dependency, edge_kind)>.
    let mut graph: HashMap<String, Vec<(String, Edge)>> = HashMap::new();
    let mut all_preds: HashSet<String> = HashSet::new();

    for rule in rules {
        let head = rule.head.predicate.clone();
        all_preds.insert(head.clone());
        let entry = graph.entry(head.clone()).or_default();
        let body_edge = if rule.head_exprs.is_empty() {
            Edge::Plain
        } else {
            Edge::HeadExpr
        };
        for atom in &rule.body {
            entry.push((atom.predicate.clone(), body_edge));
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
                        // `rank(X, N + 1) :- rank(X, N).` produces a new
                        // value every round. The fact budget would stop it by
                        // truncation, handing back an arbitrary prefix with no
                        // signal; rejecting is the only answer that cannot be
                        // mistaken for an answer.
                        Edge::HeadExpr => {
                            return Err(
                                crate::types::StratifyError::RecursionThroughHeadExpression {
                                    cycle: scc.clone(),
                                },
                            );
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
        // Tolerance, not equality: this went through a division, and
        // instruction selection differs between arm64 and x86_64.
        let got = one_value(&all, "mean", &s("alice")).expect("derived mean");
        assert!(
            (got - 100.0 / 3.0).abs() < 1e-9,
            "expected about {}, got {got}",
            100.0 / 3.0
        );
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

// ─── Grammar Completion Tests ─────────────────────────────────────

#[cfg(test)]
mod grammar_tests {
    use super::*;

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

    fn derived_keys(all: &FactSet, pred: &str) -> Vec<String> {
        let mut out: Vec<String> = all
            .get(pred)
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

    // ── Item 5: an expression in a head argument ──────────────────

    #[test]
    fn a_head_argument_may_be_an_arithmetic_expression() {
        let rule = parse_rule("scaled(X, W * 100) :- warmth(X, W).").unwrap();
        assert_eq!(rule.head_exprs.len(), 1);
        let corpus = facts(&[("warmth", vec![s("a"), n(0.25)])]);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(one_term(&all, "scaled", &s("a")), Some(n(25.0)));
    }

    #[test]
    fn several_head_arguments_may_be_expressions() {
        let rule = parse_rule("box(X, W - 1, W + 1) :- warmth(X, W).").unwrap();
        assert_eq!(rule.head_exprs.len(), 2);
        let corpus = facts(&[("warmth", vec![s("a"), n(5.0)])]);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        let row = all
            .get("box")
            .unwrap()
            .iter()
            .find(|a| a.first() == Some(&s("a")))
            .unwrap();
        assert_eq!(row[1], n(4.0));
        assert_eq!(row[2], n(6.0));
    }

    #[test]
    fn a_head_with_only_plain_terms_carries_no_expressions() {
        let rule = parse_rule("copy(X, W) :- warmth(X, W).").unwrap();
        assert!(rule.head_exprs.is_empty(), "negation must stay additive");
    }

    #[test]
    fn an_arithmetic_head_that_feeds_its_own_body_is_rejected_at_load() {
        // `rank(X, N + 1) :- rank(X, N).` never reaches a fixpoint. Today the
        // max_facts budget would stop it BY TRUNCATION, which is the
        // silently-wrong shape — the caller gets an arbitrary prefix and no
        // signal. Reject it the way recursion through negation is rejected.
        let rules = vec![parse_rule("rank(X, N + 1) :- rank(X, N).").unwrap()];
        let err = stratify(&rules).unwrap_err();
        assert!(
            matches!(
                err,
                crate::types::StratifyError::RecursionThroughHeadExpression { .. }
            ),
            "expected RecursionThroughHeadExpression, got {err:?}"
        );
    }

    #[test]
    fn an_indirect_arithmetic_recursion_is_also_rejected() {
        let rules = vec![
            parse_rule("a(X, N + 1) :- b(X, N).").unwrap(),
            parse_rule("b(X, N) :- a(X, N).").unwrap(),
        ];
        assert!(matches!(
            stratify(&rules).unwrap_err(),
            crate::types::StratifyError::RecursionThroughHeadExpression { .. }
        ));
    }

    #[test]
    fn a_non_recursive_arithmetic_head_is_allowed() {
        let rules = vec![parse_rule("scaled(X, W * 100) :- warmth(X, W).").unwrap()];
        assert!(stratify(&rules).is_ok());
    }

    #[test]
    fn plain_recursion_without_an_arithmetic_head_is_still_allowed() {
        let rules = vec![
            parse_rule("reach(X, Y) :- edge(X, Y).").unwrap(),
            parse_rule("reach(X, Z) :- reach(X, Y), edge(Y, Z).").unwrap(),
        ];
        assert!(stratify(&rules).is_ok());
    }

    #[test]
    fn a_head_expression_variable_the_body_does_not_bind_is_rejected() {
        let err = parse_rule("scaled(X, Q * 100) :- warmth(X, W).")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains('Q'),
            "must name the unbound variable, got: {err}"
        );
    }

    #[test]
    fn a_head_expression_over_a_non_number_does_not_fire() {
        // No value means no fact. Deriving one with a missing argument would
        // be a row the caller cannot tell from a real one.
        let rule = parse_rule("scaled(X, W * 100) :- warmth(X, W).").unwrap();
        let corpus = facts(&[("warmth", vec![s("a"), s("warm")])]);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert!(all.get("scaled").map(|r| r.is_empty()).unwrap_or(true));
    }

    #[test]
    fn a_head_expression_that_divides_by_zero_does_not_fire() {
        let rule = parse_rule("ratio(X, W / 0) :- warmth(X, W).").unwrap();
        let corpus = facts(&[("warmth", vec![s("a"), n(5.0)])]);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert!(all.get("ratio").map(|r| r.is_empty()).unwrap_or(true));
    }

    #[test]
    fn an_expression_is_only_computed_in_the_head_not_in_a_body_atom() {
        // A body atom is a pattern to unify against, not something to compute,
        // so `W + 1` there stays an opaque term and the rule carries no head
        // expression. It simply will not match a numeric row, which is the
        // correct reading of a pattern that no fact has.
        // Left alone it would have become a VARIABLE NAMED "W + 1" — unbound,
        // therefore matching every row, so the rule fires on everything and
        // looks like it worked. That is the silent-wrongness shape, so it is
        // rejected instead.
        let err = parse_rule("q(X) :- warmth(X, W + 1).")
            .unwrap_err()
            .to_string();
        assert!(err.contains("compute"), "got: {err}");
    }

    #[test]
    fn a_uuid_or_date_in_a_head_argument_is_not_mistaken_for_arithmetic() {
        // Both are full of hyphens. Reading one as a subtraction would turn a
        // constant into an expression over variables that do not exist.
        let id = uuid::Uuid::from_u128(0x550e8400_e29b_41d4_a716_446655440000);
        let rule = parse_rule(&format!("q(X, {id}) :- p(X).")).unwrap();
        assert!(rule.head_exprs.is_empty());
        let rule2 = parse_rule(r#"q(X, "2026-01-15") :- p(X)."#).unwrap();
        assert!(rule2.head_exprs.is_empty());
    }

    // ── Item 4: min/max over any ordered term ─────────────────────

    /// Assert a computed numeric answer to within float tolerance.
    ///
    /// Exact bit equality is the wrong assertion for anything that has been
    /// through a square root or a division: instruction selection differs by
    /// architecture, and this suite runs on arm64 locally and x86_64 in CI.
    /// `stddev` over the textbook group is 2.0 on one and 1.9999999999999998
    /// on the other, and both are right.
    fn assert_near(got: Option<Term>, want: f64) {
        match got {
            Some(Term::ConstFloat(OrderedFloat(v))) => {
                assert!((v - want).abs() < 1e-9, "expected about {want}, got {v}")
            }
            other => panic!("expected a number near {want}, got {other:?}"),
        }
    }

    /// The term bound to `pred`'s second argument for a given key.
    fn one_term(all: &FactSet, pred: &str, key: &Term) -> Option<Term> {
        all.get(pred)?
            .iter()
            .find(|args| args.first() == Some(key))
            .and_then(|args| args.get(1).cloned())
    }

    #[test]
    fn min_and_max_order_strings() {
        // Timestamps are stored as strings in several places here, which is
        // the case that actually bites.
        let corpus = facts(&[
            ("doc", vec![s("d")]),
            ("created", vec![s("d"), s("2026-03-01")]),
            ("created", vec![s("d"), s("2026-01-15")]),
            ("created", vec![s("d"), s("2026-07-09")]),
        ]);
        let lo = parse_rule("earliest(X, T) :- doc(X), min(created(X, C), C, T).").unwrap();
        let hi = parse_rule("latest(X, T) :- doc(X), max(created(X, C), C, T).").unwrap();
        let (all_lo, _) = evaluate(&[lo], &corpus, 100, 10_000);
        let (all_hi, _) = evaluate(&[hi], &corpus, 100, 10_000);
        assert_eq!(
            one_term(&all_lo, "earliest", &s("d")),
            Some(s("2026-01-15"))
        );
        assert_eq!(one_term(&all_hi, "latest", &s("d")), Some(s("2026-07-09")));
    }

    #[test]
    fn min_and_max_order_uuids() {
        let (a, b) = (uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2));
        let corpus = facts(&[
            ("grp", vec![s("g")]),
            ("member", vec![s("g"), Term::Const(b)]),
            ("member", vec![s("g"), Term::Const(a)]),
        ]);
        let rule = parse_rule("first(X, M) :- grp(X), min(member(X, U), U, M).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(one_term(&all, "first", &s("g")), Some(Term::Const(a)));
    }

    #[test]
    fn min_and_max_still_order_numbers_and_stay_numeric() {
        let corpus = facts(&[
            ("acct", vec![s("a")]),
            ("spend", vec![s("a"), n(10.0)]),
            ("spend", vec![s("a"), n(60.0)]),
        ]);
        let rule = parse_rule("lo(X, T) :- acct(X), min(spend(X, A), A, T).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(one_term(&all, "lo", &s("a")), Some(n(10.0)));
    }

    #[test]
    fn a_group_mixing_value_types_derives_nothing() {
        // Comparing a string against a number has no meaningful answer, so
        // the group is refused rather than ordered by an arbitrary rule.
        let corpus = facts(&[
            ("acct", vec![s("a")]),
            ("spend", vec![s("a"), n(10.0)]),
            ("spend", vec![s("a"), s("ten")]),
        ]);
        let rule = parse_rule("lo(X, T) :- acct(X), min(spend(X, A), A, T).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert!(all.get("lo").map(|r| r.is_empty()).unwrap_or(true));
    }

    #[test]
    fn sum_and_avg_still_refuse_a_non_numeric_value() {
        // Only min/max were generalised; totalling strings has no meaning.
        for text in [
            "t(X, T) :- acct(X), sum(spend(X, A), A, T).",
            "t(X, T) :- acct(X), avg(spend(X, A), A, T).",
        ] {
            let corpus = facts(&[("acct", vec![s("a")]), ("spend", vec![s("a"), s("ten")])]);
            let rule = parse_rule(text).unwrap();
            let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
            assert!(
                all.get("t").map(|r| r.is_empty()).unwrap_or(true),
                "{text} should refuse a non-numeric value"
            );
        }
    }

    #[test]
    fn min_over_strings_streams_a_large_group() {
        let mut corpus = FactSet::new();
        corpus.insert("doc", vec![s("d")]);
        for i in 0..20_000u32 {
            corpus.insert("created", vec![s("d"), s(&format!("2026-{i:06}"))]);
        }
        let rule = parse_rule("earliest(X, T) :- doc(X), min(created(X, C), C, T).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 200_000);
        assert_eq!(one_term(&all, "earliest", &s("d")), Some(s("2026-000000")));
    }

    // ── Item 3: boolean connectives in a filter ───────────────────

    fn v_corpus() -> FactSet {
        facts(&[
            ("p", vec![s("a"), n(1.0)]),
            ("p", vec![s("b"), n(5.0)]),
            ("p", vec![s("c"), n(20.0)]),
        ])
    }

    #[test]
    fn a_filter_can_say_or() {
        let rule = parse_rule("q(X) :- p(X, V), V > 10 || V < 2.").unwrap();
        let (all, _) = evaluate(&[rule], &v_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a", "c"]);
    }

    #[test]
    fn a_filter_can_say_and_explicitly() {
        let rule = parse_rule("q(X) :- p(X, V), V > 1 && V < 10.").unwrap();
        let (all, _) = evaluate(&[rule], &v_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["b"]);
    }

    #[test]
    fn a_filter_can_say_not() {
        let rule = parse_rule("q(X) :- p(X, V), !(V > 4).").unwrap();
        let (all, _) = evaluate(&[rule], &v_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // V > 100 || V > 1 && V < 10  is  V > 100 || (V > 1 && V < 10)
        // so only b. If `||` bound tighter it would be (…||…) && V < 10,
        // which would also be only b — so use a case that separates them:
        // V < 2 || V > 4 && V < 10  ==  V < 2 || (V > 4 && V < 10)  -> a, b
        let rule = parse_rule("q(X) :- p(X, V), V < 2 || V > 4 && V < 10.").unwrap();
        let (all, _) = evaluate(&[rule], &v_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a", "b"]);
    }

    #[test]
    fn parentheses_override_boolean_precedence() {
        // (V < 2 || V > 4) && V < 10  -> a and b, but NOT c (20 fails V < 10)
        let rule = parse_rule("q(X) :- p(X, V), (V < 2 || V > 4) && V < 10.").unwrap();
        let (all, _) = evaluate(&[rule], &v_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a", "b"]);
    }

    #[test]
    fn boolean_connectives_compose_with_string_predicates() {
        let corpus = facts(&[
            ("item", vec![s("a"), s("tmp_x")]),
            ("item", vec![s("b"), s("keep.pdf")]),
            ("item", vec![s("c"), s("other.txt")]),
        ]);
        let rule = parse_rule(
            r#"pick(X) :- item(X, N), str_starts_with(N, "tmp_") || str_ends_with(N, ".pdf")."#,
        )
        .unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "pick"), vec!["a", "b"]);
    }

    #[test]
    fn arithmetic_still_parses_inside_a_boolean_filter() {
        // `(V + 1) > 3` must not be mistaken for a parenthesised sub-filter.
        let rule = parse_rule("q(X) :- p(X, V), (V + 1) > 3 && V < 10.").unwrap();
        let (all, _) = evaluate(&[rule], &v_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["b"]);
    }

    #[test]
    fn an_undefined_branch_refuses_the_whole_filter() {
        // Conservative on purpose: an arithmetic error anywhere in the tree
        // refuses, rather than letting a true branch carry a rule that also
        // contains a mistake. Loud beats lucky.
        let rule = parse_rule("q(X) :- p(X, V), V / 0 == 0 || V > 0.").unwrap();
        let (all, _) = evaluate(&[rule], &v_corpus(), 100, 10_000);
        assert!(all.get("q").map(|r| r.is_empty()).unwrap_or(true));
    }

    #[test]
    fn a_multi_part_body_still_ands_its_filters() {
        // The implicit AND between comma-separated filters is unchanged.
        let rule = parse_rule("q(X) :- p(X, V), V > 1, V < 10.").unwrap();
        let (all, _) = evaluate(&[rule], &v_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["b"]);
    }

    // ── Item 2: string predicates ─────────────────────────────────

    fn name_corpus() -> FactSet {
        facts(&[
            ("item", vec![s("a"), s("tmp_scratch")]),
            ("item", vec![s("b"), s("report.pdf")]),
            ("item", vec![s("c"), s("tmp_other")]),
            ("item", vec![s("d"), s("summary.pdf")]),
        ])
    }

    #[test]
    fn starts_with_selects_by_prefix() {
        let rule = parse_rule(r#"temp(X) :- item(X, N), str_starts_with(N, "tmp_")."#).unwrap();
        let (all, _) = evaluate(&[rule], &name_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "temp"), vec!["a", "c"]);
    }

    #[test]
    fn ends_with_selects_by_suffix() {
        let rule = parse_rule(r#"doc(X) :- item(X, N), str_ends_with(N, ".pdf")."#).unwrap();
        let (all, _) = evaluate(&[rule], &name_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "doc"), vec!["b", "d"]);
    }

    #[test]
    fn contains_selects_by_substring() {
        let rule = parse_rule(r#"has(X) :- item(X, N), str_contains(N, "mm")."#).unwrap();
        let (all, _) = evaluate(&[rule], &name_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "has"), vec!["d"]);
    }

    #[test]
    fn a_string_predicate_composes_with_negation() {
        // The requirement that motivated this: "everything except the
        // scratch files", which has no positive encoding.
        let rule =
            parse_rule(r#"keep(X) :- item(X, N), not tombstoned(X), !str_starts_with(N, "tmp_")."#)
                .unwrap();
        let (all, _) = evaluate(&[rule], &name_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "keep"), vec!["b", "d"]);
    }

    #[test]
    fn a_string_predicate_over_both_variables_compares_two_bindings() {
        let corpus = facts(&[
            ("pair", vec![s("a"), s("foobar"), s("foo")]),
            ("pair", vec![s("b"), s("foobar"), s("bar")]),
        ]);
        let rule =
            parse_rule("pre(X) :- pair(X, Whole, Part), str_starts_with(Whole, Part).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "pre"), vec!["a"]);
    }

    #[test]
    fn a_string_predicate_on_a_non_string_derives_nothing() {
        // Undefined, not false and not a pass: asking whether a number
        // starts with a string has no answer.
        let corpus = facts(&[("item", vec![s("a"), n(42.0)])]);
        let rule = parse_rule(r#"bad(X) :- item(X, N), str_starts_with(N, "4")."#).unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert!(all.get("bad").map(|r| r.is_empty()).unwrap_or(true));
    }

    #[test]
    fn a_string_predicate_with_the_wrong_arity_is_rejected_at_parse() {
        let err = parse_rule(r#"bad(X) :- item(X, N), str_starts_with(N)."#)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("str_starts_with"),
            "rejection should name the predicate, got: {err}"
        );
    }

    #[test]
    fn the_builtins_do_not_collide_with_the_contains_edge_type() {
        // `contains` is a real edge type in this system (enrich.rs matches on
        // it), so `contains(X, Y)` is a legitimate stored relation. That is why
        // the builtins carry a reserved `str_` prefix instead of taking the
        // bare names — taking them would have broken rules already written.
        let corpus = facts(&[("contains", vec![s("box"), s("ball")])]);
        let rule = parse_rule("inside(Y) :- contains(X, Y).").unwrap();
        assert!(rule.body.iter().any(|a| a.predicate == "contains"));
        assert!(rule.filters.is_empty(), "not read as a builtin");
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "inside"), vec!["ball"]);
    }

    #[test]
    fn an_unknown_str_prefixed_predicate_is_rejected_rather_than_read_as_a_relation() {
        // The `str_` prefix is reserved. Silently treating a typo'd builtin as
        // a relation over stored facts would derive nothing and look like "no
        // match", which is the silent-wrongness shape.
        let err = parse_rule(r#"bad(X) :- item(X, N), str_startswith(N, "a")."#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("str_startswith"), "got: {err}");
    }

    // ── Round 2, item 5: count over distinct values ───────────────

    #[test]
    fn identical_rows_are_already_deduped_so_both_counts_agree() {
        let corpus = facts(&[
            ("acct", vec![s("a")]),
            ("tag", vec![s("a"), s("red")]),
            ("tag", vec![s("a"), s("red")]),
            ("tag", vec![s("a"), s("blue")]),
        ]);
        let distinct = parse_rule("d(X, N) :- acct(X), count_distinct(tag(X, T), T, N).").unwrap();
        let plain = parse_rule("c(X, N) :- acct(X), count(tag(X, T), N).").unwrap();
        let (all_d, _) = evaluate(&[distinct], &corpus, 100, 10_000);
        let (all_c, _) = evaluate(&[plain], &corpus, 100, 10_000);
        // A FactSet is a set, so identical rows collapse before either fold
        // sees them and the two agree here. Distinctness only becomes visible
        // on rows that DIFFER but repeat in the counted column — which is what
        // the next test does. Kept because it is the boundary between them.
        assert_eq!(one_term(&all_d, "d", &s("a")), Some(n(2.0)));
        assert_eq!(one_term(&all_c, "c", &s("a")), Some(n(2.0)));
    }

    #[test]
    fn count_distinct_collapses_repeats_that_count_does_not() {
        // Two owners, one shared colour: three rows, two distinct colours.
        let corpus = facts(&[
            ("acct", vec![s("a")]),
            ("owns", vec![s("a"), s("x"), s("red")]),
            ("owns", vec![s("a"), s("y"), s("red")]),
            ("owns", vec![s("a"), s("z"), s("blue")]),
        ]);
        let distinct =
            parse_rule("d(X, N) :- acct(X), count_distinct(owns(X, _, C), C, N).").unwrap();
        let plain = parse_rule("c(X, N) :- acct(X), count(owns(X, _, C), N).").unwrap();
        let (all_d, _) = evaluate(&[distinct], &corpus, 100, 10_000);
        let (all_c, _) = evaluate(&[plain], &corpus, 100, 10_000);
        assert_eq!(one_term(&all_d, "d", &s("a")), Some(n(2.0)), "two colours");
        assert_eq!(one_term(&all_c, "c", &s("a")), Some(n(3.0)), "three rows");
    }

    #[test]
    fn count_distinct_over_no_rows_is_zero_and_fires() {
        let corpus = facts(&[("acct", vec![s("carol")])]);
        let rule = parse_rule("d(X, N) :- acct(X), count_distinct(tag(X, T), T, N).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(one_term(&all, "d", &s("carol")), Some(n(0.0)));
    }

    #[test]
    fn count_distinct_needs_a_value_variable_like_the_other_folds() {
        let err = parse_rule("d(X, N) :- acct(X), count_distinct(tag(X, T), N).")
            .unwrap_err()
            .to_string();
        assert!(err.contains("count_distinct"), "got: {err}");
    }

    #[test]
    fn count_distinct_orders_mixed_kinds_without_confusing_them() {
        // Distinctness is equality, not ordering, so mixed kinds are fine
        // here even though min/max refuses them.
        let corpus = facts(&[
            ("acct", vec![s("a")]),
            ("v", vec![s("a"), s("k1"), s("1")]),
            ("v", vec![s("a"), s("k2"), n(1.0)]),
        ]);
        let rule = parse_rule("d(X, N) :- acct(X), count_distinct(v(X, _, V), V, N).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(
            one_term(&all, "d", &s("a")),
            Some(n(2.0)),
            "the string \"1\" and the number 1 are different values"
        );
    }

    #[test]
    fn count_distinct_refuses_a_group_past_its_cap_rather_than_growing_without_bound() {
        // This is the one fold that cannot stream: distinctness needs a set,
        // and the set grows with the answer. Bounded and loud beats unbounded.
        let mut corpus = FactSet::new();
        corpus.insert("acct", vec![s("a")]);
        for i in 0..=(DISTINCT_VALUE_CAP as u32) {
            corpus.insert("tag", vec![s("a"), s(&format!("t{i}"))]);
        }
        let rule = parse_rule("d(X, N) :- acct(X), count_distinct(tag(X, T), T, N).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 5_000_000);
        assert!(
            all.get("d").map(|r| r.is_empty()).unwrap_or(true),
            "past the cap the group derives nothing rather than a truncated count"
        );
    }

    // ── Round 2, item 4: atom-level disjunction ───────────────────

    #[test]
    fn a_body_can_offer_alternatives() {
        let rules = parse_rules("q(X) :- p(X) ; r(X).").unwrap();
        assert_eq!(rules.len(), 2, "one rule per alternative");
        assert!(rules.iter().all(|r| r.head.predicate == "q"));
        let corpus = facts(&[("p", vec![s("a")]), ("r", vec![s("b")])]);
        let (all, _) = evaluate(&rules, &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a", "b"]);
    }

    #[test]
    fn a_comma_binds_tighter_than_a_semicolon() {
        // `a, b ; c` is `(a, b) ; c`, as in Prolog.
        let rules = parse_rules("q(X) :- p(X), t(X) ; r(X).").unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(
            rules[0].body.len(),
            2,
            "first alternative is the conjunction"
        );
        assert_eq!(rules[1].body.len(), 1);
    }

    #[test]
    fn a_parenthesised_alternative_distributes_over_the_conjunction() {
        let rules = parse_rules("q(X) :- base(X), (p(X) ; r(X)).").unwrap();
        assert_eq!(rules.len(), 2);
        for rule in &rules {
            assert!(rule.body.iter().any(|a| a.predicate == "base"));
        }
        let corpus = facts(&[
            ("base", vec![s("a")]),
            ("base", vec![s("b")]),
            ("base", vec![s("c")]),
            ("p", vec![s("a")]),
            ("r", vec![s("b")]),
        ]);
        let (all, _) = evaluate(&rules, &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a", "b"], "c matches neither");
    }

    #[test]
    fn two_groups_produce_the_cartesian_product_of_alternatives() {
        let rules = parse_rules("q(X) :- (a(X) ; b(X)), (c(X) ; d(X)).").unwrap();
        assert_eq!(rules.len(), 4);
    }

    #[test]
    fn a_rule_with_no_disjunction_expands_to_exactly_itself() {
        let rules = parse_rules("q(X) :- p(X), r(X).").unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0], parse_rule("q(X) :- p(X), r(X).").unwrap());
    }

    #[test]
    fn the_singular_parser_refuses_a_rule_that_expands_to_several() {
        // parse_rule is used where exactly one rule is expected; silently
        // returning the first alternative would drop the others.
        let err = parse_rule("q(X) :- p(X) ; r(X).").unwrap_err().to_string();
        assert!(
            err.contains("parse_rules") || err.contains("alternative"),
            "got: {err}"
        );
    }

    #[test]
    fn disjunction_composes_with_negation_and_filters() {
        let corpus = facts(&[
            ("p", vec![s("a")]),
            ("r", vec![s("b")]),
            ("r", vec![s("c")]),
            ("banned", vec![s("c")]),
        ]);
        let rules = parse_rules("q(X) :- (p(X) ; r(X)), not banned(X).").unwrap();
        let (all, _) = evaluate(&rules, &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a", "b"]);
    }

    #[test]
    fn a_semicolon_inside_a_string_is_not_an_alternative() {
        let rules = parse_rules(r#"q(X) :- p(X, "a;b")."#).unwrap();
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn a_runaway_expansion_is_refused_rather_than_silently_enormous() {
        // Six binary groups is 64 rules; seven is 128. The cap fails loud
        // rather than quietly compiling one rule into hundreds.
        let body: String = (0..7)
            .map(|i| format!("(a{i}(X) ; b{i}(X))"))
            .collect::<Vec<_>>()
            .join(", ");
        let err = parse_rules(&format!("q(X) :- {body}."))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("alternative") || err.contains("expand"),
            "got: {err}"
        );
    }

    // ── Round 2, item 3: bind a computed value in the body ────────

    #[test]
    fn a_body_can_bind_a_computed_value() {
        let corpus = facts(&[("p", vec![s("a"), n(4.0)])]);
        let rule = parse_rule("q(X, D) :- p(X, V), D := V * 2.").unwrap();
        assert_eq!(rule.bindings.len(), 1);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(one_term(&all, "q", &s("a")), Some(n(8.0)));
    }

    #[test]
    fn a_bound_value_is_usable_by_a_later_filter() {
        let corpus = facts(&[("p", vec![s("a"), n(4.0)]), ("p", vec![s("b"), n(1.0)])]);
        let rule = parse_rule("q(X) :- p(X, V), D := V * 2, D > 5.").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    #[test]
    fn a_binding_may_build_on_an_earlier_binding() {
        let corpus = facts(&[("p", vec![s("a"), n(2.0)])]);
        let rule = parse_rule("q(X, F) :- p(X, V), D := V * 2, F := D + 1.").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(one_term(&all, "q", &s("a")), Some(n(5.0)));
    }

    #[test]
    fn equals_still_compares_and_does_not_bind() {
        // Redefining `=` would silently change the meaning of stored rules.
        // `X = W + 1` remains a comparison, and with X unbound it is
        // undecidable, so it passes — exactly as before.
        let rule = parse_rule("q(X) :- p(X, V), Y = V + 1.").unwrap();
        assert!(rule.bindings.is_empty(), "`=` binds nothing");
        assert_eq!(rule.filters.len(), 1, "`=` is still a comparison");
    }

    #[test]
    fn a_binding_whose_right_side_is_unbound_is_rejected() {
        let err = parse_rule("q(X, D) :- p(X, V), D := Missing * 2.")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Missing"), "must name it, got: {err}");
    }

    #[test]
    fn a_binding_that_shadows_a_body_variable_is_rejected() {
        // Rebinding a variable the body already bound would make the rule read
        // two ways, and which one wins would depend on evaluation order.
        let err = parse_rule("q(X, V) :- p(X, V), V := 1.")
            .unwrap_err()
            .to_string();
        assert!(err.contains('V'), "must name it, got: {err}");
    }

    #[test]
    fn two_bindings_cannot_use_the_same_name() {
        let err = parse_rule("q(X, D) :- p(X, V), D := V + 1, D := V + 2.")
            .unwrap_err()
            .to_string();
        assert!(err.contains('D'), "must name it, got: {err}");
    }

    #[test]
    fn a_binding_that_has_no_value_does_not_fire_the_rule() {
        let corpus = facts(&[("p", vec![s("a"), s("text")])]);
        let rule = parse_rule("q(X, D) :- p(X, V), D := V * 2.").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert!(all.get("q").map(|r| r.is_empty()).unwrap_or(true));
    }

    #[test]
    fn a_binding_can_call_a_function() {
        let corpus = facts(&[("item", vec![s("a"), s("Report")])]);
        let rule = parse_rule("q(X, L) :- item(X, N), L := lower(N).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(one_term(&all, "q", &s("a")), Some(s("report")));
    }

    // ── Round 2, item 2: function calls in an expression ──────────

    #[test]
    fn numeric_functions_are_available_in_an_expression() {
        let corpus = facts(&[("p", vec![s("a"), n(-3.7)]), ("p", vec![s("b"), n(2.2)])]);
        for (text, expect) in [
            ("q(X) :- p(X, V), abs(V) > 3.", vec!["a"]),
            ("q(X) :- p(X, V), floor(V) == -4.", vec!["a"]),
            ("q(X) :- p(X, V), ceil(V) == 3.", vec!["b"]),
            ("q(X) :- p(X, V), round(V) == 2.", vec!["b"]),
        ] {
            let rule = parse_rule(text).unwrap();
            let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
            assert_eq!(derived_keys(&all, "q"), expect, "{text}");
        }
    }

    #[test]
    fn string_functions_are_available_in_an_expression() {
        let corpus = facts(&[("item", vec![s("a"), s("Report.PDF")])]);
        for text in [
            r#"q(X) :- item(X, N), len(N) == 10."#,
            r#"q(X) :- item(X, N), lower(N) == "report.pdf"."#,
            r#"q(X) :- item(X, N), upper(N) == "REPORT.PDF"."#,
            r#"q(X) :- item(X, N), concat(N, "!") == "Report.PDF!"."#,
        ] {
            let rule = parse_rule(text).unwrap();
            let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
            assert_eq!(derived_keys(&all, "q"), vec!["a"], "{text}");
        }
    }

    #[test]
    fn a_function_call_nests_and_composes_with_arithmetic() {
        let corpus = facts(&[("p", vec![s("a"), n(-2.0)])]);
        let rule = parse_rule("q(X) :- p(X, V), abs(V) * 3 == 6.").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    #[test]
    fn a_function_call_works_in_a_head_expression() {
        let corpus = facts(&[("p", vec![s("a"), n(-2.5)])]);
        let rule = parse_rule("r(X, abs(V)) :- p(X, V).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(one_term(&all, "r", &s("a")), Some(n(2.5)));
    }

    #[test]
    fn an_unknown_function_is_rejected_by_name() {
        // A closed whitelist. Silently reading `frobnicate(V)` as a variable
        // would make it match everything, which is the failure this keeps
        // refusing.
        let err = parse_rule("q(X) :- p(X, V), frobnicate(V) > 1.")
            .unwrap_err()
            .to_string();
        assert!(err.contains("frobnicate"), "got: {err}");
    }

    #[test]
    fn a_function_called_with_the_wrong_arity_is_rejected_by_name() {
        let err = parse_rule("q(X) :- p(X, V), abs(V, 2) > 1.")
            .unwrap_err()
            .to_string();
        assert!(err.contains("abs"), "got: {err}");
    }

    #[test]
    fn a_function_applied_to_the_wrong_type_derives_nothing() {
        // Undefined, matching the rest of the evaluator — not false, which
        // `!` would flip into a spurious pass.
        let corpus = facts(&[("item", vec![s("a"), s("text")])]);
        let rule = parse_rule("q(X) :- item(X, N), abs(N) > 1.").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert!(all.get("q").map(|r| r.is_empty()).unwrap_or(true));
    }

    #[test]
    fn a_user_predicate_may_still_be_named_like_a_function() {
        // Functions live in expressions; atoms are matched. `len(X, Y)` in a
        // body atom position is a relation, and stays one.
        let corpus = facts(&[("len", vec![s("a"), n(3.0)])]);
        let rule = parse_rule("q(X) :- len(X, Y).").unwrap();
        assert!(rule.body.iter().any(|a| a.predicate == "len"));
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    #[test]
    fn a_comma_inside_a_string_literal_is_not_an_argument_separator() {
        // Regression, and pre-existing: `split_top_level` tracked parentheses
        // and brackets but not strings, so this split in the middle of its
        // own argument. Nothing had used a comma inside a literal before
        // group_concat's separator made it unavoidable.
        let rule = parse_rule(r#"q(X) :- p(X, "a,b")."#).unwrap();
        assert_eq!(rule.body.len(), 1);
        assert_eq!(rule.body[0].args.len(), 2);
        assert_eq!(rule.body[0].args[1], s("a,b"));

        let corpus = facts(&[("p", vec![s("x"), s("a,b")])]);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["x"]);
    }

    #[test]
    fn an_escaped_quote_inside_a_literal_does_not_end_it() {
        let rule = parse_rule(r#"q(X) :- p(X, "a\",b")."#).unwrap();
        assert_eq!(rule.body[0].args.len(), 2);
    }

    // ── Null and three-valued semantics ───────────────────────────

    fn null_corpus() -> FactSet {
        facts(&[
            ("p", vec![s("a"), n(10.0)]),
            ("p", vec![s("b"), Term::ConstNull]),
            ("p", vec![s("c"), n(1.0)]),
        ])
    }

    #[test]
    fn null_parses_as_a_value_not_an_identifier() {
        let rule = parse_rule("q(X) :- p(X, V), is_null(V).").unwrap();
        assert_eq!(rule.filters.len(), 1);
        let head = parse_rule("q(X, null) :- p(X, _).").unwrap();
        assert_eq!(head.head.args[1], Term::ConstNull);
    }

    #[test]
    fn an_identifier_merely_containing_null_is_still_a_variable() {
        let rule = parse_rule("q(X) :- p(X, Nullable), Nullable > 1.").unwrap();
        assert!(
            rule.body[0]
                .args
                .iter()
                .any(|a| *a == Term::Var("Nullable".into()))
        );
    }

    #[test]
    fn comparing_to_null_is_unknown_so_it_never_fires() {
        // SQL's rule: `WHERE x = NULL` returns no rows, and neither does
        // `x != NULL`. Unknown is not false, but it does not pass.
        for text in [
            "q(X) :- p(X, V), V == null.",
            "q(X) :- p(X, V), V != null.",
            "q(X) :- p(X, V), V > null.",
        ] {
            let rule = parse_rule(text).unwrap();
            let (all, _) = evaluate(&[rule], &null_corpus(), 100, 10_000);
            assert!(
                all.get("q").map(|r| r.is_empty()).unwrap_or(true),
                "{text} must derive nothing"
            );
        }
    }

    #[test]
    fn is_null_is_how_you_actually_ask() {
        let rule = parse_rule("q(X) :- p(X, V), is_null(V).").unwrap();
        let (all, _) = evaluate(&[rule], &null_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["b"]);
    }

    #[test]
    fn not_is_null_gives_the_negative_and_is_never_unknown() {
        let rule = parse_rule("q(X) :- p(X, V), !is_null(V).").unwrap();
        let (all, _) = evaluate(&[rule], &null_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a", "c"]);
    }

    // ── Kleene, and the line between Unknown and Undefined ────────

    #[test]
    fn true_or_unknown_is_true() {
        // The Kleene rule. Under the old poisoning propagation this derived
        // nothing, because any no-answer refused the whole tree.
        let rule = parse_rule("q(X) :- p(X, V), V > 5 || V == null.").unwrap();
        let (all, _) = evaluate(&[rule], &null_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"], "a is 10, so true wins");
    }

    #[test]
    fn false_and_unknown_is_false_which_negates_to_true() {
        // The only way to observe False rather than Unknown is through `!`,
        // since neither passes on its own.
        let rule = parse_rule("q(X) :- p(X, V), !(V > 100 && V == null).").unwrap();
        let (all, _) = evaluate(&[rule], &null_corpus(), 100, 10_000);
        assert_eq!(
            derived_keys(&all, "q"),
            vec!["a", "c"],
            "false && unknown is false, and !false is true"
        );
    }

    #[test]
    fn not_unknown_is_still_unknown() {
        let rule = parse_rule("q(X) :- p(X, V), !(V == null).").unwrap();
        let (all, _) = evaluate(&[rule], &null_corpus(), 100, 10_000);
        assert!(
            all.get("q").map(|r| r.is_empty()).unwrap_or(true),
            "negating unknown does not make it true"
        );
    }

    #[test]
    fn an_error_still_poisons_even_beside_a_true_branch() {
        // The safety decision this design exists to preserve. Kleene applies
        // to Unknown; an ERROR is not Unknown and must still refuse.
        let rule = parse_rule("q(X) :- p(X, V), V > 5 || V / 0 == 1.").unwrap();
        let (all, _) = evaluate(&[rule], &null_corpus(), 100, 10_000);
        assert!(
            all.get("q").map(|r| r.is_empty()).unwrap_or(true),
            "an arithmetic error must not be masked by a true sibling"
        );
    }

    #[test]
    fn arithmetic_on_null_is_null_not_a_type_error() {
        // If `null + 1` were a type error it would poison, and the whole
        // design would collapse back to the old behaviour.
        let rule = parse_rule("q(X) :- p(X, V), V + 1 > 5 || V > 5.").unwrap();
        let (all, _) = evaluate(&[rule], &null_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"], "b's null stays unknown");
    }

    #[test]
    fn a_function_of_null_is_null() {
        let rule = parse_rule("q(X) :- p(X, V), abs(V) > 5 || V > 5.").unwrap();
        let (all, _) = evaluate(&[rule], &null_corpus(), 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    // ── Aggregates ────────────────────────────────────────────────

    #[test]
    fn value_aggregates_skip_nulls_the_way_sql_does() {
        let corpus = facts(&[
            ("acct", vec![s("a")]),
            ("v", vec![s("a"), s("k1"), n(2.0)]),
            ("v", vec![s("a"), s("k2"), Term::ConstNull]),
            ("v", vec![s("a"), s("k3"), n(4.0)]),
        ]);
        for (text, pred, want) in [
            ("t(X, R) :- acct(X), sum(v(X, _, V), V, R).", "t", 6.0),
            ("t(X, R) :- acct(X), avg(v(X, _, V), V, R).", "t", 3.0),
            ("t(X, R) :- acct(X), min(v(X, _, V), V, R).", "t", 2.0),
            ("t(X, R) :- acct(X), max(v(X, _, V), V, R).", "t", 4.0),
        ] {
            let rule = parse_rule(text).unwrap();
            let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
            assert_near(one_term(&all, pred, &s("a")), want);
        }
    }

    #[test]
    fn count_counts_rows_including_the_null_ones() {
        // `count` counts unifications, so it is `count(*)`, not `count(col)`.
        let corpus = facts(&[
            ("acct", vec![s("a")]),
            ("v", vec![s("a"), s("k1"), n(2.0)]),
            ("v", vec![s("a"), s("k2"), Term::ConstNull]),
        ]);
        let rule = parse_rule("t(X, N) :- acct(X), count(v(X, _, V), N).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_near(one_term(&all, "t", &s("a")), 2.0);
    }

    #[test]
    fn a_group_of_nothing_but_nulls_is_an_empty_group() {
        let corpus = facts(&[
            ("acct", vec![s("a")]),
            ("v", vec![s("a"), s("k1"), Term::ConstNull]),
        ]);
        let sum = parse_rule("t(X, R) :- acct(X), sum(v(X, _, V), V, R).").unwrap();
        let min = parse_rule("m(X, R) :- acct(X), min(v(X, _, V), V, R).").unwrap();
        let (all_s, _) = evaluate(&[sum], &corpus, 100, 10_000);
        let (all_m, _) = evaluate(&[min], &corpus, 100, 10_000);
        assert_near(one_term(&all_s, "t", &s("a")), 0.0);
        assert!(
            all_m.get("m").map(|r| r.is_empty()).unwrap_or(true),
            "there is no minimum of nothing"
        );
    }

    // ── Time ──────────────────────────────────────────────────────

    fn at(iso: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(iso)
            .expect("test timestamp")
            .with_timezone(&chrono::Utc)
    }

    /// Documents dated across three weeks, as ISO-8601 STRINGS — which is how
    /// timestamps are actually stored in this corpus.
    fn dated_corpus() -> FactSet {
        facts(&[
            ("doc", vec![s("fresh"), s("2026-08-27T09:00:00Z")]),
            ("doc", vec![s("lastweek"), s("2026-08-24T09:00:00Z")]),
            ("doc", vec![s("old"), s("2026-07-01T09:00:00Z")]),
        ])
    }

    const NOW: &str = "2026-08-28T12:00:00Z";

    #[test]
    fn date_parses_the_strings_timestamps_are_stored_as() {
        let rule = parse_rule(r#"q(X) :- doc(X, C), date(C) > date("2026-08-25")."#).unwrap();
        let (all, _) = evaluate_at(&[rule], &dated_corpus(), 100, 10_000, at(NOW));
        assert_eq!(derived_keys(&all, "q"), vec!["fresh"]);
    }

    #[test]
    fn date_accepts_a_bare_day_as_well_as_a_full_timestamp() {
        let rule =
            parse_rule(r#"q(X) :- p(X), date("2026-08-25") < date("2026-08-26T00:00:00Z")."#)
                .unwrap();
        let corpus = facts(&[("p", vec![s("a")])]);
        let (all, _) = evaluate_at(&[rule], &corpus, 100, 10_000, at(NOW));
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    #[test]
    fn an_unparseable_date_is_an_error_not_a_null() {
        // The data is not what the rule thought it was, which is a mistake
        // rather than an absence — so Undefined, which poisons.
        let corpus = facts(&[("doc", vec![s("a"), s("not-a-date")])]);
        let rule = parse_rule("q(X) :- doc(X, C), date(C) < now() || 1 == 1.").unwrap();
        let (all, _) = evaluate_at(&[rule], &corpus, 100, 10_000, at(NOW));
        assert!(
            all.get("q").map(|r| r.is_empty()).unwrap_or(true),
            "a bad timestamp must not be masked by a true sibling"
        );
    }

    #[test]
    fn date_of_null_is_null() {
        let corpus = facts(&[("doc", vec![s("a"), Term::ConstNull])]);
        let rule = parse_rule("q(X) :- doc(X, C), date(C) < now() || 1 == 1.").unwrap();
        let (all, _) = evaluate_at(&[rule], &corpus, 100, 10_000, at(NOW));
        assert_eq!(
            derived_keys(&all, "q"),
            vec!["a"],
            "true || unknown is true"
        );
    }

    #[test]
    fn last_week_is_expressible() {
        // The sentence that started this.
        let rule = parse_rule("recent(X) :- doc(X, C), date(C) > now() - days(7).").unwrap();
        let (all, _) = evaluate_at(&[rule], &dated_corpus(), 100, 10_000, at(NOW));
        assert_eq!(derived_keys(&all, "recent"), vec!["fresh", "lastweek"]);
    }

    #[test]
    fn older_than_is_expressible_as_the_other_direction() {
        let rule = parse_rule("stale(X) :- doc(X, C), now() - date(C) > days(30).").unwrap();
        let (all, _) = evaluate_at(&[rule], &dated_corpus(), 100, 10_000, at(NOW));
        assert_eq!(derived_keys(&all, "stale"), vec!["old"]);
    }

    #[test]
    fn every_duration_helper_works() {
        let corpus = facts(&[("p", vec![s("a")])]);
        for text in [
            "q(X) :- p(X), weeks(1) == days(7).",
            "q(X) :- p(X), days(1) == hours(24).",
            "q(X) :- p(X), hours(1) == minutes(60).",
        ] {
            let rule = parse_rule(text).unwrap();
            let (all, _) = evaluate_at(&[rule], &corpus, 100, 10_000, at(NOW));
            assert_eq!(derived_keys(&all, "q"), vec!["a"], "{text}");
        }
    }

    #[test]
    fn a_time_minus_a_time_is_a_duration() {
        let corpus = facts(&[("p", vec![s("a")])]);
        let rule =
            parse_rule(r#"q(X) :- p(X), date("2026-08-28") - date("2026-08-21") == days(7)."#)
                .unwrap();
        let (all, _) = evaluate_at(&[rule], &corpus, 100, 10_000, at(NOW));
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    #[test]
    fn a_time_survives_into_a_head_and_orders_like_any_term() {
        let rule = parse_rule("when(X, date(C)) :- doc(X, C).").unwrap();
        let (all, _) = evaluate_at(&[rule], &dated_corpus(), 100, 10_000, at(NOW));
        assert_eq!(
            one_term(&all, "when", &s("old")),
            Some(Term::ConstTime(at("2026-07-01T09:00:00Z")))
        );

        // min/max order times through the same machinery that orders any term.
        let mx = parse_rule("latest(T) :- anchor(A), max(doc(_, C), C, T).").unwrap();
        let corpus = {
            let mut f = dated_corpus();
            f.insert("anchor", vec![s("a")]);
            f
        };
        let (all2, _) = evaluate_at(&[mx], &corpus, 100, 10_000, at(NOW));
        assert!(all2.get("latest").is_some());
    }

    // ── The two correctness properties ────────────────────────────

    #[test]
    fn the_clock_is_read_once_so_every_row_sees_the_same_instant() {
        // Read per row, two rows in one run could disagree about "now", and a
        // rule near a boundary would include one and exclude the other for no
        // reason a person could see.
        let corpus = facts(&[("p", vec![s("a")]), ("p", vec![s("b")])]);
        let rule = parse_rule("stamp(X, now()) :- p(X).").unwrap();
        let (all, _) = evaluate_at(&[rule], &corpus, 100, 10_000, at(NOW));
        let a = one_term(&all, "stamp", &s("a"));
        let b = one_term(&all, "stamp", &s("b"));
        assert_eq!(a, b, "both rows must see one instant");
        assert_eq!(a, Some(Term::ConstTime(at(NOW))));
    }

    #[test]
    fn a_rule_that_reads_the_clock_is_never_cached() {
        // The same non-monotonicity negation has, by a different door: this
        // fact stops being true with no base fact changing at all.
        let id = uuid::Uuid::new_v4();
        let corpus = facts(&[("doc", vec![Term::Const(id), s("2026-08-27T09:00:00Z")])]);
        let rule = parse_rule("recent(X, X) :- doc(X, C), date(C) > now() - days(7).").unwrap();
        let (_, derived) = evaluate_at(&[rule], &corpus, 100, 10_000, at(NOW));
        let fact = derived
            .iter()
            .find(|d| d.pred == "recent")
            .expect("derived");
        assert!(
            !fact.is_cacheable(),
            "a clock-dependent derivation must never be persisted"
        );
    }

    #[test]
    fn a_rule_that_does_not_read_the_clock_stays_cacheable() {
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let corpus = facts(&[("p", vec![Term::Const(a), Term::Const(b)])]);
        let rule = parse_rule("q(X, Y) :- p(X, Y).").unwrap();
        let (_, derived) = evaluate_at(&[rule], &corpus, 100, 10_000, at(NOW));
        let fact = derived.iter().find(|d| d.pred == "q").unwrap();
        assert!(fact.is_cacheable());
    }

    // ── The completeness guard ────────────────────────────────────

    /// Three specs in a row have claimed the grammar was complete by reading
    /// the types and writing prose. Twice that was wrong.
    ///
    /// This test is the claim in a form that breaks. It pins the surface each
    /// spec enumerated, so ADDING a variant fails here and forces the next
    /// author to re-run the completeness pass and record the answer, rather
    /// than inheriting a stale "complete".
    #[test]
    fn the_grammar_surface_is_the_one_the_spec_signed_off() {
        use crate::types::{AggregateKind as A, ArithOp, CmpOp, Func, StrOp};

        // Aggregates: 5 streaming folds, 1 bounded-distinct, 3 whole-group.
        let aggregates = [
            A::Count,
            A::Sum,
            A::Min,
            A::Max,
            A::Avg,
            A::StdDev,
            A::CountDistinct,
            A::Median,
            A::Percentile,
            A::GroupConcat,
        ];
        assert_eq!(aggregates.len(), 10, "an aggregate was added or removed");
        assert_eq!(
            aggregates.iter().filter(|k| k.retains_group()).count(),
            3,
            "the set of folds that cannot stream changed; that is a design \
             decision, not an implementation detail"
        );

        // Arithmetic, comparison, string shape, functions.
        let _exhaustive_arith = |op: ArithOp| match op {
            ArithOp::Add
            | ArithOp::Sub
            | ArithOp::Mul
            | ArithOp::Div
            | ArithOp::Rem
            | ArithOp::Pow => (),
        };
        let _exhaustive_cmp = |op: CmpOp| match op {
            CmpOp::Eq | CmpOp::Ne | CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => (),
        };
        assert_eq!(StrOp::ALL.len(), 3, "a string predicate changed");
        // 16: eight general, four temporal, two spatial, and the clock.
        // Grew when GeoJSON arrived, which is the third time this guard has
        // forced a re-read rather than let a stale claim ride.
        assert_eq!(Func::ALL.len(), 16, "a function was added or removed");
        assert_eq!(
            crate::geojson::SpatialRelation::ALL.len(),
            8,
            "the DE-9IM set is fixed by the standard, not by us"
        );
        assert_eq!(
            Func::ALL.iter().filter(|f| f.reads_the_clock()).count(),
            1,
            "exactly one function is non-deterministic; that is a design \
             decision, since every such rule becomes uncacheable"
        );

        // Terms: Var, Const(Uuid), ConstStr, ConstFloat, ConstNull.
        //
        // `ConstNull` is here because this guard did its job. It failed when
        // the variant was added, which forced the completeness pass to be run
        // again rather than inherited — and that pass is what found the
        // earlier reasoning for declining null to be wrong.
        //
        // Boolean and list remain declined; see the two tests above.
        let _exhaustive_term = |t: Term| match t {
            Term::Var(_)
            | Term::Const(_)
            | Term::ConstStr(_)
            | Term::ConstFloat(_)
            | Term::ConstNull
            // Added when the NL-editor grill turned up a sentence with no
            // representation at all — "from last week". The guard failed on
            // it, which is the second time it has forced a real re-read
            // rather than an inherited claim.
            | Term::ConstTime(_) => (),
        };

        // Five verdicts, because "no answer" has three causes with three
        // propagations. Collapsing any pair has been a bug each time.
        let _exhaustive_verdict = |v: Verdict| match v {
            Verdict::True
            | Verdict::False
            | Verdict::Unknown
            | Verdict::Undefined
            | Verdict::Unbound => (),
        };
    }

    // ── Final, item 5: the term kinds, decided ────────────────────

    #[test]
    fn truth_has_exactly_one_spelling_which_is_why_a_boolean_term_is_declined() {
        // A boolean term would give truth a SECOND spelling. `flag(X, true)`
        // and `flag(X, "true")` would be different terms that do not unify,
        // and every flag already stored is a string — so a rule written with
        // the literal would silently stop matching its own data.
        let corpus = facts(&[("flag", vec![s("a"), s("true")])]);
        let rule = parse_rule(r#"on(X) :- flag(X, "true")."#).unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "on"), vec!["a"]);

        // And the idiomatic form needs no value at all: presence is truth.
        let corpus2 = facts(&[("active", vec![s("b")])]);
        let rule2 = parse_rule("on(X) :- active(X).").unwrap();
        let (all2, _) = evaluate(&[rule2], &corpus2, 100, 10_000);
        assert_eq!(derived_keys(&all2, "on"), vec!["b"]);
    }

    #[test]
    fn absence_is_already_expressible_which_is_why_a_null_term_is_declined() {
        // Datalog's answer to "no value" is the absence of a fact, and
        // negation says it directly. A null VALUE would need three-valued
        // comparison semantics the engine deliberately does not have, and
        // would be a THIRD kind of no-value beside Unbound and Undefined,
        // which the filter evaluator already distinguishes on purpose.
        let corpus = facts(&[
            ("item", vec![s("a")]),
            ("item", vec![s("b")]),
            ("owner", vec![s("b"), s("someone")]),
        ]);
        let rule = parse_rule("unowned(X) :- item(X), not owner(X, _).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "unowned"), vec!["a"]);
    }

    #[test]
    fn a_group_of_values_comes_back_as_a_string_which_is_why_a_list_term_is_declined() {
        // The capability a list term was wanted for. `DerivedFact` carries
        // its endpoints as strings, so a list-valued argument would be
        // flattened at that boundary regardless — the list would buy nothing
        // and cost unification, ordering and a stored-format change.
        let corpus = facts(&[
            ("acct", vec![s("a")]),
            ("tag", vec![s("a"), s("k1"), s("x")]),
            ("tag", vec![s("a"), s("k2"), s("y")]),
        ]);
        let rule =
            parse_rule(r#"g(X, S) :- acct(X), group_concat(tag(X, _, T), T, "|", S)."#).unwrap();
        let (_, derived) = evaluate(&[rule], &corpus, 100, 10_000);
        let fact = derived.iter().find(|d| d.pred == "g").unwrap();
        assert_eq!(
            fact.dst_id, "x|y",
            "the values survive as a string endpoint"
        );
    }

    // ── Final, items 2-4: the last aggregates ─────────────────────

    fn spread_corpus() -> FactSet {
        // 2, 4, 4, 4, 5, 5, 7, 9 — the textbook population with mean 5 and
        // population stddev exactly 2.
        let mut f = FactSet::new();
        f.insert("acct", vec![s("a")]);
        for (k, v) in [
            ("k1", 2.0),
            ("k2", 4.0),
            ("k3", 4.0),
            ("k4", 4.0),
            ("k5", 5.0),
            ("k6", 5.0),
            ("k7", 7.0),
            ("k8", 9.0),
        ] {
            f.insert("v", vec![s("a"), s(k), n(v)]);
        }
        f
    }

    #[test]
    fn stddev_folds_the_spread_of_a_group() {
        let rule = parse_rule("d(X, S) :- acct(X), stddev(v(X, _, V), V, S).").unwrap();
        let (all, _) = evaluate(&[rule], &spread_corpus(), 100, 10_000);
        assert_near(one_term(&all, "d", &s("a")), 2.0);
    }

    #[test]
    fn stddev_of_a_single_value_is_zero() {
        let corpus = facts(&[("acct", vec![s("a")]), ("v", vec![s("a"), s("k"), n(7.0)])]);
        let rule = parse_rule("d(X, S) :- acct(X), stddev(v(X, _, V), V, S).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_near(one_term(&all, "d", &s("a")), 0.0);
    }

    #[test]
    fn stddev_over_no_rows_does_not_fire() {
        let corpus = facts(&[("acct", vec![s("carol")])]);
        let rule = parse_rule("d(X, S) :- acct(X), stddev(v(X, _, V), V, S).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert!(all.get("d").map(|r| r.is_empty()).unwrap_or(true));
    }

    #[test]
    fn stddev_streams_a_large_group() {
        // Welford: one pass, constant memory. This would be a 20k-element
        // vector if it were retained like the order statistics.
        let mut corpus = FactSet::new();
        corpus.insert("acct", vec![s("a")]);
        for i in 0..20_000u32 {
            corpus.insert("v", vec![s("a"), s(&format!("k{i}")), n(5.0)]);
        }
        let rule = parse_rule("d(X, S) :- acct(X), stddev(v(X, _, V), V, S).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 200_000);
        assert_near(one_term(&all, "d", &s("a")), 0.0);
    }

    #[test]
    fn median_takes_the_middle_of_a_group() {
        // 2,4,4,4,5,5,7,9 -> even count, so the mean of the middle pair.
        let rule = parse_rule("m(X, M) :- acct(X), median(v(X, _, V), V, M).").unwrap();
        let (all, _) = evaluate(&[rule], &spread_corpus(), 100, 10_000);
        assert_near(one_term(&all, "m", &s("a")), 4.5);
    }

    #[test]
    fn median_of_an_odd_group_is_an_actual_member() {
        let corpus = facts(&[
            ("acct", vec![s("a")]),
            ("v", vec![s("a"), s("k1"), n(1.0)]),
            ("v", vec![s("a"), s("k2"), n(100.0)]),
            ("v", vec![s("a"), s("k3"), n(3.0)]),
        ]);
        let rule = parse_rule("m(X, M) :- acct(X), median(v(X, _, V), V, M).").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(one_term(&all, "m", &s("a")), Some(n(3.0)));
    }

    #[test]
    fn percentile_takes_a_fraction_and_median_is_the_half_of_it() {
        let corpus = spread_corpus();
        let p50 = parse_rule("p(X, R) :- acct(X), percentile(v(X, _, V), V, 0.5, R).").unwrap();
        let med = parse_rule("p(X, R) :- acct(X), median(v(X, _, V), V, R).").unwrap();
        let (all_p, _) = evaluate(&[p50], &corpus, 100, 10_000);
        let (all_m, _) = evaluate(&[med], &corpus, 100, 10_000);
        assert_eq!(
            one_term(&all_p, "p", &s("a")),
            one_term(&all_m, "p", &s("a"))
        );

        let hi = parse_rule("t(X, R) :- acct(X), percentile(v(X, _, V), V, 1.0, R).").unwrap();
        let (all_hi, _) = evaluate(&[hi], &corpus, 100, 10_000);
        assert_eq!(
            one_term(&all_hi, "t", &s("a")),
            Some(n(9.0)),
            "p100 is the max"
        );
    }

    #[test]
    fn a_percentile_outside_zero_to_one_is_rejected_at_parse() {
        let err = parse_rule("t(X, R) :- acct(X), percentile(v(X, _, V), V, 1.5, R).")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("1.5") || err.contains("percentile"),
            "got: {err}"
        );
    }

    #[test]
    fn group_concat_returns_the_values_not_a_statistic() {
        let corpus = facts(&[
            ("acct", vec![s("a")]),
            ("tag", vec![s("a"), s("k1"), s("red")]),
            ("tag", vec![s("a"), s("k2"), s("blue")]),
        ]);
        let rule =
            parse_rule(r#"g(X, S) :- acct(X), group_concat(tag(X, _, T), T, ", ", S)."#).unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        // Sorted, so the answer does not depend on fact-set iteration order.
        assert_eq!(one_term(&all, "g", &s("a")), Some(s("blue, red")));
    }

    #[test]
    fn group_concat_over_no_rows_is_the_empty_string_and_fires() {
        let corpus = facts(&[("acct", vec![s("carol")])]);
        let rule =
            parse_rule(r#"g(X, S) :- acct(X), group_concat(tag(X, _, T), T, ",", S)."#).unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(one_term(&all, "g", &s("carol")), Some(s("")));
    }

    #[test]
    fn the_whole_group_folds_refuse_a_group_past_the_cap() {
        // median, percentile and group_concat all retain the group, so all
        // three are bounded the way count_distinct is.
        let mut corpus = FactSet::new();
        corpus.insert("acct", vec![s("a")]);
        for i in 0..=(RETAINED_VALUE_CAP as u32) {
            corpus.insert("v", vec![s("a"), s(&format!("k{i}")), n(f64::from(i))]);
        }
        for (text, pred) in [
            ("m(X, R) :- acct(X), median(v(X, _, V), V, R).", "m"),
            (
                "p(X, R) :- acct(X), percentile(v(X, _, V), V, 0.9, R).",
                "p",
            ),
            (
                r#"g(X, R) :- acct(X), group_concat(v(X, _, V), V, ",", R)."#,
                "g",
            ),
        ] {
            let rule = parse_rule(text).unwrap();
            let (all, _) = evaluate(&[rule], &corpus, 100, 5_000_000);
            assert!(
                all.get(pred).map(|r| r.is_empty()).unwrap_or(true),
                "{text} must derive nothing past the cap"
            );
        }
    }

    // ── Final, item 1: exponentiation ─────────────────────────────

    #[test]
    fn arithmetic_supports_exponentiation() {
        let corpus = facts(&[("p", vec![s("a"), n(3.0)])]);
        let rule = parse_rule("q(X) :- p(X, V), V ** 2 == 9.").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    #[test]
    fn exponentiation_binds_tighter_than_multiplication() {
        // 2 * 3 ** 2  is  2 * (3 ** 2) = 18, not (2 * 3) ** 2 = 36.
        let corpus = facts(&[("p", vec![s("a"), n(3.0)])]);
        let rule = parse_rule("q(X) :- p(X, V), 2 * V ** 2 == 18.").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    #[test]
    fn exponentiation_is_right_associative() {
        // 2 ** 3 ** 2 is 2 ** (3 ** 2) = 512, not (2 ** 3) ** 2 = 64.
        let corpus = facts(&[("p", vec![s("a"), n(2.0)])]);
        let rule = parse_rule("q(X) :- p(X, V), V ** 3 ** 2 == 512.").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    #[test]
    fn exponentiation_works_in_a_head_and_a_binding() {
        let corpus = facts(&[("p", vec![s("a"), n(4.0)])]);
        let head = parse_rule("sq(X, V ** 2) :- p(X, V).").unwrap();
        let bind = parse_rule("b(X, S) :- p(X, V), S := V ** 2.").unwrap();
        let (all_h, _) = evaluate(&[head], &corpus, 100, 10_000);
        let (all_b, _) = evaluate(&[bind], &corpus, 100, 10_000);
        assert_eq!(one_term(&all_h, "sq", &s("a")), Some(n(16.0)));
        assert_eq!(one_term(&all_b, "b", &s("a")), Some(n(16.0)));
    }

    #[test]
    fn an_exponentiation_with_no_real_answer_derives_nothing() {
        // (-8) ** 0.5 is not a real number. f64 answers NaN, which is a value
        // the caller cannot tell from a real one.
        let corpus = facts(&[("p", vec![s("a"), n(-8.0)])]);
        let rule = parse_rule("q(X) :- p(X, V), V ** 0.5 > 0.").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert!(all.get("q").map(|r| r.is_empty()).unwrap_or(true));
    }

    // ── Round 2, item 1: set membership ───────────────────────────

    #[test]
    fn a_filter_can_test_set_membership() {
        let corpus = facts(&[
            ("item", vec![s("a"), s("red")]),
            ("item", vec![s("b"), s("blue")]),
            ("item", vec![s("c"), s("green")]),
        ]);
        let rule = parse_rule(r#"warm(X) :- item(X, C), C in ["red", "orange"]."#).unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "warm"), vec!["a"]);
    }

    #[test]
    fn set_membership_works_over_numbers() {
        let corpus = facts(&[("p", vec![s("a"), n(1.0)]), ("p", vec![s("b"), n(7.0)])]);
        let rule = parse_rule("q(X) :- p(X, V), V in [1, 2, 3].").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    #[test]
    fn set_membership_desugars_to_a_disjunction_of_equalities() {
        // Not a new evaluator path — it is the `Any(Eq..)` the author would
        // otherwise have typed by hand.
        let rule = parse_rule(r#"q(X) :- p(X, C), C in ["a", "b"]."#).unwrap();
        match &rule.filters[0] {
            crate::types::BuiltinFilter::Any(branches) => {
                assert_eq!(branches.len(), 2);
                assert!(branches.iter().all(|b| matches!(
                    b,
                    crate::types::BuiltinFilter::Compare {
                        op: crate::types::CmpOp::Eq,
                        ..
                    }
                )));
            }
            other => panic!("expected Any of Eq, got {other:?}"),
        }
    }

    #[test]
    fn a_single_element_set_is_a_plain_equality() {
        let rule = parse_rule(r#"q(X) :- p(X, C), C in ["a"]."#).unwrap();
        assert!(matches!(
            &rule.filters[0],
            crate::types::BuiltinFilter::Compare {
                op: crate::types::CmpOp::Eq,
                ..
            }
        ));
    }

    #[test]
    fn set_membership_negates_and_composes() {
        let corpus = facts(&[
            ("item", vec![s("a"), s("red")]),
            ("item", vec![s("b"), s("blue")]),
        ]);
        let rule = parse_rule(r#"cool(X) :- item(X, C), !(C in ["red"])."#).unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "cool"), vec!["b"]);
    }

    #[test]
    fn an_empty_set_is_rejected_rather_than_matching_nothing() {
        // `C in []` can never hold. It is always a mistake, and a filter that
        // silently matches nothing looks exactly like "no rows".
        let err = parse_rule("q(X) :- p(X, C), C in [].")
            .unwrap_err()
            .to_string();
        assert!(!err.is_empty(), "an empty set literal is rejected");
    }

    #[test]
    fn the_left_side_of_in_may_be_an_expression() {
        let corpus = facts(&[("p", vec![s("a"), n(2.0)])]);
        let rule = parse_rule("q(X) :- p(X, V), V * 2 in [4, 8].").unwrap();
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "q"), vec!["a"]);
    }

    // ── Item 1: modulo ────────────────────────────────────────────

    #[test]
    fn arithmetic_supports_modulo() {
        let rule = parse_rule("even(X) :- num(X, V), V % 2 == 0.").unwrap();
        assert_eq!(rule.filters.len(), 1);
        let corpus = facts(&[
            ("num", vec![s("a"), n(4.0)]),
            ("num", vec![s("b"), n(7.0)]),
            ("num", vec![s("c"), n(0.0)]),
        ]);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "even"), vec!["a", "c"]);
    }

    #[test]
    fn modulo_binds_as_tightly_as_multiplication() {
        // 1 + 7 % 4  parses as  1 + (7 % 4)  = 4, not (1 + 7) % 4 = 0.
        let rule = parse_rule("ok(X) :- num(X, V), 1 + V % 4 == 4.").unwrap();
        let corpus = facts(&[("num", vec![s("a"), n(7.0)])]);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "ok"), vec!["a"]);
    }

    #[test]
    fn modulo_by_zero_derives_nothing_rather_than_a_nan() {
        // Matches division by zero, which already refuses rather than
        // producing a value the caller cannot tell from a real one.
        let rule = parse_rule("bad(X) :- num(X, V), V % 0 == 0.").unwrap();
        let corpus = facts(&[("num", vec![s("a"), n(4.0)])]);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert!(
            all.get("bad").map(|r| r.is_empty()).unwrap_or(true),
            "modulo by zero must not derive anything"
        );
    }

    #[test]
    fn division_by_zero_no_longer_passes_the_filter() {
        // Regression: `eval_expr` returned None both for an unbound variable
        // and for a zero divisor, and `check_one_filter` passed on None, so
        // this derived a fact off an arithmetic error.
        let rule = parse_rule("bad(X) :- num(X, V), V / 0 == 0.").unwrap();
        let corpus = facts(&[("num", vec![s("a"), n(4.0)])]);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert!(all.get("bad").map(|r| r.is_empty()).unwrap_or(true));
    }

    #[test]
    fn an_unbound_variable_still_passes_the_filter() {
        // The other half of the split: "cannot be decided yet" keeps the
        // legacy semantics that partial bindings rely on.
        let rule = parse_rule("ok(X) :- num(X, _), Unbound > 5.").unwrap();
        let corpus = facts(&[("num", vec![s("a"), n(4.0)])]);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert_eq!(derived_keys(&all, "ok"), vec!["a"]);
    }

    #[test]
    fn modulo_of_a_non_number_derives_nothing() {
        let rule = parse_rule("bad(X) :- num(X, V), V % 2 == 0.").unwrap();
        let corpus = facts(&[("num", vec![s("a"), s("not-a-number")])]);
        let (all, _) = evaluate(&[rule], &corpus, 100, 10_000);
        assert!(all.get("bad").map(|r| r.is_empty()).unwrap_or(true));
    }
}

#[cfg(test)]
mod numeric_harm_measurement {
    use super::*;

    /// Measurement for round-2 item 6, kept as a test so the conclusion stays
    /// checkable rather than becoming a claim in a commit message.
    #[test]
    fn a_whole_number_already_renders_without_a_decimal_point() {
        assert_eq!(term_to_string(&Term::ConstFloat(OrderedFloat(3.0))), "3");
        assert_eq!(term_to_string(&Term::ConstFloat(OrderedFloat(0.0))), "0");
        assert_eq!(term_to_string(&Term::ConstFloat(OrderedFloat(2.5))), "2.5");
    }

    #[test]
    fn f64_is_exact_for_integers_far_beyond_anything_this_engine_counts() {
        // Every numeric fact the loader produces is a score in [0,1]
        // (`confidence`, `warmth`); the rest are folds over those. f64 holds
        // integers exactly to 2^53, which is nine quadrillion.
        let limit = (2f64).powi(53);
        assert_eq!(limit + 1.0, limit, "2^53 is where exactness ends");
        assert!(limit > 9.0e15, "and that is far past any count here");
    }
}
