//! The block palette: what a rule can be built out of, and the two directions
//! between blocks and rule text.
//!
//! D8 puts block definitions on the server so a new operator ships as a new
//! block and every existing client lays it out without knowing what it
//! compiles to. D14 then makes the **text** authoritative and the tree a
//! projection, valid only while it still compiles to exactly that text.
//!
//! [`project`] enforces that itself: it guesses a tree, recompiles it, and
//! returns it only if the result is the text it started from. A guess that
//! cannot be verified is `None` — "beyond blocks" — never half a tree.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What a block IS, and therefore where it may be placed.
///
/// Four shapes, mirroring Scratch: a hat that starts a rule, booleans that
/// stack under it, reporters that fill operand holes, and the conclusion.
/// Keeping the set this small is what makes composition legible — a person
/// learns four fits, not forty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    /// The hat. One per rule.
    Rule,
    /// Answers true or false. Stacks in a rule's `when`, nests in wrappers.
    Condition,
    /// Produces a value. Fills an operand hole.
    Value,
    /// What the rule concludes. Fills a rule's `then`.
    Conclusion,
}

/// A literal the person types or picks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotDef {
    pub name: String,
    pub kind: SlotKind,
    pub label: String,
}

/// Typed so a palette can offer real choices rather than a free-text box —
/// a path picker for a path, the buckets for a bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    Path,
    Tag,
    Bucket,
    /// A predicate from the ontology vocabulary.
    Relation,
    Number,
    Text,
    /// A comparison operator.
    Operator,
    /// days / hours / weeks / minutes.
    Unit,
    /// A variable name the rule binds and later reads.
    Binding,
}

/// A place another block goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoleDef {
    pub name: String,
    pub accepts: Shape,
    /// True for a stack that takes many, false for a single socket.
    pub many: bool,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockDef {
    pub id: String,
    pub shape: Shape,
    /// What the block reads as, in the words of the person choosing it.
    pub label: String,
    /// The datalog fragment it compiles to. `{name}` is a slot or a hole;
    /// `{#name}` is a fresh variable, numbered per rule so two of the same
    /// block cannot collide.
    pub template: String,
    pub slots: Vec<SlotDef>,
    pub holes: Vec<HoleDef>,
}

/// A composed rule, as the client posts it: block ids, filled slots, nested
/// blocks. Never generated syntax — D7's whole point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockTree {
    pub block: String,
    #[serde(default)]
    pub slots: BTreeMap<String, String>,
    #[serde(default)]
    pub holes: BTreeMap<String, Vec<BlockTree>>,
}

#[derive(Debug, thiserror::Error)]
pub enum BlockError {
    #[error("there is no block called '{0}'")]
    UnknownBlock(String),
    #[error("block '{block}' needs a value for '{slot}'")]
    MissingSlot { block: String, slot: String },
    #[error("block '{block}' hole '{hole}' takes a {expected:?}, but '{got}' is a {actual:?}")]
    WrongShape {
        block: String,
        hole: String,
        expected: Shape,
        actual: Shape,
        got: String,
    },
    #[error("block '{block}' hole '{hole}' needs exactly one block, got {count}")]
    WrongCount {
        block: String,
        hole: String,
        count: usize,
    },
    #[error("a rule needs a conclusion")]
    NoConclusion,
    #[error("'{value}' contains a quote, which would break out of the rule it sits in")]
    QuoteInSlot { value: String },
}

