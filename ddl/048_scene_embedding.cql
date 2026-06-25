-- Ferrosa Memory MCP Server — semantic scene embeddings
-- Run against Ferrosa CQL interface after 047_foresight_facts.cql
--
-- Adds a centroid embedding to each consolidation scene (the mean of its member
-- entity embeddings, computed during consolidation — no embedder call needed).
-- Lets hybrid_search match scenes SEMANTICALLY (cosine vs the query embedding),
-- not just by shared summary tokens, surfacing coherent clusters whose wording
-- differs from the query. Stored as list<float> (scored in-memory, bounded scene
-- count) rather than an ANN-indexed vector.

USE agent_memory;

ALTER TABLE mem_scenes ADD scene_embedding list<float>;
