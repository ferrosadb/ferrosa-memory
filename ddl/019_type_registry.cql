-- Type registry tables for dynamic entity and edge type discovery.
-- These tables are the source of truth for allowed types — the MCP server
-- reads them at startup and builds tool schemas dynamically.

CREATE TABLE IF NOT EXISTS agent_memory.entity_types (
    type_name text PRIMARY KEY,
    description text,
    created_at timestamp
);

CREATE TABLE IF NOT EXISTS agent_memory.edge_types (
    type_name text PRIMARY KEY,
    description text,
    src_types text,   -- comma-separated entity types that can be source (empty = any)
    dst_types text,   -- comma-separated entity types that can be destination (empty = any)
    created_at timestamp
);

-- Seed entity types
INSERT INTO agent_memory.entity_types (type_name, description, created_at)
VALUES ('person', 'A named person — user, colleague, stakeholder', toTimestamp(now()));
INSERT INTO agent_memory.entity_types (type_name, description, created_at)
VALUES ('place', 'A location, venue, or environment', toTimestamp(now()));
INSERT INTO agent_memory.entity_types (type_name, description, created_at)
VALUES ('event', 'A historical or planned event', toTimestamp(now()));
INSERT INTO agent_memory.entity_types (type_name, description, created_at)
VALUES ('concept', 'An abstract idea, pattern, or domain concept', toTimestamp(now()));
INSERT INTO agent_memory.entity_types (type_name, description, created_at)
VALUES ('org', 'An organization, team, or company', toTimestamp(now()));
INSERT INTO agent_memory.entity_types (type_name, description, created_at)
VALUES ('decision', 'An architectural or design decision and its rationale', toTimestamp(now()));
INSERT INTO agent_memory.entity_types (type_name, description, created_at)
VALUES ('pattern', 'A recurring code, design, or behavior pattern', toTimestamp(now()));
INSERT INTO agent_memory.entity_types (type_name, description, created_at)
VALUES ('preference', 'A user preference or working style observation', toTimestamp(now()));
INSERT INTO agent_memory.entity_types (type_name, description, created_at)
VALUES ('bug', 'A discovered bug. State: active=open, resolved=fixed. Context should include reproduction and resolution details.', toTimestamp(now()));
INSERT INTO agent_memory.entity_types (type_name, description, created_at)
VALUES ('document', 'A documentation file (spec, README, architecture doc)', toTimestamp(now()));
INSERT INTO agent_memory.entity_types (type_name, description, created_at)
VALUES ('section', 'A section within a document (heading-delimited)', toTimestamp(now()));

-- Seed edge types
INSERT INTO agent_memory.edge_types (type_name, description, src_types, dst_types, created_at)
VALUES ('depends_on', 'Source depends on destination (code dependency)', '', '', toTimestamp(now()));
INSERT INTO agent_memory.edge_types (type_name, description, src_types, dst_types, created_at)
VALUES ('contains', 'Source contains destination (parent-child hierarchy)', '', '', toTimestamp(now()));
INSERT INTO agent_memory.edge_types (type_name, description, src_types, dst_types, created_at)
VALUES ('calls', 'Source calls/imports destination (code usage)', '', '', toTimestamp(now()));
INSERT INTO agent_memory.edge_types (type_name, description, src_types, dst_types, created_at)
VALUES ('references', 'Source references destination (documentation cross-ref)', 'document,section', '', toTimestamp(now()));
INSERT INTO agent_memory.edge_types (type_name, description, src_types, dst_types, created_at)
VALUES ('CO_OCCURS', 'Entities co-occur in the same context (consolidation)', '', '', toTimestamp(now()));
INSERT INTO agent_memory.edge_types (type_name, description, src_types, dst_types, created_at)
VALUES ('MENTIONED_IN', 'Entity is mentioned in a fold', '', '', toTimestamp(now()));
INSERT INTO agent_memory.edge_types (type_name, description, src_types, dst_types, created_at)
VALUES ('FOLDED_INTO', 'Fold is a child of another fold', '', '', toTimestamp(now()));
INSERT INTO agent_memory.edge_types (type_name, description, src_types, dst_types, created_at)
VALUES ('SUPERSEDES', 'Entity supersedes a prior version', '', '', toTimestamp(now()));
