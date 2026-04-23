//! Graph client for Ferrosa's HTTP Cypher endpoint.
//!
//! Ferrosa's graph model is property-graph-on-CQL: vertices are CQL rows in
//! tables annotated with `graph.type=vertex`, edges are CQL rows in tables
//! annotated with `graph.type=edge`. The graph adjacency index is maintained
//! automatically.
//!
//! Graph-owned reads and writes go through the public graph HTTP API.
//! App-owned tables remain on direct CQL elsewhere in the system.
//!
//! ## Edge types
//!
//! - `FOLDED_INTO` — child fold -> parent fold
//! - `CO_OCCURS_WITH` — entity <-> entity (same fold)
//! - `MENTIONED_IN` — entity -> fold
//! - `SUPERSEDES` — new temporal fact -> old fact

use chrono::{DateTime, Utc};
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

#[derive(Debug, Clone, PartialEq)]
pub struct CoOccursEdgeRow {
    pub src_id: Uuid,
    pub dst_id: Uuid,
    pub strength: f32,
    pub last_reinforced: Option<DateTime<Utc>>,
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

    /// Execute a graph mutation via the public Cypher API.
    pub async fn execute_mutation(&self, cypher: &str) -> anyhow::Result<()> {
        let _ = self.query(cypher).await?;
        Ok(())
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

    pub async fn put_typed_edge(
        &self,
        tenant_id: Uuid,
        session_id: Uuid,
        src_id: Uuid,
        edge_type: &str,
        dst_id: Uuid,
        weight: f64,
        metadata: Option<&str>,
    ) -> anyhow::Result<()> {
        let cypher = build_typed_edge_merge_query(
            tenant_id, session_id, src_id, edge_type, dst_id, weight, metadata,
        );
        self.execute_mutation(&cypher).await
    }

    pub async fn delete_typed_edge(
        &self,
        tenant_id: Uuid,
        session_id: Uuid,
        src_id: Uuid,
        edge_type: &str,
        dst_id: Uuid,
    ) -> anyhow::Result<()> {
        let cypher = build_typed_edge_delete_query(tenant_id, session_id, src_id, edge_type, dst_id);
        self.execute_mutation(&cypher).await
    }

    pub async fn put_folded_into_edge(
        &self,
        tenant_id: Uuid,
        session_id: Uuid,
        source_fold_id: Uuid,
        target_fold_id: Uuid,
    ) -> anyhow::Result<()> {
        let cypher =
            build_folded_into_merge_query(tenant_id, session_id, source_fold_id, target_fold_id);
        self.execute_mutation(&cypher).await
    }

    pub async fn put_mentioned_in_edge(
        &self,
        tenant_id: Uuid,
        session_id: Uuid,
        entity_id: Uuid,
        fold_id: Uuid,
    ) -> anyhow::Result<()> {
        let cypher = build_mentioned_in_merge_query(tenant_id, session_id, entity_id, fold_id);
        self.execute_mutation(&cypher).await
    }

    pub async fn put_co_occurs_edge(
        &self,
        tenant_id: Uuid,
        session_id: Uuid,
        entity_a: Uuid,
        entity_b: Uuid,
        strength: f32,
    ) -> anyhow::Result<()> {
        let cypher =
            build_co_occurs_merge_query(tenant_id, session_id, entity_a, entity_b, strength);
        self.execute_mutation(&cypher).await
    }

    pub async fn list_co_occurs_edges(
        &self,
        tenant_id: Uuid,
    ) -> anyhow::Result<Vec<CoOccursEdgeRow>> {
        let cypher = format!(
            "MATCH (a:Entity)-[r:CO_OCCURS_WITH {{tenant_id: {}}}]->(b:Entity) \
             RETURN a.entity_id AS src_id, b.entity_id AS dst_id, r.strength AS strength, \
             r.last_reinforced AS last_reinforced",
            quote_cypher(&tenant_id.to_string())
        );
        let resp = self.query(&cypher).await?;
        let mut rows = Vec::with_capacity(resp.rows.len());
        for row in resp.rows {
            let src_id = row
                .first()
                .and_then(|value| value.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing src_id from graph response"))?;
            let dst_id = row
                .get(1)
                .and_then(|value| value.as_str())
                .ok_or_else(|| anyhow::anyhow!("missing dst_id from graph response"))?;
            let strength = row.get(2).and_then(|value| value.as_f64()).unwrap_or(1.0) as f32;
            let last_reinforced = row
                .get(3)
                .and_then(|value| value.as_str())
                .map(DateTime::parse_from_rfc3339)
                .transpose()?
                .map(|dt| dt.with_timezone(&Utc));
            rows.push(CoOccursEdgeRow {
                src_id: Uuid::parse_str(src_id)?,
                dst_id: Uuid::parse_str(dst_id)?,
                strength,
                last_reinforced,
            });
        }
        Ok(rows)
    }

    pub async fn set_co_occurs_strength(
        &self,
        tenant_id: Uuid,
        entity_a: Uuid,
        entity_b: Uuid,
        strength: f32,
    ) -> anyhow::Result<()> {
        let cypher = format!(
            "MATCH (a:Entity {{tenant_id: {}, entity_id: {}}})\
             -[r:CO_OCCURS_WITH {{tenant_id: {}}}]->\
             (b:Entity {{tenant_id: {}, entity_id: {}}}) \
             SET r.strength = {} RETURN r",
            quote_cypher(&tenant_id.to_string()),
            quote_cypher(&entity_a.to_string()),
            quote_cypher(&tenant_id.to_string()),
            quote_cypher(&tenant_id.to_string()),
            quote_cypher(&entity_b.to_string()),
            strength,
        );
        self.execute_mutation(&cypher).await
    }

    pub async fn delete_co_occurs_edge(
        &self,
        tenant_id: Uuid,
        entity_a: Uuid,
        entity_b: Uuid,
    ) -> anyhow::Result<()> {
        let cypher = format!(
            "MATCH (a:Entity {{tenant_id: {}, entity_id: {}}})\
             -[r:CO_OCCURS_WITH {{tenant_id: {}}}]->\
             (b:Entity {{tenant_id: {}, entity_id: {}}}) \
             DELETE r RETURN 1",
            quote_cypher(&tenant_id.to_string()),
            quote_cypher(&entity_a.to_string()),
            quote_cypher(&tenant_id.to_string()),
            quote_cypher(&tenant_id.to_string()),
            quote_cypher(&entity_b.to_string()),
        );
        self.execute_mutation(&cypher).await
    }

    pub async fn put_supersedes_edge(
        &self,
        tenant_id: Uuid,
        entity_id: Uuid,
        new_event_id: Uuid,
        old_event_id: Uuid,
    ) -> anyhow::Result<()> {
        let cypher = build_supersedes_merge_query(tenant_id, entity_id, new_event_id, old_event_id);
        self.execute_mutation(&cypher).await
    }

    pub async fn count_edges(&self, tenant_id: Uuid) -> anyhow::Result<usize> {
        let tenant = quote_cypher(&tenant_id.to_string());
        let queries = [
            format!(
                "MATCH (a:Entity)-[r:CO_OCCURS_WITH {{tenant_id: {tenant}}}]->(b:Entity) RETURN count(r)"
            ),
            format!(
                "MATCH (a:Entity)-[r:MENTIONED_IN {{tenant_id: {tenant}}}]->(b:Fold) RETURN count(r)"
            ),
            format!(
                "MATCH (a:Fold)-[r:FOLDED_INTO {{tenant_id: {tenant}}}]->(b:Fold) RETURN count(r)"
            ),
            format!(
                "MATCH (a:Fact)-[r:SUPERSEDES {{tenant_id: {tenant}}}]->(b:Fact) RETURN count(r)"
            ),
            format!(
                "MATCH (a:Entity)-[r:TYPED_EDGE {{tenant_id: {tenant}}}]->(b:Entity) RETURN count(r)"
            ),
        ];

        let mut total = 0usize;
        for cypher in queries {
            let resp = self.query(&cypher).await?;
            total += extract_usize_scalar(&resp)?;
        }
        Ok(total)
    }
}

/// Build the Cypher query for the cycle check. Extracted so unit tests can
/// assert on the exact query shape without a live graph endpoint.
///
/// The edge_type must be alphanumeric-or-underscore — enforced by the
/// callers (edge type registry validation). A separate `sanitize_edge_type`
/// helper trims anything unusual defensively.
///
/// All typed edges live in one table under `graph.label='TYPED_EDGE'`
/// (see `ddl/017_typed_edges.cql`), with the CQL `edge_type` column exposed
/// as a relationship property. Traversals filter by `{edge_type: '<type>'}`
/// on the TYPED_EDGE label rather than naming the edge_type as its own label.
fn build_cycle_query(src: Uuid, dst: Uuid, edge_type: &str) -> String {
    // take_while stops at the first unsafe char — prevents an attacker from
    // stuffing injected Cypher after a benign prefix (filter would keep the
    // good chars while silently dropping the bad ones, producing a
    // malformed-but-concatenated identifier).
    let safe_type: String = edge_type
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    // The anchor node must carry the `:Entity` label so the planner can
    // resolve it to the entity_store table (ddl/003 registers that label).
    // Without the label the planner falls through to "relationship pattern
    // found before any anchor node".
    format!(
        "MATCH path = (dst:Entity {{entity_id: '{dst}'}})\
         -[:TYPED_EDGE*1..32 {{edge_type: '{safe_type}'}}]->\
         (src:Entity {{entity_id: '{src}'}}) \
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

fn extract_usize_scalar(resp: &CypherResponse) -> anyhow::Result<usize> {
    let value = resp
        .rows
        .first()
        .and_then(|row| row.first())
        .ok_or_else(|| anyhow::anyhow!("missing scalar value in graph response"))?;
    let Some(count) = value.as_u64() else {
        anyhow::bail!("graph scalar response was not an unsigned integer: {value}");
    };
    Ok(count as usize)
}

fn quote_cypher(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn build_typed_edge_merge_query(
    _tenant_id: Uuid,
    _session_id: Uuid,
    src_id: Uuid,
    edge_type: &str,
    dst_id: Uuid,
    weight: f64,
    _metadata: Option<&str>,
) -> String {
    format!(
        "MERGE (a:Entity {{entity_id: {}}})\
         MERGE (b:Entity {{entity_id: {}}})\
         MERGE (a)-[r:TYPED_EDGE {{edge_type: {}}}]->(b) \
         SET r.weight = {} RETURN r",
        quote_cypher(&src_id.to_string()),
        quote_cypher(&dst_id.to_string()),
        quote_cypher(edge_type),
        weight,
    )
}

fn build_typed_edge_delete_query(
    tenant_id: Uuid,
    session_id: Uuid,
    src_id: Uuid,
    edge_type: &str,
    dst_id: Uuid,
) -> String {
    format!(
        "MATCH (a:Entity {{tenant_id: {}, session_id: {}, entity_id: {}}})\
         -[r:TYPED_EDGE {{tenant_id: {}, session_id: {}, edge_type: {}}}]->\
         (b:Entity {{tenant_id: {}, session_id: {}, entity_id: {}}}) \
         DELETE r",
        quote_cypher(&tenant_id.to_string()),
        quote_cypher(&session_id.to_string()),
        quote_cypher(&src_id.to_string()),
        quote_cypher(&tenant_id.to_string()),
        quote_cypher(&session_id.to_string()),
        quote_cypher(edge_type),
        quote_cypher(&tenant_id.to_string()),
        quote_cypher(&session_id.to_string()),
        quote_cypher(&dst_id.to_string()),
    )
}

fn build_folded_into_merge_query(
    tenant_id: Uuid,
    session_id: Uuid,
    source_fold_id: Uuid,
    target_fold_id: Uuid,
) -> String {
    let created_at = Utc::now().to_rfc3339();
    format!(
        "MERGE (a:Fold {{fold_id: {}}})\
         MERGE (b:Fold {{fold_id: {}}})\
         MERGE (a)-[r:FOLDED_INTO]->(b) \
         SET r.tenant_id = {}, r.session_id = {}, r.created_at = {} RETURN r",
        quote_cypher(&source_fold_id.to_string()),
        quote_cypher(&target_fold_id.to_string()),
        quote_cypher(&tenant_id.to_string()),
        quote_cypher(&session_id.to_string()),
        quote_cypher(&created_at),
    )
}

fn build_mentioned_in_merge_query(
    tenant_id: Uuid,
    session_id: Uuid,
    entity_id: Uuid,
    fold_id: Uuid,
) -> String {
    let created_at = Utc::now().to_rfc3339();
    format!(
        "MERGE (e:Entity {{entity_id: {}}})\
         MERGE (f:Fold {{fold_id: {}}})\
         MERGE (e)-[r:MENTIONED_IN]->(f) \
         SET r.tenant_id = {}, r.session_id = {}, r.created_at = {} RETURN r",
        quote_cypher(&entity_id.to_string()),
        quote_cypher(&fold_id.to_string()),
        quote_cypher(&tenant_id.to_string()),
        quote_cypher(&session_id.to_string()),
        quote_cypher(&created_at),
    )
}

fn build_co_occurs_merge_query(
    tenant_id: Uuid,
    session_id: Uuid,
    entity_a: Uuid,
    entity_b: Uuid,
    strength: f32,
) -> String {
    let now = Utc::now().to_rfc3339();
    format!(
        "MERGE (a:Entity {{entity_id: {}}})\
         MERGE (b:Entity {{entity_id: {}}})\
         MERGE (a)-[r:CO_OCCURS_WITH]->(b) \
         SET r.tenant_id = {}, r.session_id = {}, r.strength = {}, r.first_seen = {}, r.last_reinforced = {} RETURN r",
        quote_cypher(&entity_a.to_string()),
        quote_cypher(&entity_b.to_string()),
        quote_cypher(&tenant_id.to_string()),
        quote_cypher(&session_id.to_string()),
        strength,
        quote_cypher(&now),
        quote_cypher(&now),
    )
}

fn build_supersedes_merge_query(
    tenant_id: Uuid,
    entity_id: Uuid,
    new_event_id: Uuid,
    old_event_id: Uuid,
) -> String {
    let created_at = Utc::now().to_rfc3339();
    format!(
        "MERGE (n:Fact {{event_id: {}}})\
         MERGE (o:Fact {{event_id: {}}})\
         MERGE (n)-[r:SUPERSEDES]->(o) \
         SET r.tenant_id = {}, r.entity_id = {}, r.created_at = {} RETURN r",
        quote_cypher(&new_event_id.to_string()),
        quote_cypher(&old_event_id.to_string()),
        quote_cypher(&tenant_id.to_string()),
        quote_cypher(&entity_id.to_string()),
        quote_cypher(&created_at),
    )
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
        assert!(q.contains("would_cycle"));
    }

    #[test]
    fn cycle_query_traverses_typed_edge_label_with_edge_type_property_filter() {
        // The typed_edges table carries a single graph.label=TYPED_EDGE, with
        // the edge_type column exposed as a relationship property. Traversing
        // `[:PARENT_TAG*...]` as if PARENT_TAG were its own label errors out
        // with "no table with graph.label 'PARENT_TAG'". Traverse TYPED_EDGE
        // filtered by `{edge_type: '<type>'}` instead.
        let src = Uuid::from_u128(0x1);
        let dst = Uuid::from_u128(0x2);
        for edge_type in ["PARENT_TAG", "TAGGED_AS", "REQUIRES"] {
            let q = build_cycle_query(src, dst, edge_type);
            assert!(
                q.contains("[:TYPED_EDGE") || q.contains(":TYPED_EDGE*"),
                "query must traverse the TYPED_EDGE label, got: {q}"
            );
            assert!(
                q.contains("(dst:Entity") && q.contains("(src:Entity"),
                "anchor + target must carry :Entity label so the planner can resolve them, got: {q}"
            );
            assert!(
                !q.contains(&format!("[:{edge_type}")),
                "query must not use `{edge_type}` as a graph label, got: {q}"
            );
            assert!(
                q.contains(&format!("edge_type: '{edge_type}'")),
                "query must filter relationships by edge_type property, got: {q}"
            );
            assert!(q.contains("*1..32"), "var-length must be preserved: {q}");
        }
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
        assert!(
            !q.contains(" MATCH (n)"),
            "nested MATCH must be stripped: {q}"
        );
        // Only the benign prefix ("DROP") survives as the edge_type literal
        // inside the relationship property filter.
        assert!(
            q.contains("edge_type: 'DROP'"),
            "sanitized prefix must land in edge_type filter: {q}"
        );
    }

    #[test]
    fn cycle_query_accepts_underscore_edge_types() {
        let src = Uuid::from_u128(0x1);
        let dst = Uuid::from_u128(0x2);
        let q = build_cycle_query(src, dst, "TAGGED_AS");
        assert!(q.contains("edge_type: 'TAGGED_AS'"));
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

    #[test]
    fn typed_edge_merge_query_uses_public_graph_labels_and_edge_properties() {
        let tenant_id = Uuid::from_u128(0x1);
        let session_id = Uuid::from_u128(0x2);
        let src_id = Uuid::from_u128(0x3);
        let dst_id = Uuid::from_u128(0x4);
        let query = build_typed_edge_merge_query(
            tenant_id,
            session_id,
            src_id,
            "related",
            dst_id,
            0.75,
            Some("probe"),
        );
        assert!(query.contains("MERGE (a:Entity"));
        assert!(query.contains("-[r:TYPED_EDGE"));
        assert!(query.contains("edge_type: 'related'"));
        assert!(query.contains("r.weight = 0.75"));
        assert!(query.contains("r.metadata = 'probe'"));
    }

    #[test]
    fn typed_edge_delete_query_uses_public_graph_labels_without_return_clause() {
        let tenant_id = Uuid::from_u128(0x1);
        let session_id = Uuid::from_u128(0x2);
        let src_id = Uuid::from_u128(0x3);
        let dst_id = Uuid::from_u128(0x4);
        let query = build_typed_edge_delete_query(tenant_id, session_id, src_id, "related", dst_id);
        assert!(query.contains("MATCH (a:Entity"));
        assert!(query.contains("-[r:TYPED_EDGE"));
        assert!(query.contains("edge_type: 'related'"));
        assert!(query.contains("DELETE r"));
        assert!(!query.contains("RETURN"));
    }

    #[test]
    fn specialized_edge_merge_queries_target_public_labels() {
        let tenant_id = Uuid::from_u128(0x1);
        let session_id = Uuid::from_u128(0x2);
        let source = Uuid::from_u128(0x3);
        let target = Uuid::from_u128(0x4);
        let folded = build_folded_into_merge_query(tenant_id, session_id, source, target);
        let mentioned = build_mentioned_in_merge_query(tenant_id, session_id, source, target);
        let co_occurs = build_co_occurs_merge_query(tenant_id, session_id, source, target, 0.5);
        let supersedes = build_supersedes_merge_query(tenant_id, source, source, target);
        assert!(folded.contains(":FOLDED_INTO"));
        assert!(mentioned.contains(":MENTIONED_IN"));
        assert!(co_occurs.contains(":CO_OCCURS_WITH"));
        assert!(co_occurs.contains("r.strength = 0.5"));
        assert!(supersedes.contains(":SUPERSEDES"));
    }
}
