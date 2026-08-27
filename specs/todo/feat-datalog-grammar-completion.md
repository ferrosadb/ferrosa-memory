---
title: Complete the datalog grammar
executive_summary: >
  Negation and value-folding aggregates closed the two largest holes in the
  rule language. What is left is smaller and enumerable: the arithmetic is
  missing modulo, a filter cannot say "or", nothing can ask about the shape of
  a string, min/max refuse anything that is not a number, and a head argument
  cannot hold an expression. Each is a thing a tenant currently cannot write,
  and each currently costs a bespoke enumeration in Rust.
status: todo
priority: P45
component: ferrosa-memory-core/src/datalog.rs
last_updated: 2026-08-26
---

# Complete the datalog grammar

Follows `feat-datalog-stratified-negation.md`. The grammar as it stands:

```text
rule    ::= head ":-" body "."
body    ::= (atom | "not" atom | filter | aggregate) ("," ...)*
filter  ::= expr cmp_op expr
cmp_op  ::= "==" | "!=" | "<=" | ">=" | "=" | "<" | ">"
expr    ::= term (("+" | "-") term)*
term    ::= factor (("*" | "/") factor)*
factor  ::= number | string | identifier | "(" expr ")" | "-" factor
agg     ::= ("count" "(" atom+ "," Out ")") | (("sum"|"min"|"max"|"avg") "(" atom+ "," Value "," Out ")")
head    ::= predicate "(" term* ")"
```

Every gap below is a rule a tenant cannot write today.

## Checklist

- [x] **1. Modulo.** `ArithOp` is Add/Sub/Mul/Div. `%` is missing, so no rule can
      bucket, sample every Nth, or test parity. Division by zero already fails
      loud (`eval_expr` warns and yields no value); modulo by zero must match it
      rather than produce NaN. *Correction found while implementing: division by
      zero did NOT fail loud at the filter — `eval_expr` returned `None` for both
      an unbound variable and a zero divisor, and `check_one_filter` passed on
      `None`, so `V / 0 == 0` derived a fact. Fixed as part of this item.*

- [ ] **2. String predicates.** Nothing can ask about the shape of a string.
      "Everything except items whose name starts with `tmp_`" is unwritable,
      which is the same class of requirement that motivated negation.
      `starts_with`, `ends_with`, `contains`.

- [ ] **3. Disjunction in a filter.** `check_filters` is `.all()`, so the body's
      filters are implicitly AND-ed and there is no way to say "or". Today that
      costs a second rule per alternative, which multiplies with each additional
      disjunct. Needs `||`, `&&` and `!` with parentheses and real precedence.

- [ ] **4. min/max over any ordered term.** The streaming `Fold` holds `f64`.
      `sum` and `avg` genuinely require that; `min` and `max` need only a total
      order. Taking the earliest timestamp or the first name alphabetically is
      unwritable. `Term` already orders strings, floats and uuids. (t_b906d58c)

- [ ] **5. An expression in a head argument.** `Atom.args` is `Vec<Term>`, so a
      head can only repeat a bound variable or a constant. `next(X, N + 1) :-
      rank(X, N).` is unwritable and every derived arithmetic value must be
      precomputed by whatever wrote the base facts. The design question is
      **termination**: an arithmetic head feeding its own body is an infinite
      fixpoint, bounded today only by the `max_facts` budget — that is bounding
      by truncation, which is the silent-wrongness shape this codebase rejects
      elsewhere. Decide before implementing. (t_b97086d0)

## Constraints

- **Streaming.** Nothing proportional to a relation may be materialised. The
  aggregate fold is already a visitor over the conjunction backtracker; keep it.
- **Additive.** Every rule set that uses none of these must evaluate exactly as
  it does now. `tests/fixtures/datalog_pre_negation_digest.txt` is the guard: it
  was recorded from the commit before any of this work and must keep matching
  byte-for-byte.
- **No migration.** `RuleEntry.rule_body` is TEXT holding rule source, so new
  syntax round-trips through the existing column. Any new serde field is
  `#[serde(default)]` regardless, for in-flight JSON.
- **Fail loud, never fake.** A type error or an undefined result must stop the
  rule firing and say so. It must never produce a value the caller cannot
  distinguish from a real one.
- **Write-time validation.** `manage_rules put` validates with the same
  `parse_rule` the loader uses, so every rejection below must land at write
  time, naming what is wrong.

## Not in scope

- Well-founded semantics, DRed, incremental view maintenance — see the negation
  spec.
- Regular expressions in string predicates. `starts_with`/`ends_with`/`contains`
  are bounded and cheap; a regex engine in a tenant-editable rule is an ReDoS
  surface and needs its own decision.
