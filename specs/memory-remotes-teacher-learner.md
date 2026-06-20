# Ferrosa Memory Remotes — Teacher/Learner Blueprint

> Status: Blueprint
> Created: 2026-05-11
> Scope: ferrosa-memory native feature set for trust-scoped knowledge transfer between memory systems.

## 1. Summary

Ferrosa Memory Remotes let one `ferrosa-memory` instance learn from another through learner-initiated, trust-scoped teaching instead of synchronization or replication.

A local learner queries its own memory first. On explicit user request, local recall miss, stale/insufficient local results, or a Datalog policy fallback, it consults configured teacher remotes. The teacher enforces what it may reveal. The learner enforces what it may trust, import, activate, autocommit, quarantine, or keep only as a stub. The exchange is signed for non-repudiation and returns compact streamed teaching packets with progressive detail references rather than dumping a whole graph.

This feature is intended first for personal machine-to-machine transfer, for example teaching a laptop what a workstation or GPU box knows. The same design should later support team, enterprise, partner, and archive remotes with explicit grants modeled after GitHub-style access control.

## 2. Goals

1. Enable a local memory to learn useful curated recall from another memory system.
2. Preserve local-first behavior: local memory remains the primary write target and authority.
3. Support explicit natural-language and CLI flows such as:
   - `memory pull gpu "CUDA build failures"`
   - `memory ask --fallback gpu "how did we fix vLLM OOM?"`
   - `skill pull gpu "vLLM benchmarking"`
   - `memory promote team "fmem streaming architecture"`
4. Use hybrid retrieval on the teacher: BM25/lexical + vector + optional graph expansion + temporal/freshness ranking.
5. Stream results without materializing large remote graphs before paging.
6. Return small summaries first, with opaque detail references for progressive disclosure.
7. Track provenance: source remote, source item hash/ID, import batch, import date, policy versions, trust scores, and signatures.
8. Detect exact duplicates, near duplicates, conflicts, stale facts, and environment/scope mismatches.
9. Keep OS/deployment-specific operational facts safe, e.g. do not apply a Linux/Docker GPU-box fact to a Mac/Podman laptop query.
10. Treat negative feedback (`no`, `stop`, `wrong`, `WTF`, etc.) as strong structured reinforcement.
11. Keep skills and memories separate because skills have procedural authority.
12. Support archive remotes for pruning cold details while leaving local stubs.

## 3. Non-goals

1. No blind bidirectional sync.
2. No full-fidelity graph replication as the default product path.
3. No automatic remote writes. Upward sharing/promotion is explicit.
4. No remote memory content becomes system/developer instruction authority by default.
5. No raw context transfer by default.
6. No silent import of skills from partially trusted remotes.
7. No local workaround for Ferrosa public API bugs; per repo convention, public DB bugs are fixed upstream.

## 4. Terminology

- Learner: the local `ferrosa-memory` instance receiving knowledge.
- Teacher: a remote `ferrosa-memory` instance answering a teaching request.
- Remote: a configured teacher, archive, team, partner, or promotion target.
- TeachingPacket: a signed streamed response containing summaries, provenance, safety labels, conflict/duplicate hints, and detail references.
- TeachingItem: one candidate unit of knowledge inside a packet.
- DetailRef: an opaque, grant-scoped reference allowing the learner to request more detail later.
- Stub: a local searchable summary/pointer that says where deeper knowledge lives.
- Activation: the step that turns an untrusted or partially trusted item from summary/stub into active local memory.
- Autocommit: policy-granted import of safe non-conflicting knowledge without an explicit per-item approval prompt.
- ApplicabilityFrame: structured scope for an item: OS, host, deployment runtime, repo, environment, service, hardware, date range, and confidence.
- Negative knowledge: durable record that a remote/item/query class is irrelevant, untrusted, stale, not applicable, or explicitly rejected.

## 5. User-facing behavior

### 5.1 Explicit pull

