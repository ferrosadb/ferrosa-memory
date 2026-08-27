//! Module: What a rule can be built out of, discovered from the engine itself.
//! Correctness: Correct when a block is offered only if this build's parser
//! actually accepts what it compiles to, when a composition round-trips to the
//! text it produced, and when an unknown block is refused rather than guessed.
//! Last revised: 2026-08-27
//! Last changed: New.
//!
//! # Why the palette is probed and not written down
//!
//! The rule language is growing — negation has landed, aggregates are in
//! flight, and more operators follow. A palette listed by hand goes stale the
//! day the next one merges, and the failure is silent in the worst direction:
//! a block offered for an operator this build cannot parse produces a rule the
//! server then rejects, with the person told their sentence was wrong when it
//! was the palette that was.
//!
//! So each block carries a **probe** — a rule using that operator and nothing
//! else new — and the block is advertised only if [`crate::datalog::parse_rule`]
//! accepts it here. One source of truth, and it is the parser.
//!
//! # Why the client posts a tree and never syntax
//!
//! The client has no grammar (D1). It arranges named blocks and fills slots;
//! the server turns that into text. A client that generated
//! `tier(E, "wisdom") :- …` would know the grammar as of the day it shipped,
//! which is the thing this design exists to avoid.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Where a block can sit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    /// A condition. "Anything that is…"
    When,
    /// A conclusion. "…goes in"
    Then,
}

/// What kind of value fills a block's hole.
///
/// Typed so the app can offer real choices — a folder picker for a folder, the
/// bucket list for a bucket — rather than a text field, which would be a text
/// box wearing a form's clothes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fill {
    /// One of this tenant's roots.
    Root,
    /// One of this tenant's buckets.
    Bucket,
    /// Free text, when nothing narrower is true.
    Text,
}

/// One thing a person can put in a rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// Stable across versions. The app posts this, never a label.
    pub id: String,
    /// What the block says on screen, with `{}` where the fill goes.
    pub label: String,
    pub slot: Slot,
    pub fill: Fill,
    /// Whether this block negates. Drawn differently, and it makes the whole
    /// rule live rather than materialised.
    pub negates: bool,
}

/// A block, plus the machinery to compile and prove it.
struct Definition {
    block: Block,
    /// A complete rule exercising this block, used to ask the parser whether
    /// this build supports it.
    probe: &'static str,
    /// A token that must NOT survive as a body-atom predicate.
    ///
    /// Parsing successfully is not evidence of support, and assuming it was
    /// is a mistake this file made first. Datalog has no reserved predicate
    /// names, so on a build without negation `not source_root(E, "y")` parses
    /// perfectly — as a relation *called* `"not source_root"`, which has no
    /// rows. The rule is accepted, derives nothing, and reads as "no matches".
    ///
    /// So the probe asks what the parser BUILT, not whether it succeeded. If
    /// the operator is still sitting there as an ordinary predicate, this
    /// build does not have it.
    absent_from_atoms: Option<&'static str>,
    /// A body atom this block's output depends on, emitted ONCE however many
    /// blocks need it.
    ///
    /// A filter can only speak about a variable something else bound. Three
    /// blocks that each need the root in hand must not each emit
    /// `source_root(E, R)`: the duplicate is harmless to the engine and
    /// confusing to read, and the rule text is what a person sees in the
    /// advanced editor.
    requires: Option<&'static str>,
    /// How the block becomes a datalog literal.
    render: fn(&str) -> String,
}

fn quote(value: &str) -> String {
    // Datalog string literals here are double-quoted with no escape sequence,
    // so a value containing a quote cannot be represented. Refused at
    // compile time rather than silently mangled — see `compile`.
    value.replace('"', "")
}

