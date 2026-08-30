//! The reasoning vocabulary: what a rule can talk *about*, and the rules that
//! follow from what each relation IS.
//!
//! Two ideas carry this module.
//!
//! **If it can be computed, it must not be inferred.** The inference engine
//! exists to build a reasoning engine, not to recompute arithmetic. "Recent"
//! is a date comparison and belongs in a filter; it is deliberately not a
//! predicate here, and [`PredicateKind::Computed`] marks the ones that are
//! read straight off the data so nothing derives them.
//!
//! **A relation's characteristics generate its reasoning.** Declaring
//! `part_of` transitive is what produces its closure rule; declaring
//! `contains` the inverse of `part_of` is what produces the other direction.
//! The alternative — hand-writing the same four rule shapes per relation — is
//! where an ontology quietly stops being consistent.

use serde::{Deserialize, Serialize};

/// Which part of the world a predicate is about. Drives grouping in a palette;
/// carries no reasoning of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    /// How an organisation is arranged and who answers for what.
    Business,
    /// How software is arranged and what rests on what.
    Technical,
    /// Classes, parts and containment — the shape any ontology needs.
    Structural,
    /// When something happened. Facts only; the reasoning over them is
    /// computed, never inferred.
    Temporal,
}

/// Where a predicate's rows come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredicateKind {
    /// Asserted by the corpus. Rules read it; nothing derives it.
    Base,
    /// Produced by reasoning, from the characteristics below.
    Derived,
    /// Read straight off the data, and therefore never derived.
    ///
    /// This is the load-bearing distinction. The inference engine is here to
    /// build a reasoning engine, not to recompute arithmetic somebody could
    /// have asked for directly — so anything computable stays a computation.
    Computed,
}

/// What a relation IS, from which what it implies is generated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Characteristic {
    /// `p(X,Y), p(Y,Z) => p(X,Z)`.
    Transitive,
    /// `p(X,Y) => p(Y,X)`.
    Symmetric,
    /// `p(X,Y) => q(Y,X)`, and the same back. Must be declared on both.
    InverseOf(String),
    /// Every `p` is also a `q`. `calls` is a kind of `depends_on`.
    SubPropertyOf(String),
    /// Nothing may relate to itself — a CONSTRAINT, so it yields a violation
    /// report rather than a fact. Under transitivity this is how a cycle
    /// surfaces instead of quietly closing.
    Irreflexive,
    /// Two classes that share no individual. Yields a violation report.
    DisjointClasses,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PredicateDef {
    pub name: String,
    pub arity: usize,
    pub domain: Domain,
    pub kind: PredicateKind,
    /// One line, in the words of the person choosing it from a palette.
    pub meaning: String,
    pub characteristics: Vec<Characteristic>,
}

impl PredicateDef {
    fn new(
        name: &str,
        arity: usize,
        domain: Domain,
        kind: PredicateKind,
        meaning: &str,
        characteristics: Vec<Characteristic>,
    ) -> Self {
        Self {
            name: name.to_string(),
            arity,
            domain,
            kind,
            meaning: meaning.to_string(),
            characteristics,
        }
    }
}

/// The predicate a constraint violation is reported as.
///
/// One predicate for every kind of violation, carrying which rule tripped and
/// what tripped it, so a broken ontology is answerable rather than merely
/// wrong.
pub const VIOLATION_PREDICATE: &str = "ontology_violation";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Vocabulary {
    pub version: u32,
    pub predicates: Vec<PredicateDef>,
}

