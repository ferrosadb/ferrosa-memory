---
title: A null value, with Kleene semantics that keep errors loud
executive_summary: >
  The grammar has no way to represent a known-absent value, and the reason
  given for declining one was wrong. The engine already has a three-valued
  filter lattice; what it lacks is Kleene propagation. This adds null and
  gives it Kleene, while keeping the poisoning propagation that makes an
  arithmetic error refuse a rule rather than be masked by a true sibling.
status: implemented
priority: P50
component: ferrosa-memory-core/src/datalog.rs
last_updated: 2026-08-27
---

# A null value, with Kleene semantics that keep errors loud

## Correcting the reason it was declined

`feat-datalog-grammar-final.md` declined a null term because it "would need
three-valued comparison semantics the engine deliberately does not have, and
would be a third no-value beside `Unbound` and `Undefined`".

**Both halves of that were wrong.** The engine already has a three-valued
lattice — `Eval { Value, Unbound, Undefined }` and `Verdict { True, False,
Undefined, Unbound }`. And a null value does not add a state at the verdict
level: comparing to null yields a no-answer, which is a verdict the evaluator
already models.

The real obstacle was never the state count. It is that the existing
propagation is **poisoning**, not Kleene: any `Undefined` refuses the whole
filter tree. That was a deliberate safety choice — an arithmetic error should
not be masked by a sibling branch that happens to hold — and under it a null
would be contagious and useless.

## The design

Split the two reasons a filter has no answer, because they deserve opposite
propagation:

| Verdict | Arises from | Propagation |
|---|---|---|
| `Unknown` | a null value | **Kleene** — `true \|\| unknown = true`, `false && unknown = false` |
| `Undefined` | an error: `V / 0`, `abs("x")`, a non-finite power | **poisons**, unchanged |

Errors stay loud. Null behaves as it does in every other three-valued language.

A filter that evaluates to `Unknown` does **not** pass, matching SQL: `WHERE x
= NULL` returns no rows. `Unbound` keeps its legacy pass.

## Checklist

- [x] **1. The value.** `Term::ConstNull`, written `null`. Arithmetic and
      function calls propagate it — `null + 1` is null, `abs(null)` is null —
      so a null reaches the comparison rather than being mistaken for a type
      error on the way.

- [x] **2. Kleene at the comparison.** Comparing anything to null is `Unknown`,
      never true and never false. `Unknown` propagates by Kleene through
      `&&`, `||` and `!`, while `Undefined` keeps poisoning. The two must not
      collapse into each other.

- [x] **3. `is_null`.** Because `V == null` is `Unknown` and therefore never
      fires, there has to be a way to ask. `!is_null(V)` covers the negative;
      `is_null` itself answers true or false and never `Unknown`.

- [x] **4. Aggregates skip nulls.** SQL's rule, and the least surprising:
      `sum`, `avg`, `min`, `max` and the order statistics ignore null values,
      and `avg` divides by the count of non-null values. `count` counts rows,
      so it counts them. A group of nothing but nulls is an empty group.

- [x] **5. The completeness guard fires, and is updated deliberately.**
      Adding `Term::ConstNull` and `Verdict::Unknown` must break
      `the_grammar_surface_is_the_one_the_spec_signed_off`. That is the guard
      working. Updating it is the act of re-running the completeness pass.

## Not in scope

- **Well-founded semantics for unstratifiable programs.** `p :- not p.` is
  still rejected by `stratify`. This spec gives a third value to *filters and
  values*, not to *facts*, and the two are separate projects — see the note in
  the negation spec and `t_c16aac9a`.


## Implementation Notes

### The truth tables

`Verdict` is five states, because "no answer" has three causes and they want
three propagations:

| | `Undefined` (error) | `Unknown` (null) | `Unbound` |
|---|---|---|---|
| `a && X` | `Undefined` always | `false && unknown = false` | passes |
| `a \|\| X` | `Undefined` always | `true \|\| unknown = true` | passes |
| `!X` | `Undefined` | `Unknown` | `Unbound` |
| passes? | no | **no** | yes |

`Undefined` is tested first in both connectives — that is the poisoning rule,
unchanged. Below it the tables are Kleene's.

`Unknown` not passing is what makes this match SQL: `WHERE x = NULL` returns no
rows.

### The discriminating tests

Two tests exist specifically to prove the two propagations have not collapsed
into each other, since that is the failure this design is shaped to prevent:

- `true_or_unknown_is_true` — a null beside a true branch no longer refuses.
- `an_error_still_poisons_even_beside_a_true_branch` — the identical shape with
  a division by zero instead still refuses.

If either propagation were applied to both causes, one of these fails.

`false_and_unknown_is_false_which_negates_to_true` is the only way to observe
`False` rather than `Unknown` from a conjunction, since neither passes on its
own — it reads the difference through `!`.

### Null is a value, not an error

`null + 1` is null and `abs(null)` is null, as in SQL. This matters more than
it looks: if either were a type error it would produce `Undefined`, which
poisons, and the whole design would collapse back into refusing. A computed
head or a `:=` binding that evaluates to null therefore fires **carrying** the
null, rather than not firing.

### Aggregates

Value folds ignore nulls — `avg` divides by the count of non-null values,
`min` never returns one — while `count` still counts the row, because it looks
at no value at all and is therefore `count(*)` rather than `count(col)`. A
group of nothing but nulls is an empty group, so `sum` is 0 and `min` does not
fire.

### The guard worked

Adding `Term::ConstNull` **failed**
`the_grammar_surface_is_the_one_the_spec_signed_off`, exactly as intended. That
forced the completeness pass to be re-run rather than inherited — and that pass
is what found the previous spec's reason for declining null to be wrong on both
halves. The guard now also pins the five verdicts.

### Verification

1468 workspace lib tests, 16 in the rule contract suite, plus governance,
tool-catalog and additivity. `clippy --workspace --all-targets -D warnings` and
`fmt --check` clean. The pre-negation digest still matches byte-for-byte.
**Live-cluster tests were not run.**

### Still not in scope

Well-founded semantics for unstratifiable programs. This gives a third value to
**filters and values**; `p :- not p.` is still rejected by `stratify`, and
giving a third value to **facts** is a separate project — `t_c16aac9a`.
