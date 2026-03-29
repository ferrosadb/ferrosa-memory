-- Datalog rule registry with versioning
-- Sprint 5 — Datalog inference engine

CREATE TABLE IF NOT EXISTS agent_memory.rules_by_id (
    tenant_id uuid,
    rule_id text,
    version int,
    name text,
    family text,
    state text,
    rule_body text,
    rule_weight double,
    incremental boolean,
    created_at timestamp,
    updated_at timestamp,
    PRIMARY KEY ((tenant_id, rule_id), version)
) WITH CLUSTERING ORDER BY (version DESC);

CREATE TABLE IF NOT EXISTS agent_memory.rules_by_family (
    tenant_id uuid,
    family text,
    state text,
    rule_id text,
    version int,
    updated_at timestamp,
    PRIMARY KEY ((tenant_id, family, state), rule_id, version)
) WITH CLUSTERING ORDER BY (rule_id ASC, version DESC);
