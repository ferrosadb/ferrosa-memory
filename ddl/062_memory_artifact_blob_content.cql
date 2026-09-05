-- Content-addressed blobs are globally deduplicated. The artifact_link rows
-- remain the tenant-scoped semantic and policy-bearing references.
ALTER TABLE agent_memory.artifact_blob ADD content blob;
