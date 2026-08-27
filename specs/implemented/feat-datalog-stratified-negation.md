---
title: Stratified negation for the datalog engine
executive_summary: >
  The rule engine can only say what IS, never what is NOT, so every exclusion
  in the corpus is written backwards as a positive floor. The stratifier
  already built for aggregates is most of the machinery negation needs; the
  work that is genuinely new is range-restriction safety, why-not provenance,
  and invalidating persisted derived facts that a later fact makes false.
status: implemented
priority: P100
source: sharing blueprint Phase 0
component: ferrosa-memory-core/src/datalog.rs
last_updated: 2026-08-26
---

# Stratified negation for the datalog engine

## Why this exists

`ferrosa-memory-core/src/datalog.rs` has no negation. Every rule body is a
conjunction of positive atoms, so no rule can express "and NOT that".

This is not an abstract gap. It has already bent one design out of shape.
Sharing wanted "skills and their links, but **not** corpus data", and because
the engine cannot say "not", D4 was written as a positive tier floor instead:

```datalog
shareable(E) :- tier(E, "wisdom").
shareable(E) :- tier(E, "knowledge").
```

That floor is a *positive encoding of a negative intent*. It happens to be
equivalent today only because the tier lattice is totally ordered and closed.
It stops being equivalent the moment anyone adds a tier, adds a corpus kind
that is not tier-ranked, or wants an exclusion that does not lie along the tier
axis — "share everything except items tagged `secret`" cannot be written at
all. Each such requirement currently costs a bespoke enumeration in Rust,
outside the rule system, where it is neither inspectable nor tenant-editable.

