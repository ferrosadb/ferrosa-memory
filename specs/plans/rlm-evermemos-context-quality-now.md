# RLM + EverMemOS context-quality eval checklist

This branch implements the Now slice from the RLM/EverMemOS roadmap. It gives fmem a deterministic evaluation contract for context quality before live recursive retrieval and lifecycle management are fully wired.

## Now checklist

- [x] Add an eval harness skeleton that can represent current hooks, score-threshold search, RLM controller, EverMemOS lifecycle, combined RLM+EverMemOS, and combined+native-FTS modes.
- [x] Add typed memory lanes for episodic, semantic, procedural, corpus, task, profile, foresight, and bug/decision memories.
- [x] Add an EverMemOS-style MemCell shape with episode, atomic facts, time-bounded foresight, and provenance metadata.
- [x] Add a MemScene shape for later scene-level consolidation experiments.
- [x] Add an RLM-style controller v0 that gates candidates before context injection.
- [x] Add silence/clutter metrics so low-confidence retrieval can be rewarded for returning nothing.
- [x] Add a CLI summary command for smoke checks: `ferrosa-memory-eval rlm-evermemos-plan`.

## Controller v0 policy

- Require provenance by default.
- Drop raw episodic `raw_context` candidates by default.
- Apply source-aware lane thresholds instead of a single score cutoff.
- Filter expired foresight candidates.
- Enforce accepted-count and injected-token budgets.
- Preserve dropped-result reasons for later training/eval traces.

## Deferred

The follow-on work is tracked in the forge task board:

- MemScene consolidation from MemCells.
- Profile/workspace-state summaries updated from scenes.
- Time-bounded foresight with explicit future-effective intervals.
- Scope correctness for global/nil corpus search.
- Accepted/dropped retrieval trace storage.
- Learned retrieval policy.
- Benchmark packs.
- Native FTS ablation integration once FTS returns rows.
