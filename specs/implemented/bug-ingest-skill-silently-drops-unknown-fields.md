---
type: bug
priority: P2
status: implemented
created: 2026-04-16
updated: 2026-04-20
reported-by: deploy smoke test (2026-04-16)
---

# `ingest_skill` silently drops unknown / misnamed fields

## Observed

During the post-launch smoke test on 2026-04-16, a client called
`ingest_skill` with a step shape of `{title, body}` instead of the
documented `{phase, instruction}`:

```json
"steps": [
    {"title": "Step one", "body": "Verify the thing"},
    {"title": "Step two", "body": "Confirm it worked"}
]
```

The call returned `{"action": "created", ...}` with no error. In storage, the
`steps` field came back as `[]`. Two real steps were lost with zero operator
feedback. Because `Step.instruction` is a required field in the Rust struct
but no step key matched any real field, serde filled in the defaults and
produced an empty `Vec<Step>`.

This contradicts the fail-loud rule: the ingest call implied success while
silently dropping payload data.

## Root Cause

`crates/ferrosa-memory-core/src/skill.rs` defines:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Step {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub instruction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestSkillParams { /* ... */ }
```

Neither struct carries `#[serde(deny_unknown_fields)]`, so serde silently
ignores any unknown JSON keys during deserialization. Combined with
`#[serde(default)]` on `Vec<Step>` in `IngestSkillParams`, a malformed step
array silently becomes `[]`.

## Expected

Calling `ingest_skill` with unknown keys on `IngestSkillParams` or `Step`
MUST return an error (`InvalidParams`) naming the unknown fields. Callers
learn about schema drift the first time they fat-finger a request, not weeks
later when they notice missing data.

## Proposed Fix

Add `#[serde(deny_unknown_fields)]` to:

- `IngestSkillParams`
- `Step`

Both live in `crates/ferrosa-memory-core/src/skill.rs`.

Consider (bounded scope — do NOT expand this bug):
- `IngestSkillActionResult` and similar **response** types can stay permissive;
  the client is the party at risk of breakage on response-shape changes.
- Other tool-param structs (SmartIngestParams, CreateEdgeParams, etc.) likely
  have the same latent issue — that is out of scope for *this* bug but would
  make a fine follow-up sweep.

## Acceptance Criteria

- [ ] `ingest_skill` with a step of `{"title": ..., "body": ...}` returns
      `InvalidParams` naming `title` / `body` as unknown fields on `Step`.
- [ ] `ingest_skill` with a spurious top-level field (e.g.
      `{"name": "...", "foo_bar": 1, ...}`) returns `InvalidParams` naming
      `foo_bar` as unknown on `IngestSkillParams`.
- [ ] A well-formed call with correct `{phase, instruction}` steps still
      succeeds and round-trips the steps through storage.
- [ ] Unit test added in `crates/ferrosa-memory-core/src/skill.rs` (or a
      dedicated test module) exercising all three paths above.

## Related

- `Fail-loud never fake` project rule: silent data loss on a successful-looking
  API call is the worst class of failure.

## Implementation Notes

Three changes in the same commit — the struct attribute alone didn't reach
the dispatch path because the handler parses args field-by-field instead of
round-tripping through the struct.

1. Added `#[serde(deny_unknown_fields)]` to both `Step` and
   `IngestSkillParams` in `crates/ferrosa-memory-core/src/skill.rs`.
2. `handle_ingest_skill` in `crates/ferrosa-memory-core/src/dispatch.rs`
   was parsing `steps` with
   `serde_json::from_value(v).unwrap_or_default()` — a fail-quiet path
   that replaced malformed arrays with `[]`. Replaced with an error-
   propagating `match` so serde errors surface as
   `-32602 / invalid \`steps\` payload: …`.
3. Added an explicit `KNOWN_KEYS` allow-list at the top of
   `handle_ingest_skill` so unknown top-level fields are rejected with
   `-32602 / unknown field(s) on ingest_skill: <keys>. Known: <list>`.
   The struct-level `deny_unknown_fields` doesn't reach this path on its
   own — the handler never passes the whole JSON through the struct
   deserializer.

Four new unit tests in `skill::tests`:

- `step_deserialization_rejects_unknown_fields` — `{title, body}` → error
  naming `title`
- `step_deserialization_accepts_known_fields` — `{phase, instruction}`
  deserializes cleanly
- `ingest_skill_params_rejects_unknown_top_level_field` — `spurious_field`
  named in the serde error
- `ingest_skill_params_accepts_minimum_valid_payload` — smallest valid
  payload round-trips

E2E verified against the live 3-node cluster:

```
ingest_skill(steps=[{"title":"x","body":"y"}])
  → invalid `steps` payload: unknown field `body`,
    expected `phase` or `instruction`

ingest_skill(foo_bar=1, …)
  → unknown field(s) on ingest_skill: foo_bar.
    Known: name, category, description, session_id, trigger_keywords,
    tags, prerequisites, steps, output_artifacts, completion_criteria,
    content_hash
```

Scope deliberately narrow: response types (`IngestSkillActionResult`, etc.)
intentionally remain permissive so that a future server-side additive
response change does not break older clients. Other tool-param handlers
(`smart_ingest`, `create_edge`, …) likely have the same latent
silent-drop issue — see "Proposed Fix" for the deferred sweep.