The same shape will recur anywhere rules meet policy: retention ("everything
older than N **except** pinned"), promotion ("promote unless contradicted"),
sharing exclusions, and agent restriction rules.

## What already exists (most of the mechanism)

`stratify()` at `datalog.rs:1293` was built for aggregates and is, structurally,
the stratifier negation requires:

- a predicate dependency graph with **typed edges** — `Edge::Plain`,
  `Edge::Aggregate`
- iterative Tarjan SCC (explicit work stack, no recursion — Power-of-10 rule 1)
- rejection of any SCC containing a special edge **within** the SCC, returning
  `StratifyError::RecursionThroughAggregate { cycle }`
- stratum assignment lifting `+1` for plain edges and `+2` across an aggregate
  edge, so the aggregated relation is settled before it is read
- `evaluate()` running `evaluate_stratum()` per stratum against the accumulated
  fact set

Negation is a third edge kind on exactly this machinery. Soundness follows the
same argument the aggregate case already relies on: a negated atom may only
reference a **lower** stratum, so during any one stratum's semi-naive fixpoint
the negated relation is constant, and the monotonicity that semi-naive
evaluation assumes still holds within the stratum.

## What is actually missing

### 1. Syntax and representation

`parse_rule` (`datalog.rs:54`) splits the body on commas and knows nothing of
`not`. `DatalogRule.body` is `Vec<Atom>` with no polarity field.

`DatalogRule` is `Serialize`/`Deserialize` and reaches CQL through `RuleEntry`
(`cql_storage.rs:1023`, `6315`, `6375`). Changing `body` to `Vec<Literal>`
is therefore a **stored-format change**, not a local refactor. `types.rs:708`
already carries a note about keeping `RuleEntry` rows deserializable.

Prefer an additive field — `#[serde(default)] negated: Vec<Atom>` — over
retyping `body`. Old rows deserialize with an empty negated set and mean
exactly what they meant before. This is the same choice `aggregates` already
made.

### 2. Range-restriction safety (the one that bites)

Every variable appearing in a negated atom must also be bound by a **positive**
body atom. Otherwise `not p(X)` asks for every `X` in the universe that is not
in `p` — an infinite or store-sized answer, and in practice a full scan of
everything.

There is no safety check in the engine today because positive-only rules are
trivially range-restricted. Negation makes one mandatory, and it must be a
**load-time rejection with a named variable**, not a runtime surprise:

```
UnsafeNegation { rule: String, unbound: Vec<String> }
```

`filter_references_any` (`datalog.rs:568`) already walks filter expressions for
variable references and is the model for the check.

### 3. Why-not provenance

`DerivedFact` carries `provenance: Vec<ProvenanceStep>`, `support_count`, and a
`confidence` from `compute_confidence(&provenance_steps, &all_facts)`. A
positive atom contributes a fact to point at. **A negated literal contributes an
absence, and there is no row to cite.**

Left alone this degrades silently: a rule that fires largely because something
was absent produces fewer provenance steps, hence a lower `support_count` and a
lower confidence, and the explanation surfaced by `bounded_explanation`
(`dispatch.rs:10330`) simply omits the reason it fired. Decide deliberately:

- a `ProvenanceStep` variant that names the *absent* predicate and its bindings
- whether an absence counts toward `support_count` (recommend: yes, it is
  support), and toward confidence (recommend: a distinct, lower weight —
  absence under an open-world store is weaker evidence than presence)

### 4. Non-monotonic invalidation of persisted derived facts

This is the largest risk and it is not in the evaluator at all.

Derived facts are **persisted** — `cql_storage.rs:6776` writes them,
`dream.rs:291` caches them. Today the engine is monotonic: adding a base fact
can only ever add derived facts, so an append-only store of derivations is
always correct.

Negation breaks that. Adding a base fact can make a previously-derived fact
**false**. An append-only derived-fact store then serves a stale derivation
forever, and for a permission rule that means access that should have been
revoked is still granted by the cache.

Options, in ascending cost:

- **Scope negation to non-persisted evaluation.** Rules using negation are
  evaluated live and their results are never cached. Cheapest, and enough for
  sharing/permissions, which want a live answer anyway.
- **Invalidate by rule family** on any base-fact change touching a predicate a
  negated rule reads. Coarse, simple, correct.
- **DRed** (delete-and-rederive) incremental view maintenance. Correct and
  general; a project in its own right.

Recommend the first for the initial slice, with the persistence path refusing
to cache any `DerivedFact` whose rule contains a negated literal — enforced in
code, not by convention.

### 5. Unstratifiable rule sets are currently silent

`evaluate()` warns and returns `initial_facts` unchanged when `stratify` fails,
deriving nothing. For a *permission* rule set, deriving nothing denies, which is
fail-closed and safe. For a *tier floor* it is silently wrong — the caller
cannot distinguish "no rules matched" from "your rule set was rejected".

Negation multiplies the ways a rule set becomes unstratifiable, so this should
become a typed error the caller must handle, rather than a log line. Related to
the fail-loud rule in `skills/rules/safety.md`.

## Acceptance criteria

- [x] `parse_rule` accepts `not p(X, Y)` in a rule body; round-trips through
      `to_string`/serde; existing rows without the field still deserialize
- [x] A negated atom whose variables are not all bound by a positive atom is
      **rejected at rule load** with the offending variable named
- [x] `stratify` gains `Edge::Negated`, rejects recursion through negation with
      `StratifyError::RecursionThroughNegation { cycle }`, and lifts a stratum
      across a negated edge
- [x] `evaluate_rule` filters bindings against negated atoms using the settled
      lower-stratum fact set
- [x] Provenance names the absent predicate; confidence weighting for absence is
      an explicit, tested decision rather than an emergent one
- [x] A `DerivedFact` from a rule containing negation is never written to the
      persisted derived-fact store, enforced in code
- [x] Property test: for any rule set with no negated literals, the evaluator's
      output is byte-identical to today's — negation is additive
- [x] Test: adding a base fact that falsifies a previously-derived fact yields
      the correct live answer on re-evaluation
- [x] The D4 tier floor is re-expressible as an exclusion, and both forms derive
      the same set on the current corpus

## Not in scope

- Well-founded or stable-model semantics for unstratifiable programs. Stratified
  negation is the whole of this item; anything requiring three-valued semantics
  is a separate decision.
- DRed / incremental view maintenance (see §4 option 3).
- Rewriting D4 or the sharing closure. That is downstream and follows from this.


## Implementation Notes

Branch `feat/datalog-stratified-negation`. All nine acceptance criteria are met
and covered by tests; the two items below were deliberately left out and are
tracked separately.

**Representation.** `DatalogRule` gained `#[serde(default)] negated: Vec<Atom>`
rather than retyping `body` to carry polarity, as the spec recommended. Worth
recording: **no schema migration was needed.** `RuleEntry.rule_body` is a TEXT
column holding the rule *source*, which `parse_rule` reads — `DatalogRule` is
never itself serialised into CQL. Negation therefore round-trips through the
existing column as text. The `serde(default)` remains as defence for any
in-flight JSON.

**Parsing.** `strip_not_prefix` requires whitespace after the keyword, so
`nothing(X)` stays a positive atom. Negated parts are deferred to a third pass,
like aggregates, because the safety check needs the final positive body.

**Range restriction.** Every *named* variable in a negated atom must be bound by
a positive body atom; rejection is at parse time and names the offending
variable. One deliberate narrowing of the spec's wording: an **anonymous**
variable is allowed. The parser renames each `_` uniquely, so it can never be
named by the head or a filter, and the check asks only whether *some* row
matches — it never enumerates the universe, which is the harm the rule exists to
prevent. `not r(X, _)` is therefore legal and tested.

**Stratification.** `Edge::Negated` is a third kind on the existing typed
dependency graph. A negated edge inside an SCC yields
`StratifyError::RecursionThroughNegation`. The `+2` lift is shared with
aggregates (renamed `had_settling_edge`): both read a relation rather than
extend it, and the extra lift is what makes the negated atom constant during the
stratum's semi-naive fixpoint.

**Provenance and confidence.** An absence has no row to cite, so it is recorded
as a `ProvenanceStep` with `parent_kind == PROVENANCE_KIND_ABSENCE` naming the
absent predicate and the bindings it was checked under. Both decisions the spec
asked to be explicit are made and tested: an absence **does** count toward
`support_count`, and it discounts confidence by `ABSENCE_CONFIDENCE_WEIGHT`
(0.8), applied **once** however many absences a derivation rests on — the
weakness is in the kind of evidence, not its count.

**Non-monotonic invalidation.** Option 1 from §4, enforced in code:
`DerivedFact::is_cacheable()` is false for any fact whose provenance contains an
absence, and both persistence call sites (`query_predicate`, `dream.rs`) filter
on it. Deriving this from provenance rather than a new struct field avoided a
second stored-format change and kept all twelve existing `DerivedFact`
construction sites untouched.

**Additivity, verified rather than asserted.** `tests/fixtures/`
`datalog_pre_negation_digest.txt` was generated by running
`tests/datalog_negation_additivity.rs` against commit `6462988` — the commit
before this work. The new engine reproduces it byte-for-byte. The digest covers
predicate, endpoints, confidence, support count, rule id and full provenance, so
drift in any of them fails the test.

**Verification.** 1334 workspace lib tests pass (25 new negation tests), the
four contract suites pass, `cargo clippy --workspace --all-targets -D warnings`
is clean, `cargo fmt --check` is clean. Live-cluster tests were not run.

### Left out, tracked separately

- §5 — making an unstratifiable rule set a typed error from `evaluate()` rather
  than a `tracing::warn!` that derives nothing. Real and worth doing, but it
  changes `evaluate`'s signature and every caller, which does not belong in the
  same change as the feature. Today's silence is unchanged by this work.
- §4 options 2 and 3 (family-level invalidation, DRed) remain out of scope, as
  the spec states; option 1 is what shipped.
- Rewriting D4 itself to use the exclusion form. This work proves the two forms
  derive the same set; adopting it in `sharing.rs` is downstream.