User:

```text
Consult gpu about vLLM benchmark setup.
```

Expected flow:

1. Learner records explicit remote request for `gpu`.
2. Learner evaluates local policy: `can_query(gpu, knowledge)`.
3. Learner sends a signed teaching request to `gpu`.
4. Teacher verifies learner identity and teacher-side grants.
5. Teacher runs hybrid retrieval and streams a signed TeachingPacket.
6. Learner verifies packet signature and evaluates learner-side policy.
7. Safe, non-conflicting, consented-autocommit items become active local memory.
8. Items outside the remote's trusted namespace become stubs, conflicts, or activation-required records.
9. Answer cites learned provenance when relevant: `learned from gpu on 2026-05-11`.

### 5.2 Local miss fallback

User:

```text
How did we fix CUDA OOM in vLLM?
```

Expected flow:

1. Learner searches local memory.
2. Local coverage is below threshold or lacks a GPU/Linux scope.
3. Datalog policy derives `should_consult(gpu, query)` because `fallback_enabled(gpu, gpu_builds)` and `trusted_for(gpu, gpu_builds)`.
4. Learner pulls from `gpu` and imports safe results.
5. Learner answers using local + newly learned active memories.

### 5.3 Stub progressive disclosure

Initial packet item:

```text
GPU has detailed vLLM benchmark notes. Summary sufficient for simple questions. More detail available with detail_ref=opaque-token.
```

If the answer needs detail and policy allows it, the learner asks:

```text
memory detail gpu <detail_ref>
```

The teacher streams a detail packet containing expanded summaries, evidence excerpts, or raw context only if both teacher and learner policies allow raw detail.

### 5.4 Skill teaching is separate

Knowledge:

```text
What does gpu know about vLLM benchmark setup?
```

Skill:

```text
Ask gpu to teach the vLLM benchmarking skill.
```

Skill imports create candidate skills or explicit skill updates. They are never silently merged into active procedural skills from team/partner remotes.

### 5.5 Archive remote

Local pruning may send cold details to an archive remote and keep a local stub:

```text
Detailed history for fmem-viz streaming is archived in archive:batch:abc. Fetch if needed.
```

Archive facts are low-priority historical detail and should not override active current operational facts without freshness/trust evidence.

## 6. Trust and policy model

Both teacher and learner enforce policy.

### 6.1 Teacher-side enforcement

The teacher decides:

- Is the learner identity known?
- What namespaces may the learner query?
- Can the learner request detail?
- Can the learner request raw context?
- Can the learner request skills?
- What redactions must be applied?
- Should secrets, raw context, user profile facts, or intentions be denied?

### 6.2 Learner-side enforcement

The learner decides:

- Is this remote trusted for this namespace?
- Is autocommit granted for this item kind and namespace?
- Does the item match the current applicability frame?
- Is this item prompt-injection-like, secret-like, or instruction-like?
- Is this a duplicate or near duplicate?
- Does it conflict with local or higher-trust memory?
- Should it become active, stub, activation-required, conflicting, superseded, archived, or quarantined?

### 6.3 Datalog policy facts

Policy should be represented as Datalog facts/rules so decisions are explainable.

Example remote facts:

```prolog
remote(gpu).
is_os(gpu, linux).
deploy_runtime(gpu, docker).
has_gpu(gpu, nvidia).
machine_role(gpu, research_box).
trust_class(gpu, personal).
trusted_for(gpu, research).
trusted_for(gpu, gpu_builds).
not_trusted_for(gpu, deployment_info).
grant(gpu, read, knowledge).
grant(gpu, detail_fetch, knowledge).
grant(gpu, autocommit, knowledge).
deny(gpu, read, raw_context).
deny(gpu, autocommit, skills).
fallback_enabled(gpu, gpu_builds).
```

Example derived rules:

