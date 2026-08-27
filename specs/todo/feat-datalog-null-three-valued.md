---
title: A null value, with Kleene semantics that keep errors loud
executive_summary: >
  The grammar has no way to represent a known-absent value, and the reason
  given for declining one was wrong. The engine already has a three-valued
  filter lattice; what it lacks is Kleene propagation. This adds null and
  gives it Kleene, while keeping the poisoning propagation that makes an
  arithmetic error refuse a rule rather than be masked by a true sibling.
status: todo
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

- [ ] **1. The value.** `Term::ConstNull`, written `null`. Arithmetic and
      function calls propagate it — `null + 1` is null, `abs(null)` is null —
      so a null reaches the comparison rather than being mistaken for a type
      error on the way.

- [ ] **2. Kleene at the comparison.** Comparing anything to null is `Unknown`,
      never true and never false. `Unknown` propagates by Kleene through
      `&&`, `||` and `!`, while `Undefined` keeps poisoning. The two must not
      collapse into each other.

- [ ] **3. `is_null`.** Because `V == null` is `Unknown` and therefore never
      fires, there has to be a way to ask. `!is_null(V)` covers the negative;
      `is_null` itself answers true or false and never `Unknown`.

- [ ] **4. Aggregates skip nulls.** SQL's rule, and the least surprising:
      `sum`, `avg`, `min`, `max` and the order statistics ignore null values,
      and `avg` divides by the count of non-null values. `count` counts rows,
      so it counts them. A group of nothing but nulls is an empty group.

- [ ] **5. The completeness guard fires, and is updated deliberately.**
      Adding `Term::ConstNull` and `Verdict::Unknown` must break
      `the_grammar_surface_is_the_one_the_spec_signed_off`. That is the guard
      working. Updating it is the act of re-running the completeness pass.

## Not in scope

- **Well-founded semantics for unstratifiable programs.** `p :- not p.` is
  still rejected by `stratify`. This spec gives a third value to *filters and
  values*, not to *facts*, and the two are separate projects — see the note in
  the negation spec and `t_c16aac9a`.
