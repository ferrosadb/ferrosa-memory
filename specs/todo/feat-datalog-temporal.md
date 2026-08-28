---
title: Time in the rule language
executive_summary: >
  The grammar has no time, so "anything from last week" and "anything older
  than 90 days" cannot be written at all. This adds a time value, a clock, a
  bridge from the ISO-8601 strings timestamps are actually stored as, and
  duration arithmetic — with the clock read once per evaluation, and rules
  that read it never cached.
status: todo
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

- [ ] **1. The value and the bridge.** `Term::ConstTime`, and `date(S)` to reach
      it from the strings timestamps are stored as. An unparseable string is
      `Undefined` — an error, not a null — because it means the data is not what
      the rule thought it was. `date(null)` is null.

- [ ] **2. The clock, read once.** `now()` is stamped into the rules **once per
      evaluation**, before the fixpoint runs. Reading it per row would let two
      rows in one run disagree about what "now" is, and a rule near a boundary
      would then include one and exclude the other for no reason a person could
      see. `evaluate_at` takes the instant so this is testable; `evaluate` calls
      it with `Utc::now()`.

- [ ] **3. Durations and arithmetic.** The four helpers, plus Time ± Number and
      Time − Time.

- [ ] **4. A rule that reads the clock is never cached.** This is the same
      non-monotonicity as negation, arriving by a different door: a fact derived
      because something was "in the last 7 days" stops being true *without any
      base fact changing*. `DerivedFact::is_cacheable()` must refuse it, by the
      same provenance mechanism absence already uses.

- [ ] **5. The completeness guard fires and is re-run.** Adding `Term::ConstTime`
      must break it. That is the guard working.

## Not in scope

- **Time zones.** Everything is UTC. A rule that means "last week" in a person's
  local time is a display concern until someone has a requirement for it.
- **Calendar arithmetic.** `days(30)` is thirty times twenty-four hours, not "a
  month". Months and years are not fixed durations, and pretending otherwise is
  the kind of quiet wrongness this codebase keeps refusing.