```prolog
can_query(Remote, Namespace) :-
  remote(Remote),
  grant(Remote, read, Namespace),
  not deny(Remote, read, Namespace).

can_fetch_detail(Remote, Item) :-
  grant(Remote, detail_fetch, knowledge),
  safe_item(Item),
  not denied_detail(Item).

can_autocommit(Remote, Item) :-
  grant(Remote, autocommit, knowledge),
  item_namespace(Item, Namespace),
  trusted_for(Remote, Namespace),
  safe_item(Item),
  not conflict(Item),
  not prompt_injection_risk(Item),
  not secret_risk(Item).

requires_activation(Remote, Item) :-
  not can_autocommit(Remote, Item).

should_consult(Remote, Query) :-
  explicit_remote(Query, Remote).

should_consult(Remote, Query) :-
  local_coverage(Query, low),
  query_namespace(Query, Namespace),
  fallback_enabled(Remote, Namespace),
  trusted_for(Remote, Namespace).
```

### 6.4 Policy explanation

Every skip/import/quarantine decision must be explainable:

- `Imported because gpu is trusted_for gpu_builds and autocommit is granted.`
- `Skipped because gpu is not_trusted_for deployment_info.`
- `Stub only because team does not grant autocommit.`
- `Quarantined because content contained instruction-like prompt-injection markers.`

## 7. Non-repudiation and signatures

### 7.1 Instance identity

Each `ferrosa-memory` instance should have an Ed25519 keypair generated at setup time or imported from enterprise identity. The public key is used in remote registry entries and handshake/capability responses.

### 7.2 Teacher signature

The teacher signs:

- packet_id
- teacher_instance_id
- teacher_public_key_fingerprint
- learner_instance_id
- request_id
- query_hash
- generated_at
- expires_at
- content_hash
- item hashes
- teacher_policy_version/hash
- optional previous_packet_hash

Meaning: `I, teacher, produced this packet for this learner under this policy.`

### 7.3 Learner import signature

The learner signs an import decision:

- import_batch_id
- packet_id
- learner_instance_id
- imported item IDs/hashes
- rejected item IDs/hashes
- activation decisions
- learner_policy_version/hash
- imported_at

Meaning: `I, learner, imported/rejected/activated these items under this policy.`

### 7.4 Storage expectations

Audit/provenance records must be append-only from the MCP surface. Administrative pruning/archival can compact detail but must preserve signed metadata and stubs.

## 8. TeachingPacket protocol

### 8.1 Teaching request

Fields:

```json
{
  "request_id": "uuid",
  "learner_instance_id": "uuid-or-fingerprint",
  "remote_id": "gpu",
  "query": "CUDA build failures",
  "query_embedding": [0.1],
  "query_namespace_hints": ["gpu_builds", "research"],
  "current_applicability_frame": {
    "os": "linux",
    "host": "gpu",
    "deploy_runtime": "docker",
    "repo": "ferrosa-memory",
    "environment": "local-dev"
  },
  "requested_kind": "knowledge",
  "detail_level": "summary",
  "allow_raw_context": false,
  "max_wall_time_ms": 30000,
  "page_size_hint": 20,
  "policy_hash": "...",
  "signature": "..."
}
```

### 8.2 TeachingPacket

Fields:

```json
{
  "packet_id": "uuid",
  "request_id": "uuid",
  "teacher_instance_id": "uuid-or-fingerprint",
  "learner_instance_id": "uuid-or-fingerprint",
  "remote_name": "gpu",
  "query_hash": "sha256",
  "generated_at": "rfc3339",
  "expires_at": "rfc3339-or-null",
  "teacher_policy_hash": "sha256",
  "trust_context": {
    "trust_class": "personal",
    "trusted_for": ["research", "gpu_builds"],
    "not_trusted_for": ["deployment_info"]
  },
  "teacher_environment": {
    "os": "linux",
    "deploy_runtime": "docker",
    "hardware": ["nvidia_gpu"]
  },
  "items": [],
  "negative_knowledge": [],
  "duplicate_candidates": [],
  "conflict_candidates": [],
  "more_available": true,
  "continuation_token": "opaque-or-null",
  "content_hash": "sha256",
  "signature": "ed25519"
}
```

