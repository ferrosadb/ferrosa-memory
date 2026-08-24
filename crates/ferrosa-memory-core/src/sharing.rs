//! Module: What a share actually contains — a seed, some hops, and a floor.
//! Correctness: Correct when the reachable set is what the Datalog engine
//! derives rather than what a hand-written walk produces, when nothing below
//! the floor is ever emitted, and when a floor item cannot be used as a bridge
//! to something above it.
//! Last revised: 2026-08-24
//! Last changed: New.
//!
//! # The shape of a share
//!
//! Ben: *"build me a sharing permission that allows for sharing skills +2 links
//! but not corpus data. All of this should be computable datalog rules on the
//! knowledge graph."*
//!
//! A grant is a seed set, a hop depth, and a tier floor. The reachable set is
//! computed by the engine from generated rules, not by a graph walk in Rust —
//! so the same evaluator, provenance and limits apply to a share as to every
//! other inference, and the rules can be read and audited as text.
//!
//! # Why the floor is an allowlist
//!
//! "Not corpus data" cannot be written: the engine has no negation. The same
//! intent is a floor — traverse only through items at or above a tier — which
//! is expressible with the equality the engine does have, once the four tiers
//! are enumerated.
//!
//! The floor is not only a filter on the OUTPUT. An item below it cannot act as
//! a bridge either, because every hop rule requires its target to be shareable,
//! so a path that leaves the allowed set never comes back. That is stricter
//! than "filter the results", and it is the honest reading of "but not corpus
//! data": corpus is not merely hidden, it is not walked.
//!
//! # Why depth is a small number
//!
//! Transitive closure in Datalog is unbounded by construction. A bounded walk
//! is one rule per hop, so depth is chosen from a small set rather than typed
//! as any integer. Two hops is two rules.

use uuid::Uuid;

use crate::datalog::{evaluate, parse_rule};
use crate::tiers::Tier;
use crate::types::{FactSet, Term};

/// The most hops a grant may span.
///
/// Each hop is a generated rule, and a graph of this size fans out fast: the
/// cost of a share is the cost of evaluating it, and an operator choosing "5"
/// from a menu has no way to know they asked for the whole store. Raise it when
/// something real needs it, not in advance.
pub const MAX_HOPS: usize = 3;

/// A durable permission to read part of a graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareGrant {
    /// Where the walk starts. Entity ids, as the engine represents them.
    pub seeds: Vec<Uuid>,
    /// How far it may travel. Clamped to [`MAX_HOPS`].
    pub hops: usize,
    /// Nothing below this tier is emitted, or walked through.
    pub floor: Tier,
}

impl ShareGrant {
    pub fn new(seeds: impl IntoIterator<Item = Uuid>, hops: usize, floor: Tier) -> Self {
        Self {
            seeds: seeds.into_iter().collect(),
            hops: hops.min(MAX_HOPS),
            floor,
        }
    }

    /// The rules this grant compiles to.
    ///
    /// Text, deliberately: a grant that can be read is a grant that can be
    /// audited, and these are the same rules the engine runs — not a
    /// description of them.
    pub fn as_datalog(&self) -> Vec<String> {
        let mut rules = Vec::new();

        // The floor, enumerated. `>=` over an ordered tier is not something the
        // engine can express, so the tiers at or above the floor are listed.
        for tier in Tier::ALL.iter().filter(|tier| **tier >= self.floor) {
            rules.push(format!("shareable(E) :- tier(E, \"{}\").", tier.as_str()));
        }

        // Hop zero: the seeds, if they are themselves shareable. A seed below
        // the floor shares nothing, which is the correct reading of a grant
        // whose own subject is excluded.
        rules.push("share_0(E) :- seed(E), shareable(E).".to_owned());

        // One rule per hop. Every target must be shareable, which is what stops
        // a below-floor item bridging to something above it.
        for hop in 1..=self.hops {
            rules.push(format!(
                "share_{hop}(Y) :- share_{}(X), edge(X, _, Y), shareable(Y).",
                hop - 1
            ));
        }

        // One predicate to ask for, whatever the depth.
        for hop in 0..=self.hops {
            rules.push(format!("shared(E) :- share_{hop}(E)."));
        }
        rules
    }

