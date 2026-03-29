-- Heat telemetry for promotion decisions
-- B10 — Workload-driven promotion pipeline

CREATE TABLE IF NOT EXISTS agent_memory.query_heat_by_predicate_day (
    tenant_id uuid,
    day text,
    pred text,
    hits counter,
    PRIMARY KEY ((tenant_id, day), pred)
);

CREATE TABLE IF NOT EXISTS agent_memory.compute_cost_by_predicate_day (
    tenant_id uuid,
    day text,
    pred text,
    total_compute_ms counter,
    total_requests counter,
    PRIMARY KEY ((tenant_id, day), pred)
);
