-- Ferrosa Memory MCP Server — Knowledge as a lifecycle.
-- Run after 055_entity_source_by_root.cql.
--
-- Every other tier is DERIVED: tier = f(source path), resolved through the
-- alias and rule tables and never stored, so re-tiering a directory is one
-- rule edit rather than a rewrite of every row.
--
-- Knowledge cannot work that way (D16). A deliverable is Knowledge because a
-- person curated it and because it is still true — its tier depends on its
-- STATE, its author and its version, not on where it sits. So Knowledge is
-- stored, and Information is the raw material it is made from (D25).
--
-- The tier a demoted item lands in says WHY it left (D34): an approved item
-- that went stale becomes Information, because the record of what was believed
-- is worth keeping; a rejected one becomes Data, because it is the residue of
-- an attempt rather than information about anything.

-- One deliverable, across all its revisions.
--
-- Partitioned by tenant and keyed by a stable id, because a share resolves to
-- the ITEM and a recipient browses to versions from there (D17). Making the
-- version the identity would mean a share pointed at a frozen thing and every
-- revision broke the link.
CREATE TABLE IF NOT EXISTS agent_memory.knowledge_item (
    tenant_id        uuid,
    knowledge_id     uuid,
    title            text,
    -- pull_request | presentation | report | ... (D22).
    -- Text rather than an enum: the set is meant to grow by rule, and a schema
    -- change per deliverable type is the thing that stops people adding one.
    kind             text,
    -- proposed | revisit | approved | expired | superseded | rejected (D16).
    state            text,
    -- Which version is current. Null while the first one is still pending.
    current_version  int,
    -- When approval lapses. Default 30 days at approval (D27); an item that
    -- should never expire is promoted to Wisdom instead of carrying a null
    -- here (D32), so a null on an APPROVED row is a bug rather than "forever".
    expires_at       timestamp,
    -- The agent that produced it, so it can be sent back with feedback rather
    -- than silently rejected (D24). Session id too, because the agent may be
    -- gone by review time and its session is what a replacement picks up.
    author_agent     text,
    author_session   uuid,
    -- The work this answers. Supplies the overview's ordering (D23).
    task_id          text,
    priority         int,
    repo             text,
    created_at       timestamp,
    updated_at       timestamp,
    -- Who reviewed, and when. Null until a person has looked (D26).
    reviewed_by      text,
    reviewed_at      timestamp,
    PRIMARY KEY ((tenant_id), knowledge_id)
);

-- The version chain.
--
-- Clustered ascending by version so the history reads in the order it happened
-- and a range read gets a prefix. Not DESC: this engine accepts
-- CLUSTERING ORDER BY ... DESC and then ignores it (see 055 and
-- ../ferrosa/specs/bug-clustering-order-desc-ignored.md), so declaring a
-- direction it will not honour would be a lie in the schema. Readers that want
-- newest-first say ORDER BY at query time, which IS honoured.
CREATE TABLE IF NOT EXISTS agent_memory.knowledge_version (
    tenant_id     uuid,
    knowledge_id  uuid,
    version       int,
    -- Where the deliverable actually is. For a pull request this is the URL
    -- and nothing is stored (D19) — Ferrosa holds no token and takes no action
    -- on GitHub, so approving one is a link a person follows under their own
    -- identity. For a file it is the artifact id, resolved through the chunk
    -- store once that exists (D18, D35).
    body_url      text,
    artifact_id   uuid,
    summary       text,
    -- approved | superseded | pending. Approval is per VERSION; the item's
    -- state is the lifecycle. Superseding is automatic on the next approval
    -- (D30).
    version_state text,
    -- The feedback that sent the PREVIOUS version back, carried forward so the
    -- next one can be read against what was asked for (D36).
    feedback      text,
    created_by    text,
    created_at    timestamp,
    approved_at   timestamp,
    PRIMARY KEY ((tenant_id, knowledge_id), version)
);

-- The review queue, newest first at read time.
--
-- A separate partition per state so the Approvals tab reads only what needs a
-- person, rather than filtering the whole tenant. `proposed` is the one that
-- matters; the others are here so a state change is a move between partitions
-- and not a scan to find the row.
--
-- page_key repeats the trick from 055: {millis:013}-{knowledge_id}, unique by
-- construction, because recorded time alone is not — several deliverables can
-- land in one millisecond and a cursor over tied rows steps over them.
CREATE TABLE IF NOT EXISTS agent_memory.knowledge_by_state (
    tenant_id     uuid,
    state         text,
    -- 'high' when priority >= 50, else 'low'.
    --
    -- In the partition key, because the overview's order is
    -- new-high, older-high, new-low, old-low (D23) -- which is two bands by
    -- recency, not a sort. As a regular column it would be a client-side sort
    -- of whatever page came back, and that is wrong the moment there is more
    -- than one page. Two seeks instead: read the high band, then the low.
    priority_band text,
    page_key      text,
    knowledge_id  uuid,
    title         text,
    kind          text,
    priority      int,
    repo          text,
    expires_at    timestamp,
    PRIMARY KEY ((tenant_id, state, priority_band), page_key)
);

-- Which items lapse on which day.
--
-- Expiry demotes rather than deletes (D34), so a TTL is the wrong primitive --
-- it would destroy the record the design keeps on purpose. The sweep is app
-- work, and this makes it cheap: read today's bucket, which is O(items
-- expiring today) rather than O(everything the tenant has ever approved).
--
-- Bucketed by day as `YYYY-MM-DD`, so a sweeper that missed a day can catch up
-- by reading the days it missed, and a late start does not mean a scan.
CREATE TABLE IF NOT EXISTS agent_memory.knowledge_by_expiry (
    tenant_id    uuid,
    expiry_day   text,
    page_key     text,
    knowledge_id uuid,
    expires_at   timestamp,
    PRIMARY KEY ((tenant_id, expiry_day), page_key)
);