impl Vocabulary {
    /// The reasoning rules implied by every characteristic in the vocabulary.
    ///
    /// Generated rather than hand-written: four rule shapes across a few dozen
    /// relations is where an ontology stops being consistent, because the
    /// twentieth one gets typed slightly differently from the first.
    pub fn reasoning_rules(&self) -> Vec<String> {
        let mut out = Vec::new();
        for p in &self.predicates {
            debug_assert!(
                p.kind != PredicateKind::Computed || p.characteristics.is_empty(),
                "a computed predicate must not carry characteristics that derive it"
            );
            for c in &p.characteristics {
                let n = &p.name;
                match c {
                    // Deliberately WITHOUT an `X != Z` guard.
                    //
                    // That guard looks like it protects termination and does
                    // not: the fact set is bounded by the square of the
                    // domain, so the fixpoint closes on its own. What it
                    // actually does is suppress `p(X, X)` — which is exactly
                    // the evidence a cycle leaves, and exactly what the
                    // Irreflexive check reads. A guard that hides the symptom
                    // it was meant to make safe is worse than no guard.
                    Characteristic::Transitive => {
                        out.push(format!("{n}(X, Z) :- {n}(X, Y), {n}(Y, Z)."))
                    }
                    Characteristic::Symmetric => out.push(format!("{n}(Y, X) :- {n}(X, Y).")),
                    Characteristic::InverseOf(other) => {
                        out.push(format!("{other}(Y, X) :- {n}(X, Y)."))
                    }
                    Characteristic::SubPropertyOf(parent) => {
                        out.push(format!("{parent}(X, Y) :- {n}(X, Y)."))
                    }
                    // A constraint reports; it does not assert. Deriving a
                    // fact from a violation would make the broken ontology
                    // look consistent.
                    Characteristic::Irreflexive => out.push(format!(
                        "{VIOLATION_PREDICATE}(X, \"{n} is irreflexive\") :- {n}(X, X)."
                    )),
                    Characteristic::DisjointClasses => out.push(format!(
                        "{VIOLATION_PREDICATE}(E, \"disjoint classes\") :- \
                         instance_of(E, A), instance_of(E, B), {n}(A, B)."
                    )),
                }
            }
        }
        out
    }

