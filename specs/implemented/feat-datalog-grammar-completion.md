---
title: Complete the datalog grammar
executive_summary: >
  Negation and value-folding aggregates closed the two largest holes in the
  rule language. What is left is smaller and enumerable: the arithmetic is
  missing modulo, a filter cannot say "or", nothing can ask about the shape of
  a string, min/max refuse anything that is not a number, and a head argument
  cannot hold an expression. Each is a thing a tenant currently cannot write,
  and each currently costs a bespoke enumeration in Rust.
status: implemented
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

- [x] **2. String predicates.** Nothing can ask about the shape of a string.
      "Everything except items whose name starts with `tmp_`" is unwritable,
      which is the same class of requirement that motivated negation.
      `starts_with`, `ends_with`, `contains`. *Named `str_starts_with`,
      `str_ends_with`, `str_contains` in the end: `contains` is already an edge
      type here, so `contains(X, Y)` is a legitimate stored relation and taking
      the bare name would have silently changed rules already written. The
      `str_` prefix is reserved so a typo is rejected rather than read as a
      relation.*

- [x] **3. Disjunction in a filter.** `check_filters` is `.all()`, so the body's
      filters are implicitly AND-ed and there is no way to say "or". Today that
      costs a second rule per alternative, which multiplies with each additional
      disjunct. Needs `||`, `&&` and `!` with parentheses and real precedence.

- [x] **4. min/max over any ordered term.** The streaming `Fold` holds `f64`.
      `sum` and `avg` genuinely require that; `min` and `max` need only a total
      order. Taking the earliest timestamp or the first name alphabetically is
      unwritable. `Term` already orders strings, floats and uuids. (t_b906d58c)

- [x] **5. An expression in a head argument.** `Atom.args` is `Vec<Term>`, so a
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


## Implementation Notes

All five items done, one commit each, TDD throughout. The grammar now reads:

```text
rule     ::= head ":-" body "."
body     ::= (atom | "not" atom | filter | aggregate) ("," ...)*
filter   ::= bool_or
bool_or  ::= bool_and ("||" bool_and)*
bool_and ::= bool_not ("&&" bool_not)*
bool_not ::= "!"* bool_primary
bool_primary ::= "(" filter ")" | str_pred | expr cmp_op expr
str_pred ::= ("str_starts_with"|"str_ends_with"|"str_contains") "(" expr "," expr ")"
expr     ::= term (("+" | "-") term)*
term     ::= factor (("*" | "/" | "%") factor)*
agg      ::= ("count" "(" atom+ "," Out ")")
           | (("sum"|"min"|"max"|"avg") "(" atom+ "," Value "," Out ")")
head     ::= predicate "(" (term | expr)* ")"
```

### What the checklist got wrong

**Division by zero did not fail loud.** `eval_expr` returned `None` for both an
unbound variable and a zero divisor, and `check_one_filter` passed the filter on
`None`, so `V / 0 == 0` derived a fact. Found while implementing item 1 and
fixed there. The evaluator now answers with three states, and the distinction
earns its keep again in item 3, where `!` over an undefined comparison must not
become a pass.

**Arithmetic in a body atom was silently accepted** as a variable whose name
happened to be `"W + 1"` — unbound, so it matched every row and the rule fired
on everything. Found by an item-5 test that asserted the wrong thing. Now a
parse error.

### Decisions worth keeping

- **Reserved `str_` prefix, not the bare names.** `contains` is already an edge
  type here, so `contains(X, Y)` is a legitimate stored relation; taking the
  name would have quietly changed rules already written. The prefix is reserved
  so a typo is rejected rather than read as a relation matching nothing.
- **An undefined branch refuses the whole filter**, even under a true sibling.
  A rule containing a mistake should say so rather than fire because another
  branch happened to hold.
- **A computed head that can reach its own body is rejected at load**
  (`RecursionThroughHeadExpression`), on the same SCC machinery negation and
  aggregates use. The fact budget would have bounded it by truncation, which is
  the silently-wrong shape.
- **A group mixing value kinds derives nothing.** `Term`'s derived ordering
  would happily order a string against a number by variant position, answering
  a question nobody asked.
- **`+`/`-` need whitespace to count as operators.** Uuids and dates are full of
  hyphens; `*`, `/` and `%` are unambiguous on sight.

### Streaming

Unchanged and still true: the aggregate fold is a visitor over the conjunction
backtracker, holding one accumulator. Generalising `min`/`max` from `f64` to
`Term` did not change that. Two 20,000-row tests, one numeric and one over
strings.

### Verification

1391 workspace lib tests, 15 in the rule contract suite, plus the governance,
tool-catalog and additivity suites. `clippy --workspace --all-targets -D
warnings` and `fmt --check` clean. The pre-negation fixture digest — recorded
from the commit before any of this work — still matches byte-for-byte after
every item. **Live-cluster tests were not run.**

### Not done

Regular expressions in string predicates, as scoped out above: a regex engine in
a tenant-editable rule is a ReDoS surface and needs its own decision.
