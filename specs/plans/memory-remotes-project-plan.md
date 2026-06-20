# Ferrosa Memory Remotes Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task. Do not parallelize tasks that touch the same trait/DDL/tool-dispatch seams unless the task explicitly says it is safe.

**Goal:** Build signed, policy-governed teacher/learner knowledge transfer between `ferrosa-memory` instances.

**Architecture:** `ferrosa-memory` owns remotes, Datalog policy, signed TeachingPackets, learner-side import decisions, provenance, stubs, feedback, and archive lifecycle. Hermes and other agents call natural-language-friendly MCP tools, but the core federation semantics live in fmem so Codex, Claude, Hermes, and CLI users share behavior.

**Tech Stack:** Rust, Tokio, serde, existing fmem Storage trait/CQL/MockStorage patterns, existing Datalog/rule engine, existing hybrid search/context segment/entity/fold/skill surfaces, Ed25519 signing crate selected during Packet A.

**Primary specs:**
- `specs/memory-remotes-teacher-learner.md`
- `specs/remote-memory.md`

**Key rules:**
- TDD only: RED -> GREEN -> REFACTOR.
- Remote memory content is data, not authority.
- Teacher and learner both enforce policy.
- No raw context, skills, user profile, or intentions by default.
- No graph-table/private Ferrosa API workarounds.
- Streaming paths must not materialize entire remote graphs before sending initial events.

---

## Workstream map for multiple agents

The plan is split into packets that can be assigned to different agents with minimal overlap.

| Packet | Owner | Depends on | Main files | Parallelizable? |
|--------|-------|------------|------------|-----------------|
| A | Identity + signatures | none | `crates/ferrosa-memory-core/src/remote_identity.rs`, config | Yes |
| B | Types + DDL + Storage | A API shapes | `types.rs`, `storage.rs`, `cql_storage.rs`, `ddl/` | No, single agent |
| C | Policy/Datalog | B types | Datalog/policy modules | Yes after B type skeleton |
| D | Applicability + safety classifiers | B types | classifier modules | Yes |
| E | Teaching query stream | B/C/D | hybrid search, MCP dispatch | No with F, but yes after B/C/D |
| F | Pull preview/import | B/C/D/E | import engine, provenance, stubs | No with E |
| G | Detail fetch + stubs | B/E/F | detail refs, remote stubs | Yes after F |
| H | Feedback/reinforcement | B/C/F | feedback tables/rules | Yes after B/C |
| I | Skill teaching path | B/C/D | skill modules/tools | Yes after base knowledge path |
| J | Archive remote | B/F/G/H | archive tools | Yes late |
| K | CLI/MCP/natural-language polish | E/F/G/H/I/J | schemas/docs/tests | Late |
| L | System/integration smokes | all | tests/system, docs | Late |

Recommended execution:

1. Do Packet A and B first.
2. Run C and D in parallel once B has stable types.
3. Build E, then F.
4. Build G and H in parallel.
5. Build I and J after the knowledge path is green.
6. Finish K/L.

---

## Acceptance smoke bundle

Final implementation must pass all of these scenarios:

1. Trusted GPU research autocommit:
   - `gpu` remote is trusted_for `gpu_builds` and autocommit knowledge is granted.
   - Pull query imports safe `gpu_builds` item as active.
   - Imported entity/stub stores signed provenance.

2. GPU deployment blocked:
   - `not_trusted_for(gpu, deployment_info)`.
   - Deployment item from `gpu` does not autocommit active.
   - Import plan explains the policy block.

3. Mac query avoids Linux facts:
   - Current applicability frame is Mac/Podman.
   - GPU packet has Linux/Docker scope.
   - Import/apply path refuses or caveats item.

4. Untrusted team activation:
   - Team packet returns relevant knowledge.
   - No autocommit.
   - Item becomes `needs_activation` or `active_stub`.

