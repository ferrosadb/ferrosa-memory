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
    /// The kind of thing this relates FROM, as an `fo:` class id.
    ///
    /// Required by the published package contract, and worth having anyway:
    /// without it nothing can check that a rule relates the kinds of thing
    /// the relation was meant for.
    pub domain_of: String,
    /// The kind of thing this relates TO.
    pub range_of: String,
    pub characteristics: Vec<Characteristic>,
}

impl PredicateDef {
    #[allow(clippy::too_many_arguments)]
    fn new(
        name: &str,
        arity: usize,
        domain: Domain,
        kind: PredicateKind,
        meaning: &str,
        domain_of: &str,
        range_of: &str,
        characteristics: Vec<Characteristic>,
    ) -> Self {
        Self {
            name: name.to_string(),
            arity,
            domain,
            kind,
            meaning: meaning.to_string(),
            domain_of: domain_of.to_string(),
            range_of: range_of.to_string(),
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
                    "fo:Class",
                    "fo:Class",
                    vec![Transitive, Irreflexive],
                ),
                p(
                    "instance_of",
                    2,
                    Structural,
                    Base,
                    "this particular thing is one of that kind",
                    "fo:Instance",
                    "fo:Class",
                    vec![],
                ),
                p(
                    "part_of",
                    2,
                    Structural,
                    Base,
                    "the first thing is a part of the second",
                    "fo:Instance",
                    "fo:Instance",
                    vec![Transitive, Irreflexive, InverseOf("contains".into())],
                ),
                p(
                    "contains",
                    2,
                    Structural,
                    Base,
                    "the first thing has the second inside it",
                    "fo:Instance",
                    "fo:Instance",
                    vec![InverseOf("part_of".into())],
                ),
                p(
                    "disjoint_with",
                    2,
                    Structural,
                    Base,
                    "nothing can be both of these kinds at once",
                    "fo:Class",
                    "fo:Class",
                    vec![Symmetric, DisjointClasses],
                ),
                p(
                    "related_to",
                    2,
                    Structural,
                    Base,
                    "these two are connected, without saying how",
                    "fo:Instance",
                    "fo:Instance",
                    vec![Symmetric],
                ),
                // ── Business: who answers for what ────────────────────
                p(
                    "owns",
                    2,
                    Business,
                    Base,
                    "this person or team owns that thing",
                    "fo:Instance",
                    "fo:Instance",
                    vec![InverseOf("owned_by".into())],
                ),
                p(
                    "owned_by",
                    2,
                    Business,
                    Base,
                    "this thing is owned by that person or team",
                    "fo:Instance",
                    "fo:Instance",
                    vec![InverseOf("owns".into())],
                ),
                p(
                    "reports_to",
                    2,
                    Business,
                    Base,
                    "this person reports to that one",
                    "fo:Instance",
                    "fo:Instance",
                    vec![Transitive, Irreflexive, InverseOf("manages".into())],
                ),
                p(
                    "manages",
                    2,
                    Business,
                    Base,
                    "this person manages that one",
                    "fo:Instance",
                    "fo:Instance",
                    vec![InverseOf("reports_to".into())],
                ),
                p(
                    "member_of",
                    2,
                    Business,
                    Base,
                    "this person or team belongs to that group",
                    "fo:Instance",
                    "fo:Instance",
                    vec![Transitive],
                ),
                p(
                    "accountable_for",
                    2,
                    Business,
                    Base,
                    "this person answers for that thing",
                    "fo:Instance",
                    "fo:Instance",
                    vec![],
                ),
                p(
                    "approves",
                    2,
                    Business,
                    Base,
                    "this person approved that thing",
                    "fo:Instance",
                    "fo:Instance",
                    vec![],
                ),
                p(
                    "supersedes",
                    2,
                    Business,
                    Base,
                    "this replaces that",
                    "fo:Instance",
                    "fo:Instance",
                    vec![Transitive, Irreflexive],
                ),
                p(
                    "version_of",
                    2,
                    Business,
                    Base,
                    "this is one version of that work",
                    "fo:Instance",
                    "fo:Instance",
                    vec![],
                ),
                p(
                    "located_in",
                    2,
                    Business,
                    Base,
                    "this is in that place",
                    "fo:Instance",
                    "fo:Instance",
                    vec![Transitive, Irreflexive],
                ),
                // ── Technical: what rests on what ─────────────────────
                p(
                    "depends_on",
                    2,
                    Technical,
                    Base,
                    "this needs that to work",
                    "fo:Instance",
                    "fo:Instance",
                    vec![Transitive, Irreflexive],
                ),
                p(
                    "calls",
                    2,
                    Technical,
                    Base,
                    "this calls that",
                    "fo:Instance",
                    "fo:Instance",
                    vec![SubPropertyOf("depends_on".into())],
                ),
                p(
                    "uses",
                    2,
                    Technical,
                    Base,
                    "this uses that",
                    "fo:Instance",
                    "fo:Instance",
                    vec![SubPropertyOf("depends_on".into())],
                ),
                p(
                    "implements",
                    2,
                    Technical,
                    Base,
                    "this is an implementation of that",
                    "fo:Instance",
                    "fo:Instance",
                    vec![],
                ),
                p(
                    "references",
                    2,
                    Technical,
                    Base,
                    "this mentions that",
                    "fo:Instance",
                    "fo:Instance",
                    vec![],
                ),
                p(
                    "deployed_on",
                    2,
                    Technical,
                    Base,
                    "this runs on that",
                    "fo:Instance",
                    "fo:Instance",
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
                    "fo:Instance",
                    "fo:Property",
                    vec![],
                ),
                p(
                    "updated_at",
                    2,
                    Temporal,
                    Computed,
                    "when this last changed",
                    "fo:Instance",
                    "fo:Property",
                    vec![],
                ),
                p(
                    "valid_until",
                    2,
                    Temporal,
                    Computed,
                    "when this stops applying",
                    "fo:Instance",
                    "fo:Property",
                    vec![],
                ),
            ],
        }
    }
}