    /// The starting vocabulary: enough to reason about an organisation and a
    /// codebase, and no more than can be explained a line at a time.
    pub fn seed() -> Self {
        use Characteristic::*;
        use Domain::*;
        use PredicateKind::*;
        let p = PredicateDef::new;
        Vocabulary {
            version: 1,
            predicates: vec![
                // ── Structural: the shape any ontology needs ──────────
                p(
                    "subclass_of",
                    2,
                    Structural,
                    Base,
                    "the first kind is a kind of the second",
                    vec![Transitive, Irreflexive],
                ),
                p(
                    "instance_of",
                    2,
                    Structural,
                    Base,
                    "this particular thing is one of that kind",
                    vec![],
                ),
                p(
                    "part_of",
                    2,
                    Structural,
                    Base,
                    "the first thing is a part of the second",
                    vec![Transitive, Irreflexive, InverseOf("contains".into())],
                ),
                p(
                    "contains",
                    2,
                    Structural,
                    Base,
                    "the first thing has the second inside it",
                    vec![InverseOf("part_of".into())],
                ),
                p(
                    "disjoint_with",
                    2,
                    Structural,
                    Base,
                    "nothing can be both of these kinds at once",
                    vec![Symmetric, DisjointClasses],
                ),
                p(
                    "related_to",
                    2,
                    Structural,
                    Base,
                    "these two are connected, without saying how",
                    vec![Symmetric],
                ),
                // ── Business: who answers for what ────────────────────
                p(
                    "owns",
                    2,
                    Business,
                    Base,
                    "this person or team owns that thing",
                    vec![InverseOf("owned_by".into())],
                ),
                p(
                    "owned_by",
                    2,
                    Business,
                    Base,
                    "this thing is owned by that person or team",
                    vec![InverseOf("owns".into())],
                ),
                p(
                    "reports_to",
                    2,
                    Business,
                    Base,
                    "this person reports to that one",
                    vec![Transitive, Irreflexive, InverseOf("manages".into())],
                ),
                p(
                    "manages",
                    2,
                    Business,
                    Base,
                    "this person manages that one",
                    vec![InverseOf("reports_to".into())],
                ),
                p(
                    "member_of",
                    2,
                    Business,
                    Base,
                    "this person or team belongs to that group",
                    vec![Transitive],
                ),
                p(
                    "accountable_for",
                    2,
                    Business,
                    Base,
                    "this person answers for that thing",
                    vec![],
                ),
                p(
                    "approves",
                    2,
                    Business,
                    Base,
                    "this person approved that thing",
                    vec![],
                ),
                p(
                    "supersedes",
                    2,
                    Business,
                    Base,
                    "this replaces that",
                    vec![Transitive, Irreflexive],
                ),
                p(
                    "version_of",
                    2,
                    Business,
                    Base,
                    "this is one version of that work",
                    vec![],
                ),
                p(
                    "located_in",
                    2,
                    Business,
                    Base,
                    "this is in that place",
                    vec![Transitive, Irreflexive],
                ),
                // ── Technical: what rests on what ─────────────────────
                p(
                    "depends_on",
                    2,
                    Technical,
                    Base,
                    "this needs that to work",
                    vec![Transitive, Irreflexive],
                ),
                p(
                    "calls",
                    2,
                    Technical,
                    Base,
                    "this calls that",
                    vec![SubPropertyOf("depends_on".into())],
                ),
                p(
                    "uses",
                    2,
                    Technical,
                    Base,
                    "this uses that",
                    vec![SubPropertyOf("depends_on".into())],
                ),
                p(
                    "implements",
                    2,
                    Technical,
                    Base,
                    "this is an implementation of that",
                    vec![],
                ),
                p(
                    "references",
                    2,
                    Technical,
                    Base,
                    "this mentions that",
                    vec![],
                ),
                p(
                    "deployed_on",
                    2,
                    Technical,
                    Base,
                    "this runs on that",
                    vec![],
                ),
                // ── Temporal: facts only ──────────────────────────────
                //
                // Nothing here carries a characteristic, because everything
                // worth asking about time is a COMPUTATION. "Recent" is
                // `date(C) > now() - days(7)`, asked when the question is
                // asked — not a fact somebody derived and then had to keep
                // true.
                p(
                    "created_at",
                    2,
                    Temporal,
                    Computed,
                    "when this was made",
                    vec![],
                ),
                p(
                    "updated_at",
                    2,
                    Temporal,
                    Computed,
                    "when this last changed",
                    vec![],
                ),
                p(
                    "valid_until",
                    2,
                    Temporal,
                    Computed,
                    "when this stops applying",
                    vec![],
                ),
            ],
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn v() -> Vocabulary {
        Vocabulary::seed()
    }

    fn find(name: &str) -> PredicateDef {
        v().predicates
            .into_iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("no predicate '{name}' in the vocabulary"))
    }

    // ── The vocabulary is a contract ──────────────────────────────

    #[test]
    fn the_vocabulary_is_versioned_and_every_predicate_carries_its_meaning() {
        let vocab = v();
        assert!(vocab.version >= 1);
        assert!(
            vocab.predicates.len() >= 20,
            "a usable vocabulary, not a token one"
        );
        for p in &vocab.predicates {
            assert!(p.arity >= 1, "{} has no arguments", p.name);
            assert!(
                !p.meaning.trim().is_empty(),
                "{} has no meaning, so nobody can pick it in a palette",
                p.name
            );
        }
    }

    #[test]
    fn predicate_names_are_unique() {
        let vocab = v();
        let mut seen = HashSet::new();
        for p in &vocab.predicates {
            assert!(seen.insert(p.name.clone()), "'{}' declared twice", p.name);
        }
    }

    #[test]
    fn both_business_and_technical_reasoning_are_covered() {
        let vocab = v();
        for domain in [Domain::Business, Domain::Technical, Domain::Structural] {
            assert!(
                vocab
                    .predicates
                    .iter()
                    .filter(|p| p.domain == domain)
                    .count()
                    >= 4,
                "{domain:?} is too thin to reason with"
            );
        }
    }

    // ── The principle, enforced ───────────────────────────────────

    #[test]
    fn a_computed_predicate_never_generates_a_rule() {
        // "If we can compute we shouldn't infer." A computed predicate is read
        // off the data; deriving it would be the engine recomputing arithmetic
        // it was never meant to do.
        let vocab = v();
        let computed: Vec<&str> = vocab
            .predicates
            .iter()
            .filter(|p| p.kind == PredicateKind::Computed)
            .map(|p| p.name.as_str())
            .collect();
        assert!(!computed.is_empty(), "some predicates ARE computed");

        for rule in vocab.reasoning_rules() {
            let head = rule.split(&['(', ' '][..]).next().unwrap_or_default();
            assert!(
                !computed.contains(&head),
                "'{head}' is computed, so nothing may derive it: {rule}"
            );
        }
    }

    #[test]
    fn recency_is_absent_on_purpose_because_it_is_a_computation() {
        // The clearest case of the principle. A rule asks
        // `date(C) > now() - days(7)`; it does not consult a `recent` fact
        // somebody had to derive and keep true.
        let vocab = v();
        for banned in ["recent", "stale_by_age", "is_old", "expired"] {
            assert!(
                !vocab.predicates.iter().any(|p| p.name == banned),
                "'{banned}' is a computation wearing a predicate's clothes"
            );
        }
    }

    // ── Characteristics generate the reasoning ────────────────────

    #[test]
    fn a_transitive_relation_gets_its_closure() {
        assert!(
            find("part_of")
                .characteristics
                .contains(&Characteristic::Transitive)
        );
        let rules = v().reasoning_rules();
        assert!(
            rules
                .iter()
                .any(|r| r.starts_with("part_of(X, Z) :- part_of(X, Y), part_of(Y, Z)")),
            "no closure rule for part_of in {rules:#?}"
        );
    }

    #[test]
    fn a_symmetric_relation_gets_its_mirror() {
        let rules = v().reasoning_rules();
        assert!(
            rules
                .iter()
                .any(|r| r.contains("disjoint_with(Y, X) :- disjoint_with(X, Y)")),
            "no mirror rule for disjoint_with"
        );
    }

    #[test]
    fn an_inverse_pair_generates_both_directions_and_is_declared_on_both_sides() {
        // A one-sided inverse is the shape that makes an ontology quietly
        // asymmetric: one direction reasons and the other does not.
        let vocab = v();
        for p in &vocab.predicates {
            for c in &p.characteristics {
                if let Characteristic::InverseOf(other) = c {
                    let o = vocab
                        .predicates
                        .iter()
                        .find(|x| &x.name == other)
                        .unwrap_or_else(|| panic!("{} names unknown inverse {other}", p.name));
                    assert!(
                        o.characteristics
                            .contains(&Characteristic::InverseOf(p.name.clone())),
                        "{} says it inverts {}, but {} does not say so back",
                        p.name,
                        other,
                        other
                    );
                }
            }
        }
        let rules = v().reasoning_rules();
        assert!(
            rules
                .iter()
                .any(|r| r.contains("contains(Y, X) :- part_of(X, Y)"))
        );
        assert!(
            rules
                .iter()
                .any(|r| r.contains("part_of(Y, X) :- contains(X, Y)"))
        );
    }

    #[test]
    fn a_sub_property_lifts_into_its_parent() {
        // `calls` is a kind of `depends_on`, so anything that calls, depends.
        let rules = v().reasoning_rules();
        assert!(
            rules
                .iter()
                .any(|r| r.contains("depends_on(X, Y) :- calls(X, Y)")),
            "calls does not lift into depends_on"
        );
    }

    // ── Constraints are checks, not facts ─────────────────────────

    #[test]
    fn an_irreflexive_relation_yields_a_violation_check_not_a_derivation() {
        // A part that is part of itself is a broken ontology, and the engine
        // should be able to SAY so rather than quietly closing the loop.
        let rules = v().reasoning_rules();
        assert!(
            rules
                .iter()
                .any(|r| r.starts_with("ontology_violation") && r.contains("part_of(X, X)")),
            "no irreflexivity check for part_of"
        );
    }

    #[test]
    fn disjoint_classes_yield_a_violation_check() {
        let rules = v().reasoning_rules();
        assert!(
            rules
                .iter()
                .any(|r| r.starts_with("ontology_violation") && r.contains("disjoint_with")),
            "nothing catches an individual in two disjoint classes"
        );
    }

    // ── The engine is the judge of all of it ──────────────────────

    #[test]
    fn every_generated_rule_parses() {
        // D1: the engine decides validity, so ask it rather than trusting the
        // generator.
        for r in v().reasoning_rules() {
            crate::datalog::parse_rules(&r)
                .unwrap_or_else(|e| panic!("generated rule does not parse: {r}\n  {e}"));
        }
    }

    #[test]
    fn the_whole_generated_set_stratifies() {
        // Generating rules per relation makes accidental recursion easy. If
        // the set cannot be stratified the engine derives NOTHING, silently,
        // which would take the entire vocabulary down at once.
        let rules: Vec<crate::types::DatalogRule> = v()
            .reasoning_rules()
            .iter()
            .flat_map(|r| crate::datalog::parse_rules(r).expect("parses"))
            .collect();
        crate::datalog::stratify(&rules).expect("the vocabulary must stratify");
    }

    // ── It actually reasons ───────────────────────────────────────

    #[test]
    fn transitivity_reaches_a_grandparent() {
        use crate::types::{FactSet, Term};
        let rules: Vec<_> = v()
            .reasoning_rules()
            .iter()
            .flat_map(|r| crate::datalog::parse_rules(r).unwrap())
            .collect();
        let mut f = FactSet::new();
        let s = |x: &str| Term::ConstStr(x.to_string());
        f.insert("part_of", vec![s("wheel"), s("car")]);
        f.insert("part_of", vec![s("car"), s("fleet")]);
        let (all, _) = crate::datalog::evaluate(&rules, &f, 100, 100_000);
        assert!(
            all.contains("part_of", &[s("wheel"), s("fleet")]),
            "transitive closure did not reach the grandparent"
        );
        assert!(
            all.contains("contains", &[s("fleet"), s("car")]),
            "the inverse did not follow"
        );
    }

    #[test]
    fn a_cycle_is_reported_rather_than_silently_closed() {
        use crate::types::{FactSet, Term};
        let rules: Vec<_> = v()
            .reasoning_rules()
            .iter()
            .flat_map(|r| crate::datalog::parse_rules(r).unwrap())
            .collect();
        let mut f = FactSet::new();
        let s = |x: &str| Term::ConstStr(x.to_string());
        f.insert("part_of", vec![s("a"), s("b")]);
        f.insert("part_of", vec![s("b"), s("a")]);
        let (all, _) = crate::datalog::evaluate(&rules, &f, 100, 100_000);
        assert!(
            all.get("ontology_violation").is_some_and(|r| !r.is_empty()),
            "a part_of cycle must be reported, not absorbed"
        );
    }

    #[test]
    fn the_motivating_question_is_answerable() {
        // "All versions of the presentation from last week" — the sentence
        // that started the temporal work. Vocabulary supplies `version_of`;
        // the clock supplies the rest, as a COMPUTATION rather than a fact.
        use crate::types::{FactSet, Term};
        let s = |x: &str| Term::ConstStr(x.to_string());
        let mut f = FactSet::new();
        f.insert("version_of", vec![s("deck-v1"), s("deck")]);
        f.insert("version_of", vec![s("deck-v2"), s("deck")]);
        f.insert("version_of", vec![s("other-v1"), s("other")]);
        f.insert("created_at", vec![s("deck-v1"), s("2026-08-27T09:00:00Z")]);
        f.insert("created_at", vec![s("deck-v2"), s("2026-08-26T09:00:00Z")]);
        f.insert("created_at", vec![s("other-v1"), s("2026-01-01T09:00:00Z")]);

        let rule = crate::datalog::parse_rule(
            r#"wanted(V) :- version_of(V, "deck"), created_at(V, C), date(C) > now() - days(7)."#,
        )
        .unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-28T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let (all, _) = crate::datalog::evaluate_at(&[rule], &f, 100, 100_000, now);
        let mut got: Vec<String> = all
            .get("wanted")
            .map(|rows| {
                rows.iter()
                    .filter_map(|a| a.first())
                    .map(|t| match t {
                        Term::ConstStr(x) => x.clone(),
                        other => format!("{other:?}"),
                    })
                    .collect()
            })
            .unwrap_or_default();
        got.sort();
        assert_eq!(got, vec!["deck-v1", "deck-v2"]);
    }
}
