//! Graph client for Ferrosa's HTTP Cypher endpoint.
//!
//! Ferrosa's graph model is property-graph-on-CQL: vertices are CQL rows in
//! tables annotated with `graph.type=vertex`, edges are CQL rows in tables
//! annotated with `graph.type=edge`. The graph adjacency index is maintained
//! automatically.
//!
//! **Writes** go through CQL (via `CqlStorage`) — INSERT into vertex/edge tables.
//! **Reads/traversals** go through the graph HTTP API — MATCH queries via Cypher.
//!
//! ## Edge types
//!
//! - `FOLDED_INTO` — child fold -> parent fold
//! - `CO_OCCURS_WITH` — entity <-> entity (same fold)
//! - `MENTIONED_IN` — entity -> fold
//! - `SUPERSEDES` — new temporal fact -> old fact

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Graph client wrapping an HTTP connection to Ferrosa's graph endpoint.
pub struct GraphClient {
    client: reqwest::Client,
    base_url: String,
    auth_header: String,
    keyspace: String,
}

/// Configuration for the graph connection.
pub struct GraphConfig {
    pub http_url: String,
    pub username: String,
    pub password: String,
    pub keyspace: String,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            http_url: "http://localhost:17474".into(),
            username: "cassandra".into(),
            password: "cassandra".into(),
            keyspace: "agent_memory".into(),
        }
    }
}

#[derive(Serialize)]
struct CypherRequest<'a> {
    query: &'a str,
    keyspace: &'a str,
}

#[derive(Deserialize, Debug)]
struct CypherResponse {
    #[serde(default)]
    _columns: Vec<String>,
    #[serde(default)]
    rows: Vec<Vec<serde_json::Value>>,
    #[serde(default)]
    error: Option<String>,
}

impl GraphClient {
    /// Connect to Ferrosa's graph HTTP endpoint.
    pub async fn connect(config: &GraphConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;

        let auth = base64_encode(&format!("{}:{}", config.username, config.password));
        let auth_header = format!("Basic {auth}");

        // Health check
        let resp = client
            .get(format!("{}/graph/health", config.http_url))
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("graph health check failed: {}", resp.status());
        }

        tracing::info!(url = %config.http_url, "graph client connected via HTTP");

        Ok(Self {
            client,
            base_url: config.http_url.clone(),
            auth_header,
            keyspace: config.keyspace.clone(),
        })
    }

    /// Execute a Cypher MATCH query against the graph endpoint.
    async fn query(&self, cypher: &str) -> anyhow::Result<CypherResponse> {
        let req = CypherRequest {
            query: cypher,
            keyspace: &self.keyspace,
        };

        let resp = self
            .client
            .post(format!("{}/graph/query", self.base_url))
            .header("Authorization", &self.auth_header)
            .header("Content-Type", "application/json")
            .json(&req)
            .send()
            .await?;

        let body = resp.text().await?;
        let parsed: CypherResponse = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("graph response parse error: {e}, body: {body}"))?;

        if let Some(err) = &parsed.error {
            anyhow::bail!("graph query error: {err}");
        }

        Ok(parsed)
    }

    /// Traverse the fold hierarchy: get all ancestors of a fold.
    pub async fn get_fold_ancestors(
        &self,
        fold_id: Uuid,
        session_id: Uuid,
        max_depth: usize,
    ) -> anyhow::Result<Vec<String>> {
        let cypher = format!(
            "MATCH (start:Fold {{fold_id: '{fold_id}', session_id: '{session_id}'}})\
             -[:FOLDED_INTO*1..{max_depth}]->(ancestor) \
             RETURN ancestor.fold_id AS ancestor_id"
        );
        let resp = self.query(&cypher).await?;
        Ok(extract_string_column(&resp))
    }

    /// Find entities related to a given entity within N hops.
    pub async fn find_related_entities(
        &self,
        entity_id: Uuid,
        session_id: Uuid,
        max_hops: usize,
    ) -> anyhow::Result<Vec<String>> {
        let cypher = format!(
            "MATCH (start:Entity {{entity_id: '{entity_id}', session_id: '{session_id}'}})\
             -[:CO_OCCURS_WITH*1..{max_hops}]-(related) \
             WHERE related <> start \
             RETURN DISTINCT related.entity_id AS related_id"
        );
        let resp = self.query(&cypher).await?;
        Ok(extract_string_column(&resp))
    }

    /// Get entities mentioned in a specific fold.
    pub async fn get_entities_in_fold(
        &self,
        fold_id: Uuid,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<String>> {
        let cypher = format!(
            "MATCH (e:Entity)-[:MENTIONED_IN]->(f:Fold {{fold_id: '{fold_id}', session_id: '{session_id}'}}) \
             RETURN e.entity_id AS entity_id"
        );
        let resp = self.query(&cypher).await?;
        Ok(extract_string_column(&resp))
    }

    /// Get the temporal supersession chain for a fact.
    pub async fn get_supersession_chain(
        &self,
        event_id: Uuid,
        entity_id: Uuid,
    ) -> anyhow::Result<Vec<String>> {
        let cypher = format!(
            "MATCH (start:Fact {{event_id: '{event_id}', entity_id: '{entity_id}'}})\
             -[:SUPERSEDES*1..]->(older) \
             RETURN older.event_id AS event_id"
        );
        let resp = self.query(&cypher).await?;
        Ok(extract_string_column(&resp))
    }
}

/// Extract first column as strings from a Cypher response.
fn extract_string_column(resp: &CypherResponse) -> Vec<String> {
    resp.rows
        .iter()
        .filter_map(|row| row.first().and_then(|v| v.as_str().map(String::from)))
        .collect()
}

fn base64_encode(input: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len() * 4 / 3 + 4);
    for chunk in input.as_bytes().chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encode_works() {
        assert_eq!(
            base64_encode("cassandra:cassandra"),
            "Y2Fzc2FuZHJhOmNhc3NhbmRyYQ=="
        );
    }

    #[test]
    fn extract_string_column_from_response() {
        let resp = CypherResponse {
            _columns: vec!["id".into()],
            rows: vec![
                vec![serde_json::json!("abc")],
                vec![serde_json::json!("def")],
            ],
            error: None,
        };
        assert_eq!(extract_string_column(&resp), vec!["abc", "def"]);
    }
}