fn definitions() -> Vec<Definition> {
    vec![
        Definition {
            block: Block {
                id: "in_folder".into(),
                label: "in folder {}".into(),
                slot: Slot::When,
                fill: Fill::Root,
                negates: false,
            },
            probe: r#"tier(E, "data") :- source_root(E, "x")."#,
            absent_from_atoms: None,
            requires: None,
            render: |v| format!("source_root(E, \"{}\")", quote(v)),
        },
        Definition {
            block: Block {
                id: "not_in_folder".into(),
                label: "not in folder {}".into(),
                slot: Slot::When,
                fill: Fill::Root,
                negates: true,
            },
            probe: r#"tier(E, "data") :- source_root(E, "x"), not source_root(E, "y")."#,
            absent_from_atoms: Some("not "),
            // A negated atom cannot stand alone. Datalog safety requires every
            // variable to be bound by a POSITIVE body atom, so
            // `tier(E, ...) :- not source_root(E, "x").` leaves E unbound and
            // would derive over everything that does not exist. The parser
            // refuses it, and it is right to.
            //
            // This block was written before the parser enforced that, so it
            // compiled a rule nothing would accept. The block's own probe had
            // the answer all along -- it pairs the negation with
            // `source_root(E, "x")` -- it simply never declared the dependency.
            //
            // `source_root(E, R)` is also the honest reading of "not in folder":
            // entities that HAVE a source root, and whose root is not this one.
            requires: Some("source_root(E, R)"),
            render: |v| format!("not source_root(E, \"{}\")", quote(v)),
        },
        // The string predicates from the grammar-completion work. Each is
        // offered only where the parser has them, so on a build without that
        // work these simply do not appear.
        Definition {
            block: Block {
                id: "folder_starts_with".into(),
                label: "in any folder starting {}".into(),
                slot: Slot::When,
                fill: Fill::Text,
                negates: false,
            },
            probe: r#"tier(E, "data") :- source_root(E, R), str_starts_with(R, "x")."#,
            absent_from_atoms: Some("str_starts_with"),
            requires: Some("source_root(E, R)"),
            render: |v| format!("str_starts_with(R, \"{}\")", quote(v)),
        },
        Definition {
            block: Block {
                id: "folder_contains".into(),
                label: "in any folder mentioning {}".into(),
                slot: Slot::When,
                fill: Fill::Text,
                negates: false,
            },
            probe: r#"tier(E, "data") :- source_root(E, R), str_contains(R, "x")."#,
            absent_from_atoms: Some("str_contains"),
            requires: Some("source_root(E, R)"),
            render: |v| format!("str_contains(R, \"{}\")", quote(v)),
        },
        Definition {
            block: Block {
                id: "folder_not_containing".into(),
                label: "not in a folder mentioning {}".into(),
                slot: Slot::When,
                fill: Fill::Text,
                negates: true,
            },
            probe: r#"tier(E, "data") :- source_root(E, R), !str_contains(R, "x")."#,
            absent_from_atoms: Some("str_contains"),
            requires: Some("source_root(E, R)"),
            render: |v| format!("!str_contains(R, \"{}\")", quote(v)),
        },
        Definition {
            block: Block {
                id: "goes_in".into(),
                label: "goes in {}".into(),
                slot: Slot::Then,
                fill: Fill::Bucket,
                negates: false,
            },
            probe: r#"tier(E, "data") :- source_root(E, "x")."#,
            absent_from_atoms: None,
            requires: None,
            render: |v| format!("tier(E, \"{}\")", quote(v)),
        },
    ]
}

/// The blocks this build can actually honour.
///
/// Each is offered only if the parser here accepts its probe. A build without
/// negation simply does not advertise the negating blocks, and the composer
/// shows what it can do rather than offering something that will be refused.
pub fn palette() -> Vec<Block> {
    definitions()
        .into_iter()
        .filter(|definition| supported(definition))
        .map(|definition| definition.block)
        .collect()
}

/// Whether this build really has the operator a block needs.
///
/// Two questions, and the second is the one that matters. Parsing must
/// succeed — and whatever the parser produced must not still contain the
/// operator as a plain relation, which is what an unsupported operator
/// degrades into rather than an error.
fn supported(definition: &Definition) -> bool {
    let Ok(rule) = crate::datalog::parse_rule(definition.probe) else {
        return false;
    };
    let Some(token) = definition.absent_from_atoms else {
        return true;
    };
    !rule.body.iter().any(|atom| atom.predicate.contains(token))
}

/// One placed block: which block, and what fills it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placed {
    pub block_id: String,
    pub value: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CompileError {
    #[error("no block called {0} on this machine")]
    UnknownBlock(String),
    #[error("a rule needs something to look for")]
    NoCondition,
    #[error("a rule needs to say where things go")]
    NoConclusion,
    #[error("a rule can only put things in one place, and this names {0}")]
    ManyConclusions(usize),
    #[error("{0} was left empty")]
    EmptyValue(String),
    #[error("a value cannot contain a quotation mark")]
    QuoteInValue,
}

