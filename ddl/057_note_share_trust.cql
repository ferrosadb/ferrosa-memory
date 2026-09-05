-- Owner-authoritative note-share state in the replicated trust/system data.
-- The signed entitlement is retained for verification; note content is never
-- copied into this table. Read counts advance only after a successful result.
CREATE TABLE IF NOT EXISTS agent_memory.note_share_entitlements (
       share_id uuid PRIMARY KEY,
       owner_account_id uuid,
       note_id uuid,
    recipient_account_id uuid,
    entitlement_json text,
    expires_at timestamp,
       revoked boolean
);

CREATE TABLE IF NOT EXISTS agent_memory.note_share_read_counters (
       share_id uuid PRIMARY KEY,
       successful_reads counter
);

CREATE TABLE IF NOT EXISTS agent_memory.note_share_audit (
    share_id uuid,
    occurred_at timestamp,
    event_id timeuuid,
    recipient_account_id uuid,
    result text,
    PRIMARY KEY ((share_id), occurred_at, event_id)
) WITH CLUSTERING ORDER BY (occurred_at DESC);
