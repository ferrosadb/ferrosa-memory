# Evaluation Regression Gates

## Status

Planned follow-up after the first BRIGHT-Pro eval wiring.

## Goal

Make memory quality evals behave like coverage: every change should either preserve benchmark quality or intentionally improve the baseline. Regressions in retrieval, duplicate suppression, consolidation quality, or memory learning should block feature and bug branches before they land.

## Benchmark Families

### BRIGHT-Pro

Source: arXiv `2605.04018`, "Rethinking Reasoning-Intensive Retrieval: Evaluating and Advancing Retrievers in Agentic Search Systems".

Use for reasoning-intensive retrieval:

- aspect-aware ground truth,
- alpha-nDCG,
- weighted aspect recall,
- fixed-round and adaptive-round protocols,
- agentic failure labels.

Current local status:

- `ferrosa-memory-eval` has pure BRIGHT-Pro metric helpers.
- Scenarios can define `[bright_pro]` aspect ground truth.
- Runner grades `hybrid_search` traces.
- CLI reports render BRIGHT-Pro scores.
- Corpus-backed BRIGHT-Pro fixtures can run through `ferrosa-memory-eval fixture-smoke`.

### MemoryBench

Source: arXiv `2510.17281`, "MemoryBench: A Benchmark for Memory and Continual Learning in LLM Systems".

Version note: the user referenced `2510.17281v4`, submitted December 12, 2025. The arXiv abstract page currently lists v7 as the latest version, submitted June 3, 2026. Fixture work should pin the exact paper/dataset version used for each baseline.

Use for continual memory learning:

- simulated explicit and implicit user feedback,
- procedural memory learned during service-time interaction,
- off-policy and on-policy evaluation,
- task-native scores across domains/languages/task types,
- efficiency and memory-update cost.

Ferrosa-memory adaptation:

- map feedback events to `record_outcome`, temporal facts, entity updates, and session capture,
- evaluate whether later retrieval/agent behavior reflects accumulated feedback,
- track forgetting by replaying earlier feedback after later contradictory or unrelated feedback,
- score both quality and cost.

Current local status:

- MemoryBench-style fixtures model static corpus, training conversations, synthetic conversations, feedback signals, and test cases.
- Synthetic fixtures use two-agent conversations and verify retrieval from additional synthetic conversations.
- Optional local Ollama-compatible generation is available through `fixture-smoke --use-local-llm`; deterministic fallback remains the CI default.
- Property tests generate synthetic topics, distractors, and BRIGHT-Pro corpus mutations.

## Official Corpus Downloads

Large official benchmark corpora must stay out of git. Populate a local ignored corpus directory with:

```bash
scripts/download-eval-corpora.sh
```

Defaults:

- BRIGHT-Pro from `yale-nlp/Bright-Pro`
- MemoryBench full corpus from `THUIR/MemoryBench-Full`
- output under `.eval-corpus/`

Useful variants:

```bash
scripts/download-eval-corpora.sh --corpus bright-pro
scripts/download-eval-corpora.sh --corpus memorybench --memorybench-variant balanced
scripts/download-eval-corpora.sh --corpus memorybench --memorybench-variant both
scripts/download-eval-corpora.sh --output-dir /data/ferrosa-eval-corpus --clean
```

Each downloaded dataset directory gets a `manifest.json` with repo id, requested revision, resolved SHA, file count, and byte totals.

## CI Shape

### Required PR Gate

Run deterministic, low-cost eval smoke tests on every PR:

- fixed seed,
- small scenario subset,
- no external paid model dependency,
- JSON report artifact,
- fail on hard correctness regressions,
- fail on metric drops beyond configured tolerance.

Initial gate candidates:

- BRIGHT-Pro static/fixed-round toy scenarios,
- smart_ingest exact/cross-session duplicate suppression,
- consolidation status and edge creation smoke,
- migration_status availability,
- record_outcome serialization and retrieval-miss scoring.

### Scheduled Benchmark Gate

Run heavier evals nightly or on manual dispatch:

- larger BRIGHT-Pro scenarios,
- MemoryBench-style feedback sequences,
- property/metamorphic sweeps,
- live retrieval backend if available,
- trend report against main-branch baseline.

Nightly failures should open an issue or produce a clear artifact, but should not block every PR until flakiness and runtime are controlled.

### Baseline Policy

Store baselines as versioned JSON:

```json
{
  "suite": "bright_pro_smoke",
  "version": 1,
  "git_sha": "baseline-sha",
  "metrics": {
    "alpha_ndcg": 0.82,
    "aspect_recall": 0.75,
    "duplicate_rate": 0.0
  },
  "thresholds": {
    "alpha_ndcg": { "min": 0.80 },
    "aspect_recall": { "min": 0.73 },
    "duplicate_rate": { "max": 0.01 }
  }
}
```

Baseline updates require an intentional command or reviewable artifact, not silent overwrite during CI.

## Property And Metamorphic Sweeps

Use property-based testing to search parameter space and expose tuning opportunities, rather than only checking fixed examples.

Target tunables:

- `smart_ingest` skip/update/create thresholds,
- cross-session exact/fuzzy/ANN duplicate rules,
- hybrid retrieval rank fusion weights,
- context expansion window sizes,
- consolidation thresholds and edge-confidence cutoffs,
- BRIGHT-Pro alpha/gamma values,
- MemoryBench feedback replay limits.

Example properties:

- Adding relevant evidence should not reduce aspect recall at the same cutoff.
- Duplicating same-aspect evidence should not improve alpha-nDCG more than adding new-aspect evidence.
- Exact same `(tenant, entity_name, entity_type)` across sessions should not create duplicate active entities.
- Tenant/session isolation must hold under generated interleavings.
- Increasing `k` should not decrease raw recall.
- Consolidation should be idempotent under repeated runs on unchanged input.
- A later explicit correction should supersede older feedback without deleting provenance.

The property suite should emit counterexample seeds and the parameter values that produced the failure. Those seeds become fixed regression scenarios.

## Implementation Slices

1. Add `eval-baseline-check` command that compares a report JSON against a checked-in baseline.
2. Add a required CI job for deterministic smoke evals.
3. Add report artifacts and human-readable regression summaries.
4. Add property/metamorphic tests for `smart_ingest` thresholds and BRIGHT-Pro metrics.
5. Add scheduled MemoryBench-style feedback replay scenarios.
6. Add baseline update workflow with explicit review.

## Acceptance Criteria

- [ ] PR CI runs a deterministic eval smoke suite.
- [ ] CI fails when any required metric crosses its threshold.
- [ ] CI uploads raw JSON and Markdown eval summaries.
- [x] Property-based tests cover synthetic MemoryBench retrieval and BRIGHT-Pro monotonic recall invariants.
- [ ] Baseline updates are explicit and reviewable.
- [ ] Nightly job runs heavier BRIGHT-Pro and MemoryBench-style suites.
