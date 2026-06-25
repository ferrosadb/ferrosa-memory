---
name: set-foresight
description: Declare a time-bounded fact or temporary constraint in Ferrosa Memory (valid_from / valid_until). Use when something only holds for a window — a deadline, code freeze, deprecation, or planned-future change — so search surfaces it only while it is valid.
---

# Set Foresight — record time-bounded facts

Foresight memory stores **prospective, time-bounded facts**: things that only hold
during a validity window. Retrieval surfaces a foresight fact **only while it is valid
at the current time**, so expired deadlines and not-yet-active plans never pollute
context. This is distinct from ordinary facts (always-on) and supersession (one fact
replacing another).

## When to use

Call `set_foresight` whenever a fact has a clear start and/or end:

- "Code freeze on `main` until 2026-07-01" → `valid_until`
- "Migration plan goes live on 2026-06-30" → `valid_from`
- "API v1 is deprecated as of today" → `valid_until` at the cutover
- "Use the staging cluster this week" → both `valid_from` and `valid_until`

## How

Call the `set_foresight` tool:

```jsonc
set_foresight {
  "content": "Code freeze on the release branch until the 0.17 cut",
  "valid_until": "2026-07-01T00:00:00Z"   // RFC3339; omit for no expiry
  // "valid_from": "2026-06-30T00:00:00Z" // RFC3339; omit for "active now"
  // "session_id": "<uuid>"               // defaults to the current session
}
```

Both `valid_from` and `valid_until` are optional RFC3339 timestamps — omit either for
an open-ended bound. Returns the new `fact_id`.

## Retrieving it

Nothing special is required: a normal `hybrid_search` surfaces active foresight facts
(labeled `result_type: "foresight"`, with a `valid until …` hint) ranked alongside
other results. A fact whose window has passed — or hasn't started — is filtered out
automatically.

## Notes

- Foresight facts are session-scoped. Pass `session_id` to scope to a specific session.
- Validity is evaluated at *now*. (Parameterized "as-of T" time-travel queries are a
  future extension.)
- Pairs with [`memory-session-start`](../memory-session-start/SKILL.md) — active
  foresight surfaces automatically when you restore context.
