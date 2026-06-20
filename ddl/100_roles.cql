-- Seed roles for the ferrosa-memory cluster. Applied after system
-- keyspaces exist. Idempotent via IF NOT EXISTS.
--
-- The two base roles are also seeded in-process by ferrosa's
-- startup bootstrap (see ferrosa-schema::auth::bootstrap), so on
-- a fresh cluster this file's CREATE ROLE statements are no-ops.
-- The file is retained for operators who rebuild from cold DDL
-- and for the grant statements below (which the in-process
-- bootstrap intentionally does NOT perform — application topology
-- drives grants, not the server binary).
--
-- See specs/decisions/design-cql-role-auth-rollout.md Sprint B.

USE system_auth;

CREATE ROLE IF NOT EXISTS ferrosa_admin
  WITH PASSWORD = 'ferrosa_admin' AND LOGIN = true AND SUPERUSER = true;

CREATE ROLE IF NOT EXISTS ferrosa_user
  WITH PASSWORD = 'ferrosa_user' AND LOGIN = true;

-- Application grants — ferrosa_user can read everything in agent_memory
-- but write only to application-owned tables. The graph-owned tables
-- (typed_edges, folded_into, mentioned_in, co_occurs_with, supersedes,
-- derived_edges_by_pred, derived_edges_by_src) are NOT granted MODIFY
-- to ferrosa_user — they are reachable only via Cypher under
-- ferrosa_admin.

USE agent_memory;

GRANT SELECT ON KEYSPACE agent_memory TO ferrosa_user;

-- Application-owned tables (writable by ferrosa_user)
GRANT MODIFY ON agent_memory.entity_store      TO ferrosa_user;
GRANT MODIFY ON agent_memory.tool_usage_log    TO ferrosa_user;
GRANT MODIFY ON agent_memory.temporal_events   TO ferrosa_user;
GRANT MODIFY ON agent_memory.feedback_outcomes TO ferrosa_user;
GRANT MODIFY ON agent_memory.intentions        TO ferrosa_user;
GRANT MODIFY ON agent_memory.memo_cache        TO ferrosa_user;
GRANT MODIFY ON agent_memory.plan_state        TO ferrosa_user;
GRANT MODIFY ON agent_memory.trajectory_folds  TO ferrosa_user;
GRANT MODIFY ON agent_memory.audit_log         TO ferrosa_user;
GRANT MODIFY ON agent_memory.entity_warmth     TO ferrosa_user;
GRANT MODIFY ON agent_memory.rules_by_id       TO ferrosa_user;
GRANT MODIFY ON agent_memory.rules_by_family   TO ferrosa_user;
GRANT MODIFY ON agent_memory.rules_active_by_state TO ferrosa_user;
GRANT MODIFY ON agent_memory.derived_cache_by_query  TO ferrosa_user;
GRANT MODIFY ON agent_memory.derived_cache_by_pred   TO ferrosa_user;
GRANT MODIFY ON agent_memory.derived_cache_ttl_track TO ferrosa_user;
GRANT MODIFY ON agent_memory.derivation_provenance   TO ferrosa_user;
GRANT MODIFY ON agent_memory.confidence_scores       TO ferrosa_user;

-- Forget feature (v0.16): retraction audit/restore records + the forget
-- journal are application-owned and written by ferrosa_user via the forget
-- tool and its purge sweep.
GRANT MODIFY ON agent_memory.retraction    TO ferrosa_user;
GRANT MODIFY ON agent_memory.forget_journal TO ferrosa_user;

-- Remote knowledge sharing (migration 041): teacher/learner transfer tables.
-- All application-owned, written by ferrosa_user via the remote-memory tools
-- (teach/pull/import). Additive grants — additive to the forget feature above.
GRANT MODIFY ON agent_memory.memory_remotes      TO ferrosa_user;
GRANT MODIFY ON agent_memory.remote_policy_facts TO ferrosa_user;
GRANT MODIFY ON agent_memory.teaching_packets    TO ferrosa_user;
GRANT MODIFY ON agent_memory.teaching_items      TO ferrosa_user;
GRANT MODIFY ON agent_memory.remote_stubs        TO ferrosa_user;
GRANT MODIFY ON agent_memory.memory_provenance   TO ferrosa_user;
GRANT MODIFY ON agent_memory.memory_conflicts    TO ferrosa_user;
GRANT MODIFY ON agent_memory.memory_feedback     TO ferrosa_user;
GRANT MODIFY ON agent_memory.import_batches      TO ferrosa_user;

-- graph-owned tables: NOT GRANTED to ferrosa_user on purpose.
