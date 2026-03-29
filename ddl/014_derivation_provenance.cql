-- Derivation provenance: tracks parent facts for each derived fact
-- Sprint 5 — STRIDE I8 mitigation: tenant_id in partition key

CREATE TABLE IF NOT EXISTS agent_memory.derivation_provenance (
    tenant_id uuid,
    derived_edge_id text,
    seq int,
    parent_src text,
    parent_pred text,
    parent_dst text,
    parent_kind text,
    PRIMARY KEY ((tenant_id, derived_edge_id), seq)
);
