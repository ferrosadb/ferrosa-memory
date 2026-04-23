CREATE TABLE IF NOT EXISTS agent_memory.aliases_by_name (
    tenant_id uuid,
    alias_name text,
    scope_kind text,
    scope_ref text,
    alias_id uuid,
    canonical_tool text,
    parameter_map text,
    fixed_arguments text,
    args_templates text,
    status text,
    created_at timestamp,
    updated_at timestamp,
    PRIMARY KEY ((tenant_id, alias_name), scope_kind, scope_ref)
);
