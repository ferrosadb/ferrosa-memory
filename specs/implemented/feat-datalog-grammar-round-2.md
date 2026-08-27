---
title: The rest of the datalog grammar
executive_summary: >
  The previous grammar spec claimed its list of remaining gaps was exhaustive.
  It was not. This one enumerates what a re-read of the types actually found,
  and does not claim to be the last word — it says which gaps are closed, which
  are deliberately left, and what it would take to know the list is complete.
status: implemented
priority: P45
component: ferrosa-memory-core/src/datalog.rs
last_updated: 2026-08-27
---

# The rest of the datalog grammar

Follows `feat-datalog-grammar-completion.md`, which closed modulo, string-shape
predicates, filter-level disjunction, ordered extremes and computed heads.

**That spec overclaimed.** It said the remaining gaps were "small enough to list
exhaustively, so list it". The list was not exhaustive. Re-reading `Term`,
`CmpOp`, `ArithOp`, `FilterExpr` and `AggregateKind` against what a policy rule
actually needs turned up the items below. Recording the miss because the same
overconfidence is the thing most likely to leave this one incomplete too.

## Verified state

```rust
Term          = Var | Const(Uuid) | ConstStr | ConstFloat        // no int, bool, null, list
CmpOp         = Eq | Ne | Lt | Le | Gt | Ge                       // no `in`
ArithOp       = Add | Sub | Mul | Div | Rem                       // no pow
FilterExpr    = Var | LitNum | LitStr | BinOp | Neg               // no Call
AggregateKind = Count | Sum | Min | Max | Avg                     // no distinct
DatalogRule.body: Vec<Atom>                                       // pure conjunction
```

`=` parses to `CmpOp::Eq` — a comparison, never a binding. `count` yields
`Term::ConstFloat`.

## Checklist

- [x] **1. Set membership.** `X in ["a", "b", "c"]` — today it is a hand-written
      chain of `||`. Pure sugar over `Any(Eq..)`, which is why it is first: it
      is the smallest thing that exercises the whole loop.

- [x] **2. Function calls in an expression.** `FilterExpr` has no `Call`, so
      there is no `abs`, `floor`, `ceil`, `round`, `len`, `lower`, `upper` or
      `concat`. Every one of those is a bespoke enumeration in Rust today.
      A closed whitelist, not an open extension point: an unknown name is a
      parse error naming it, and a wrong arity is too. Wrong type is Undefined,
      matching the rest of the evaluator.

- [x] **3. Bind a computed value in the body.** A head can compute now; a body
      still cannot, so there is no way to name an intermediate. `X = W + 1`
      does *not* bind — it parses to `CmpOp::Eq` and, with `X` unbound, passes
      as "cannot decide yet". Needs a distinct operator (`:=`) because
      redefining `=` would silently change the meaning of stored rules. RHS
      variables must already be bound; LHS must be fresh.

- [x] **4. Atom-level disjunction.** `body` is a pure conjunction and the
      splitter does not know `;`, so `q(X) :- p(X) ; r(X).` still needs two
      rules. The previous spec closed disjunction *for filters* and read as if
      it had closed disjunction. The justification used there — each
      alternative costs a whole extra rule, and the cost multiplies with each
      disjunct — applies identically here.
      Expand to disjunctive normal form at load: no runtime cost, and the
      evaluator never learns a new shape. `parse_rule` keeps its signature and
      a new `parse_rules` returns the expansion, so no caller changes meaning
      by accident.

- [x] **5. `count_distinct`.** `count(p(X, Y), N)` counts *unifications*, not
      distinct values. **This is the one item that cannot be fully streaming**,
      and the spec should say so rather than imply otherwise: distinctness
      needs a set, and the set is proportional to the number of distinct
      values. Bound it explicitly with a cap and fail loud when exceeded —
      deriving a truncated count would be a number the caller cannot tell from
      a real one.

- [~] **6. Integer numerals — MEASURED, then DEFERRED.** Every number is an
      `f64`. `count` returns `3.0`, and `%` loses precision above 2^53.
      The cost is high: a `Term::ConstInt` variant touches parsing (when is `2`
      an int?), every arithmetic and comparison path, the stored format, and
      the return type of `count`.
      **Measured, and the harm is not there.** Two tests in
      `numeric_harm_measurement` keep the finding checkable:

      1. A whole number already renders without a decimal point —
         `term_to_string(ConstFloat(3.0))` is `"3"`, not `"3.0"`. The cosmetic
         complaint was against something that does not happen.
      2. f64 holds integers exactly to 2^53, which is nine quadrillion. Every
         numeric fact the loader produces is a score in [0,1] — `confidence`
         and `warmth` are the only two — and everything else is a fold over
         those. Nothing in this engine can approach the limit. Uuids are their
         own term and timestamps are stored as strings, so neither is a route
         to a large integer either.

      Deferred, not done. A `Term::ConstInt` would touch parsing, every
      arithmetic and comparison path, the stored format and the return type of
      `count`, for no measured benefit. Tracked so the decision is findable if
      a numeric source is ever added that changes the measurement.

