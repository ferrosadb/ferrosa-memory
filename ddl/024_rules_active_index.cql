-- Active rule index for wildcard registry/effective listing without filtering scans.
-- Supports tenant+state scoped enumeration across all families.

CREATE TABLE IF NOT EXISTS agent_memory.rules_active_by_state (
    tenant_id uuid,
    state text,
    family text,
    rule_id text,
    version int,
    updated_at timestamp,
    PRIMARY KEY ((tenant_id, state), family, rule_id, version)
) WITH CLUSTERING ORDER BY (family ASC, rule_id ASC, version DESC);