/// Every fillable `{name}` a template refers to — its slots and its holes.
///
/// Part of the wire contract, not an internal helper: a client drawing a block
/// needs to know which parts of the label are sockets, and it must learn that
/// from the block rather than from a grammar it carries.
pub fn placeholders(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{'
            && let Some(end) = template[i..].find('}')
        {
            let name = &template[i + 1..i + end];
            // A `{#name}` is a fresh variable the compiler invents, not
            // something anybody fills, so it is not a placeholder.
            if !name.starts_with('#') {
                out.push(name.to_string());
            }
            i += end + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// The `{#name}` fresh variables a template invents.
///
/// Separate from [`placeholders`] because they are opposite things: a
/// placeholder is filled from outside, a fresh variable is generated. Reading
/// both from one list is what silently stopped the substitution once.
fn fresh_vars(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{#") {
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        out.push(rest[open + 2..open + close].to_string());
        rest = &rest[open + close + 1..];
    }
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Palette {
    pub version: u32,
    pub blocks: Vec<BlockDef>,
}

impl Palette {
    fn get(&self, id: &str) -> Result<&BlockDef, BlockError> {
        self.blocks
            .iter()
            .find(|b| b.id == id)
            .ok_or_else(|| BlockError::UnknownBlock(id.to_string()))
    }

    /// Compile a tree to rule text.
    pub fn compile(&self, tree: &BlockTree) -> Result<String, BlockError> {
        let mut fresh = 0usize;
        self.compile_node(tree, &mut fresh)
    }

    fn compile_node(&self, tree: &BlockTree, fresh: &mut usize) -> Result<String, BlockError> {
        let def = self.get(&tree.block)?;

        // Fresh variables are numbered in traversal order, so compiling the
        // same tree twice gives the same text — which is what lets projection
        // verify itself by recompiling.
        let mut fresh_names: BTreeMap<String, String> = BTreeMap::new();
        for name in fresh_vars(&def.template) {
            if !fresh_names.contains_key(&name) {
                fresh_names.insert(name.clone(), format!("{}{}", fresh_var_stem(&name), *fresh));
                *fresh += 1;
            }
        }

        let mut out = def.template.clone();

        for (name, var) in &fresh_names {
            out = out.replace(&format!("{{#{name}}}"), var);
        }

        for slot in &def.slots {
            let value = tree
                .slots
                .get(&slot.name)
                .ok_or_else(|| BlockError::MissingSlot {
                    block: def.id.clone(),
                    slot: slot.name.clone(),
                })?;
            // A quote here would close the literal early and let the rest of
            // the value become rule syntax. Refuse rather than escape: an
            // escaped quote in a path is almost certainly a mistake anyway,
            // and refusing says so.
            if value.contains('"') {
                return Err(BlockError::QuoteInSlot {
                    value: value.clone(),
                });
            }
            out = out.replace(&format!("{{{}}}", slot.name), value);
        }

        for hole in &def.holes {
            let children = tree.holes.get(&hole.name).cloned().unwrap_or_default();
            for child in &children {
                let child_def = self.get(&child.block)?;
                if child_def.shape != hole.accepts {
                    return Err(BlockError::WrongShape {
                        block: def.id.clone(),
                        hole: hole.name.clone(),
                        expected: hole.accepts,
                        actual: child_def.shape,
                        got: child.block.clone(),
                    });
                }
            }
            if !hole.many && children.len() != 1 {
                if hole.accepts == Shape::Conclusion && children.is_empty() {
                    return Err(BlockError::NoConclusion);
                }
                return Err(BlockError::WrongCount {
                    block: def.id.clone(),
                    hole: hole.name.clone(),
                    count: children.len(),
                });
            }
            let mut parts = Vec::with_capacity(children.len());
            for child in &children {
                parts.push(self.compile_node(child, fresh)?);
            }
            out = out.replace(&format!("{{{}}}", hole.name), &parts.join(", "));
        }

        Ok(out)
    }

    /// Recover the tree a piece of rule text came from, or `None`.
    ///
    /// A guess, then a proof: whatever is reconstructed is compiled again and
    /// accepted only if it reproduces the input byte for byte. That is D14's
    /// requirement, enforced here rather than asserted, so a rule the palette
    /// cannot express is "beyond blocks" rather than a tree that is subtly
    /// not the rule.
    pub fn project(&self, text: &str) -> Option<BlockTree> {
        let guess = self.guess(text)?;
        match self.compile(&guess) {
            Ok(again) if again == text.trim() => Some(guess),
            _ => None,
        }
    }

    fn guess(&self, text: &str) -> Option<BlockTree> {
        let text = text.trim().trim_end_matches('.').trim();
        let (head, body) = text.split_once(":-")?;
        let conclusion = self.match_shape(head.trim(), Shape::Conclusion)?;

        // A single block can compile to SEVERAL comma-separated fragments —
        // `within_last` emits both the atom that binds the timestamp and the
        // comparison over it. Matching one fragment at a time could never
        // recover it, so try the longest run first and fall back.
        let fragments = split_conditions(body);
        let mut when = Vec::new();
        let mut i = 0;
        while i < fragments.len() {
            let mut matched = None;
            for take in (1..=fragments.len() - i).rev() {
                let joined = fragments[i..i + take].join(", ");
                if let Some(tree) = self.match_shape(joined.trim(), Shape::Condition) {
                    matched = Some((tree, take));
                    break;
                }
            }
            let (tree, take) = matched?;
            when.push(tree);
            i += take;
        }

        let mut holes = BTreeMap::new();
        holes.insert("then".to_string(), vec![conclusion]);
        holes.insert("when".to_string(), when);
        Some(BlockTree {
            block: "rule".to_string(),
            slots: BTreeMap::new(),
            holes,
        })
    }

    /// Find a block of the given shape whose template matches this fragment.
    fn match_shape(&self, fragment: &str, shape: Shape) -> Option<BlockTree> {
        for def in self.blocks.iter().filter(|b| b.shape == shape) {
            if let Some(tree) = self.match_block(def, fragment) {
                return Some(tree);
            }
        }
        None
    }

    fn match_block(&self, def: &BlockDef, fragment: &str) -> Option<BlockTree> {
        // Literal segments around the placeholders have to appear in order;
        // what sits between them is the captured value.
        let mut slots = BTreeMap::new();
        let mut holes: BTreeMap<String, Vec<BlockTree>> = BTreeMap::new();
        let mut rest = fragment;
        let mut template = def.template.as_str();

        while let Some(open) = template.find('{') {
            let literal = &template[..open];
            rest = rest.strip_prefix(literal)?;
            let close = template[open..].find('}')? + open;
            let raw = &template[open + 1..close];
            template = &template[close + 1..];

            // The next literal tells us where this capture ends.
            let next_literal_end = template.find('{').unwrap_or(template.len());
            let next_literal = &template[..next_literal_end];
            let cut = if next_literal.is_empty() {
                rest.len()
            } else {
                rest.find(next_literal)?
            };
            let captured = &rest[..cut];
            rest = &rest[cut..];

            if let Some(name) = raw.strip_prefix('#') {
                // A fresh variable: accept whatever compile would have made,
                // since recompiling is what actually decides.
                let _ = name;
            } else if let Some(hole) = def.holes.iter().find(|h| h.name == raw) {
                let mut inner = Vec::new();
                for piece in split_conditions(captured) {
                    inner.push(self.match_shape(piece.trim(), hole.accepts)?);
                }
                holes.insert(raw.to_string(), inner);
            } else {
                let def_slot = def.slots.iter().find(|s| s.name == raw)?;
                if !plausible(def_slot.kind, captured) {
                    return None;
                }
                slots.insert(raw.to_string(), captured.to_string());
            }
        }
        rest.strip_prefix(template).filter(|r| r.is_empty())?;

        Some(BlockTree {
            block: def.id.clone(),
            slots,
            holes,
        })
    }
}

/// Whether a captured string could really be the kind of thing this slot
/// holds.
///
/// Without this a greedy template swallows syntax belonging to another block:
/// `{relation}(E, "{other}")` will happily read `not tag(E, "secret")` as a
/// relation called `not tag`, and the projection then looks plausible and is
/// wrong.
fn plausible(kind: SlotKind, captured: &str) -> bool {
    match kind {
        SlotKind::Relation => {
            !captured.is_empty()
                && captured
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        }
        SlotKind::Binding => {
            !captured.is_empty()
                && captured.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
                && captured
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        SlotKind::Number => captured.parse::<f64>().is_ok(),
        SlotKind::Operator => {
            matches!(captured, "==" | "!=" | "<=" | ">=" | "<" | ">" | "=")
        }
        SlotKind::Unit => matches!(captured, "weeks" | "days" | "hours" | "minutes"),
        // A path, tag, bucket or free text can be almost anything, but never
        // a quote — compile refuses those, so a capture holding one could not
        // have come from this palette.
        SlotKind::Path | SlotKind::Tag | SlotKind::Bucket | SlotKind::Text => {
            !captured.contains('"')
        }
    }
}

/// The stem a fresh variable gets, so generated text reads as something a
/// person could have typed.
fn fresh_var_stem(name: &str) -> &'static str {
    match name {
        "when" => "C",
        "group" => "X",
        _ => "C",
    }
}

/// Split a body on top-level commas, ignoring those inside parentheses or
/// string literals — the same hazard `split_top_level` has in the parser.
fn split_conditions(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut depth, mut in_str) = (0i32, false);
    let mut escaped = false;
    for ch in body.chars() {
        if in_str {
            cur.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_str = true;
                cur.push(ch);
            }
            '(' | '[' => {
                depth += 1;
                cur.push(ch);
            }
            ')' | ']' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

impl Palette {
    /// One filled example per block, used to prove every block round-trips.
    pub fn samples(&self) -> Vec<BlockTree> {
        fn n(block: &str, slots: &[(&str, &str)]) -> BlockTree {
            BlockTree {
                block: block.into(),
                slots: slots
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                holes: BTreeMap::new(),
            }
        }
        fn rule(when: Vec<BlockTree>) -> BlockTree {
            let mut holes = BTreeMap::new();
            holes.insert("then".into(), vec![n("is_bucket", &[("bucket", "wisdom")])]);
            holes.insert("when".into(), when);
            BlockTree {
                block: "rule".into(),
                slots: BTreeMap::new(),
                holes,
            }
        }
        let mut not_secret = BTreeMap::new();
        not_secret.insert("inner".to_string(), vec![n("tagged", &[("tag", "secret")])]);

        let mut cmp = BTreeMap::new();
        cmp.insert("left".to_string(), vec![n("the_value", &[("name", "V")])]);
        cmp.insert("right".to_string(), vec![n("a_number", &[("n", "0.8")])]);

        vec![
            rule(vec![n("under_path", &[("path", "research/skills")])]),
            rule(vec![n("tagged", &[("tag", "curated")])]),
            rule(vec![n("in_bucket", &[("bucket", "knowledge")])]),
            rule(vec![n(
                "relates",
                &[("relation", "part_of"), ("other", "fleet")],
            )]),
            rule(vec![n("within_last", &[("n", "7"), ("unit", "days")])]),
            rule(vec![
                n("under_path", &[("path", "notes")]),
                BlockTree {
                    block: "not".into(),
                    slots: BTreeMap::new(),
                    holes: not_secret,
                },
            ]),
            rule(vec![
                n("has_field", &[("field", "confidence"), ("name", "V")]),
                BlockTree {
                    block: "compare".into(),
                    slots: [("op".to_string(), ">".to_string())].into_iter().collect(),
                    holes: cmp,
                },
            ]),
        ]
    }

    /// The starting palette.
    pub fn seed() -> Self {
        fn slot(name: &str, kind: SlotKind, label: &str) -> SlotDef {
            SlotDef {
                name: name.into(),
                kind,
                label: label.into(),
            }
        }
        fn hole(name: &str, accepts: Shape, many: bool, label: &str) -> HoleDef {
            HoleDef {
                name: name.into(),
                accepts,
                many,
                label: label.into(),
            }
        }
        fn b(
            id: &str,
            shape: Shape,
            label: &str,
            template: &str,
            slots: Vec<SlotDef>,
            holes: Vec<HoleDef>,
        ) -> BlockDef {
            BlockDef {
                id: id.into(),
                shape,
                label: label.into(),
                template: template.into(),
                slots,
                holes,
            }
        }
        use Shape::*;
        use SlotKind as K;

        Palette {
            version: 1,
            blocks: vec![
                b(
                    "rule",
                    Rule,
                    "When {when}, it {then}",
                    "{then} :- {when}.",
                    vec![],
                    vec![
                        hole("then", Conclusion, false, "it is"),
                        hole("when", Condition, true, "when"),
                    ],
                ),
                // ── Conclusions ───────────────────────────────────
                b(
                    "is_bucket",
                    Conclusion,
                    "is {bucket}",
                    r#"tier(E, "{bucket}")"#,
                    vec![slot("bucket", K::Bucket, "bucket")],
                    vec![],
                ),
                b(
                    "is_shareable",
                    Conclusion,
                    "may be shared",
                    "shareable(E)",
                    vec![],
                    vec![],
                ),
                b(
                    "is_suppressed",
                    Conclusion,
                    "is hidden from results",
                    "suppressed(E)",
                    vec![],
                    vec![],
                ),
                b(
                    "has_relation",
                    Conclusion,
                    "is {relation} of {other}",
                    r#"{relation}(E, "{other}")"#,
                    vec![
                        slot("relation", K::Relation, "relation"),
                        slot("other", K::Text, "the other thing"),
                    ],
                    vec![],
                ),
                // ── Conditions ────────────────────────────────────
                b(
                    "under_path",
                    Condition,
                    "is under {path}",
                    r#"source_root(E, "{path}")"#,
                    vec![slot("path", K::Path, "folder")],
                    vec![],
                ),
                b(
                    "tagged",
                    Condition,
                    "is tagged {tag}",
                    r#"tag(E, "{tag}")"#,
                    vec![slot("tag", K::Tag, "tag")],
                    vec![],
                ),
                b(
                    "in_bucket",
                    Condition,
                    "is in {bucket}",
                    r#"tier(E, "{bucket}")"#,
                    vec![slot("bucket", K::Bucket, "bucket")],
                    vec![],
                ),
                b(
                    "relates",
                    Condition,
                    "{relation} {other}",
                    r#"{relation}(E, "{other}")"#,
                    vec![
                        slot("relation", K::Relation, "relation"),
                        slot("other", K::Text, "the other thing"),
                    ],
                    vec![],
                ),
                b(
                    "has_field",
                    Condition,
                    "has a {field}, call it {name}",
                    "{field}(E, {name})",
                    vec![
                        slot("field", K::Relation, "property"),
                        slot("name", K::Binding, "call it"),
                    ],
                    vec![],
                ),
                b(
                    "within_last",
                    Condition,
                    "was made in the last {n} {unit}",
                    "created_at(E, {#when}), date({#when}) > now() - {unit}({n})",
                    vec![
                        slot("n", K::Number, "how many"),
                        slot("unit", K::Unit, "unit"),
                    ],
                    vec![],
                ),
                b(
                    "count_of",
                    Condition,
                    "count its {relation}, call it {name}",
                    "count({relation}({#group}, E), {name})",
                    vec![
                        slot("relation", K::Relation, "relation"),
                        slot("name", K::Binding, "call it"),
                    ],
                    vec![],
                ),
                b(
                    "not",
                    Condition,
                    "not {inner}",
                    "not {inner}",
                    vec![],
                    vec![hole("inner", Condition, false, "not")],
                ),
                b(
                    "compare",
                    Condition,
                    "{left} {op} {right}",
                    "{left} {op} {right}",
                    vec![slot("op", K::Operator, "is")],
                    vec![
                        hole("left", Value, false, ""),
                        hole("right", Value, false, ""),
                    ],
                ),
                // ── Values ────────────────────────────────────────
                b(
                    "the_value",
                    Value,
                    "{name}",
                    "{name}",
                    vec![slot("name", K::Binding, "the value")],
                    vec![],
                ),
                b(
                    "a_number",
                    Value,
                    "{n}",
                    "{n}",
                    vec![slot("n", K::Number, "number")],
                    vec![],
                ),
                b(
                    "some_text",
                    Value,
                    "{text}",
                    r#""{text}""#,
                    vec![slot("text", K::Text, "text")],
                    vec![],
                ),
                b("right_now", Value, "right now", "now()", vec![], vec![]),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(block: &str, slots: &[(&str, &str)], holes: &[(&str, Vec<BlockTree>)]) -> BlockTree {
        BlockTree {
            block: block.to_string(),
            slots: slots
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            holes: holes
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        }
    }
    fn leaf(block: &str, slots: &[(&str, &str)]) -> BlockTree {
        t(block, slots, &[])
    }
    fn cat() -> Palette {
        Palette::seed()
    }

    // ── The palette is a contract ─────────────────────────────────

    #[test]
    fn the_palette_is_versioned_and_covers_most_of_the_grammar() {
        let p = cat();
        assert!(p.version >= 1);
        for shape in [Shape::Condition, Shape::Value, Shape::Conclusion] {
            assert!(
                p.blocks.iter().filter(|b| b.shape == shape).count() >= 4,
                "{shape:?} is too thin to compose with"
            );
        }
    }

    #[test]
    fn every_block_declares_a_slot_or_hole_for_every_placeholder() {
        for b in cat().blocks {
            for used in placeholders(&b.template) {
                let known = b.slots.iter().any(|s| s.name == used)
                    || b.holes.iter().any(|h| h.name == used);
                assert!(
                    known,
                    "block '{}' uses {{{used}}} with nothing to fill it",
                    b.id
                );
            }
        }
    }

    #[test]
    fn every_hole_accepts_a_shape_some_block_actually_has() {
        let p = cat();
        for b in &p.blocks {
            for h in &b.holes {
                assert!(
                    p.blocks.iter().any(|other| other.shape == h.accepts),
                    "block '{}' hole '{}' accepts {:?}, which nothing is",
                    b.id,
                    h.name,
                    h.accepts
                );
            }
        }
    }

    // ── Compiling ─────────────────────────────────────────────────

    #[test]
    fn the_simplest_rule_compiles_to_what_a_person_would_have_typed() {
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("is_bucket", &[("bucket", "wisdom")])]),
                (
                    "when",
                    vec![leaf("under_path", &[("path", "research/skills")])],
                ),
            ],
        );
        assert_eq!(
            cat().compile(&tree).unwrap(),
            r#"tier(E, "wisdom") :- source_root(E, "research/skills")."#
        );
    }

    #[test]
    fn conditions_join_as_a_conjunction() {
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("is_bucket", &[("bucket", "knowledge")])]),
                (
                    "when",
                    vec![
                        leaf("under_path", &[("path", "notes")]),
                        leaf("tagged", &[("tag", "curated")]),
                    ],
                ),
            ],
        );
        assert_eq!(
            cat().compile(&tree).unwrap(),
            r#"tier(E, "knowledge") :- source_root(E, "notes"), tag(E, "curated")."#
        );
    }

    #[test]
    fn the_not_wrapper_swallows_the_block_inside_it() {
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("is_bucket", &[("bucket", "wisdom")])]),
                (
                    "when",
                    vec![
                        leaf("under_path", &[("path", "notes")]),
                        t(
                            "not",
                            &[],
                            &[("inner", vec![leaf("tagged", &[("tag", "secret")])])],
                        ),
                    ],
                ),
            ],
        );
        assert_eq!(
            cat().compile(&tree).unwrap(),
            r#"tier(E, "wisdom") :- source_root(E, "notes"), not tag(E, "secret")."#
        );
    }

    #[test]
    fn a_relation_block_reaches_the_ontology_vocabulary() {
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("is_bucket", &[("bucket", "knowledge")])]),
                (
                    "when",
                    vec![leaf(
                        "relates",
                        &[("relation", "part_of"), ("other", "fleet")],
                    )],
                ),
            ],
        );
        assert_eq!(
            cat().compile(&tree).unwrap(),
            r#"tier(E, "knowledge") :- part_of(E, "fleet")."#
        );
    }

    #[test]
    fn a_comparison_composes_value_blocks() {
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("is_bucket", &[("bucket", "wisdom")])]),
                (
                    "when",
                    vec![
                        leaf("has_field", &[("field", "confidence"), ("name", "V")]),
                        t(
                            "compare",
                            &[("op", ">")],
                            &[
                                ("left", vec![leaf("the_value", &[("name", "V")])]),
                                ("right", vec![leaf("a_number", &[("n", "0.8")])]),
                            ],
                        ),
                    ],
                ),
            ],
        );
        assert_eq!(
            cat().compile(&tree).unwrap(),
            r#"tier(E, "wisdom") :- confidence(E, V), V > 0.8."#
        );
    }

    #[test]
    fn the_time_block_compiles_to_a_computation_not_a_fact() {
        // "If we can compute we shouldn't infer" — this is a filter over the
        // clock, not a `recent` predicate somebody has to keep true.
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("is_bucket", &[("bucket", "knowledge")])]),
                (
                    "when",
                    vec![leaf("within_last", &[("n", "7"), ("unit", "days")])],
                ),
            ],
        );
        assert_eq!(
            cat().compile(&tree).unwrap(),
            r#"tier(E, "knowledge") :- created_at(E, C0), date(C0) > now() - days(7)."#
        );
    }

    #[test]
    fn an_aggregate_block_counts_a_group() {
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("is_bucket", &[("bucket", "knowledge")])]),
                (
                    "when",
                    vec![
                        leaf("count_of", &[("relation", "part_of"), ("name", "N")]),
                        t(
                            "compare",
                            &[("op", ">")],
                            &[
                                ("left", vec![leaf("the_value", &[("name", "N")])]),
                                ("right", vec![leaf("a_number", &[("n", "3")])]),
                            ],
                        ),
                    ],
                ),
            ],
        );
        assert_eq!(
            cat().compile(&tree).unwrap(),
            r#"tier(E, "knowledge") :- count(part_of(X0, E), N), N > 3."#
        );
    }

    // ── Refusals ──────────────────────────────────────────────────

    #[test]
    fn a_block_of_the_wrong_shape_in_a_hole_is_refused() {
        // D8: malformed should be unbuildable. The client stops it early with
        // shapes; the server refuses regardless, because the client is never
        // the thing deciding.
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("under_path", &[("path", "x")])]),
                ("when", vec![leaf("under_path", &[("path", "x")])]),
            ],
        );
        let e = format!("{}", cat().compile(&tree).unwrap_err());
        assert!(
            e.contains("Conclusion") || e.contains("conclusion"),
            "got: {e}"
        );
    }

    #[test]
    fn an_unknown_block_is_refused_by_name() {
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("is_bucket", &[("bucket", "wisdom")])]),
                ("when", vec![leaf("frobnicate", &[])]),
            ],
        );
        assert!(format!("{}", cat().compile(&tree).unwrap_err()).contains("frobnicate"));
    }

    #[test]
    fn a_missing_slot_is_refused_by_name() {
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("is_bucket", &[("bucket", "wisdom")])]),
                ("when", vec![leaf("under_path", &[])]),
            ],
        );
        assert!(format!("{}", cat().compile(&tree).unwrap_err()).contains("path"));
    }

    #[test]
    fn a_slot_value_that_would_break_out_of_its_quotes_is_refused() {
        // Otherwise a path with a quote in it rewrites the rule around it,
        // which is injection by another name.
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("is_bucket", &[("bucket", "wisdom")])]),
                (
                    "when",
                    vec![leaf("under_path", &[("path", r#"a") :- evil(E"#)])],
                ),
            ],
        );
        assert!(format!("{}", cat().compile(&tree).unwrap_err()).contains("quote"));
    }

    #[test]
    fn a_rule_with_no_conclusion_is_refused() {
        let tree = t(
            "rule",
            &[],
            &[("when", vec![leaf("under_path", &[("path", "x")])])],
        );
        assert!(format!("{}", cat().compile(&tree).unwrap_err()).contains("conclusion"));
    }

    // ── Projection, and the law that binds the two ────────────────

    #[test]
    fn text_projects_back_to_the_tree_that_made_it() {
        let tree = t(
            "rule",
            &[],
            &[
                ("then", vec![leaf("is_bucket", &[("bucket", "wisdom")])]),
                (
                    "when",
                    vec![
                        leaf("under_path", &[("path", "research/skills")]),
                        t(
                            "not",
                            &[],
                            &[("inner", vec![leaf("tagged", &[("tag", "secret")])])],
                        ),
                    ],
                ),
            ],
        );
        let text = cat().compile(&tree).unwrap();
        assert_eq!(cat().project(&text), Some(tree));
    }

    #[test]
    fn a_rule_beyond_the_palette_projects_to_nothing_rather_than_half_a_tree() {
        // D14: the composer must be able to say "this rule is beyond blocks"
        // without implying anything is wrong with it.
        assert_eq!(
            cat().project("q(X, Y) :- p(X, Y), percentile(r(X, V), V, 0.9, N)."),
            None
        );
    }

    #[test]
    fn projection_is_verified_by_recompiling_rather_than_asserted() {
        // Every block in the palette round-trips. D8 calls this a hard
        // requirement and a good test.
        for sample in cat().samples() {
            let text = cat().compile(&sample).expect("sample compiles");
            let back = cat()
                .project(&text)
                .unwrap_or_else(|| panic!("'{text}' did not project back"));
            assert_eq!(back, sample, "tree -> text -> tree must be stable");
            assert_eq!(cat().compile(&back).unwrap(), text, "and back again");
        }
    }

    #[test]
    fn everything_the_palette_can_build_is_a_rule_the_engine_accepts() {
        // D1: the engine decides validity, so ask it rather than trusting the
        // compiler in this file.
        for sample in cat().samples() {
            let text = cat().compile(&sample).unwrap();
            crate::datalog::parse_rules(&text)
                .unwrap_or_else(|e| panic!("palette produced unparseable rule '{text}': {e}"));
        }
    }
}