/// A portable ontology package, in the shape `ferrosa-experts` publishes and
/// validates.
///
/// The point of exporting is that there is then only ONE vocabulary. A Rust
/// copy and a published copy that are merely similar is the same hazard D1
/// names for the grammar: two sides holding independent copies of one
/// vocabulary, and one of them stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Package {
    pub manifest: Manifest,
    pub document: Document,
    /// Reasoning this vocabulary has that the package format cannot carry.
    ///
    /// Reported rather than dropped. A published package that silently
    /// reasons LESS than the vocabulary it claims to be is worse than one
    /// that says what it left behind.
    pub unrepresentable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub id: String,
    pub version: String,
    pub title: String,
    pub entrypoint: String,
    pub license: String,
    pub mutable: bool,
    pub dependencies: Vec<String>,
    pub exports: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    #[serde(rename = "@context")]
    pub context: serde_json::Value,
    #[serde(rename = "@graph")]
    pub graph: Vec<serde_json::Value>,
}

impl Vocabulary {
    /// Export as a portable package.
    pub fn to_package(&self) -> Package {
        use serde_json::json;

        let mut graph: Vec<serde_json::Value> = Vec::new();

        // The meta-types every term points at. Present because the contract
        // requires `fo:Ontology`, and because a reference that does not
        // resolve inside the document is a dangling pointer for an importer.
        for (id, label, definition) in [
            ("fo:Ontology", "Ontology", "A versioned set of terms."),
            (
                "fo:Class",
                "Class",
                "A concept whose instances may be classified.",
            ),
            ("fo:Instance", "Instance", "A particular thing."),
            (
                "fo:Relationship",
                "Relationship",
                "A way two things may be related.",
            ),
            ("fo:Property", "Property", "A value a thing carries."),
        ] {
            graph.push(json!({
                "id": id, "type": "fo:Class",
                "label": label, "definition": definition
            }));
        }

        let mut unrepresentable = Vec::new();

        for p in &self.predicates {
            let mut carried: Vec<&str> = Vec::new();
            for c in &p.characteristics {
                match c {
                    Characteristic::Transitive => carried.push("transitive"),
                    Characteristic::Symmetric => carried.push("symmetric"),
                    Characteristic::Irreflexive => carried.push("irreflexive"),
                    Characteristic::DisjointClasses => carried.push("disjoint_classes"),
                    // The v1 schema has nowhere to put these two, and they
                    // generate most of the reasoning here. Say so.
                    Characteristic::InverseOf(other) => {
                        unrepresentable.push(format!("{}: inverse_of({other})", p.name))
                    }
                    Characteristic::SubPropertyOf(parent) => {
                        unrepresentable.push(format!("{}: sub_property_of({parent})", p.name))
                    }
                }
            }
            graph.push(json!({
                "id": format!("fo:{}", p.name),
                "type": "fo:Relationship",
                "label": p.name.replace('_', " "),
                "definition": p.meaning,
                "domain": p.domain_of,
                "range": p.range_of,
                "predicate_kind": match p.kind {
                    PredicateKind::Base => "base",
                    PredicateKind::Derived => "derived",
                    PredicateKind::Computed => "computed",
                },
                "characteristic": carried,
            }));
        }

        Package {
            manifest: Manifest {
                format: "ferrosa-ontology-package/v1".into(),
                id: "urn:ferrosa:ontology:reasoning".into(),
                version: format!("{}.0.0", self.version),
                title: "Ferrosa Reasoning Vocabulary".into(),
                entrypoint: "ontology.jsonld".into(),
                license: "Apache-2.0".into(),
                mutable: false,
                dependencies: vec!["urn:ferrosa:ontology:base".into()],
                exports: self.predicates.iter().map(|p| p.name.clone()).collect(),
            },
            document: Document {
                context: json!({
                    "fo": "https://ferrosa.ai/ontology/v1#",
                    "id": "@id",
                    "type": "@type",
                    "label": "http://www.w3.org/2004/02/skos/core#prefLabel",
                    "definition": "http://www.w3.org/2004/02/skos/core#definition",
                    "subclass_of": {"@id": "fo:subclass_of", "@type": "@id"},
                    "domain": {"@id": "fo:domain", "@type": "@id"},
                    "range": {"@id": "fo:range", "@type": "@id"},
                    "characteristic": "fo:characteristic",
                    "predicate_kind": "fo:predicate_kind"
                }),
                graph,
            },
            unrepresentable,
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

    // ── Exporting to the portable package format ──────────────────

    #[test]
    fn every_relation_declares_what_it_relates() {
        // `ferrosa-experts`' package contract requires domain and range on
        // every relationship. Without them a vocabulary cannot be published,
        // and more importantly nothing can check that a rule relates the kinds
        // of thing the relation was meant for.
        for p in v().predicates {
            assert!(
                !p.domain_of.is_empty(),
                "{} does not say what it relates FROM",
                p.name
            );
            assert!(
                !p.range_of.is_empty(),
                "{} does not say what it relates TO",
                p.name
            );
        }
    }

    #[test]
    fn the_export_is_a_package_the_published_contract_accepts() {
        let pkg = v().to_package();
        assert_eq!(pkg.manifest.format, "ferrosa-ontology-package/v1");
        assert!(pkg.manifest.id.starts_with("urn:ferrosa:ontology:"));
        assert!(
            !pkg.manifest.mutable,
            "a published package must be immutable"
        );
        assert!(
            !pkg.manifest.entrypoint.contains('/'),
            "entrypoint is package-local"
        );

        let doc: serde_json::Value = serde_json::to_value(&pkg.document).unwrap();
        assert_eq!(doc["@context"]["fo"], "https://ferrosa.ai/ontology/v1#");
        let graph = doc["@graph"].as_array().expect("a graph");
        assert!(!graph.is_empty());
        assert!(
            graph.iter().any(|t| t["id"] == "fo:Ontology"),
            "the base meta-type must be present"
        );
    }

    #[test]
    fn every_exported_term_carries_what_the_validator_requires() {
        let doc = serde_json::to_value(v().to_package().document).unwrap();
        let graph = doc["@graph"].as_array().unwrap().clone();
        let ids: Vec<String> = graph
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_string())
            .collect();

        for term in &graph {
            let id = term["id"].as_str().unwrap();
            assert!(term["type"].is_string(), "{id} has no type");
            assert!(
                term["label"].as_str().is_some_and(|l| !l.is_empty()),
                "{id} has no label"
            );
            if term["type"] == "fo:Relationship" {
                assert!(term.get("domain").is_some(), "{id} needs a domain");
                assert!(term.get("range").is_some(), "{id} needs a range");
            }
            // Every reference must resolve inside the document, or an
            // importer receives a dangling pointer.
            for field in ["domain", "range", "subclass_of"] {
                if let Some(val) = term.get(field) {
                    let targets: Vec<String> = match val {
                        serde_json::Value::String(s) => vec![s.clone()],
                        serde_json::Value::Array(a) => {
                            a.iter().map(|x| x.as_str().unwrap().to_string()).collect()
                        }
                        other => panic!("{id}.{field} is {other:?}"),
                    };
                    for t in targets {
                        assert!(ids.contains(&t), "{id}.{field} points at unknown {t}");
                    }
                }
            }
        }
    }

    #[test]
    fn only_characteristics_the_published_schema_knows_are_exported() {
        // The schema's set is {transitive, symmetric, irreflexive,
        // disjoint_classes}. Exporting anything else would produce a package
        // its own validator rejects.
        const KNOWN: [&str; 4] = ["transitive", "symmetric", "irreflexive", "disjoint_classes"];
        let doc = serde_json::to_value(v().to_package().document).unwrap();
        for term in doc["@graph"].as_array().unwrap() {
            if let Some(cs) = term.get("characteristic").and_then(|c| c.as_array()) {
                for c in cs {
                    let c = c.as_str().unwrap();
                    assert!(KNOWN.contains(&c), "{c} is not in the published schema");
                }
            }
        }
    }

    #[test]
    fn a_characteristic_the_package_cannot_carry_is_reported_not_dropped() {
        // `inverse_of` and `sub_property_of` generate most of the reasoning
        // here and the v1 package schema has nowhere to put them. Silently
        // dropping them would publish a vocabulary that reasons LESS than the
        // one it claims to be, so the export says what it could not carry.
        let pkg = v().to_package();
        assert!(
            !pkg.unrepresentable.is_empty(),
            "the gap is real; the export must name it"
        );
        let joined = pkg.unrepresentable.join(" ");
        assert!(joined.contains("inverse_of"), "got: {joined}");
        assert!(joined.contains("sub_property_of"), "got: {joined}");
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
