-- Ferrosa Memory MCP Server — DIKW knowledge tiers and their provenance.
-- Run after 053_session_archives.cql.
--
-- A tier (Data / Information / Knowledge / Wisdom) is not stored on the
-- entity. It is DERIVED from where the entity came from, so that curating a
-- directory re-tiers everything under it instead of requiring a rewrite of
-- every row. Three inputs, one override:
--
--   entity_source     where this entity came from, as a path
--   tier_root_alias   two paths that name the same root (bkearns/research
--                     and research), longest prefix wins
--   tier_root_rule    a root's tier
--   tier_promotion    a human overriding the derived tier for one entity
--
-- The rules are also emitted as Datalog (see tiers.rs) so a sharing grant can
-- reason about tiers on the graph rather than in Rust only.

CREATE TABLE IF NOT EXISTS agent_memory.entity_source (
    tenant_id     uuid,
    entity_id     uuid,
    source_path   text,     -- as supplied by the ingest caller
    source_root   text,     -- canonical root after alias resolution
    matched_alias text,     -- which alias rule fired, or null for a literal
                            -- match; without it a mis-tier is unattributable
    recorded_at   timestamp,
    PRIMARY KEY ((tenant_id), entity_id)
);

-- Tier assignment walks root -> entities, so the root must be queryable.
CREATE INDEX IF NOT EXISTS idx_entity_source_root
    ON agent_memory.entity_source (source_root);

-- Alias rules are few (tens) and read on every resolve, so one partition per
-- tenant is the right shape: the whole rule set loads in a single read.
CREATE TABLE IF NOT EXISTS agent_memory.tier_root_alias (
    tenant_id      uuid,
    alias_prefix   text,    -- e.g. bkearns/research/corpus
    canonical_root text,    -- e.g. research/corpus
    created_at     timestamp,
    created_by     text,
    PRIMARY KEY ((tenant_id), alias_prefix)
);

CREATE TABLE IF NOT EXISTS agent_memory.tier_root_rule (
    tenant_id  uuid,
    root       text,        -- canonical root, e.g. research/skills
    tier       text,        -- data | information | knowledge | wisdom
    created_at timestamp,
    created_by text,
    note       text,        -- why this root earns this tier
    PRIMARY KEY ((tenant_id), root)
);

-- A promotion overrides the derived tier for ONE entity. It carries the actor
-- and the reason because a promotion is a curation claim, and an unattributed
-- claim cannot be reviewed or revoked.
CREATE TABLE IF NOT EXISTS agent_memory.tier_promotion (
    tenant_id  uuid,
    entity_id  uuid,
    tier       text,
    actor      text,
    reason     text,
    created_at timestamp,
    PRIMARY KEY ((tenant_id), entity_id)
);
