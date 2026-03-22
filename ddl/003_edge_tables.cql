-- Graph edge tables for ferrosa-memory-mcp
-- These are CQL tables with graph annotations — Ferrosa's adjacency
-- index picks up INSERTs automatically for Cypher traversals.

USE agent_memory;

-- Fold hierarchy: child fold -> parent fold
CREATE TABLE IF NOT EXISTS folded_into (
    source_fold_id   uuid,
    target_fold_id   uuid,
    session_id       uuid,
    tenant_id        uuid,
    created_at       timestamp,
    PRIMARY KEY ((source_fold_id), target_fold_id)
);

-- Entity mentioned in fold
CREATE TABLE IF NOT EXISTS mentioned_in (
    entity_id        uuid,
    fold_id          uuid,
    session_id       uuid,
    tenant_id        uuid,
    created_at       timestamp,
    PRIMARY KEY ((entity_id), fold_id)
);

-- Entity co-occurrence
CREATE TABLE IF NOT EXISTS co_occurs_with (
    entity_a         uuid,
    entity_b         uuid,
    session_id       uuid,
    tenant_id        uuid,
    created_at       timestamp,
    PRIMARY KEY ((entity_a), entity_b)
);

-- Temporal supersession
CREATE TABLE IF NOT EXISTS supersedes (
    new_event_id     uuid,
    old_event_id     uuid,
    entity_id        uuid,
    tenant_id        uuid,
    created_at       timestamp,
    PRIMARY KEY ((new_event_id), old_event_id)
);

-- Graph annotations (run after table creation)
-- Vertex labels
ALTER TABLE trajectory_folds WITH extensions = {'graph.type': 'vertex', 'graph.label': 'Fold'};
ALTER TABLE entity_store WITH extensions = {'graph.type': 'vertex', 'graph.label': 'Entity'};
ALTER TABLE temporal_events WITH extensions = {'graph.type': 'vertex', 'graph.label': 'Fact'};

-- Edge labels
ALTER TABLE folded_into WITH extensions = {'graph.type': 'edge', 'graph.label': 'FOLDED_INTO', 'graph.source': 'source_fold_id', 'graph.target': 'target_fold_id', 'graph.source_label': 'Fold', 'graph.target_label': 'Fold'};
ALTER TABLE mentioned_in WITH extensions = {'graph.type': 'edge', 'graph.label': 'MENTIONED_IN', 'graph.source': 'entity_id', 'graph.target': 'fold_id', 'graph.source_label': 'Entity', 'graph.target_label': 'Fold'};
ALTER TABLE co_occurs_with WITH extensions = {'graph.type': 'edge', 'graph.label': 'CO_OCCURS_WITH', 'graph.source': 'entity_a', 'graph.target': 'entity_b', 'graph.source_label': 'Entity', 'graph.target_label': 'Entity'};
ALTER TABLE supersedes WITH extensions = {'graph.type': 'edge', 'graph.label': 'SUPERSEDES', 'graph.source': 'new_event_id', 'graph.target': 'old_event_id', 'graph.source_label': 'Fact', 'graph.target_label': 'Fact'};
