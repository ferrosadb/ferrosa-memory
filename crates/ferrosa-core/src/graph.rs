//! Cypher graph client using neo4rs (Bolt protocol).
//!
//! Connects to Ferrosa's graph layer on port 7687 (Bolt) for managing
//! fold hierarchy edges, entity relationship edges, and temporal supersession.
//!
//! ## Edge types
//!
//! - `FOLDED_INTO` — child fold -> parent fold
//! - `CO_OCCURS_WITH` — entity <-> entity (same fold)
//! - `MENTIONED_IN` — entity -> fold
//! - `SUPERSEDES` — new temporal fact -> old fact
//!
//! ## Ferrosa endpoints
//!
//! - Bolt: port 7687 (used by neo4rs)
//! - HTTP: port 7474 (available for REST queries)

use neo4rs::{Graph, query};
use uuid::Uuid;

/// Graph client wrapping a neo4rs connection pool.
pub struct GraphClient {
    graph: Graph,
}

/// Configuration for the graph connection.
pub struct GraphConfig {
    pub bolt_uri: String,
    pub username: String,
    pub password: String,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            bolt_uri: "bolt://localhost:7687".into(),
            username: "neo4j".into(),
            password: "neo4j".into(),
        }
    }
}

impl GraphClient {
    /// Connect to Ferrosa's graph layer via Bolt protocol.
    pub async fn connect(config: &GraphConfig) -> anyhow::Result<Self> {
        let graph = Graph::new(&config.bolt_uri, &config.username, &config.password).await?;
        tracing::info!(uri = %config.bolt_uri, "graph client connected via Bolt");
        Ok(Self { graph })
    }

    /// Create a FOLDED_INTO edge from child fold to parent fold.
    pub async fn create_fold_edge(
        &self,
        child_fold_id: Uuid,
        parent_fold_id: Uuid,
        session_id: Uuid,
    ) -> anyhow::Result<()> {
        self.graph
            .run(
                query(
                    "MERGE (child:Fold {fold_id: $child_id, session_id: $session_id}) \
                     MERGE (parent:Fold {fold_id: $parent_id, session_id: $session_id}) \
                     MERGE (child)-[:FOLDED_INTO]->(parent)",
                )
                .param("child_id", child_fold_id.to_string())
                .param("parent_id", parent_fold_id.to_string())
                .param("session_id", session_id.to_string()),
            )
            .await?;
        Ok(())
    }

    /// Create a MENTIONED_IN edge from entity to fold.
    pub async fn create_mentioned_in_edge(
        &self,
        entity_id: Uuid,
        fold_id: Uuid,
        session_id: Uuid,
    ) -> anyhow::Result<()> {
        self.graph
            .run(
                query(
                    "MERGE (e:Entity {entity_id: $entity_id, session_id: $session_id}) \
                     MERGE (f:Fold {fold_id: $fold_id, session_id: $session_id}) \
                     MERGE (e)-[:MENTIONED_IN]->(f)",
                )
                .param("entity_id", entity_id.to_string())
                .param("fold_id", fold_id.to_string())
                .param("session_id", session_id.to_string()),
            )
            .await?;
        Ok(())
    }

    /// Create a CO_OCCURS_WITH edge between two entities.
    pub async fn create_co_occurrence_edge(
        &self,
        entity_a: Uuid,
        entity_b: Uuid,
        session_id: Uuid,
    ) -> anyhow::Result<()> {
        self.graph
            .run(
                query(
                    "MERGE (a:Entity {entity_id: $a_id, session_id: $session_id}) \
                     MERGE (b:Entity {entity_id: $b_id, session_id: $session_id}) \
                     MERGE (a)-[:CO_OCCURS_WITH]-(b)",
                )
                .param("a_id", entity_a.to_string())
                .param("b_id", entity_b.to_string())
                .param("session_id", session_id.to_string()),
            )
            .await?;
        Ok(())
    }

    /// Create a SUPERSEDES edge from new temporal fact to old fact.
    pub async fn create_supersedes_edge(
        &self,
        new_event_id: Uuid,
        old_event_id: Uuid,
        entity_id: Uuid,
    ) -> anyhow::Result<()> {
        self.graph
            .run(
                query(
                    "MERGE (new_f:Fact {event_id: $new_id, entity_id: $entity_id}) \
                     MERGE (old_f:Fact {event_id: $old_id, entity_id: $entity_id}) \
                     MERGE (new_f)-[:SUPERSEDES]->(old_f)",
                )
                .param("new_id", new_event_id.to_string())
                .param("old_id", old_event_id.to_string())
                .param("entity_id", entity_id.to_string()),
            )
            .await?;
        Ok(())
    }

    /// Traverse the fold hierarchy: get all ancestors of a fold.
    pub async fn get_fold_ancestors(
        &self,
        fold_id: Uuid,
        session_id: Uuid,
        max_depth: usize,
    ) -> anyhow::Result<Vec<String>> {
        let mut result = self
            .graph
            .execute(
                query(
                    "MATCH (start:Fold {fold_id: $fold_id, session_id: $session_id}) \
                     MATCH path = (start)-[:FOLDED_INTO*1..]->(ancestor) \
                     WHERE length(path) <= $max_depth \
                     RETURN ancestor.fold_id AS ancestor_id",
                )
                .param("fold_id", fold_id.to_string())
                .param("session_id", session_id.to_string())
                .param("max_depth", max_depth as i64),
            )
            .await?;

        let mut ancestors = Vec::new();
        while let Some(row) = result.next().await? {
            if let Ok(id) = row.get::<String>("ancestor_id") {
                ancestors.push(id);
            }
        }
        Ok(ancestors)
    }

    /// Find entities related to a given entity within N hops.
    pub async fn find_related_entities(
        &self,
        entity_id: Uuid,
        session_id: Uuid,
        max_hops: usize,
    ) -> anyhow::Result<Vec<String>> {
        let mut result = self
            .graph
            .execute(
                query(
                    "MATCH (start:Entity {entity_id: $entity_id, session_id: $session_id}) \
                     MATCH path = (start)-[:CO_OCCURS_WITH*1..]-(related) \
                     WHERE length(path) <= $max_hops AND related <> start \
                     RETURN DISTINCT related.entity_id AS related_id",
                )
                .param("entity_id", entity_id.to_string())
                .param("session_id", session_id.to_string())
                .param("max_hops", max_hops as i64),
            )
            .await?;

        let mut related = Vec::new();
        while let Some(row) = result.next().await? {
            if let Ok(id) = row.get::<String>("related_id") {
                related.push(id);
            }
        }
        Ok(related)
    }

    /// Get the temporal supersession chain for an entity.
    pub async fn get_supersession_chain(
        &self,
        event_id: Uuid,
        entity_id: Uuid,
    ) -> anyhow::Result<Vec<String>> {
        let mut result = self
            .graph
            .execute(
                query(
                    "MATCH (start:Fact {event_id: $event_id, entity_id: $entity_id}) \
                     MATCH path = (start)-[:SUPERSEDES*1..]->(older) \
                     RETURN older.event_id AS event_id \
                     ORDER BY length(path)",
                )
                .param("event_id", event_id.to_string())
                .param("entity_id", entity_id.to_string()),
            )
            .await?;

        let mut chain = Vec::new();
        while let Some(row) = result.next().await? {
            if let Ok(id) = row.get::<String>("event_id") {
                chain.push(id);
            }
        }
        Ok(chain)
    }
}