5. Progressive detail:
   - Initial pull stores stub with `detail_ref` and `more_available=true`.
   - Detail fetch verifies teacher signature and grant.

6. Negative feedback:
   - `No, WTF, that is for the GPU box, stop.` records stop/wrong_scope feedback.
   - Trust for source namespace demotes.
   - Future policy explanation reflects the demotion.

7. Prompt injection quarantine:
   - Packet item containing instruction-like attack text does not autocommit.
   - Item enters `quarantined` with safety explanation.

---

## Packet A — Instance identity and signatures

### Task A1: Add remote identity types

**Objective:** Define instance identity, signing key references, signatures, and hash wrappers.

**Files:**
- Create: `crates/ferrosa-memory-core/src/remote_identity.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs`
- Test: `crates/ferrosa-memory-core/src/remote_identity.rs` unit tests

**Step 1: Write failing tests**

Test behaviors:

- `InstanceId` serializes/deserializes.
- `ContentHash::sha256_json` is deterministic for semantically same JSON object ordering if canonicalization is implemented; otherwise document byte-order deterministic behavior and test that.
- Signature verification fails for mutated payload.

Suggested test names:

```rust
#[test]
fn content_hash_is_stable_for_same_payload() {}

#[test]
fn signature_verification_fails_after_payload_mutation() {}
```

**Step 2: Run RED**

```bash
cargo test -p ferrosa-memory-core remote_identity -- --nocapture
```

Expected: compile failure because module/types do not exist.

**Step 3: Implement minimal types**

Use a well-maintained Ed25519 crate. Candidate: `ed25519-dalek` with serde-compatible public key/signature wrappers, or project-standard crypto crate if already present.

Core types:

```rust
pub struct InstanceId(pub uuid::Uuid);
pub struct PublicKeyFingerprint(pub String);
pub struct ContentHash(pub String);
pub struct SignatureBytes(pub Vec<u8>);

pub struct SignedEnvelope<T> {
    pub payload: T,
    pub content_hash: ContentHash,
    pub signer: InstanceId,
    pub public_key_fingerprint: PublicKeyFingerprint,
    pub signature: SignatureBytes,
}
```

**Step 4: Run GREEN**

```bash
cargo test -p ferrosa-memory-core remote_identity -- --nocapture
```

Expected: tests pass.

**Step 5: Commit**

```bash
git add crates/ferrosa-memory-core/src/remote_identity.rs crates/ferrosa-memory-core/src/lib.rs Cargo.toml Cargo.lock
git commit -m "feat: add remote memory identity signatures"
```

### Task A2: Add config for instance keys

**Objective:** Load or generate per-instance signing identity without committing secrets.

**Files:**
- Modify: config module files; locate with `search_files("struct.*Config", path="crates/ferrosa-memory-core/src")`
- Test: existing config tests or new config unit tests

**RED tests:**

- config without remote identity uses default key path under configured runtime/home directory.
- config with explicit key path loads it.
- missing key can be generated by setup helper, but runtime parse does not silently create files in tests unless helper is called.

**Commands:**

```bash
cargo test -p ferrosa-memory-core config -- --nocapture
```

**Implementation notes:**

Config section:

```toml
[remote_identity]
instance_id = "..." # optional; generate if absent during setup
key_path = "/path/to/instance.ed25519"
public_key_path = "/path/to/instance.pub"
```

Never log private keys. Never expose private key material through MCP.

---

## Packet B — Types, DDL, and Storage wiring

This packet is mechanical and cross-cuts traits, mock storage, CQL storage, DDL, and migrations. Assign to one agent.

### Task B1: Add core remote memory types

**Objective:** Define serializable Rust structs/enums for remotes, policies, packets, items, stubs, provenance, conflicts, feedback, and import batches.

**Files:**
- Create: `crates/ferrosa-memory-core/src/remotes/types.rs`
- Create: `crates/ferrosa-memory-core/src/remotes/mod.rs`
- Modify: `crates/ferrosa-memory-core/src/lib.rs`
- Test: `crates/ferrosa-memory-core/src/remotes/types.rs`