### 8.3 TeachingItem

Fields:

```json
{
  "item_id": "uuid",
  "kind": "fact|decision|pattern|bug|summary|skill_stub|procedure_stub|negative",
  "title": "GPU vLLM CUDA OOM workaround",
  "summary": "Compact content suitable for local recall.",
  "namespace": "gpu_builds",
  "scope_tags": ["linux", "docker", "nvidia", "research"],
  "applicability_frame": {
    "os": "linux",
    "host": "gpu",
    "hardware": ["nvidia"],
    "deploy_runtime": "docker",
    "repo": null,
    "environment": "local-dev",
    "confidence": 0.86
  },
  "confidence": 0.84,
  "freshness": {
    "source_observed_at": "rfc3339",
    "staleness": "fresh|warm|stale|unknown"
  },
  "provenance": {
    "source_entity_ids": ["uuid"],
    "source_context_segment_ids": ["uuid"],
    "source_content_hash": "sha256",
    "source_tenant_id": "uuid-or-null",
    "source_session_id": "uuid-or-null"
  },
  "safety": {
    "prompt_injection_risk": "none|low|medium|high",
    "secret_risk": "none|suspected|redacted",
    "instruction_like": false,
    "raw_context_included": false
  },
  "detail_ref": {
    "available": true,
    "token": "opaque",
    "detail_kinds": ["expanded_summary", "evidence_excerpt"],
    "raw_context_available": false
  }
}
```

### 8.4 Streaming events

The stream should support:

- `teaching_started`
- `remote_hit`
- `teaching_item`
- `negative_knowledge`
- `duplicate_candidate`
- `conflict_candidate`
- `packet_summary`
- `continuation_available`
- `teaching_complete`
- `teaching_error`

The teacher must not wait to materialize a full graph before sending initial events. A start event and first item/negative result should be sent as soon as available.

## 9. Local import states

Imported/stubbed records may be in one of these states:

- `active`: trusted enough for normal recall.
- `active_stub`: local has searchable summary and may fetch detail automatically.
- `needs_activation`: untrusted/partial-trust item summarized but not active.
- `conflicting`: useful but disagrees with local or higher-trust item in overlapping scope.
- `quarantined`: prompt-injection/secret/procedure risk or unsafe content.
- `superseded`: replaced by newer or higher-trust scoped knowledge.
- `archived`: cold local detail moved to an archive remote, local stub remains.
- `rejected`: user or policy explicitly rejected it.

## 10. Duplicate, conflict, and scope detection

### 10.1 Exact duplicate

Use source_content_hash, canonical title hash, normalized content hash, and previous import provenance.

### 10.2 Near duplicate

Signals:

- same entity type and normalized name/title
- embedding similarity over threshold
- lexical similarity over threshold
- overlapping graph neighborhood
- same namespace and applicability frame

Near duplicates become merge candidates, not automatic destructive merges.

### 10.3 Conflict

A conflict requires overlapping scope plus incompatible claims. Disjoint scopes are not conflicts.

Not a conflict:

- `laptop: macos + podman`
- `gpu: linux + docker`

Conflict:

- `gpu: linux deployment uses docker`
- `local: gpu linux deployment uses podman`

### 10.4 Chosen facts and trust boost

If policy chooses one conflict candidate:

- chosen item gets a small trust boost
- loser remains visible as conflicting/superseded evidence
- user confirmation or successful repeated use adds larger boosts
- correction/failure demotes item/source/namespace

## 11. Applicability frames

Every operational item should attempt to extract:

- OS: linux, macos, windows, unknown
- host/machine: gpu, laptop, workstation, server, unknown
- hardware: nvidia, apple_silicon, cpu_only, unknown
- deployment runtime: docker, podman, k8s, systemd, unknown
- repo/project
- service/component
- environment: local-dev, test, staging, prod, unknown
- date range / freshness
- confidence

