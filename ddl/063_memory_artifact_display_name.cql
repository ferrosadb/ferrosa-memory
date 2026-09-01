-- The semantic link owns its display name; opaque blobs do not.
ALTER TABLE agent_memory.artifact_link ADD display_name text;
