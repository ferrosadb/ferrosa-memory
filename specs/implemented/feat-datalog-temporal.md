---
title: Time in the rule language
executive_summary: >
  The grammar has no time, so "anything from last week" and "anything older
  than 90 days" cannot be written at all. This adds a time value, a clock, a
  bridge from the ISO-8601 strings timestamps are actually stored as, and
  duration arithmetic — with the clock read once per evaluation, and rules
  that read it never cached.
status: implemented
priority: P60
component: ferrosa-memory-core/src/datalog.rs
last_updated: 2026-08-28
---

# Time in the rule language

## Why

Found while grilling the natural-language rules editor. The motivating example
was *"all versions of the presentation from last week"*, and it turns out that
sentence has no representation: `Term` is Var / Const(Uuid) / ConstStr /
ConstFloat / ConstNull. There is no time, no clock, and no date arithmetic.

This is not only an NL-editor problem. Retention (*"everything older than N
except pinned"*) was named in the negation spec as a shape the language would
need, and the rules plane's D3 stores `expires_at` on a rule while giving rules
no way to reason about time themselves.

## The design

- **`Term::ConstTime`** — an instant, `DateTime<Utc>`. Orderable, so `min` and
  `max` work on it through the machinery that already orders terms by kind.
- **`now()`** — the clock.
- **`date(S)`** — parses an ISO-8601 string into a time. This is the load-bearing
  one: timestamps in this corpus are **stored as strings**, so without it a time
  value would have nothing to compare against.
- **`days(N)` / `hours(N)` / `minutes(N)` / `weeks(N)`** — durations, as
  milliseconds. Deliberately numbers rather than a second new type: a duration
  is a quantity, and making it one keeps all the existing arithmetic working on
  it for free.
- **Time ± Number is a Time. Time − Time is a Number.** The two rules that make
  `now() - days(7)` and "how long ago" both expressible.

```datalog
recent(X) :- doc(X, C), date(C) > now() - days(7).
stale(X)  :- doc(X, C), now() - date(C) > days(90).
```

## Checklist

- [x] **1. The value and the bridge.** `Term::ConstTime`, and `date(S)` to reach
      it from the strings timestamps are stored as. An unparseable string is
      `Undefined` — an error, not a null — because it means the data is not what
      the rule thought it was. `date(null)` is null.

- [x] **2. The clock, read once.** `now()` is stamped into the rules **once per
      evaluation**, before the fixpoint runs. Reading it per row would let two
      rows in one run disagree about what "now" is, and a rule near a boundary
      would then include one and exclude the other for no reason a person could
      see. `evaluate_at` takes the instant so this is testable; `evaluate` calls
      it with `Utc::now()`.

- [x] **3. Durations and arithmetic.** The four helpers, plus Time ± Number and
      Time − Time.

- [x] **4. A rule that reads the clock is never cached.** This is the same
      non-monotonicity as negation, arriving by a different door: a fact derived
      because something was "in the last 7 days" stops being true *without any
      base fact changing*. `DerivedFact::is_cacheable()` must refuse it, by the
      same provenance mechanism absence already uses.

- [x] **5. The completeness guard fires and is re-run.** Adding `Term::ConstTime`
      must break it. That is the guard working.

## Not in scope

- **Time zones.** Everything is UTC. A rule that means "last week" in a person's
  local time is a display concern until someone has a requirement for it.
- **Calendar arithmetic.** `days(30)` is thirty times twenty-four hours, not "a
  month". Months and years are not fixed durations, and pretending otherwise is
  the kind of quiet wrongness this codebase keeps refusing.


## Implementation Notes

### The clock is stamped, not called

`now()` never reaches the evaluator. `evaluate_at` replaces it with a
`FilterExpr::LitTime` in every rule **before** the fixpoint runs, so the whole
evaluation agrees about when it is. `evaluate` calls `evaluate_at` with
`Utc::now()`, which keeps the existing signature and makes the behaviour
testable at a fixed instant.

A `LitTime` cannot be written by hand — it only arises from stamping. That is
what lets **one** predicate serve two jobs: `rule_reads_the_clock` matches the
unstamped `now()` for the stamper, and the stamped `LitTime` for the cache
guard, with no flag threaded between them.

### Clock-dependence is the same hazard as negation

A fact derived because something was "in the last 7 days" stops being true
**with no base fact changing at all**. That is exactly the non-monotonicity
negation introduced, arriving by a different door, so it gets the same
treatment: a `PROVENANCE_KIND_CLOCK` step beside `PROVENANCE_KIND_ABSENCE`, and
`is_cacheable()` refuses either.

### `date()` is the load-bearing function

Timestamps in this corpus are stored as **strings**. Without a bridge, a time
value would have had nothing to compare against and the whole feature would
have been literals comparing to literals. `date()` reads RFC-3339 and bare
`YYYY-MM-DD`, because a corpus holds both — a machine stamp and a person's day.

An unparseable string is `Undefined`, not null: the data is not what the rule
thought it was, which is a mistake rather than an absence, so it poisons rather
than propagating by Kleene. `date(null)` is null.

### Durations are numbers

`days(7)` is milliseconds, not a second new type. A duration is a quantity, and
making it one means every existing arithmetic path works on it for free —
`days(7) + hours(3)` needed no code.

### Found on the way

- **Zero-argument calls did not parse.** `call` used `separated_list1`, so
  `now()` failed. Now `separated_list0`; a wrong argument count is still
  refused, by the arity check rather than by the parser.
- **`apply_cmp` had the ordering logic written out three times**, once per kind.
  Extracted to `ordered(op, ord)` while adding the fourth, so a future kind
  cannot get a subtly different `Le`.

### Verification

1543 workspace lib tests, 16 in the rule contract suite, plus governance,
tool-catalog and additivity. clippy `-D warnings` and fmt clean. The
pre-negation digest still matches byte-for-byte. **Live-cluster tests were not
run.**