    /// Everything this grant reaches, given the graph as facts.
    ///
    /// The caller supplies `tier/2` and `edge/3` facts; the seeds are added
    /// here so a grant cannot be evaluated against seeds other than its own.
    pub fn resolve(&self, graph: &FactSet, max_facts: usize) -> Result<Vec<Uuid>, ShareError> {
        let mut rules = Vec::new();
        for body in self.as_datalog() {
            rules.push(parse_rule(&body).map_err(|error| ShareError::Rule {
                body: body.clone(),
                message: error.to_string(),
            })?);
        }

        let mut facts = graph.clone();
        for seed in &self.seeds {
            facts.insert("seed", vec![Term::Const(*seed)]);
        }

        // Iterations bounded by the rule count: this program is a fixed chain
        // and cannot need more rounds than it has links, plus room for the
        // floor and collector rules to settle.
        let (derived, _) = evaluate(&rules, &facts, rules.len() + 2, max_facts);

        let mut reached: Vec<Uuid> = derived
            .facts
            .get("shared")
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| match row.first() {
                        Some(Term::Const(value)) => Some(*value),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Sorted so a grant answers the same way twice. A set iteration order
        // that varies between reads would make a share look like it had
        // changed when nothing had.
        reached.sort();
        reached.dedup();
        Ok(reached)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareError {
    /// A generated rule was rejected by the engine. Always a bug here rather
    /// than bad input, which is why it carries the rule text.
    Rule { body: String, message: String },
}

impl std::fmt::Display for ShareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rule { body, message } => {
                write!(formatter, "generated rule rejected: {body} ({message})")
            }
        }
    }
}

