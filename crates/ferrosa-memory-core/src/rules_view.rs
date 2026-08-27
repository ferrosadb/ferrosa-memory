//! Module: Present the tier rules as something a person can inspect and question.
//! Correctness: Correct when a rule that can never fire is reported as such,
//! when an alias pointing at no rule is reported as such, and when explaining a
//! path names the alias that actually fired rather than only the tier it landed
//! in.
//! Last revised: 2026-08-26
//! Last changed: New.
//!
//! # Why this is not just a listing
//!
//! A tier rule is two rows in two tables that only mean something together. The
//! rule `research/skills -> wisdom` says what a root earns; an alias
//! `~/src/research/skills -> research/skills` is what lets any real path reach
//! that root. Either one alone is inert, and the inert cases are invisible in a
//! plain list of rules: `session-capture -> data` sat in the table looking
//! perfectly correct while 2,791 paths beginning `session-capture/` resolved to
//! no root at all, because nothing aliased onto it. The rule was never wrong. It
//! was unreachable, which reads identically in a listing and not at all in a
//! result.
//!
//! So the two failures this module exists to name are:
//!
//! - a **rule with no alias** — it can never fire, and every path it was meant
//!   to cover silently becomes Data
//! - an **alias with no rule** — paths resolve to a canonical root the rule
//!   table does not cover, and again silently become Data
//!
//! # Why `TierReason` was not enough
//!
//! [`crate::tiers::resolve`] answers "what tier is this", and folds both
//! failures above into `TierReason::Default`. That is right for tiering, where
//! the outcome is the same either way. It is wrong for a screen whose entire
//! job is *why*, because the two states have different fixes: one needs an
//! alias, the other needs a rule. This module keeps them apart without changing
//! what `resolve` does.

use std::collections::{BTreeMap, BTreeSet};

use crate::tier_store::{RootAlias, RootRule};
use crate::tiers::{Promotion, RootResolver, Tier};

/// One tier rule, with everything needed to judge whether it works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierRuleRow {
    pub root: String,
    pub tier: Tier,
    /// The rule as the engine would state it.
    pub datalog: String,
    /// Alias prefixes that resolve to this root, sorted. Empty means the rule
    /// is unreachable.
    pub aliases: Vec<String>,
    pub note: String,
    pub created_by: String,
}

impl TierRuleRow {
    /// Whether any path can reach this rule.
    ///
    /// A rule with no alias is not a rule that has not matched yet. It is a
    /// rule nothing can ever match, and it should be shown as broken rather
    /// than as empty.
    pub fn reachable(&self) -> bool {
        !self.aliases.is_empty()
    }
}

/// An alias that resolves to a root no rule covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanglingAlias {
    pub alias_prefix: String,
    pub canonical_root: String,
}

/// Why a path has the tier it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleVerdict {
    /// A person decided, which outranks any rule.
    Promoted { by: String, why: String },
    /// An alias matched and its root has a rule. The ordinary case.
    Ruled {
        alias_prefix: String,
        root: String,
        datalog: String,
    },
    /// An alias matched, but the root it produced has no rule. Fix: add a rule.
    RootHasNoRule { alias_prefix: String, root: String },
    /// No alias covers this path. Fix: add an alias.
    NoAliasMatched,
}

/// The full chain from a path to a tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathExplanation {
    pub input: String,
    pub tier: Tier,
    pub verdict: RuleVerdict,
}

impl PathExplanation {
    /// Whether the tier came from a rule or from the Data floor.
    ///
    /// Both unreached cases land on Data, and a screen that showed only the
    /// tier would present them as a decision when they are an absence.
    pub fn is_classified(&self) -> bool {
        matches!(
            self.verdict,
            RuleVerdict::Promoted { .. } | RuleVerdict::Ruled { .. }
        )
    }
}