/// Turn a composition into rule text.
///
/// The text is the rule (D14); this is the only place a tree becomes one, and
/// the caller must still hand the result to `parse_rule` before storing it.
/// Compiling and validating are kept apart on purpose: this function knows how
/// to write the language, and only the parser knows whether the result is in
/// it.
pub fn compile(placed: &[Placed]) -> Result<String, CompileError> {
    let by_id: BTreeMap<String, Definition> = definitions()
        .into_iter()
        .map(|definition| (definition.block.id.clone(), definition))
        .collect();

    let mut required: Vec<&'static str> = Vec::new();
    let mut whens = Vec::new();
    let mut thens = Vec::new();

    for item in placed {
        let Some(definition) = by_id.get(&item.block_id) else {
            return Err(CompileError::UnknownBlock(item.block_id.clone()));
        };
        let value = item.value.trim();
        if value.is_empty() {
            return Err(CompileError::EmptyValue(definition.block.label.clone()));
        }
        if value.contains('"') {
            return Err(CompileError::QuoteInValue);
        }
        if let Some(needs) = definition.requires {
            if !required.contains(&needs) {
                required.push(needs);
            }
        }
        let rendered = (definition.render)(value);
        match definition.block.slot {
            Slot::When => whens.push(rendered),
            Slot::Then => thens.push(rendered),
        }
    }

    if whens.is_empty() {
        return Err(CompileError::NoCondition);
    }

    // Bindings first: a filter can only speak about a variable already in
    // hand, so the atom that binds it has to precede every filter reading it.
    let mut body: Vec<String> = required.into_iter().map(str::to_owned).collect();
    body.extend(whens);

    match thens.len() {
        0 => Err(CompileError::NoConclusion),
        1 => Ok(format!("{} :- {}.", thens[0], body.join(", "))),
        many => Err(CompileError::ManyConclusions(many)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(id: &str, value: &str) -> Placed {
        Placed {
            block_id: id.to_owned(),
            value: value.to_owned(),
        }
    }

    /// The ordinary case, and the shape the composer produces most often.
    #[test]
    fn a_folder_and_a_bucket_compile_to_a_rule() {
        let text = compile(&[
            placed("in_folder", "research/skills"),
            placed("goes_in", "wisdom"),
        ])
        .expect("compiles");
        assert_eq!(
            text,
            r#"tier(E, "wisdom") :- source_root(E, "research/skills")."#
        );
    }

    /// Whatever the composer emits must be something the engine accepts. This
    /// is the contract between the two halves, and the reason compiling and
    /// validating are separate steps.
    #[test]
    fn everything_the_palette_can_build_parses() {
        for block in palette() {
            if block.slot != Slot::When {
                continue;
            }
            let text = compile(&[
                placed(&block.id, "some/root"),
                placed("goes_in", "knowledge"),
            ])
            .expect("a palette block must compile");
            crate::datalog::parse_rule(&text).unwrap_or_else(|error| {
                panic!(
                    "palette block {} produced a rule the parser refuses: {text} — {error}",
                    block.id
                )
            });
        }
    }

    /// The probe has to actually gate, and on the right question.
    ///
    /// An earlier version of this test asserted the block was offered exactly
    /// when its probe PARSED. That is the wrong criterion and it passed on a
    /// build with no negation at all — see
    /// `parsing_alone_is_not_evidence_of_support` for why. The criterion is
    /// what the parser built.
    #[test]
    fn a_block_whose_operator_is_missing_is_not_offered() {
        let offered: Vec<String> = palette().into_iter().map(|b| b.id).collect();

        let negation_is_real = crate::datalog::parse_rule(
            r#"tier(E, "data") :- source_root(E, "x"), not source_root(E, "y")."#,
        )
        .map(|rule| !rule.body.iter().any(|a| a.predicate.contains("not ")))
        .unwrap_or(false);

        assert_eq!(
            offered.contains(&"not_in_folder".to_owned()),
            negation_is_real,
            "the negating block must be offered exactly when this build has real negation"
        );

        // These use no operator beyond plain atoms, so they work on every
        // build. Their absence would mean the gate itself is broken.
        assert!(offered.contains(&"in_folder".to_owned()));
        assert!(offered.contains(&"goes_in".to_owned()));
    }

    /// A rule with nothing to look for is not a rule, and the message says so
    /// in the words the person used rather than in the parser's.
    #[test]
    fn a_composition_missing_a_half_is_refused_in_plain_words() {
        assert_eq!(
            compile(&[placed("goes_in", "wisdom")]),
            Err(CompileError::NoCondition)
        );
        assert_eq!(
            compile(&[placed("in_folder", "x")]),
            Err(CompileError::NoConclusion)
        );
    }

    /// Two conclusions is a composition the shapes should have prevented. It is
    /// still refused here, because the client's shape checking and the server's
    /// rules can disagree and the server is the one that decides.
    #[test]
    fn two_conclusions_are_refused() {
        assert_eq!(
            compile(&[
                placed("in_folder", "x"),
                placed("goes_in", "wisdom"),
                placed("goes_in", "data"),
            ]),
            Err(CompileError::ManyConclusions(2))
        );
    }

    /// A block id this build does not have is refused by NAME, never ignored.
    ///
    /// Dropping it would compile a rule missing one of its conditions — a rule
    /// that is valid, accepted, and not the one the person built.
    #[test]
    fn an_unknown_block_is_refused_rather_than_dropped() {
        assert_eq!(
            compile(&[
                placed("in_folder", "x"),
                placed("from_the_future", "y"),
                placed("goes_in", "wisdom"),
            ]),
            Err(CompileError::UnknownBlock("from_the_future".to_owned()))
        );
    }

    /// There is no escape sequence in this quoting, so a quote in a value
    /// cannot be represented. Refused rather than stripped: stripping would
    /// silently store a rule about a different string.
    #[test]
    fn a_quote_in_a_value_is_refused_not_stripped() {
        assert_eq!(
            compile(&[placed("in_folder", "a\"b"), placed("goes_in", "wisdom"),]),
            Err(CompileError::QuoteInValue)
        );
    }

    #[test]
    fn an_empty_value_is_refused() {
        assert!(matches!(
            compile(&[placed("in_folder", "   "), placed("goes_in", "wisdom")]),
            Err(CompileError::EmptyValue(_))
        ));
    }

    /// Every block's label has exactly one hole, or the app cannot render it.
    #[test]
    fn every_label_has_one_slot_marker() {
        for block in palette() {
            assert_eq!(
                block.label.matches("{}").count(),
                1,
                "block {} must have exactly one fill marker",
                block.id
            );
        }
    }

    /// The mistake this file made first, pinned so it cannot come back.
    ///
    /// Datalog has no reserved predicate names, so an unsupported operator
    /// does not fail to parse — it becomes an ordinary relation with no rows.
    /// `parse_rule` returning `Ok` therefore proves nothing, and a palette
    /// gated on it offered every block on a build that had none of them. The
    /// rules it produced were accepted and derived nothing, which reads as "no
    /// matches" rather than as "your machine cannot do this".
    #[test]
    fn parsing_alone_is_not_evidence_of_support() {
        let probe = r#"tier(E, "data") :- source_root(E, "x"), not source_root(E, "y")."#;
        let parsed = crate::datalog::parse_rule(probe);
        assert!(
            parsed.is_ok(),
            "the probe must parse either way — that is the trap"
        );

        let rule = parsed.expect("parsed");
        let degraded = rule.body.iter().any(|atom| atom.predicate.contains("not "));

        // Whichever build this is, the palette must AGREE with the structure.
        let offers_negation = palette().iter().any(|b| b.id == "not_in_folder");
        assert_eq!(
            offers_negation, !degraded,
            "the block is offered exactly when the parser produced real negation \
             rather than a relation called \"not source_root\""
        );
    }

    /// No block may be offered whose operator the parser turned into a plain
    /// relation. Covers every block at once, including ones added later.
    #[test]
    fn no_offered_block_degraded_into_a_relation() {
        for definition in definitions() {
            let Some(token) = definition.absent_from_atoms else {
                continue;
            };
            let offered = palette().iter().any(|b| b.id == definition.block.id);
            if !offered {
                continue;
            }
            let rule = crate::datalog::parse_rule(definition.probe)
                .expect("an offered block's probe must parse");
            let leaked: Vec<&str> = rule
                .body
                .iter()
                .map(|atom| atom.predicate.as_str())
                .filter(|predicate| predicate.contains(token))
                .collect();
            assert!(
                leaked.is_empty(),
                "block {} is offered, but {token:?} survived as {leaked:?} — this build \
                 does not really support it",
                definition.block.id
            );
        }
    }
}
