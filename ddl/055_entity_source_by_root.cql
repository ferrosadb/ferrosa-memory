-- Ferrosa Memory MCP Server — a seekable, recency-ordered view of provenance.
-- Run after 054_knowledge_tiers.cql.
--
-- 054's entity_source is keyed ((tenant_id), entity_id). That answers "where
-- did THIS entity come from" in one read, which is what it was written for,
-- and it is the wrong shape for the browse list the Memory tab draws.
--
-- Browsing asks the opposite question — "the most recent N of this tier" —
-- and against 054 the only way to answer it is to read the whole tenant
-- partition, derive every row's tier, sort, and discard all but a page.
-- Measured on a 69,683-row tenant: 15.0 s, paid again on EVERY page, plus a
-- second scan of the same size for promotions. It is not only slow. Sustained
-- scans of one partition starve writes to the same keyspace, and control
-- sessions were failing their 30 s cursor-block reservation while the tab was
-- open. Measured after: 0.7-1.1 s for a page, and the same for page 40.
--
-- So this table is the same facts under the access pattern that reads them:
-- partitioned by ((tenant_id, source_root)), because a tier is the union of
-- its roots and there are one or two per tier, so the tier filter stops being
-- a post-filter over everything.
--
--
-- Why the clustering key is a synthetic string
--
-- The obvious key is (recorded_at, entity_id) with CLUSTERING ORDER BY
-- recorded_at DESC. That was written first and measured against this engine,
-- and it does not work here:
--
--   * Ferrosa ACCEPTS `WITH CLUSTERING ORDER BY (recorded_at DESC, ...)` and
--     then returns rows ascending anyway. The clause is not honoured, and it
--     does not error, so a table declared newest-first quietly serves
--     oldest-first. Reported upstream — see
--     ../ferrosa/specs/bug-clustering-order-desc-ignored.md.
--   * Ferrosa has no multi-column slice: `WHERE (recorded_at, entity_id) < (?, ?)`
--     is a syntax error, so the natural two-column keyset cursor cannot be
--     expressed. Single-column `<` and query-time `ORDER BY ... DESC` both
--     work, and are what this uses.
--
-- That leaves a single-column cursor, and `recorded_at` alone is not one: it
-- is not unique. Ingesting a directory writes many rows per millisecond, and a
-- `recorded_at < ?` cursor over tied rows either repeats them or steps over
-- them. Skipping rows is the worse half — the list would be missing entries
-- and would look complete.
--
-- page_key is therefore `{unix_millis:013}-{entity_id}`: unique by
-- construction, and lexicographically ordered by time because the millis are
-- zero-padded to a fixed width. 13 digits holds every timestamp until the year
-- 2286. It is verified against a 10-row millisecond: three pages, thirteen
-- rows, no gaps and no repeats.
--
-- recorded_at is kept as a plain column. It is what the list displays and what
-- a range query would filter on; it is simply not load-bearing for ordering.
--
-- Both tables are written on every source record. entity_source stays the
-- authority: it is the one keyed by entity_id, it is what restate_sources
-- rewrites, and it is what a repair reads to rebuild this one.
CREATE TABLE IF NOT EXISTS agent_memory.entity_source_by_root (
    tenant_id     uuid,
    source_root   text,
    page_key      text,
    entity_id     uuid,
    session_id    uuid,
    title         text,
    source_path   text,
    matched_alias text,
    recorded_at   timestamp,
    PRIMARY KEY ((tenant_id, source_root), page_key)
);
