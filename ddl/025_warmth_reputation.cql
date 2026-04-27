-- Backfill the `reputation` column on entity_warmth for clusters that
-- created the table before reputation was added to ddl/011_warmth_field.cql.
--
-- Fresh installs already get the column from the CREATE TABLE in 011 and
-- this ALTER will fail with "column already exists" — `migrate` treats that
-- as a no-op (see is_idempotent_already_exists in
-- crates/ferrosa-memory-mcp/src/tools/migrate.rs).
--
-- The Rust runtime path (cql_storage.rs warmth_put / warmth_get) reads and
-- writes this column unconditionally; without the column, PREPARE fails at
-- startup with "expected 10 bind-marker column spec(s) but resolved only 9".
--
-- This is additive and non-breaking: existing rows read back NULL, which
-- deserializes to 0.0 on the Rust side (see WarmthEntry default).

ALTER TABLE agent_memory.entity_warmth ADD reputation double;
