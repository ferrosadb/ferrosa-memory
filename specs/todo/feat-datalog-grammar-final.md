---
title: Close the datalog grammar
executive_summary: >
  The completeness pass in round 2 named three remaining gaps — exponentiation,
  order statistics, and getting the values out of a group rather than only a
  statistic — plus two term kinds it skipped. This closes the capability gaps
  and gives the term kinds a final answer, so the question does not need
  asking again.
status: todo
priority: P45
component: ferrosa-memory-core/src/datalog.rs
last_updated: 2026-08-27
---

# Close the datalog grammar

Round 1 closed modulo, string shape, filter disjunction, ordered extremes and
computed heads. Round 2 closed set membership, function calls, body bindings,
atom disjunction and distinct counting, and declined integer numerals after
measuring that nothing could reach the precision limit.

Round 2's completeness pass named what was left. This closes it.

## Checklist

- [x] **1. Exponentiation.** `ArithOp` is Add/Sub/Mul/Div/Rem. `V ** 2` is
      unwritable, so a rule cannot square, cube, or apply any power law.
      Trivial, streaming, and the only arithmetic operator still missing.

- [x] **2. Standard deviation.** A real fold, and worth separating from median
      for exactly that reason: Welford's method computes variance in ONE pass
      with constant memory, so `stddev` streams like `sum` and `avg` and does
      not belong in the bounded family below.

- [x] **3. Order statistics — `median` and `percentile`.** No rule can ask for
      a middle or a tail. Unlike every streaming fold, these need the whole
      group ordered before an answer exists, so they join `count_distinct` in
      the bounded family: retain up to a cap, and past it derive nothing
      rather than a number computed from a truncated sample.
      `percentile(atoms.., Value, P, Out)` takes the fraction as a literal;
      `median` is the `P = 0.5` shorthand rather than a separate mechanism.

- [x] **4. Getting the values out of a group.** Every aggregate reduces a group
      to one number. Nothing can answer "which ones", only "how many".
      Round 2 recorded this as needing a list term, and that was wrong:
      `DerivedFact` carries `src_id`/`dst_id` as **strings**, so a list-valued
      argument would be flattened to a string at the boundary anyway.
      `group_concat(atoms.., Value, Separator, Out)` closes the capability
      without a list term, and without the unification, ordering and
      stored-format questions a list term would open. Bounded, like the order
      statistics.

- [x] **5. Final answers for the term kinds round 2 skipped.** Boolean and
      null were listed as "no requirement" — which is a reason to wait, not a
      decision. **Both are now declined for reasons stronger than absence of
      demand, each pinned by a test:**

      - **Boolean.** It would give truth a SECOND spelling. `flag(X, true)`
        and `flag(X, "true")` would be different terms that do not unify, and
        every flag already stored is a string — so a rule written with the
        literal would silently stop matching its own data. The idiomatic form
        needs no value at all: presence is truth.
      - **Null.** Datalog's answer to "no value" is the absence of a fact, and
        negation now says it directly. A null *value* would need three-valued
        comparison semantics the engine deliberately does not have, and would
        be a third kind of no-value beside `Unbound` and `Undefined`, which
        the filter evaluator distinguishes on purpose.
      - **List.** Closed by item 4 rather than declined. `group_concat`
        delivers the capability; a list term would be flattened to a string at
        the `DerivedFact` boundary anyway.

## Not in scope, with reasons rather than deferrals

- **Integer numerals.** Measured in round 2 and declined; the measurement is
  two tests that break if the premise stops holding. See `t_cb1b3744`.
- **Regular expressions.** A regex engine in a tenant-editable rule is a ReDoS
  surface. `str_starts_with` / `str_ends_with` / `str_contains` plus the
  boolean tree cover the cases that motivated it. This is a decision, not a
  gap.

## Constraints

Unchanged. Streaming, with the bounded family the stated and now-enumerated
exception; additive against the pre-negation digest; no migration; fail loud;
write-time validation.
