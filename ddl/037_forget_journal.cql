-- Forget journal: durable, replayable record of each forget operation and its
-- per-step progress. The journal is the source of truth for atomicity: a
-- multi-target / multi-store forget writes the journal entry FIRST (single
-- partition, the one place durability is required), then drives disposition of
-- the item + edges + temporal links + derived rows from a resumable sweep keyed
-- on the journal. A crash mid-forget is recovered by re-running the sweep.
--
-- (Accord transactions are not used: the LWT->Accord path in Ferrosa is a stub
-- that does not persist mutations — see ../ferrosa/specs/bug-accord-lwt-acks-phantom-write.md.)

CREATE TABLE IF NOT EXISTS agent_memory.forget_journal (
    tenant_id    uuid,
    forget_id    uuid,
    target_ids   text,       -- JSON array of {object_type, object_id, session_id}
    mode         text,       -- "retract" | "hard"
    step_states  text,       -- JSON map: step_name -> "pending"|"done"|"failed"
    status       text,       -- "in_progress" | "completed" | "failed"
    reason       text,
    actor        text,
    created_at   timestamp,
    updated_at   timestamp,
    PRIMARY KEY ((tenant_id), created_at, forget_id)
) WITH CLUSTERING ORDER BY (created_at DESC, forget_id DESC);

-- Point lookup by forget_id (status/resume) and scan for unfinished journals.
CREATE INDEX IF NOT EXISTS idx_forget_journal_id
    ON agent_memory.forget_journal (forget_id);

CREATE INDEX IF NOT EXISTS idx_forget_journal_status
    ON agent_memory.forget_journal (status);