Extraction can start with deterministic rules and entity matching, then add a small local model classifier as an optional helper. Datalog policy makes the final applicability/trust decision.

## 12. Prompt-injection and safety model

Remote content is data, not authority.

Required defenses:

1. Classify instruction-like content.
2. Classify prompt-injection phrases such as `ignore previous instructions`, `system prompt`, `developer message`, `exfiltrate`, `disable safety`.
3. Redact or quarantine secret-like content.
4. Never let remote memory alter system/developer prompt, security settings, model routing, or tool permissions.
5. Procedures from non-personal remotes require review and belong in the skill teaching path.
6. Wrap imported evidence as cited memory, not invisible authority.
7. Keep user-profile and durable preference changes behind explicit activation/promotion.

## 13. Negative knowledge and reinforcement

### 13.1 Negative feedback triggers

Strong negative triggers include:

- `no`
- `stop`
- `wrong`
- `WTF`
- `not that machine`
- `that is Mac-only`
- `do not use gpu for deployment`
- `stale`
- `bad memory`

### 13.2 Feedback categories

- `irrelevant`: retrieved but not useful.
- `wrong_scope`: true elsewhere but not applicable here.
- `wrong_fact`: fact is false or superseded.
- `bad_source_namespace`: remote/source not trusted for namespace.
- `bad_procedure`: skill/procedure caused failure.
- `stop_signal`: halt current chain and strongly penalize plan/source/tooling path.
- `prompt_injection`: content attempted to influence authority boundaries.

### 13.3 Reinforcement rules

Positive boosts:

- chosen by policy: small boost
- used in answer: small boost
- user confirms: medium boost
- tool/action succeeds using it: large boost
- repeated successful use: additive reinforcement

Negative demotions:

- retrieved but ignored: tiny demotion
- user says irrelevant: medium demotion
- user says wrong: large demotion
- caused failed action: large demotion
- `stop/no/WTF`: strong negative, possibly quarantine or review

Reinforcement is per item, source remote, namespace, and applicability frame, not only global.

## 14. Archive/pruning model

Archive is a remote type. Local memory can prune cold detail by:

1. Select archive candidates by age, low use, low warmth, and large detail size.
2. Summarize enough local stub content to route future queries.
3. Push full detail to archive remote with signed provenance.
4. Keep local `active_stub` or `archived` pointer.
5. Fetch detail only when user asks or local answer needs more.

Archive remotes are usually trusted for historical detail but not for current operations.

## 15. Skill teaching path

Skills and memories are separate.

Skill teaching packet items should include:

- skill name
- description
- trigger conditions
- prerequisites
- steps
- commands
- pitfalls
- verification
- safety notes
- source/provenance
- signature
- review status

Default policies:

- personal trusted remotes may propose candidate skill imports if `grant(remote, read, skills)`.
- skill autocommit requires explicit `grant(remote, autocommit, skills)` and should be off by default.
- team/partner skill imports require review.
- imported skills should not include raw secrets or private local paths unless explicitly allowed.

## 16. MCP/API surface

Proposed tool groups:

### Remote management

Packet K exposes the same tenant-scoped remote-management surface through MCP tools and the `memory-sync remote ...` CLI. Registry commands must never accept or print credentials; endpoint values are validated before storage and must be HTTP(S) URLs without embedded userinfo.

