---
title: Where the rules editor fits against the ontology and sandbox work
executive_summary: >
  Three efforts converged on the same model without coordinating: the reasoning
  vocabulary in ferrosa-memory, the portable ontology package in
  ferrosa-experts, and the sandbox generation that vendors the datalog engine.
  This records what is actually shared, what is duplicated, and the two gaps
  the merge surfaced — one in each direction.
status: in-progress
last_updated: 2026-08-30
validated_by: >
  ferrosa-experts' own ontology/validate.py accepts the vocabulary exported
  from ferrosa-memory, run 2026-08-30.
---

# Where the editor fits

## The three pieces, and the level each sits at

| Piece | Where | What it is |
|---|---|---|
| **Portable ontology package** | `ferrosa-experts/ontology/` | The *published* form. JSON-LD in the `fo:` namespace, an upper ontology of 14 meta-terms, validated fail-closed in CI. What crosses a boundary. |
| **Reasoning vocabulary** | `ferrosa-memory-core/src/ontology.rs` | The *executable* form. 24 domain predicates whose characteristics **generate** datalog reasoning. What actually runs. |
| **Block palette** | `ferrosa-memory-core/src/rule_blocks.rs` | The *composable* form. Four Scratch shapes over that vocabulary; compile to text, project back, round-trip proven. What a person touches. |
| **Sandbox generation** | `ferrosa-experts/vendor/ferrosa-datalog/` | The engine, **vendored** at `317a0a30`, storage-coupling stripped, so build-expert can reason without a cross-repo refactor. |

They are not competing. They are the published, executable, composable and
embedded forms of one thing.

## The convergence is real, and it was independent

`ferrosa-experts/ontology/validate.py` already knew:

```python
PREDICATE_KINDS = {"base", "derived", "computed"}
CHARACTERISTICS = {"transitive", "symmetric", "irreflexive", "disjoint_classes"}
```

`ferrosa-memory-core/src/ontology.rs` was written without seeing that file and
arrived at `PredicateKind::{Base, Derived, Computed}` and
`Characteristic::{Transitive, Symmetric, Irreflexive, DisjointClasses, ...}`.

Two people solving the same problem reached the same model. That is the
strongest available evidence the model is right, and it is why unifying these
is a merge rather than a negotiation.

## The two gaps, one in each direction

Found by exporting one into the other's format and running the real validator.

**The package format cannot carry two of the characteristics.** `inverse_of`
and `sub_property_of` are not in the schema's set, and they generate most of
the reasoning in the vocabulary — eight relations use them:

```
part_of: inverse_of(contains)        calls: sub_property_of(depends_on)
contains: inverse_of(part_of)        uses:  sub_property_of(depends_on)
owns / owned_by, reports_to / manages
```

The export **reports** these rather than dropping them. A published package
that silently reasons less than the vocabulary it claims to be is worse than
one that says what it left behind. Closing this is a `v1.1` of the package
schema, and it is the single highest-value change to make the two one thing.

**The vocabulary had no domain or range.** The package contract requires both
on every relationship, and it is right to: without them nothing can check that
a rule relates the kinds of thing the relation was meant for. Added, and the
export now validates.

## What is genuinely duplicated, and the hazard

The engine is vendored, and `VENDOR.md` already names the risk and tracks
extraction as deferred work. Worth stating plainly what the pin costs today:
the vendor sits at `317a0a30`, which **has** the temporal work and does **not**
have the vocabulary or the palette. Anything build-expert reasons about is one
grammar behind, and nothing will say so.

This is the same shape as D1 in the rules-plane decisions — *two sides holding
independent copies of one vocabulary, one of them stale* — arriving by a
different door. D1's answer was that only one copy exists, the one that parses.
The equivalent here is extracting the engine into a shared crate, which
`VENDOR.md` already calls the right end state.

## Where the panel fits

The ask is *search / categorise / derive ontology, in a simple panel*. Each
verb already has its machinery, and none of it is new work:

- **Search** — a rule is a named set. `evaluate` answers it live, and D2 says
  conclusions are derived on read rather than stored.
- **Categorise** — the conclusion blocks. `is_bucket` compiles to
  `tier(E, "…")`, which is what D10's user-named buckets are built on.
- **Derive ontology** — `Vocabulary::reasoning_rules()`. This is the part that
  is genuinely *ontology* rather than filtering: transitive closure, inverses,
  sub-property lifting, and the violation checks that report a cycle rather
  than absorbing it.

The panel is therefore a thin surface over three things that exist. What it
still needs from the server is the wiring, not the reasoning:

1. `GET` the palette and the vocabulary — D8 requires both come from the
   server, so a new block or relation reaches every client without a release.
2. `POST` a block tree — compile, then validate with `parse_rule`, because D1
   says the engine decides and the client never does.
3. Preview by count before applying — D6 and D12, and the same paged-read
   constraint R7 names.

## What this does not settle

The natural-language editor's runtime. Nothing above depends on it: the palette
and the vocabulary are the same artifacts whether a person snaps blocks
together or an agent writes the rule, which is the argument for building this
layer first regardless.
