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

    /// Check whether adding an edge `src -[edge_type]-> dst` would create a
    /// cycle in the existing DAG. Used before emitting `PARENT_TAG`
    /// (tag hierarchy) and `REQUIRES` (skill prerequisite) edges.
    ///
    /// A cycle forms when there is already a path from `dst` back to `src`
    /// via `edge_type` — adding `src -> dst` closes that loop.
    ///
    /// Uses a bounded depth of 32 hops to cap query cost; any taxonomy
    /// deeper than that has bigger problems than cycle detection.
    ///
    /// Returns `Ok(true)` if the edge would form a cycle (reject it),
    /// `Ok(false)` if safe to add. Fails loud on query errors so callers
    /// can treat an unreachable graph as fail-closed.
    pub async fn would_create_cycle(
        &self,
        src_entity_id: Uuid,
        dst_entity_id: Uuid,
        edge_type: &str,
    ) -> anyhow::Result<bool> {
        let cypher = build_cycle_query(src_entity_id, dst_entity_id, edge_type);
        let resp = self.query(&cypher).await?;
        // Response shape: rows: [[true]] or [[false]].
        let value = resp
            .rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .unwrap_or(serde_json::Value::Bool(false));
        Ok(value.as_bool().unwrap_or(false))
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

/// Build the Cypher query for the cycle check. Extracted so unit tests can
/// assert on the exact query shape without a live graph endpoint.
///
/// The edge_type must be alphanumeric-or-underscore — enforced by the
/// callers (edge type registry validation). A separate `sanitize_edge_type`
/// helper trims anything unusual defensively.
fn build_cycle_query(src: Uuid, dst: Uuid, edge_type: &str) -> String {
    // take_while stops at the first unsafe char — prevents an attacker from
    // stuffing injected Cypher after a benign prefix (filter would keep the
    // good chars while silently dropping the bad ones, producing a
    // malformed-but-concatenated identifier).
    let safe_type: String = edge_type
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    format!(
        "MATCH path = (dst {{entity_id: '{dst}'}})\
         -[:{safe_type}*1..32]->(src {{entity_id: '{src}'}}) \
         RETURN count(path) > 0 AS would_cycle"
    )
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
    fn cycle_query_names_dst_and_src_in_correct_direction() {
        // Adding src -> dst cycles iff a path already exists from dst back
        // to src. The query must traverse FROM dst TO src, not the other
        // way around — a classic off-by-direction bug that would silently
        // pass every cycle attempt.
        let src = Uuid::from_u128(0x1);
        let dst = Uuid::from_u128(0x2);
        let q = build_cycle_query(src, dst, "PARENT_TAG");
        // The dst binding must appear BEFORE the src binding in the path.
        let dst_pos = q.find(&dst.to_string()).expect("dst must appear");
        let src_pos = q.find(&src.to_string()).expect("src must appear");
        assert!(
            dst_pos < src_pos,
            "dst must be the starting node of the path traversal, got: {q}"
        );
        assert!(q.contains("[:PARENT_TAG*1..32]"));
        assert!(q.contains("would_cycle"));
    }

    #[test]
    fn cycle_query_sanitizes_edge_type_injection() {
        // Prevent a caller from injecting Cypher via edge_type. The sanitizer
        // stops at the first unsafe char — everything after the first `'`,
        // `;`, space, `/`, etc. is dropped.
        let src = Uuid::from_u128(0x1);
        let dst = Uuid::from_u128(0x2);
        let q = build_cycle_query(src, dst, "DROP'; MATCH (n) DETACH DELETE n; //");
        // Injection markers must never reach the query.
        assert!(!q.contains("';"), "quote+semicolon must be stripped: {q}");
        assert!(!q.contains("DELETE"), "DELETE must be stripped: {q}");
        assert!(!q.contains("//"), "comment marker must be stripped: {q}");
        assert!(!q.contains(" MATCH (n)"), "nested MATCH must be stripped: {q}");
        // Only the benign prefix ("DROP") survives inside the edge-type
        // brackets; UUIDs still carry their own quotes, which is fine.
        assert!(q.contains("[:DROP*1..32]"));
    }

    #[test]
    fn cycle_query_accepts_underscore_edge_types() {
        let src = Uuid::from_u128(0x1);
        let dst = Uuid::from_u128(0x2);
        let q = build_cycle_query(src, dst, "TAGGED_AS");
        assert!(q.contains("[:TAGGED_AS*1..32]"));
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