## Deliberately not in scope

- **Regular expressions** in string predicates. Unchanged from the previous
  spec: a regex engine in a tenant-editable rule is a ReDoS surface and needs
  its own decision.
- **Boolean, null and list terms.** A list term is the prerequisite for a
  `collect` / `group_concat` aggregate; none of the three has a requirement
  behind it yet. Listed so the next reader knows they were seen and skipped,
  not missed.
- **Conjunctive heads.** No requirement.

## Constraints

Unchanged from the previous spec, and they are what the items above are shaped
around:

- **Streaming**, with item 5 the stated exception.
- **Additive** — `tests/fixtures/datalog_pre_negation_digest.txt`, recorded
  before any of this work, must keep matching byte-for-byte.
- **No migration** — `rule_body` is TEXT holding rule source.
- **Fail loud** — an undefined result stops the rule firing and says so.
- **Write-time validation** — every rejection lands at `manage_rules put`,
  naming what is wrong.

## On calling this complete

The previous spec's mistake was asserting exhaustiveness from memory. The check
that would actually earn the claim is a pass over `Term`, `CmpOp`, `ArithOp`,
`FilterExpr`, `AggregateKind` and `DatalogRule` asking of each: *what can a
policy rule need that this cannot represent?* That pass produced this list. It
should be repeated when this list is closed, and the answer written down —
including "nothing found", which is a result rather than a guarantee.


## Implementation Notes

Five items closed, one measured and declined, one commit each.

### The completeness pass

The spec said the check that would earn the word "complete" is a pass over each
type asking *what can a policy rule need that this cannot represent*. Run after
the five items, the answer is below — including where it is "nothing found",
which is a result and not a guarantee.

| Type | Can it still fail a rule? |
|---|---|
| `Term` | No boolean, null or list. **No requirement behind any of them** — a list is the prerequisite for `collect`/`group_concat`, which nobody has asked for. |
| `CmpOp` | Nothing found. Six comparisons plus `in`. |
| `ArithOp` | No exponentiation. **No requirement**; `V * V` covers the only case seen. |
| `FilterExpr` | Nothing found. Var, literals, arithmetic, negation, calls. |
| `AggregateKind` | No median or stddev. **Median cannot stream** — it needs the whole group, unlike `count_distinct` which needs only the distinct set — so it would be a larger decision than this spec's constraints allow. |
| `BuiltinFilter` | Nothing found. Comparison, string shape, membership, and a full boolean tree. |
| `DatalogRule` | Nothing found. Conjunction, disjunction, negation, aggregates, filters, bindings, computed heads. |

So: not "complete", but the remaining gaps are now **named, and each is either
scoped out with a reason or waiting on a requirement**. That is a different
state from the previous spec, which asserted exhaustiveness and was wrong.

### Two bugs found by testing rather than reading

**`split_top_level` did not nest brackets.** It tracked parenthesis depth only,
so the body splitter cut `["red", "orange"]` in half at its comma and handed the
filter parser `C in ["red"`. Nothing before item 1 used brackets, which is why
it had never mattered.

**A doc comment was stolen from `parse_rule`.** Inserting the disjunction code
above `pub fn parse_rule` put it *between* that function and its own doc
comment, silently reattaching the documentation to a constant. Caught by
clippy's `doc_lazy_continuation`, not by any test — worth noting because it is
the class of thing tests do not catch.

### Decisions

- **`:=` binds, `=` still compares.** Redefining `=` would have changed the
  meaning of rules already stored. A new symbol is cheaper than a silent change
  of meaning.
- **Function calls are a closed whitelist.** An open set would mean an unknown
  name had to be read as something, and the only alternative reading is a
  variable — unbound, therefore matching every row, so a typo would look like a
  rule that works.
- **Disjunction expands at load.** One rule per alternative, capped at 64, so
  the evaluator never learns a new body shape and a runaway is an error naming
  the count rather than hundreds of rules to evaluate.
- **`parse_rule` refuses a disjunctive body** rather than returning the first
  alternative and dropping the rest.
- **`count_distinct` is bounded and says so.** It is the only fold that cannot
  stream; ten thousand distinct values, then it derives nothing rather than a
  truncated count.

### Streaming

Every fold but `count_distinct` still holds one accumulator over the
conjunction backtracker. Disjunction and set membership cost nothing at
evaluation time — both are expansions done once at parse. Bindings and function
calls are pure per-candidate work with no accumulation.

### Verification

1432 workspace lib tests, 16 in the rule contract suite, plus governance,
tool-catalog and additivity. `clippy --workspace --all-targets -D warnings` and
`fmt --check` clean. The pre-negation fixture digest — recorded from before any
of this work began — still matches byte-for-byte. **Live-cluster tests were not
run.**
