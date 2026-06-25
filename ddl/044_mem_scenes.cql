-- Ferrosa Memory MCP Server — MemScene consolidation
-- Run against Ferrosa CQL interface after 043_entity_content_fts.cql
--
-- A "scene" is a durable, summarized cluster of related entities produced by
-- dream consolidation. Retrieval can select a coherent scene (and its member
-- entities) as a single semantic unit, instead of stitching together loose
-- individual hits — improving multi-hop and temporal recall.

USE agent_memory;

CREATE TABLE IF NOT EXISTS mem_scenes (
    tenant_id     uuid,
    session_id    uuid,
    scene_id      uuid,
    member_ids    list<uuid>,
    summary       text,
    member_count  int,
    created_at    timestamp,
    PRIMARY KEY ((tenant_id, session_id), scene_id)
);
