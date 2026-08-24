//! Module: DIKW tiers — what a memory item is, and how it got that way.
//! Correctness: Correct when a tier is derived from where an item came from,
//! when a person's promotion outranks the derivation, and when the same file
//! resolves to one root however it was named.
//! Last revised: 2026-08-24
//! Last changed: New.
//!
//! # The four tiers
//!
//! | Tier | What it holds |
//! |---|---|
//! | Data | Raw capture — the exhaust of LLM sessions |
//! | Information | Human-curated raw material, mostly trusted |
//! | Knowledge | derived structure and agent-authored artifacts |
//! | Wisdom | Hand-curated, adjudicated |
//!
//! Knowledge is a queue as much as a shelf. What unites consolidation output
//! with an agent's spec is not their form but their status: asserted by a
//! machine, not yet confirmed by anyone. Its exit is adjudication.
//!
//! Decisions in `ferrosa-suite/specs/knowledge-tiers/decisions.md`.
//!
//! # Why roots are normalised here and not matched in a rule
//!
//! The tier of a file is decided by which root it lives under, and the obvious
//! way to write that is a prefix test in a Datalog rule. The engine cannot: its
//! filters are comparison and arithmetic only, with no string operations.
//!
//! So the prefix work happens once, at ingest, and the rule matches a
//! normalised root by equality — which is also faster, since a path is resolved
//! once rather than on every evaluation. The alias table is data and editable;
//! the root-to-tier mapping is the rule.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where an item sits in the DIKW model.
///
/// Ordered, because the sharing rules need a floor: "at or above Knowledge"
/// has to be a comparison rather than a set membership test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Raw capture. What an LLM session left behind.
    Data,
    /// Human-curated raw material. The corpus.
    Information,
    /// A claim: asserted by a machine, not yet adjudicated.
    Knowledge,
    /// Hand-curated by a person. NOT adjudicated: one person's curation is an
    /// endorsement, and a Wisdom floor on a share means "someone vouched for
    /// this", never "this was verified". See specs/knowledge-tiers D13.
    Wisdom,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Information => "information",
            Self::Knowledge => "knowledge",
            Self::Wisdom => "wisdom",
        }
    }

    /// Parse a stored tier.
    ///
    /// `None` for anything unrecognised rather than a default. A tier that
    /// cannot be read is not Data — it is a bug, and defaulting would bury it
    /// in the largest tier where nobody would notice.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "data" => Some(Self::Data),
            "information" => Some(Self::Information),
            "knowledge" => Some(Self::Knowledge),
            "wisdom" => Some(Self::Wisdom),
            _ => None,
        }
    }

    pub const ALL: [Tier; 4] = [Tier::Data, Tier::Information, Tier::Knowledge, Tier::Wisdom];
}

/// The tier an item has, and why it has it.
///
/// The reason travels with the answer because the two are read together: a
/// person deciding whether to promote something needs to know whether its
/// current tier was inferred from a directory or chosen by someone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierAssignment {
    pub tier: Tier,
    pub reason: TierReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierReason {
    /// From the root the item was ingested under.
    Root(String),
    /// A person said so.
    Promoted { by: String, why: String },
    /// Nothing said otherwise.
    Default,
}

/// Resolves a path to a canonical root.
///
/// The same file reached three ways — `~/src/research/corpus/x.md`,
/// `/Users/bkearns/src/research/corpus/x.md`, `bkearns/research/corpus/x.md` —
/// is one file, and must land in one tier. Aliases are longest-first so a more
/// specific prefix wins over a shorter one that also matches.
#[derive(Debug, Clone, Default)]
pub struct RootResolver {
    /// Prefix to canonical root, longest prefix first.
    aliases: Vec<(String, String)>,
}

impl RootResolver {
    pub fn new(aliases: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut aliases: Vec<(String, String)> = aliases
            .into_iter()
            .map(|(prefix, root)| (normalise_separators(&prefix), root))
            .collect();
        // Longest first. `research/corpus/private` must beat `research/corpus`,
        // and a shorter alias registered later must not shadow it.
        aliases.sort_by(|left, right| right.0.len().cmp(&left.0.len()).then(left.0.cmp(&right.0)));
        Self { aliases }
    }