**RED tests:**

- serde roundtrip for `TeachingPacket`.
- serde roundtrip for `TeachingItem` with applicability and safety fields.
- `ImportState` serializes to stable lowercase snake_case strings.
- `RemoteGrant`/`RemoteDeny` serializes with namespace/kind.

**Command:**

```bash
cargo test -p ferrosa-memory-core remotes::types -- --nocapture
```

**Implementation types:**

Enums:

- `RemoteTrustClass`: `Personal`, `Team`, `Partner`, `Public`, `Archive`
- `TeachingKind`: `Fact`, `Decision`, `Pattern`, `Bug`, `Summary`, `SkillStub`, `ProcedureStub`, `Negative`
- `ImportState`: `Active`, `ActiveStub`, `NeedsActivation`, `Conflicting`, `Quarantined`, `Superseded`, `Archived`, `Rejected`
- `SafetyRisk`: `None`, `Low`, `Medium`, `High`, `Suspected`, `Redacted`
- `FeedbackType`: `Irrelevant`, `WrongScope`, `WrongFact`, `BadSourceNamespace`, `BadProcedure`, `StopSignal`, `PromptInjection`

Structs:

- `MemoryRemote`
- `RemotePolicyFact`
- `TeachingRequest`
- `TeachingPacket`
- `TeachingItem`
- `ApplicabilityFrame`
- `SafetyClassification`
- `DetailRef`
- `RemoteStub`
- `MemoryProvenance`
- `MemoryConflict`
- `MemoryFeedback`
- `ImportBatch`

### Task B2: Add greenfield DDL

**Objective:** Add CQL tables for remotes and imports.

**Files:**
- Create: `ddl/034_memory_remotes.cql` or next available migration number after inspecting `ddl/`
- Modify: migration registry in `crates/ferrosa-memory-core/src/migration.rs`
- Test: migration registry tests and DDL parse tests if present

**Pre-step:** inspect current `ddl/` to choose correct next number.

**RED tests:**

- migration registry includes new DDL in monotonic order.
- DDL text includes all required tables.
- no destructive statements (`DROP`, narrowing `ALTER`) are present.

**Commands:**

```bash
cargo test -p ferrosa-memory-core migration -- --nocapture
```

**Tables:**

At minimum:

- `memory_remotes`
- `remote_policy_facts`
- `teaching_packets`
- `teaching_items`
- `remote_stubs`
- `memory_provenance`
- `memory_conflicts`
- `memory_feedback`
- `import_batches`

Keep CQL additive/idempotent.

### Task B3: Storage trait and MockStorage

**Objective:** Add storage trait methods and mock implementations.

**Files:**
- Modify: `crates/ferrosa-memory-core/src/storage.rs`
- Modify: mock storage file; locate with `search_files("struct MockStorage", path="crates/ferrosa-memory-core/src")`
- Test: mock storage unit tests

**RED tests:**

For each family:

- remote put/get/list/update policy
- teaching packet put/get
- teaching item list by packet
- stub put/list/update state
- provenance put/list by entity
- conflict put/list/resolve
- feedback put/list by target
- import batch put/get

**Command:**

```bash
cargo test -p ferrosa-memory-core remote_storage_mock -- --nocapture
```

Expected RED: trait methods missing.

**Implementation note:**

Use strongly typed methods. Avoid raw JSON strings at call sites except for fields intentionally stored as JSON blobs.

### Task B4: CqlStorage implementation

**Objective:** Implement new storage methods against CQL tables.

**Files:**
- Modify: CQL storage implementation files; locate with `search_files("impl.*Storage.*Cql", path="crates/ferrosa-memory-core/src")`
- Test: unit tests for query builders; live integration tests should be `#[ignore]` if cluster required

**RED tests:**