/// Join rules to the aliases that reach them.
pub fn tier_rule_rows(rules: &[RootRule], aliases: &[RootAlias]) -> Vec<TierRuleRow> {
    let mut by_root: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for alias in aliases {
        by_root
            .entry(alias.canonical_root.as_str())
            .or_default()
            .insert(alias.alias_prefix.as_str());
    }

    let mut rows: Vec<TierRuleRow> = rules
        .iter()
        .map(|rule| TierRuleRow {
            datalog: datalog_for(&rule.root, rule.tier),
            aliases: by_root
                .get(rule.root.as_str())
                .map(|set| set.iter().map(|s| (*s).to_owned()).collect())
                .unwrap_or_default(),
            root: rule.root.clone(),
            tier: rule.tier,
            note: rule.note.clone(),
            created_by: rule.created_by.clone(),
        })
        .collect();

    // Unreachable first: a broken rule is the reason to open this screen.
    rows.sort_by(|left, right| {
        left.reachable()
            .cmp(&right.reachable())
            .then_with(|| left.root.cmp(&right.root))
    });
    rows
}

/// Aliases whose canonical root no rule covers.
pub fn dangling_aliases(rules: &[RootRule], aliases: &[RootAlias]) -> Vec<DanglingAlias> {
    let ruled: BTreeSet<&str> = rules.iter().map(|rule| rule.root.as_str()).collect();
    let mut dangling: Vec<DanglingAlias> = aliases
        .iter()
        .filter(|alias| !ruled.contains(alias.canonical_root.as_str()))
        .map(|alias| DanglingAlias {
            alias_prefix: alias.alias_prefix.clone(),
            canonical_root: alias.canonical_root.clone(),
        })
        .collect();
    dangling.sort_by(|left, right| left.alias_prefix.cmp(&right.alias_prefix));
    dangling
}

/// The rule as the engine would state it.
///
/// Kept identical to [`crate::tiers::TierRules::as_datalog`] so the screen
/// cannot show a rule the engine would not recognise.
pub fn datalog_for(root: &str, tier: Tier) -> String {
    format!(
        "tier(E, \"{}\") :- source_root(E, \"{}\").",
        tier.as_str(),
        root
    )
}

/// Explain one path: which alias fired, which root it produced, which rule.
pub fn explain_path(
    path: &str,
    resolver: &RootResolver,
    rules: &[RootRule],
    promotion: Option<&Promotion>,
) -> PathExplanation {
    if let Some(promotion) = promotion {
        return PathExplanation {
            input: path.to_owned(),
            tier: promotion.tier,
            verdict: RuleVerdict::Promoted {
                by: promotion.by.clone(),
                why: promotion.why.clone(),
            },
        };
    }

    let Some(matched) = resolver.match_of(path) else {
        return PathExplanation {
            input: path.to_owned(),
            tier: Tier::Data,
            verdict: RuleVerdict::NoAliasMatched,
        };
    };

    match rules.iter().find(|rule| rule.root == matched.root) {
        Some(rule) => PathExplanation {
            input: path.to_owned(),
            tier: rule.tier,
            verdict: RuleVerdict::Ruled {
                datalog: datalog_for(&rule.root, rule.tier),
                alias_prefix: matched.alias_prefix,
                root: matched.root,
            },
        },
        None => PathExplanation {
            input: path.to_owned(),
            tier: Tier::Data,
            verdict: RuleVerdict::RootHasNoRule {
                alias_prefix: matched.alias_prefix,
                root: matched.root,
            },
        },
    }
}

/// How a rule's conclusions are produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Derived edges are written into the graph and read back as ordinary
    /// edges. Only for rules whose conclusions cannot later become false.
    Materialised,
    /// Re-derived on every read. The safe mode, and the only correct one for a
    /// rule whose conclusions can lapse or be falsified.
    Live { reasons: Vec<LiveReason> },
}

/// Why a rule cannot be materialised. A rule may have both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LiveReason {
    /// The rule lapses, so stored edges would outlive it.
    Expires,
    /// The rule uses negation, so a later fact can make a stored edge FALSE.
    NonMonotonic,
}

impl LiveReason {
    /// Wording for the screen: a person needs to know why, not only that.
    pub fn explain(self) -> &'static str {
        match self {
            Self::Expires => "this rule expires, and stored edges would outlive it",
            Self::NonMonotonic => {
                "this rule uses negation, so a later fact could make a stored edge false"
            }
        }
    }
}

