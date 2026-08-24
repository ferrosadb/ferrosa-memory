-- Ferrosa Memory MCP Server — archived agent sessions.
-- Run after 052_mobile_control_cursor_reservation_token.cql.
--
-- An archived session is what a finished shift leaves behind: the tmux buffer
-- as it looked when the session ended, so it can be read later exactly as the
-- operator would have seen it live.
--
-- Application-owned, on the memory server the session was connected to. The
-- gateway never sees any of this.

CREATE TABLE IF NOT EXISTS agent_memory.session_archives (
    tenant_id uuid,
    -- Time-ordered so a listing is newest-first without a secondary index, and
    -- so two archives of the same agent in the same second cannot collide.
    archive_id timeuuid,
    -- Which agent this was a run of. Kept as an id AND a name because the
    -- agent may be deleted afterwards, and an archive that can only say
    -- "config 8f2a…" is an archive nobody will ever open.
    agent_id text,
    agent_name text,
    -- What actually ran, and where. The whole point of archiving a shift is
    -- being able to tell later what it was doing.
    command text,
    working_dir text,
    -- Which machine it ran on, by fingerprint.
    machine_fingerprint text,
    -- The tmux scrollback as RAW BYTES, escape sequences included, so a reader
    -- can feed it to a terminal emulator and see the colours the session had.
    -- Text would render more cheaply and would not be what was on screen.
    buffer blob,
    -- Recorded separately from the blob so a listing can show the size without
    -- reading it, and so a truncated capture is detectable.
    buffer_bytes int,
    -- Whether the capture is the WHOLE history or was cut short. A truncated
    -- archive presented as complete is the same class of lie as a transcript
    -- with a silent hole in it.
    truncated boolean,
    archived_at timestamp,
    PRIMARY KEY (tenant_id, archive_id)
) WITH CLUSTERING ORDER BY (archive_id DESC);
