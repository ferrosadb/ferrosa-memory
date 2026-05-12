# Remote Memory Decisions

> Created: 2026-05-11
> Status: Decision record for Ferrosa Memory Remotes / teacher-learner memory transfer.

## Core framing

Remote memory is not synchronization. It is teacher/learner knowledge transfer.

A local learner memory asks a remote teacher memory what it knows about a question, topic, skill, or namespace. The teacher returns a signed, compact, provenance-rich teaching packet. The learner decides what to import, stub, activate, quarantine, or reject under local policy.

## Decisions

### D1. Teacher/learner, not sync

The feature must not try to make two memory systems identical. A learner imports useful recall from a teacher. Exact migration/full graph replication remains a separate admin-path concern.

### D2. Local-first hierarchy

Local memory is always queried first. Remotes are consulted only when:

- the user explicitly asks, e.g. `consult gpu`;
- local results are below threshold;
- local results are stale;
- local results lack the needed OS/host/repo/scope;
- local contains a stub pointing to remote detail;
- Datalog fallback policy allows a remote for the query namespace.

### D3. Writes go local by default

Pulled knowledge is written to the learner's local memory. Writes upward to team/enterprise/partner remotes require explicit promotion.

### D4. Personal remotes may autocommit only with explicit consent

A personal remote can autocommit safe non-conflicting knowledge only if the user explicitly grants autocommit for that remote/namespace/kind. Consent is required even for owned machines.

### D5. Both sides enforce policy

Teacher-side policy controls what the teacher reveals.

Learner-side policy controls what the learner trusts, imports, stubs, activates, quarantines, or rejects.

### D6. Trust is per namespace/scope, not global

A remote may be trusted for one namespace and explicitly not trusted for another.

Example:

```prolog
trusted_for(gpu, research).
trusted_for(gpu, gpu_builds).
not_trusted_for(gpu, deployment_info).
```

### D7. Datalog rules are the policy/control plane

Remote identity, machine properties, grants, denies, trust, fallback behavior, autocommit behavior, and applicability should be represented as Datalog facts/rules so decisions are explainable.

### D8. Provenance and non-repudiation are mandatory

Teaching packets are signed by the teacher. Import decisions are signed by the learner. Both packet and import records store policy hashes, timestamps, source identifiers, content hashes, and signatures.

### D9. Stubs are first-class local knowledge

A local stub means the learner knows where deeper knowledge lives and may have enough summary for simple queries. Stubs include remote ID, summary, scope, detail reference, trust score, and `more_available` metadata.

### D10. Progressive disclosure by default

Teacher responses should be relatively small summaries with `more_available`/`detail_ref` pointers. The learner or LLM can request full detail only if policy allows it.

### D11. Skills and memories are separate

Knowledge pull and skill teaching use separate paths. Skills have procedural authority and require stricter review.

Examples:

- `memory pull gpu "vLLM benchmark setup"`
- `skill pull gpu "vLLM benchmarking"`

### D12. Applicability frames are required for operational knowledge

Operational facts should be scoped by OS, host, hardware, deployment runtime, repo, service, environment, date/freshness, and confidence.

Linux/Docker GPU-box knowledge must not be applied to Mac/Podman laptop workflows unless explicitly relevant.

### D13. Semantic scope matching can use a small local model, but rules decide

A deterministic extractor should exist first. A small local model may help classify natural language into applicability frames and entity references. Datalog policy still makes the final trust/import/applicability decision.

### D14. Remote content is data, not authority

Remote memory content may inform answers. It must not silently alter system/developer prompts, tool permissions, security settings, user profile, model routing, or behavioral preferences.

### D15. Prompt-injection and secret-risk classification are required

Instruction-like, prompt-injection-like, or secret-like content is quarantined or redacted. Procedures from non-personal remotes require review and belong in the skill path.

### D16. Trusted machine imports become active; untrusted imports require activation

Trusted personal remotes with consented autocommit can import safe knowledge as active. Untrusted, partner, and most team remotes return summaries/stubs that require user activation.

### D17. Conflict detection preserves competing claims

Conflicts are surfaced, not destructively overwritten. The chosen item gets a small trust boost, but losing/conflicting items remain as evidence with provenance.

### D18. Trust reinforcement is additive

Positive signals:

- policy chose item: small boost
- answer used item: small boost
- user confirmed: medium boost
- tool/action succeeded: large boost
- repeated successful use: additive reinforcement

Negative signals:

- irrelevant retrieval: small demotion
- user says wrong/irrelevant: medium-large demotion
- action failed because of memory: large demotion
- `NO`, `STOP`, `WTF`: strong negative feedback and possible quarantine/review

Reinforcement applies per item, remote, namespace, and applicability frame.

### D19. Negative knowledge is important

No-result, stale-result, not-trusted, not-applicable, wrong-scope, and user-stop signals should be durable negative knowledge. This prevents repeated bad remote calls and repeated misuse of scoped facts.

### D20. Archive is a remote type

Local memory can prune cold details by pushing them to an archive remote and keeping searchable local stubs. Archive memory is trusted for historical detail, not current operations by default.

### D21. Teacher responses should include negative results

The teacher can say:

- no relevant knowledge;
- only stale knowledge;
- relevant only for Mac/Podman;
- detail exists but grants deny access;
- the relevant item is superseded;
- teacher is not trusted for this namespace.

### D22. Enterprise/partner path uses grants modeled after GitHub

Grants and denies should cover:

- read knowledge
- read skills
- read context summaries
- read raw context
- read user profile
- read intentions
- detail fetch
- autocommit
- promote/write
- fallback use

Deny rules override grants.

## First implementation target

The first useful implementation should prove:

1. Remote registry and Datalog policy facts.
2. Signed teacher packet for a query.
3. Learner-side pull preview/import decision.
4. Autocommit of safe personal trusted knowledge.
5. Stub creation and detail refs.
6. Scope mismatch prevention for Linux/Docker vs Mac/Podman.
7. Negative feedback demotion.

See `memory-remotes-teacher-learner.md` for the full blueprint and `plans/memory-remotes-project-plan.md` for the multi-agent project plan.