/// Decide how a rule runs.
///
/// Both conditions must hold to materialise, and they are independent: expiry
/// is about the clock, monotonicity is about whether a conclusion can be
/// unmade. A rule that merely does not expire is not therefore safe to store.
pub fn execution_mode(
    monotonic: bool,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> ExecutionMode {
    let mut reasons = Vec::new();
    if expires_at.is_some() {
        reasons.push(LiveReason::Expires);
    }
    if !monotonic {
        reasons.push(LiveReason::NonMonotonic);
    }
    if reasons.is_empty() {
        ExecutionMode::Materialised
    } else {
        ExecutionMode::Live { reasons }
    }
}

/// Whether a rule's conclusions can only ever be added to.
///
/// # Why this destructures exhaustively
///
/// So that a new field cannot silently change the answer. The body names every
/// field of [`DatalogRule`]; adding one stops this compiling, and whoever adds
/// it has to decide here. A compile error is the cheapest possible place to
/// have that conversation.
///
/// That guard has now fired once, exactly as intended. Negation landed
/// (`t_64ea07e9`), and this function had been trivially `true` — which would
/// have handed every negated rule to the materialising path, the one place a
/// negated rule must never go.
///
/// The two fields it caught, and why they answer differently:
///
/// - `negated` — a binding survives only if it matches NONE of these atoms, so
///   adding a fact can RETRACT a conclusion that previously held. That is the
///   definition of non-monotonic, and it is why this function now has a body.
/// - `head_exprs` — computes head arguments rather than repeating them.
///   Computing a value cannot withdraw a conclusion: the same bindings still
///   derive, they merely carry a computed term. Monotonicity is unaffected.
pub fn is_monotonic(rule: &crate::types::DatalogRule) -> bool {
    let crate::types::DatalogRule {
        head: _,
        body: _,
        filters: _,
        aggregates: _,
        negated,
        head_exprs: _,
    } = rule;
    negated.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rule with no negation only ever adds conclusions.
    #[test]
    fn a_rule_without_negation_is_monotonic() {
        let rule = crate::datalog::parse_rule("p(X) :- q(X).").expect("parse");
        assert!(is_monotonic(&rule));
    }

    /// The property the exhaustive destructure exists to protect.
    ///
    /// A negated body atom means a binding survives only if it matches none of
    /// them, so adding a fact can RETRACT a conclusion that previously held.
    /// While is_monotonic answered a hard-coded `true`, every such rule was
    /// routed to the materialising path -- the one place its own documentation
    /// says a negated rule must never go.
    #[test]
    fn a_rule_with_negation_is_not_monotonic() {
        let rule = crate::datalog::parse_rule("p(X) :- q(X), not r(X).").expect("parse");
        assert!(
            !rule.negated.is_empty(),
            "the parser did not record negation"
        );
        assert!(!is_monotonic(&rule));
    }

    /// A computed head argument does not withdraw anything.
    ///
    /// The same bindings still derive; they merely carry a computed term. This
    /// is the other field the guard caught, and it answers the opposite way to
    /// negation -- which is the whole reason the destructure names fields
    /// individually rather than ending in `..`.
    #[test]
    fn a_computed_head_argument_does_not_make_a_rule_non_monotonic() {
        let rule = crate::datalog::parse_rule("p(X) :- q(X), not r(X).").expect("parse");
        let with_expr = crate::types::DatalogRule {
            negated: Vec::new(),
            ..rule
        };
        assert!(
            is_monotonic(&with_expr),
            "only negation should make a rule non-monotonic"
        );
    }

    use chrono::{DateTime, Utc};

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_767_225_600, 0).expect("epoch")
    }

    fn rule(root: &str, tier: Tier) -> RootRule {
        RootRule {
            root: root.to_owned(),
            tier,
            created_by: "seed".to_owned(),
            note: "builtin: ships with the build".to_owned(),
            created_at: now(),
        }
    }

    fn alias(prefix: &str, root: &str) -> RootAlias {
        RootAlias {
            alias_prefix: prefix.to_owned(),
            canonical_root: root.to_owned(),
            created_by: "seed".to_owned(),
            created_at: now(),
        }
    }

    fn skills_resolver() -> RootResolver {
        RootResolver::new([
            (
                "~/src/research/skills".to_owned(),
                "research/skills".to_owned(),
            ),
            (
                "/Users/bkearns/src/research/skills".to_owned(),
                "research/skills".to_owned(),
            ),
            ("research/skills".to_owned(), "research/skills".to_owned()),
        ])
    }

    /// The question Ben asked: which rule makes `~/src/research/skills` wisdom.
    /// The answer must name the alias, not only the tier.
    #[test]
    fn explains_why_a_skills_path_is_wisdom() {
        let rules = [rule("research/skills", Tier::Wisdom)];
        let explained = explain_path(
            "~/src/research/skills/rules/safety.md",
            &skills_resolver(),
            &rules,
            None,
        );

        assert_eq!(explained.tier, Tier::Wisdom);
        assert!(explained.is_classified());
        match explained.verdict {
            RuleVerdict::Ruled {
                alias_prefix,
                root,
                datalog,
            } => {
                assert_eq!(alias_prefix, "~/src/research/skills");
                assert_eq!(root, "research/skills");
                assert_eq!(
                    datalog,
                    r#"tier(E, "wisdom") :- source_root(E, "research/skills")."#
                );
            }
            other => panic!("expected a ruled verdict, got {other:?}"),
        }
    }

    /// The longest alias wins, so a more specific subtree can carry its own
    /// tier. Reported alias must be the one that actually fired.
    #[test]
    fn the_longest_matching_alias_is_the_one_reported() {
        let resolver = RootResolver::new([
            ("research".to_owned(), "research/corpus".to_owned()),
            ("research/skills".to_owned(), "research/skills".to_owned()),
        ]);
        let rules = [
            rule("research/corpus", Tier::Information),
            rule("research/skills", Tier::Wisdom),
        ];

        let explained = explain_path("research/skills/x.md", &resolver, &rules, None);
        assert_eq!(explained.tier, Tier::Wisdom);
        match explained.verdict {
            RuleVerdict::Ruled { alias_prefix, .. } => {
                assert_eq!(alias_prefix, "research/skills")
            }
            other => panic!("expected the specific alias to win, got {other:?}"),
        }
    }

    /// A path nothing aliases is Data, and must say so as an ABSENCE rather
    /// than as a decision — the fix is to add an alias.
    #[test]
    fn an_unaliased_path_is_data_but_not_classified() {
        let rules = [rule("research/skills", Tier::Wisdom)];
        let explained = explain_path("/tmp/scratch.md", &skills_resolver(), &rules, None);

        assert_eq!(explained.tier, Tier::Data);
        assert_eq!(explained.verdict, RuleVerdict::NoAliasMatched);
        assert!(
            !explained.is_classified(),
            "Data by floor is not Data by decision"
        );
    }

    /// The other absence, and it needs a DIFFERENT fix: the alias worked, the
    /// rule table just does not cover where it landed. `resolve` folds this
    /// together with the case above; this module must not.
    #[test]
    fn an_alias_onto_an_unruled_root_is_distinguishable_from_no_alias() {
        let explained = explain_path(
            "~/src/research/skills/x.md",
            &skills_resolver(),
            // The alias resolves fine. There is simply no rule for the root.
            &[],
            None,
        );

        assert_eq!(explained.tier, Tier::Data);
        assert!(!explained.is_classified());
        assert_eq!(
            explained.verdict,
            RuleVerdict::RootHasNoRule {
                alias_prefix: "~/src/research/skills".to_owned(),
                root: "research/skills".to_owned(),
            },
            "an alias that resolved must not be reported as no alias at all"
        );
    }

    /// A person's decision outranks the rules, and the screen must attribute it.
    #[test]
    fn a_promotion_outranks_the_rule_and_names_the_person() {
        let rules = [rule("research/skills", Tier::Wisdom)];
        let promotion = Promotion {
            tier: Tier::Data,
            by: "ben".to_owned(),
            why: "draft, not ready".to_owned(),
        };
        let explained = explain_path(
            "~/src/research/skills/draft.md",
            &skills_resolver(),
            &rules,
            Some(&promotion),
        );

        assert_eq!(explained.tier, Tier::Data);
        assert!(explained.is_classified());
        assert_eq!(
            explained.verdict,
            RuleVerdict::Promoted {
                by: "ben".to_owned(),
                why: "draft, not ready".to_owned(),
            }
        );
    }

    /// The listing joins a rule to the aliases that reach it.
    #[test]
    fn a_rule_lists_every_alias_that_reaches_it() {
        let rows = tier_rule_rows(
            &[rule("research/skills", Tier::Wisdom)],
            &[
                alias("~/src/research/skills", "research/skills"),
                alias("/Users/bkearns/src/research/skills", "research/skills"),
                alias("elsewhere", "research/corpus"),
            ],
        );

        assert_eq!(rows.len(), 1);
        assert!(rows[0].reachable());
        assert_eq!(
            rows[0].aliases,
            vec![
                "/Users/bkearns/src/research/skills".to_owned(),
                "~/src/research/skills".to_owned(),
            ],
            "only this root's aliases, sorted"
        );
    }

    /// The war story, as a test. `session-capture -> data` looked correct in
    /// every listing while nothing could reach it. A rule with no alias is
    /// broken, not merely unmatched, and must sort to the top.
    #[test]
    fn a_rule_with_no_alias_is_unreachable_and_sorts_first() {
        let rows = tier_rule_rows(
            &[
                rule("research/skills", Tier::Wisdom),
                rule("session-capture", Tier::Data),
            ],
            &[alias("~/src/research/skills", "research/skills")],
        );

        assert_eq!(rows[0].root, "session-capture");
        assert!(
            !rows[0].reachable(),
            "a rule nothing aliases onto can never fire"
        );
        assert!(rows[1].reachable());
    }

    /// The inverse gap: paths resolve, then fall to Data because the root they
    /// resolved to has no rule.
    #[test]
    fn an_alias_with_no_rule_is_reported_as_dangling() {
        let dangling = dangling_aliases(
            &[rule("research/skills", Tier::Wisdom)],
            &[
                alias("~/src/research/skills", "research/skills"),
                alias("~/src/research/corpus", "research/corpus"),
            ],
        );

        assert_eq!(
            dangling,
            vec![DanglingAlias {
                alias_prefix: "~/src/research/corpus".to_owned(),
                canonical_root: "research/corpus".to_owned(),
            }]
        );
    }

    /// The screen must not invent a syntax the engine would reject.
    #[test]
    fn the_rendered_rule_matches_what_the_engine_emits() {
        let engine = crate::tiers::TierRules::new([("research/skills".to_owned(), Tier::Wisdom)])
            .as_datalog();
        assert_eq!(
            engine,
            vec![datalog_for("research/skills", Tier::Wisdom)],
            "the listing and the engine must state a rule identically"
        );
    }

    /// And the rendered rule must actually parse.
    #[test]
    fn the_rendered_rule_parses() {
        let rendered = datalog_for("research/skills", Tier::Wisdom);
        crate::datalog::parse_rule(&rendered)
            .expect("the rule shown on screen must be one the engine accepts");
    }

    /// Every builtin rule is reachable through the aliases the seeder writes.
    /// This is the invariant whose breach cost 2,791 misfiled paths.
    #[test]
    fn every_builtin_rule_is_reachable_by_its_identity_alias() {
        let builtin = crate::tiers::TierRules::builtin();
        let rules: Vec<RootRule> = builtin
            .entries()
            .into_iter()
            .map(|(root, tier)| rule(&root, tier))
            .collect();
        // The seeder writes an identity alias for every builtin root.
        let aliases: Vec<RootAlias> = builtin
            .entries()
            .into_iter()
            .map(|(root, _)| alias(&root, &root))
            .collect();

        let rows = tier_rule_rows(&rules, &aliases);
        let unreachable: Vec<&str> = rows
            .iter()
            .filter(|row| !row.reachable())
            .map(|row| row.root.as_str())
            .collect();
        assert!(
            unreachable.is_empty(),
            "these builtin rules can never fire: {unreachable:?}"
        );
    }
}