- prepared query builders use tenant_id and stable primary keys.
- JSON blobs serialize/deserialize.
- list methods have bounded paging parameters; no unbounded tenant-wide materialization in serving paths.

**Command:**

```bash
cargo test -p ferrosa-memory-core remotes_cql -- --nocapture
```

**Implementation note:**

Avoid adding broad scans to runtime query paths. If administrative list-all is needed, gate it explicitly and page it.

---

## Packet C — Datalog remote policy

### Task C1: Add policy fact ingestion and evaluation API

**Objective:** Evaluate `can_query`, `can_fetch_detail`, `can_autocommit`, `requires_activation`, and `should_consult` from Datalog-backed policy facts.

**Files:**
- Create: `crates/ferrosa-memory-core/src/remotes/policy.rs`
- Test: `policy.rs` unit tests

**RED tests:**

- gpu trusted_for research autocommits safe knowledge.
- gpu not_trusted_for deployment blocks deployment autocommit.
- explicit ask derives should_consult.
- local coverage low + fallback_enabled derives should_consult.
- deny overrides grant.

**Command:**

```bash
cargo test -p ferrosa-memory-core remotes::policy -- --nocapture
```

**Implementation note:**

Use the existing Datalog/rule engine if it can evaluate these facts cheaply. If a wrapper is needed, keep it thin and explainable.

### Task C2: Policy explanation strings

**Objective:** Return machine-readable and human-readable explanations for every policy decision.

**RED tests:**

- blocked deployment item returns reason containing `not_trusted_for(gpu, deployment_info)`.
- autocommit item explains grant/trust/safety/conflict conditions.
- deny override explains the deny.

**Output shape:**

```rust
pub struct PolicyDecision {
    pub allowed: bool,
    pub action: PolicyAction,
    pub reasons: Vec<PolicyReason>,
    pub explanation: String,
}
```

---

## Packet D — Applicability and safety classifiers

### Task D1: Deterministic applicability extractor

**Objective:** Extract OS/host/runtime/repo/environment hints from text and query context.

**Files:**
- Create: `crates/ferrosa-memory-core/src/remotes/applicability.rs`
- Test: unit tests

**RED tests:**

- `Mac` -> `os=macos`.
- `GPU box` -> host/machine alias resolved to `gpu` when policy facts define alias.
- `Docker` -> `deploy_runtime=docker`.
- `Podman` -> `deploy_runtime=podman`.
- `Ferrosa Memory` -> project/repo hint if known.

**Command:**

```bash
cargo test -p ferrosa-memory-core remotes::applicability -- --nocapture
```

### Task D2: Applicability comparison

**Objective:** Decide exact match, disjoint, partial, unknown, or conflict-prone overlap.

**RED tests:**

- Linux/Docker vs Mac/Podman is disjoint, not conflict.
- Linux/Docker vs Linux/Podman on same host is conflict-prone.
- Unknown scope is conservative and requires caveat/review.

### Task D3: Safety classifier

**Objective:** Classify prompt-injection, instruction-like, and secret-like text.

**Files:**
- Create: `crates/ferrosa-memory-core/src/remotes/safety.rs`

**RED tests:**

- `ignore previous instructions` -> high prompt injection risk.
- `system prompt` + imperative -> medium/high risk.
- shell procedure text is instruction-like but not necessarily injection.
- suspected API key/private key -> secret risk suspected/redacted.

**Implementation note:**

Start deterministic. Optional small local model can be a later seam, not required for MVP.

---

## Packet E — Teacher-side teach query stream

### Task E1: Add teacher-side query planner

**Objective:** Given a TeachingRequest, run hybrid retrieval and produce TeachingItems.

**Files:**
- Create: `crates/ferrosa-memory-core/src/remotes/teach.rs`
- Modify: existing hybrid search/context search calls as needed
- Test: unit tests with MockStorage

**RED tests:**

- BM25 + vector hits become compact TeachingItems.
- context/entity hits include provenance refs and source hashes.
- stale/superseded hits become negative knowledge or lower-ranked items.
- no relevant hits emits negative knowledge, not empty success only.