    /// The canonical root for a path, or `None` if no alias covers it.
    ///
    /// A path outside every known root has no root — deliberately not a
    /// fallback root, because "I do not know where this came from" and "this
    /// came from somewhere unclassified" are different states and only the
    /// first should be silent.
    pub fn root_of(&self, path: &str) -> Option<String> {
        self.match_of(path).map(|matched| matched.root)
    }

    /// The same lookup, but keeping the alias that fired.
    ///
    /// Storage records the alias alongside the root it produced: when an item
    /// lands in the wrong tier, the question is always *which rule put it
    /// there*, and a bare root cannot answer that once two aliases point at
    /// the same place.
    pub fn match_of(&self, path: &str) -> Option<RootMatch> {
        let path = normalise_separators(path);
        self.aliases.iter().find_map(|(prefix, root)| {
            // A prefix must end at a separator. Without this, `research/corp`
            // would match `research/corpus-archive`, which is a different tree.
            let matches = path == *prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'));
            matches.then(|| RootMatch {
                alias_prefix: prefix.clone(),
                root: root.clone(),
            })
        })
    }
}

/// Which alias matched, and what it resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootMatch {
    pub alias_prefix: String,
    pub root: String,
}

/// Trim and collapse a path so two spellings of one location compare equal.
///
/// Only the shapes that actually occur: a trailing slash, a leading `./`, and
/// repeated separators. Deliberately NOT symlink or `..` resolution — that
/// needs the filesystem, and this has to work on a path that came off the wire
/// from a machine that is not this one.
fn normalise_separators(path: &str) -> String {
    let trimmed = path.trim().trim_start_matches("./").trim_end_matches('/');
    let mut out = String::with_capacity(trimmed.len());
    let mut last_was_separator = false;
    for character in trimmed.chars() {
        if character == '/' {
            if !last_was_separator {
                out.push('/');
            }
            last_was_separator = true;
        } else {
            out.push(character);
            last_was_separator = false;
        }
    }
    out
}

/// Maps canonical roots to tiers. The rule half of the model.
///
/// Held as data here and mirrored into the Datalog registry, so the same
/// mapping answers both a direct lookup and an inference over the graph. One
/// source, two readers — a second table would be a second truth.
#[derive(Debug, Clone, Default)]
pub struct TierRules {
    by_root: BTreeMap<String, Tier>,
}

impl TierRules {
    pub fn new(rules: impl IntoIterator<Item = (String, Tier)>) -> Self {
        Self {
            by_root: rules.into_iter().collect(),
        }
    }

    /// The tier this root implies.
    pub fn tier_of_root(&self, root: &str) -> Option<Tier> {
        self.by_root.get(root).copied()
    }

    /// As Datalog rules, for the registry.
    ///
    /// Emitted rather than hand-written so the rules cannot drift from the
    /// table above. Equality only, which is all the engine has.
    pub fn as_datalog(&self) -> Vec<String> {
        self.by_root
            .iter()
            .map(|(root, tier)| {
                format!(
                    "tier(E, \"{}\") :- source_root(E, \"{}\").",
                    tier.as_str(),
                    root
                )
            })
            .collect()
    }

    /// The default mapping for this machine.
    ///
    /// Ben's own layout, which is the only one that exists today. Roots are
    /// canonical names rather than paths — the alias table maps the many ways
    /// a path can be spelled onto these.
    pub fn builtin() -> Self {
        Self::new([
            ("research/corpus".to_owned(), Tier::Information),
            ("research/skills".to_owned(), Tier::Wisdom),
            ("research/rules".to_owned(), Tier::Wisdom),
            ("consolidation".to_owned(), Tier::Knowledge),
            ("agent-artifacts".to_owned(), Tier::Knowledge),
            ("session-capture".to_owned(), Tier::Data),
        ])
    }
}

/// A person's decision about one item, which outranks any derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promotion {
    pub tier: Tier,
    pub by: String,
    pub why: String,
}

