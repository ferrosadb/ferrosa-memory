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

-- Lexical/document indexing: ferrosa_user writes these on the serving path
-- during document/context ingest (CqlStorage::context_segment_put /
-- document_chunk_put). Missing grants make document ingest fail under auth.
GRANT MODIFY ON agent_memory.context_segments        TO ferrosa_user;
GRANT MODIFY ON agent_memory.context_segment_terms   TO ferrosa_user;
GRANT MODIFY ON agent_memory.document_chunks         TO ferrosa_user;
GRANT MODIFY ON agent_memory.document_terms          TO ferrosa_user;
GRANT MODIFY ON agent_memory.document_phonetic_terms TO ferrosa_user;

-- Session task continuity (migration 039): written by the session-task tools
-- via CqlStorage on the runtime session.
GRANT MODIFY ON agent_memory.session_tasks_by_id      TO ferrosa_user;
GRANT MODIFY ON agent_memory.session_tasks_by_status  TO ferrosa_user;
GRANT MODIFY ON agent_memory.session_tasks_by_workspace TO ferrosa_user;
GRANT MODIFY ON agent_memory.session_task_events      TO ferrosa_user;
GRANT MODIFY ON agent_memory.session_task_focus_stack TO ferrosa_user;
GRANT MODIFY ON agent_memory.session_task_policies    TO ferrosa_user;
GRANT MODIFY ON agent_memory.session_task_aliases     TO ferrosa_user;

-- Alias + approval stores (CqlStorage::alias_put / approval_append).
GRANT MODIFY ON agent_memory.aliases_by_name     TO ferrosa_user;
GRANT MODIFY ON agent_memory.approvals_by_target TO ferrosa_user;

-- Type registry: seeded by the runtime ferrosa_user at MCP server STARTUP
-- (seed_sprint1_types), not by the migrate binary. Without these grants an
-- auth-enabled cold start logs `entity insert failed` as a non-fatal warn and
-- the type/edge registry silently never persists.
GRANT MODIFY ON agent_memory.entity_types TO ferrosa_user;
GRANT MODIFY ON agent_memory.edge_types   TO ferrosa_user;

-- Temporal edge stream (CqlStorage::temporal_edge_put). App-owned — NOT the
-- graph-engine edge tables listed below.
GRANT MODIFY ON agent_memory.temporal_edges TO ferrosa_user;

-- Consolidation, profile, retrieval-learning, and foresight tables are all
-- written by CqlStorage on the application serving path. Omitting these
-- grants makes an auth-enabled deployment fail only after a session reaches
-- the corresponding feature, rather than at migration time.
GRANT MODIFY ON agent_memory.consolidation_requests TO ferrosa_user;
GRANT MODIFY ON agent_memory.consolidation_runs     TO ferrosa_user;
GRANT MODIFY ON agent_memory.mem_scenes             TO ferrosa_user;
GRANT MODIFY ON agent_memory.mem_profiles           TO ferrosa_user;
GRANT MODIFY ON agent_memory.retrieval_traces       TO ferrosa_user;
GRANT MODIFY ON agent_memory.foresight_facts        TO ferrosa_user;

-- Direct mobile control (migrations 051-052): server-owned cursor allocation,
-- replay log, and idempotent typed-command results. The gateway never reads
-- these tables; only the local Ferrosa Memory runtime does.
GRANT MODIFY ON agent_memory.mobile_control_cursor_state TO ferrosa_user;
GRANT MODIFY ON agent_memory.mobile_control_events       TO ferrosa_user;
GRANT MODIFY ON agent_memory.mobile_control_commands     TO ferrosa_user;

-- Session archives (migration 053): a swiped-away session's raw tmux buffer is
-- written on the serving path, so the runtime role must be able to write it.
-- Without this an auth-enabled deployment fails on the first archive rather
-- than at migration time.
GRANT MODIFY ON agent_memory.session_archives TO ferrosa_user;

-- Knowledge tiers (migration 054): source provenance, alias and tier rules,
-- and human promotions. All four are written on the serving path -- ingest
-- records a source, and the permissions UI edits rules and promotions.
GRANT MODIFY ON agent_memory.entity_source   TO ferrosa_user;
GRANT MODIFY ON agent_memory.tier_root_alias TO ferrosa_user;
GRANT MODIFY ON agent_memory.tier_root_rule  TO ferrosa_user;
GRANT MODIFY ON agent_memory.tier_promotion  TO ferrosa_user;

-- graph-owned tables: NOT GRANTED to ferrosa_user on purpose.