- `remote_list` / `memory-sync remote --config <ferrosa-memory.toml> --tenant-id <uuid> list [--limit N]`
- `remote_add` / `memory-sync remote --config <ferrosa-memory.toml> --tenant-id <uuid> add --remote-id <uuid> --name <name> --endpoint https://remote.example/mcp --instance-id <uuid> --public-key-fingerprint <fingerprint> --trust-class personal|team|partner|external|public|archive`
- `remote_update_policy` / `memory-sync remote ... update-policy --remote-id <uuid> --kind grant|deny --action read|detail_fetch|autocommit|requires_activation|should_consult --namespace <namespace>`
- `remote_remove` / `memory-sync remote ... remove --remote-id <uuid>`; soft-disables the remote and preserves provenance/policy audit rows.
- `remote_health` / `memory-sync remote ... health --remote-id <uuid>`; reports local registration health without dialing the remote.
- `remote_capabilities` / `memory-sync remote ... capabilities --remote-id <uuid>`; reports expected remote-memory capabilities.
- `remote_explain_policy` / `memory-sync remote ... explain-policy --remote-id <uuid> --action read|detail_fetch|autocommit|requires_activation|should_consult --namespace <namespace>`; returns `allowed`, `explanation`, `reasons`, and `policy_fact_count`.

Canonical policy action names match core `PolicyAction` variants serialized as snake_case: `read`, `detail_fetch`, `autocommit`, `requires_activation`, and `should_consult`. The CLI accepts `fetch_detail` and `detail` as aliases for `detail_fetch`, and accepts `external` as a compatibility alias for the `partner` trust class.

### Teaching / pull

- `teach_query_stream` — teacher-side endpoint/tool.
- `pull_preview` — learner-side query remote and build import plan.
- `pull_commit` — learner-side import selected preview items.
- `detail_fetch` — learner-side fetch progressive detail.

### Skills

- `teach_skill_preview`
- `teach_skill_commit`

### Conflicts / duplicates

- `duplicate_candidates`
- `conflict_list`
- `conflict_resolve`

### Feedback / reinforcement

- `usage_mark`
- `feedback_record`
- `trust_update`

### Archive

- `archive_candidates`
- `archive_commit`
- `archive_fetch_detail`

## 17. Storage model

Exact table names can change during implementation, but the following durable concepts are required.

### `memory_remotes`

- `tenant_id`
- `remote_id`
- `name`
- `endpoint`
- `trust_class`
- `rank`
- `enabled`
- `auth_ref`
- `public_key_fingerprint`
- `created_at`
- `updated_at`
- `last_seen_at`

### `remote_policy_facts`

- `tenant_id`
- `remote_id`
- `predicate`
- `subject`
- `object`
- `confidence`
- `source`
- `created_at`

### `teaching_packets`

- `tenant_id`
- `packet_id`
- `remote_id`
- `query_hash`
- `summary`
- `content_hash`
- `signature`
- `teacher_policy_hash`
- `generated_at`
- `expires_at`

### `teaching_items`

- `tenant_id`
- `packet_id`
- `item_id`
- `kind`
- `namespace`
- `title`
- `summary`
- `applicability_frame_json`
- `safety_json`
- `provenance_json`
- `detail_ref`
- `content_hash`

### `remote_stubs`

- `tenant_id`
- `stub_id`
- `remote_id`
- `title`
- `summary`
- `namespace`
- `applicability_frame_json`
- `detail_ref`
- `more_available`
- `trust_score`
- `state`
- `last_fetched_at`
- `created_at`
- `updated_at`

### `memory_provenance`

- `tenant_id`
- `local_entity_id`
- `remote_id`
- `packet_id`
- `source_entity_id`
- `source_content_hash`
- `source_observed_at`
- `import_batch_id`
- `imported_at`
- `trust_score`

### `memory_conflicts`

- `tenant_id`
- `conflict_id`
- `local_item_id`
- `remote_item_id`
- `conflict_type`
- `scope_overlap_json`
- `chosen_item_id`
- `resolution`
- `resolved_at`
- `created_at`

### `memory_feedback`

- `tenant_id`
- `feedback_id`
- `target_kind`
- `target_id`
- `remote_id`
- `namespace`
- `feedback_type`
- `weight`
- `query_hash`
- `context_json`
- `created_at`

### `import_batches`

