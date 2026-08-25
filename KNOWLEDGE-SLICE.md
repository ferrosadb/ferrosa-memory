# Knowledge slice — build checklist

Decisions: `specs/knowledge-tiers/decisions.md` D16–D49.

## Done before this checklist
- [x] Migration 056 applied live: `knowledge_item`, `knowledge_version`,
      `knowledge_by_state`, `knowledge_by_expiry`
- [x] forge: `TaskStatus::Draft`, `TaskOrigin` (defaults Agent), idempotent
      ALTER on connect, MCP `origin` advertised — verified `origin: human`
      end to end
- [x] `knowledge_by_expiry` reshaped for D45 while empty (state in PK,
      page_key from `expires_at`)

## Core — ferrosa-memory-core
- [x] `knowledge.rs`: states, legal transitions, demotion→tier, priority_band,
      expiry_day, page_key
- [x] tests: transitions, D46 demotion routing, band edges, page_key ties (15)
- [x] register the module in `lib.rs`
- [x] `KnowledgeStore` trait + `InMemoryKnowledgeStore` (22 tests)
- [x] `CqlKnowledgeStore` — 4 live conformance tests pass against the cluster
- [x] state change is a MOVE (D43) — verified by removing unindex, 2 tests go red
- [x] expiry sweep reads today's bucket

## Frames — ferrosa-memory-sync
- [x] `shell_knowledge` (approved only, D44)
- [x] `shell_knowledge_claims` (expiry-sorted, D45)
- [x] `shell_knowledge_detail` + `shell_knowledge_versions`
- [x] commands: approve / reject / send-back (send-back requires feedback)
- [x] EMITTED + size proof — caught a 1,287-byte frame, rows/frame now 2
- [x] classified Durable in `frame_priority`

## App — .wt-knowledge-ui (off ferrosa-mobile main)
- [ ] Knowledge tab: approved only, green check
- [ ] Claims tab: greyed robot, expiry sort, adjustable
- [ ] Work tab: draft column, blocked on top, archived hidden, origin split
- [ ] handle `capability_unavailable` so a refusal is not a spinner

## Deferred (captured on the board)
- artifact chunked upload/download over WebRTC — `t_0e1ed371`
- provenance edges from the retrieval tracker — `t_c1a32fe3`
- sandbox egress capture — `t_f0d7d337`
- ferrosa expiry-transition primitive — `t_ee413fb3`
- backfill `task.origin` — `t_5c72c2b7`