impl std::error::Error for ShareError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stable id for a readable name, so a failure names the entity rather
    /// than a uuid nobody can place.
    fn id(name: &str) -> Uuid {
        Uuid::new_v5(&Uuid::NAMESPACE_OID, name.as_bytes())
    }

    /// A small graph, in the shape of the example.
    ///
    /// ```text
    ///   rust-skill (W)
    ///     ├─▶ streaming-pattern (K) ─▶ deep-scene (K)
    ///     ├─▶ corpus-doc (I) ────────▶ hidden-scene (K)
    ///     └─▶ session-log (D)
    /// ```
    fn graph() -> FactSet {
        let mut facts = FactSet::new();
        for (entity, tier) in [
            ("rust-skill", "wisdom"),
            ("streaming-pattern", "knowledge"),
            ("deep-scene", "knowledge"),
            ("hidden-scene", "knowledge"),
            ("corpus-doc", "information"),
            ("session-log", "data"),
        ] {
            facts.insert(
                "tier",
                vec![Term::Const(id(entity)), Term::ConstStr(tier.to_owned())],
            );
        }
        for (from, to) in [
            ("rust-skill", "streaming-pattern"),
            ("rust-skill", "corpus-doc"),
            ("rust-skill", "session-log"),
            ("streaming-pattern", "deep-scene"),
            ("corpus-doc", "hidden-scene"),
        ] {
            facts.insert(
                "edge",
                vec![
                    Term::Const(id(from)),
                    Term::ConstStr("relates".to_owned()),
                    Term::Const(id(to)),
                ],
            );
        }
        facts
    }

    fn share(hops: usize, floor: Tier) -> Vec<Uuid> {
        ShareGrant::new([id("rust-skill")], hops, floor)
            .resolve(&graph(), 10_000)
            .expect("the generated rules must parse")
    }

    /// Assert by NAME, so a failure says which entity leaked rather than
    /// printing a uuid nobody can place.
    fn names(reached: &[Uuid]) -> Vec<&'static str> {
        [
            "rust-skill",
            "streaming-pattern",
            "deep-scene",
            "hidden-scene",
            "corpus-doc",
            "session-log",
        ]
        .into_iter()
        .filter(|name| reached.contains(&id(name)))
        .collect()
    }

    /// Ben's sentence, evaluated: skills +2 hops, no corpus.
    #[test]
    fn skills_plus_two_hops_excludes_corpus_and_capture() {
        let reached = names(&share(2, Tier::Knowledge));
        assert!(
            reached.contains(&"rust-skill"),
            "the seed itself: {reached:?}"
        );
        assert!(
            reached.contains(&"streaming-pattern"),
            "one hop: {reached:?}"
        );
        assert!(reached.contains(&"deep-scene"), "two hops: {reached:?}");
        assert!(
            !reached.contains(&"corpus-doc"),
            "corpus leaked: {reached:?}"
        );
        assert!(
            !reached.contains(&"session-log"),
            "capture leaked: {reached:?}"
        );
    }

    /// The floor is not just an output filter: a below-floor item cannot bridge.
    ///
    /// `hidden-scene` is Knowledge and two hops away, but the only path to it
    /// runs through a corpus document. It must not be reachable — otherwise
    /// "not corpus data" would mean "corpus is hidden but still walked", and a
    /// share would leak the SHAPE of what it excluded.
    #[test]
    fn a_below_floor_item_cannot_be_walked_through() {
        let reached = names(&share(2, Tier::Knowledge));
        assert!(
            !reached.contains(&"hidden-scene"),
            "walked through corpus: {reached:?}"
        );
    }

    /// Lower the floor and the bridge opens, which proves the previous test is
    /// measuring the floor rather than the graph.
    #[test]
    fn lowering_the_floor_opens_the_path() {
        let reached = names(&share(2, Tier::Information));
        assert!(reached.contains(&"corpus-doc"), "{reached:?}");
        assert!(reached.contains(&"hidden-scene"), "{reached:?}");
        assert!(
            !reached.contains(&"session-log"),
            "data is still below: {reached:?}"
        );
    }

    /// Depth is respected in both directions.
    #[test]
    fn hops_bound_the_walk() {
        let one = names(&share(1, Tier::Knowledge));
        assert!(one.contains(&"streaming-pattern"), "{one:?}");
        assert!(
            !one.contains(&"deep-scene"),
            "reached two hops at depth one: {one:?}"
        );

        assert_eq!(names(&share(0, Tier::Knowledge)), vec!["rust-skill"]);
    }

    /// A seed below its own floor shares nothing — not even itself.
    #[test]
    fn a_seed_below_the_floor_shares_nothing() {
        let grant = ShareGrant::new([id("corpus-doc")], 2, Tier::Wisdom);
        assert!(grant.resolve(&graph(), 10_000).expect("parses").is_empty());
    }

    /// Depth is clamped rather than trusted.
    #[test]
    fn depth_is_bounded() {
        assert_eq!(ShareGrant::new([id("x")], 99, Tier::Data).hops, MAX_HOPS);
    }

    /// Every generated rule parses, at every depth and floor. A rule the engine
    /// rejects is a share that silently returns less than it promised.
    #[test]
    fn every_generated_rule_parses() {
        for hops in 0..=MAX_HOPS {
            for floor in Tier::ALL {
                let grant = ShareGrant::new([id("x")], hops, floor);
                for body in grant.as_datalog() {
                    assert!(
                        parse_rule(&body).is_ok(),
                        "the engine rejected {body} (hops {hops}, floor {})",
                        floor.as_str()
                    );
                }
            }
        }
    }

    /// The rules are readable, because a grant that cannot be read cannot be
    /// audited.
    #[test]
    fn the_rules_read_as_the_grant_describes() {
        let rules = ShareGrant::new([id("rust-skill")], 2, Tier::Knowledge).as_datalog();
        assert!(rules.contains(&r#"shareable(E) :- tier(E, "wisdom")."#.to_owned()));
        assert!(rules.contains(&r#"shareable(E) :- tier(E, "knowledge")."#.to_owned()));
        assert!(
            !rules.iter().any(|rule| rule.contains("information")),
            "a floor of knowledge must not admit information"
        );
        assert!(
            rules.contains(&"share_2(Y) :- share_1(X), edge(X, _, Y), shareable(Y).".to_owned())
        );
    }

    /// The same grant over the same graph answers identically twice.
    #[test]
    fn a_grant_is_deterministic() {
        assert_eq!(share(2, Tier::Knowledge), share(2, Tier::Knowledge));
    }

    /// An empty graph is an empty share, not an error.
    #[test]
    fn an_empty_graph_shares_nothing() {
        let grant = ShareGrant::new([id("rust-skill")], 2, Tier::Knowledge);
        assert!(
            grant
                .resolve(&FactSet::new(), 10_000)
                .expect("parses")
                .is_empty()
        );
    }
}