- `tenant_id`
- `import_batch_id`
- `packet_id`
- `remote_id`
- `decision_summary`
- `imported_count`
- `stub_count`
- `quarantine_count`
- `conflict_count`
- `learner_policy_hash`
- `learner_signature`
- `created_at`

## 18. Security and threat model additions

Threats:

1. Prompt injection through imported remote memory.
2. Secret exfiltration via remote detail requests.
3. Cross-machine operational confusion, e.g. Mac/Podman vs Linux/Docker.
4. Malicious or compromised remote sending forged teaching packets.
5. Replay of old teaching packets.
6. Autocommit misconfiguration importing untrusted content.
7. Partner/team remote overexposure of raw context or user profile facts.
8. Signed harmful content being treated as authority rather than evidence.

Mitigations:

- Signed requests/responses and import decisions.
- Policy on both teacher and learner sides.
- Explicit grants and deny-overrides.
- Default-deny raw context, intentions, user profile, and skills autocommit.
- Prompt-injection/secret classifiers.
- Applicability frame extraction and scope-aware conflict detection.
- Packet expiration/replay checks.
- Audit log and policy explanation.
- Activation workflow for untrusted/partial-trust items.

## 19. Acceptance scenarios

### Scenario A: Trusted GPU research autocommit

Given:

- `gpu` is a personal remote.
- `trusted_for(gpu, gpu_builds)`.
- `grant(gpu, autocommit, knowledge)`.

When:

- local asks `Consult gpu about vLLM benchmark setup`.

Then:

- teacher packet verifies.
- safe `gpu_builds` knowledge imports as active.
- provenance links to gpu/date/source hash.
- answer cites the imported memory.

### Scenario B: GPU deployment fact blocked

Given:

- `not_trusted_for(gpu, deployment_info)`.

When:

- gpu packet includes deployment guidance.

Then:

- learner does not autocommit it as active deployment guidance.
- item becomes stub/skipped/activation-required with explanation.

### Scenario C: Mac query avoids Linux facts

Given:

- local query context is Mac/Podman.
- gpu facts are Linux/Docker.

When:

- user asks `How do I run fmem on my Mac?`

Then:

- gpu Docker facts are not applied.
- laptop remote may be consulted if trusted_for mac_setup.
- answer uses Mac-scoped knowledge or asks before using mismatched facts.

### Scenario D: Negative feedback demotes wrong scope

When:

- agent uses gpu deployment fact in Mac context.
- user says `No, WTF, that is for the GPU box, stop.`

Then:

- current action chain stops.
- item gets wrong_scope feedback.
- gpu deployment namespace trust demotes.
- future Mac/deploy queries avoid the item.

### Scenario E: Archive pruning

When:

- cold detailed local memories are archived.

Then:

- archive remote receives signed detail.
- local keeps searchable stub.
- future queries can fetch archive detail if needed.

## 20. Open questions

1. Should signatures be packet-level only for MVP, or item-level too?
   - Recommendation: packet-level signature with item hashes for MVP.
2. Should detail refs be bearer capabilities or require a signed follow-up request?
   - Recommendation: signed follow-up request plus opaque detail token.
3. Should small local model scope extraction be built into fmem or configured as an optional auxiliary endpoint?
   - Recommendation: deterministic extractor first, optional classifier seam.
4. Should local `pull_preview` persist packets before commit?
   - Recommendation: yes, persist signed packet metadata and preview decisions for audit.
5. Should archive push be implemented in the same release as pull?
   - Recommendation: later milestone; design storage now.

## 21. Relationship to existing `memory-sync`

`specs/memory-sync.md` describes full-fidelity admin-path replication. Memory remotes are different:

- `memory-sync`: bulk replication/migration, admin path, full fidelity.
- Memory remotes: learner-initiated teaching, curated recall, trust-scoped import, signed provenance, progressive disclosure.

The two may share low-level export/import helpers eventually, but their product semantics and safety boundaries are distinct.