**Command:**

```bash
cargo test -p ferrosa-memory-core remotes::teach -- --nocapture
```

### Task E2: Streaming event model

**Objective:** Emit teaching events progressively.

**Files:**
- Modify/create remote teach stream module
- Test: stream unit tests

**RED tests:**

- first event is `teaching_started` before retrieval completes.
- first item can be consumed before full packet completion.
- continuation token emitted when more available.
- error event preserves partial packet metadata.

### Task E3: Teacher-side policy enforcement

**Objective:** The teacher denies or redacts content based on teacher-side grants.

**RED tests:**

- raw_context denied by default.
- detail fetch denied if no grant.
- skill request denied if no skill grant.
- denied request emits signed negative/error event.

### Task E4: MCP tool exposure for teacher stream

**Objective:** Expose `teach_query_stream` or equivalent MCP endpoint/tool.

**Files:**
- Modify MCP tool schema/dispatch files
- Test: dispatch schema tests

**RED tests:**

- tool appears in `tools/list`.
- schema rejects missing query/remote fields.
- dispatch returns streaming-compatible events or fallback JSON array if current MCP transport lacks true streaming.

---

## Packet F — Learner pull preview and commit

### Task F1: Pull preview orchestration

**Objective:** Learner queries remote, verifies signed packet, evaluates policy, and returns an import plan without writing active memory.

**Files:**
- Create: `crates/ferrosa-memory-core/src/remotes/pull.rs`
- Test: unit tests

**RED tests:**

- valid signed packet accepted.
- mutated signed packet rejected.
- untrusted team item becomes activation-required.
- trusted gpu research item planned as active.
- gpu deployment item planned as stub/skipped due to `not_trusted_for`.

### Task F2: Duplicate and conflict candidates

**Objective:** Build exact duplicate, near duplicate, and conflict detection for preview.

**RED tests:**

- same source_content_hash -> exact duplicate skip.
- similar title/embedding/scope -> near duplicate candidate.
- disjoint Mac/Podman vs Linux/Docker is not conflict.
- same host/runtime incompatible fact is conflict.

### Task F3: Pull commit writes provenance and import batch

**Objective:** Commit approved preview items into local memory/stubs/provenance/import batch records.

**RED tests:**

- active item writes local entity/fact/summary plus provenance.
- stub item writes remote_stub only.
- quarantined item does not enter active recall.
- learner import decision is signed.

**Implementation note:**

Do not make active imports invisible. They must be searchable locally and provenance-queryable.

### Task F4: MCP tools for pull preview/commit

**Objective:** Expose `pull_preview` and `pull_commit` tools.

**RED tests:**

- preview is dry-run by default.
- commit requires preview_id/import_plan_id.
- commit rejects stale/expired preview unless policy permits refresh.

---

## Packet G — Detail refs and progressive disclosure

### Task G1: Detail ref type and storage

**Objective:** Implement opaque detail references tied to teacher grants, packet IDs, item IDs, and expiry.

**RED tests:**

- detail ref cannot be decoded by client as raw source ID.
- expired detail ref rejected.
- detail ref for item A cannot fetch item B.

### Task G2: Detail fetch endpoint/tool

**Objective:** Fetch expanded detail from teacher and verify signed response.

**RED tests:**

- trusted personal remote detail fetch succeeds.
- team detail fetch without grant fails.
- raw context detail denied unless explicitly granted by both teacher and learner.

### Task G3: Stub-driven fetch

**Objective:** Local search can find a stub and route to detail fetch when needed.

**RED tests:**

- stub summary is enough for simple query.
- complex query over stub triggers detail fetch if policy allows.
- policy explanation says why detail was or was not fetched.

---

## Packet H — Feedback and reinforcement

### Task H1: Feedback recording

**Objective:** Record structured feedback from user/system outcomes.