/// Work out an item's tier.
///
/// Promotion first, then the root, then Data. Data is the floor because
/// everything in the store arrived somehow, and unclassified capture is
/// precisely what Data means — an item with no known root is exhaust until
/// someone says otherwise.
pub fn resolve(
    path: Option<&str>,
    resolver: &RootResolver,
    rules: &TierRules,
    promotion: Option<&Promotion>,
) -> TierAssignment {
    if let Some(promotion) = promotion {
        return TierAssignment {
            tier: promotion.tier,
            reason: TierReason::Promoted {
                by: promotion.by.clone(),
                why: promotion.why.clone(),
            },
        };
    }
    if let Some(root) = path.and_then(|path| resolver.root_of(path))
        && let Some(tier) = rules.tier_of_root(&root)
    {
        return TierAssignment {
            tier,
            reason: TierReason::Root(root),
        };
    }
    TierAssignment {
        tier: Tier::Data,
        reason: TierReason::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver() -> RootResolver {
        RootResolver::new([
            (
                "~/src/research/corpus".to_owned(),
                "research/corpus".to_owned(),
            ),
            (
                "/Users/bkearns/src/research/corpus".to_owned(),
                "research/corpus".to_owned(),
            ),
            (
                "bkearns/research/corpus".to_owned(),
                "research/corpus".to_owned(),
            ),
            (
                "~/src/research/skills".to_owned(),
                "research/skills".to_owned(),
            ),
            (
                "/Users/bkearns/src/research/skills".to_owned(),
                "research/skills".to_owned(),
            ),
        ])
    }

    /// The point of the alias table: one file, however it was named.
    #[test]
    fn every_spelling_of_a_path_reaches_one_root() {
        let resolver = resolver();
        for path in [
            "~/src/research/corpus/rust/low-latency.md",
            "/Users/bkearns/src/research/corpus/rust/low-latency.md",
            "bkearns/research/corpus/rust/low-latency.md",
        ] {
            assert_eq!(
                resolver.root_of(path),
                Some("research/corpus".to_owned()),
                "{path} did not resolve"
            );
        }
    }

    /// A prefix must end at a separator.
    ///
    /// Without that, `research/corpus` would claim `research/corpus-archive`,
    /// which is a different tree and could be a different tier.
    #[test]
    fn a_prefix_does_not_match_a_longer_sibling_directory() {
        let resolver = RootResolver::new([(
            "~/src/research/corpus".to_owned(),
            "research/corpus".to_owned(),
        )]);
        assert_eq!(resolver.root_of("~/src/research/corpus-archive/x.md"), None);
        assert_eq!(resolver.root_of("~/src/research/corpusx"), None);
    }

    /// The directory itself belongs to its own root.
    #[test]
    fn the_root_directory_itself_resolves() {
        assert_eq!(
            resolver().root_of("~/src/research/corpus"),
            Some("research/corpus".to_owned())
        );
    }

    /// A more specific alias wins, whatever order it was registered in.
    #[test]
    fn the_longest_matching_alias_wins() {
        let resolver = RootResolver::new([
            ("~/src/research".to_owned(), "research".to_owned()),
            (
                "~/src/research/corpus".to_owned(),
                "research/corpus".to_owned(),
            ),
        ]);
        assert_eq!(
            resolver.root_of("~/src/research/corpus/x.md"),
            Some("research/corpus".to_owned())
        );
        assert_eq!(
            resolver.root_of("~/src/research/notes.md"),
            Some("research".to_owned())
        );
    }

    /// Spelling differences that are not different locations.
    #[test]
    fn cosmetic_path_differences_do_not_matter() {
        let resolver = resolver();
        for path in [
            "~/src/research/corpus/",
            "~/src//research///corpus",
            "  ~/src/research/corpus  ",
            "./~/src/research/corpus",
        ] {
            assert_eq!(
                resolver.root_of(path),
                Some("research/corpus".to_owned()),
                "{path:?} did not normalise"
            );
        }
    }

    /// An unknown path has NO root, rather than a fallback one.
    ///
    /// "I do not know where this came from" and "this came from somewhere
    /// unclassified" are different, and only the first should be silent.
    #[test]
    fn a_path_outside_every_root_has_none() {
        assert_eq!(resolver().root_of("/tmp/scratch.md"), None);
    }

    /// Ben's example, end to end: the corpus is Information, skills are Wisdom.
    #[test]
    fn the_corpus_is_information_and_skills_are_wisdom() {
        let rules = TierRules::builtin();
        let resolver = resolver();
        assert_eq!(
            resolve(
                Some("~/src/research/corpus/rust/x.md"),
                &resolver,
                &rules,
                None
            )
            .tier,
            Tier::Information
        );
        assert_eq!(
            resolve(
                Some("~/src/research/skills/rust.md"),
                &resolver,
                &rules,
                None
            )
            .tier,
            Tier::Wisdom
        );
    }

    /// Anything unplaced is Data — exhaust until someone says otherwise.
    #[test]
    fn an_unplaced_item_is_data() {
        let assignment = resolve(Some("/tmp/x.md"), &resolver(), &TierRules::builtin(), None);
        assert_eq!(assignment.tier, Tier::Data);
        assert_eq!(assignment.reason, TierReason::Default);
        assert_eq!(
            resolve(None, &resolver(), &TierRules::builtin(), None).tier,
            Tier::Data
        );
    }

    /// A person outranks the derivation, and the reason says who.
    #[test]
    fn a_promotion_beats_the_root() {
        let promotion = Promotion {
            tier: Tier::Wisdom,
            by: "ben".to_owned(),
            why: "adjudicated".to_owned(),
        };
        let assignment = resolve(
            Some("~/src/research/corpus/x.md"),
            &resolver(),
            &TierRules::builtin(),
            Some(&promotion),
        );
        assert_eq!(assignment.tier, Tier::Wisdom);
        assert_eq!(
            assignment.reason,
            TierReason::Promoted {
                by: "ben".to_owned(),
                why: "adjudicated".to_owned()
            }
        );
    }

    /// Promotion works downward too. Demoting something is the same mechanism,
    /// and a model that only promotes cannot correct a mistake.
    #[test]
    fn a_person_can_demote_as_well() {
        let promotion = Promotion {
            tier: Tier::Data,
            by: "ben".to_owned(),
            why: "this was never curated".to_owned(),
        };
        assert_eq!(
            resolve(
                Some("~/src/research/skills/x.md"),
                &resolver(),
                &TierRules::builtin(),
                Some(&promotion)
            )
            .tier,
            Tier::Data
        );
    }

    /// Tiers are ordered, because the sharing rules need a floor.
    #[test]
    fn tiers_are_ordered_for_the_sharing_floor() {
        assert!(Tier::Wisdom > Tier::Knowledge);
        assert!(Tier::Knowledge > Tier::Information);
        assert!(Tier::Information > Tier::Data);
        let floor = Tier::Knowledge;
        assert!(Tier::ALL.iter().filter(|tier| **tier >= floor).count() == 2);
    }

    /// An unreadable tier is not silently Data.
    #[test]
    fn an_unknown_tier_string_is_refused() {
        assert_eq!(Tier::parse("wisdom"), Some(Tier::Wisdom));
        assert_eq!(Tier::parse("Wisdom"), None);
        assert_eq!(Tier::parse(""), None);
        assert_eq!(Tier::parse("insight"), None);
    }

    /// Round-trips, so a stored tier reads back as itself.
    #[test]
    fn every_tier_round_trips() {
        for tier in Tier::ALL {
            assert_eq!(Tier::parse(tier.as_str()), Some(tier));
        }
    }

    /// The rules are emitted as Datalog, so the table and the registry cannot
    /// drift apart.
    #[test]
    fn the_rules_emit_datalog_the_engine_can_parse() {
        let rules = TierRules::new([("research/corpus".to_owned(), Tier::Information)]);
        assert_eq!(
            rules.as_datalog(),
            vec![r#"tier(E, "information") :- source_root(E, "research/corpus")."#]
        );
    }

    /// Every builtin rule parses. A rule the engine rejects is a tier that
    /// silently never applies.
    #[test]
    fn every_builtin_rule_parses() {
        for body in TierRules::builtin().as_datalog() {
            assert!(
                crate::datalog::parse_rule(&body).is_ok(),
                "the engine rejected {body}"
            );
        }
    }
}
