-- Add edge weight and TTL support for CO_OCCURS edges
-- Enables pruning of weak/stale connections

USE agent_memory;

-- Similarity score (0-1) and last reinforcement timestamp
ALTER TABLE co_occurs_with ADD strength float;
ALTER TABLE co_occurs_with ADD last_reinforced timestamp;