**Files:**
- Create: `crates/ferrosa-memory-core/src/remotes/feedback.rs`

**RED tests:**

- `no` -> negative feedback candidate.
- `stop` -> stop_signal with high weight.
- `WTF` -> strong negative requiring review.
- `that is Mac-only` -> wrong_scope with applicability correction.

### Task H2: Trust scoring updates

**Objective:** Apply positive/negative reinforcement per item/remote/namespace/scope.

**RED tests:**

- policy-chosen item gets small boost.
- user confirmation gets larger boost.
- repeated success accumulates.
- wrong_scope demotes item in that scope, not globally.
- `not_trusted_for` can be derived after repeated strong negatives.

### Task H3: MCP feedback tool

**Objective:** Expose `feedback_record`, `usage_mark`, and `trust_update` safely.

**RED tests:**

- feedback cannot forge another tenant.
- negative feedback creates queryable explanation.
- stop_signal can be surfaced to Hermes as halt/current-chain guidance.

---

## Packet I — Skill teaching path

### Task I1: Skill teaching packet types

**Objective:** Add skill-specific teaching structs or extend TeachingItem safely.

**RED tests:**

- skill packet includes steps, prerequisites, triggers, verification, pitfalls.
- skill packet cannot be committed through normal memory pull path.

### Task I2: Skill pull preview

**Objective:** Ask a remote to teach a skill separately from knowledge.

**RED tests:**

- `grant(read, skills)` required.
- skill autocommit denied by default.
- personal remote with explicit skill autocommit can create active candidate only if safe.

### Task I3: Skill commit/proposal

**Objective:** Create candidate skill docs or fmem skill entries with provenance and review state.

**RED tests:**

- team skill requires review.
- prompt-injection-like skill content quarantined.
- imported skill preserves source/provenance and does not overwrite local skill without explicit approval.

---

## Packet J — Archive remotes and pruning

### Task J1: Archive candidate selection

**Objective:** Identify cold large detail that can move to archive while retaining local stubs.

**RED tests:**

- warm/recently used items are not candidates.
- cold low-warmth large items are candidates.
- active operational facts are not archived if current/frequently used.

### Task J2: Archive commit

**Objective:** Push detail to archive remote and keep local signed stub.

**RED tests:**

- archive remote receives signed packet/detail.
- local record becomes archived/active_stub.
- archive is trusted for historical_detail but not current_ops by default.

### Task J3: Archive detail fetch

**Objective:** Fetch archived detail on demand.

**RED tests:**

- stub routes to archive detail fetch.
- archive detail does not override newer active local fact without conflict review.

---

## Packet K — CLI/MCP schemas and docs polish

### Task K1: Remote management MCP tools

**Objective:** Add tools for remote registry and policy management.

Tools:

- `remote_list`
- `remote_add`
- `remote_update_policy`
- `remote_remove`
- `remote_health`
- `remote_capabilities`
- `remote_explain_policy`

**RED tests:**

- all tools appear in `tools/list`.
- invalid remote config rejected.
- remove does not delete imported provenance; it disables/fails future fetches.

### Task K2: CLI binary/subcommands if applicable

**Objective:** Add CLI wrapper for remote operations if project has a CLI binary surface.

Commands:

```bash
ferrosa-memory remote list
ferrosa-memory remote add gpu ...
ferrosa-memory pull gpu "query" --preview
ferrosa-memory pull gpu "query" --commit-safe
ferrosa-memory detail gpu <detail-ref>
```

**RED tests:**

- CLI parses commands.
- preview prints import plan.
- commit requires explicit flag unless autocommit policy applies.

### Task K3: Docs update

**Objective:** Update specs index and user docs.

Files:

- `specs/README.md`
- `specs/threat-model.md` if threat additions are desired
- `specs/project-plan.md` only if maintaining sprint tracker is desired

**Acceptance:** User can find the remote memory blueprint and plan from the specs index.

---

## Packet L — System and integration tests

