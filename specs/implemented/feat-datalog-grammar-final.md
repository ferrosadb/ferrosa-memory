---
title: Close the datalog grammar
executive_summary: >
  The completeness pass in round 2 named three remaining gaps — exponentiation,
  order statistics, and getting the values out of a group rather than only a
  statistic — plus two term kinds it skipped. This closes the capability gaps
  and gives the term kinds a final answer, so the question does not need
  asking again.
status: implemented
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


## Implementation Notes

Five items, one commit each. Two capability gaps found nothing left behind them
after; the term kinds are decided rather than deferred.

### The grammar, complete

```text
rule      ::= head ":-" body "."
body      ::= alternative (";" alternative)*
alternative ::= element ("," element)*
element   ::= atom | "not" atom | filter | aggregate | binding | "(" body ")"
binding   ::= Var ":=" expr
filter    ::= bool_or
bool_or   ::= bool_and ("||" bool_and)*
bool_and  ::= bool_not ("&&" bool_not)*
bool_not  ::= "!"* bool_primary
bool_primary ::= "(" filter ")" | str_pred | membership | expr cmp_op expr
str_pred  ::= ("str_starts_with"|"str_ends_with"|"str_contains") "(" expr "," expr ")"
membership::= expr "in" "[" expr ("," expr)* "]"
expr      ::= term (("+" | "-") term)*
term      ::= power (("*" | "/" | "%") power)*
power     ::= factor ("**" power)?
factor    ::= number | string | call | identifier | "(" expr ")" | "-" factor
call      ::= ("abs"|"floor"|"ceil"|"round"|"len"|"lower"|"upper"|"concat") "(" expr,.. ")"
aggregate ::= "count" "(" atom+ "," Out ")"
            | ("sum"|"min"|"max"|"avg"|"stddev"|"count_distinct"|"median")
              "(" atom+ "," Value "," Out ")"
            | ("percentile"|"group_concat") "(" atom+ "," Value "," Literal "," Out ")"
head      ::= predicate "(" (term | expr)* ")"
```

### The completeness claim, in a form that breaks

Three specs in a row claimed completeness by reading the types and writing
prose. Twice that was wrong — round 1 missed five gaps, round 2 missed one of
its own (atom disjunction, closed for filters and written up as closed
generally).

So the claim is now a test. `the_grammar_surface_is_the_one_the_spec_signed_off`
pins the enumerated surface: adding a variant to `AggregateKind`, `StrOp`,
`Func`, `ArithOp`, `CmpOp` or `Term` **fails it**, forcing the next author to
re-run the pass and record the answer rather than inheriting a stale
"complete". Verified by adding a variant and watching it fail.

It also pins that exactly three folds retain their group, because *which folds
cannot stream* is a design decision and not an implementation detail.

### Streaming, finally enumerated

| Fold | Memory |
|---|---|
| count, sum, min, max, avg, **stddev** | one accumulator |
| count_distinct | the distinct set, capped at 10,000 |
| median, percentile, group_concat | the whole group, capped at 10,000 |

`stddev` is in the first row because Welford computes variance in one pass. It
was worth separating from median for that reason alone — grouping them by name
would have hidden that one streams and the other cannot.

### A pre-existing bug

`split_top_level` tracked parentheses and brackets but not string literals, so
`p(X, "a,b")` split in the middle of its own argument. Nothing had put a comma
inside a literal until `group_concat`'s separator made it unavoidable. Fixed,
with a regression test for the plain case and one for an escaped quote.

### Declined, with reasons rather than deferrals

- **Boolean term** — would give truth a second spelling that does not unify
  with the string form already stored.
- **Null term** — needs three-valued comparison semantics the engine
  deliberately lacks, and would be a third no-value beside `Unbound` and
  `Undefined`.
- **List term** — not declined but closed: `group_concat` delivers the
  capability, and a list would flatten to a string at the `DerivedFact`
  boundary anyway.
- **Integer numerals** — measured in round 2; nothing can reach the precision
  limit. `t_cb1b3744`.
- **Regular expressions** — ReDoS surface in a tenant-editable rule.

### Verification

1454 workspace lib tests, 16 in the rule contract suite, plus governance,
tool-catalog and additivity. `clippy --workspace --all-targets -D warnings` and
`fmt --check` clean. The pre-negation fixture digest — recorded before any of
this work began, three PRs ago — still matches byte-for-byte. **Live-cluster
tests were not run.**
