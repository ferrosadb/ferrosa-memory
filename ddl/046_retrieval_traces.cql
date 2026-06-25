-- Ferrosa Memory MCP Server — retrieval traces
-- Run against Ferrosa CQL interface after 045_mem_profiles.cql
--
-- A durable record of each hybrid_search: the query, which sources produced
-- candidates, the returned result ids, and how many. Lets fmem learn from
-- successful vs unhelpful retrievals (tune fusion weights, detect regressions)
-- offline. Clustered by created_at DESC so recent traces read first.

USE agent_memory;

CREATE TABLE IF NOT EXISTS retrieval_traces (
    tenant_id     uuid,
    session_id    uuid,
    created_at    timestamp,
    trace_id      uuid,
    query         text,
    source_counts text,          -- compact JSON: {"source": candidate_count, ...}
    result_ids    list<uuid>,
    result_count  int,
    PRIMARY KEY ((tenant_id, session_id), created_at, trace_id)
) WITH CLUSTERING ORDER BY (created_at DESC, trace_id ASC);