### Task L1: In-process two-memory harness

**Objective:** Build a test harness with teacher and learner using MockStorage or temp stores.

**RED tests:**

- teacher with gpu facts returns signed packet.
- learner imports active/stub/conflict states correctly.

### Task L2: Live MCP smoke tests

**Objective:** Test through MCP tool dispatch with a running local service where feasible.

**Tests:**

- `tools/list` contains remote tools.
- `remote_add`/`remote_list` works.
- `pull_preview` returns import plan.
- `pull_commit` writes provenance/stub.

Mark live cluster tests `#[ignore]` with clear reason/env vars.

### Task L3: Security regression suite

**Objective:** Verify prompt injection, secret risk, signature replay, and policy denies.

**Tests:**

- mutated packet rejected.
- expired packet rejected.
- injection item quarantined.
- raw context denied by default.
- deny overrides grant.

### Task L4: Performance/streaming guard

**Objective:** Guard against materializing whole remote graph before first event.

**Tests:**

- mocked retrieval stream emits start event immediately.
- query path can stop after first N events.
- no `Vec` accumulation of all hits before stream start in core stream function.

---

## Suggested branch/commit sequence

1. `feat: add remote memory identity signatures`
2. `feat: add remote memory data model`
3. `feat: add remote memory storage tables`
4. `feat: add remote policy evaluation`
5. `feat: add applicability and safety classifiers`
6. `feat: add teacher query packets`
7. `feat: add learner pull preview`
8. `feat: commit pulled memory with provenance`
9. `feat: add detail refs and stubs`
10. `feat: add remote feedback reinforcement`
11. `feat: add skill teaching preview`
12. `feat: add archive remote lifecycle`
13. `feat: expose remote memory MCP tools`
14. `test: add remote memory system smokes`
15. `docs: document remote memory remotes`

Do not amend after push. Use fixup commits if needed.

---

## Verification commands

Use the actual commands from CI once implementation begins. Baseline expected Rust commands:

```bash
cargo fmt --all -- --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

Focused commands per packet are included above. Live CQL/MCP tests that require local services must be marked ignored and run explicitly with documented env vars.

---

## Handoff prompts for subagents

### Handoff: Packet A

Implement Packet A from `specs/plans/memory-remotes-project-plan.md` using strict TDD. Only touch identity/signature/config surfaces. Return exact files modified and RED/GREEN commands. Do not implement storage or policy.

### Handoff: Packet B

Implement Packet B from `specs/plans/memory-remotes-project-plan.md` using strict TDD. This is a mechanical trait/DDL/storage wiring task; do it as one coherent pass. Return migration number chosen, files modified, and tests run. Do not implement teaching/query business logic.

### Handoff: Packet C

Implement Packet C policy evaluation. Use existing Datalog engine where possible. Add explanation tests. Do not modify storage beyond using Packet B APIs.

### Handoff: Packet D

Implement deterministic applicability and safety classifiers. Keep optional small-model classifier as a trait seam only; do not add model dependency unless explicitly approved.

### Handoff: Packet E

Implement teacher-side teach query stream. Preserve streaming: emit start/negative/item events progressively. Do not materialize whole graphs before first event.

### Handoff: Packet F

Implement learner pull preview/commit with signature verification, policy decisions, duplicate/conflict detection, provenance, and import batches. Do not implement skill import or archive.

### Handoff: Packet G

Implement detail refs, progressive detail fetch, and stub-driven fetch behavior.

### Handoff: Packet H

Implement feedback/reinforcement, including strong negative signals (`NO`, `STOP`, `WTF`) and scoped trust demotion.

### Handoff: Packet I

Implement skill teaching separately from memory. Imported skills are candidate/review-first by default.

### Handoff: Packet J

Implement archive remote lifecycle: candidate selection, archive push, local stubs, detail fetch.

### Handoff: Packet L

Implement system/integration/security smokes. Verify acceptance scenarios and update docs with exact commands.
