//! CQL storage backend using cdrs-tokio.
//!
//! Implements the [`Storage`] trait against a real Ferrosa/Cassandra cluster.
//! All queries use prepared statements with parameterized bindings (STRIDE T4).
//! Every query includes `tenant_id` from auth context (STRIDE I1).

use std::sync::Arc;

use cdrs_tokio::authenticators::NoneAuthenticatorProvider;
use cdrs_tokio::cluster::session::{Session, SessionBuilder, TcpSessionBuilder};
use cdrs_tokio::cluster::{NodeTcpConfigBuilder, TcpConnectionManager};
use cdrs_tokio::load_balancing::RoundRobinLoadBalancingStrategy;
use cdrs_tokio::query::*;
use cdrs_tokio::query_values;
use cdrs_tokio::transport::TransportTcp;
use cdrs_tokio::types::ByName;
use cdrs_tokio::types::rows::Row;
use uuid::Uuid;

use crate::config::FerrosaCqlConfig;
use crate::storage::Storage;
use crate::types::*;

fn parse_decay_zone(s: &str) -> DecayZone {
    match s {
        "identity" => DecayZone::Identity,
        "operational" => DecayZone::Operational,
        _ => DecayZone::Knowledge,
    }
}

fn parse_rule_state(s: &str) -> RuleState {
    match s {
        "deprecated" => RuleState::Deprecated,
        "superseded" => RuleState::Superseded,
        _ => RuleState::Active,
    }
}

/// Extract the Sprint 1 "rich schema" columns from a CQL row.
///
/// Returns the fields as a tuple in the order they appear on `EntityEntry`:
/// `(description, description_embedding, tags, properties, content_hash,
/// updated_at, scope, ingested_by_session)`. Every field is tolerant of
/// missing/NULL data — legacy rows ingested before the migration return
/// sensible defaults (None / empty / Session) so reads never fail.
#[allow(clippy::type_complexity)]
fn extract_rich_entity_fields(
    row: &Row,
) -> (
    Option<String>,
    Option<Vec<f32>>,
    Vec<String>,
    serde_json::Value,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
    EntityScope,
    Option<Uuid>,
) {
    let description = row.r_by_name::<String>("description").ok();
    let description_embedding = row
        .r_by_name::<cdrs_tokio::types::blob::Blob>("description_embedding")
        .ok()
        .map(|blob| blob.into_vec())
        .filter(|v| !v.is_empty())
        .map(|v| crate::vector::decode_vector(&v));
    let tags = row
        .r_by_name::<String>("tags")
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default();
    let properties = row
        .r_by_name::<String>("properties")
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let content_hash = row.r_by_name::<String>("content_hash").ok();
    let updated_at = row
        .r_by_name::<chrono::NaiveDateTime>("updated_at")
        .ok()
        .map(|ndt| ndt.and_utc());
    let scope = row
        .r_by_name::<String>("scope")
        .ok()
        .and_then(|s| match s.as_str() {
            "global" => Some(EntityScope::Global),
            "session" => Some(EntityScope::Session),
            _ => None,
        })
        .unwrap_or_default();
    let ingested_by_session = row.r_by_name::<Uuid>("ingested_by_session").ok();
    (
        description,
        description_embedding,
        tags,
        properties,
        content_hash,
        updated_at,
        scope,
        ingested_by_session,
    )
}

/// Type alias for the cdrs-tokio TCP session.
pub type CqlSession = Session<
    TransportTcp,
    TcpConnectionManager,
    RoundRobinLoadBalancingStrategy<TransportTcp, TcpConnectionManager>,
>;

/// Prepared statement cache for all table operations.
struct PreparedStatements {
    // Memo
    memo_get: PreparedQuery,
    memo_touch: PreparedQuery,
    memo_put: PreparedQuery,
    // Plan
    plan_put: PreparedQuery,
    plan_get: PreparedQuery,
    plan_get_depth: PreparedQuery,
    plan_update: PreparedQuery,
    // Fold
    fold_put: PreparedQuery,
    fold_get: PreparedQuery,
    fold_append: PreparedQuery,
    fold_complete: PreparedQuery,
    // Entity
    entity_put: PreparedQuery,
    entity_count: PreparedQuery,
    entity_list_session: PreparedQuery,
    entity_list_all: PreparedQuery,
    entity_update_state: PreparedQuery,
    // Count queries for stats
    fold_count: PreparedQuery,
    memo_count: PreparedQuery,
    // Temporal
    temporal_put: PreparedQuery,
    temporal_get_current: PreparedQuery,
    temporal_invalidate: PreparedQuery,
    // Edges
    edge_folded_into: PreparedQuery,
    edge_mentioned_in: PreparedQuery,
    edge_co_occurs: PreparedQuery,
    edge_supersedes: PreparedQuery,
    // Entity neighbor queries (spreading activation)
    edge_mentioned_in_by_entity: PreparedQuery,
    edge_co_occurs_by_a: PreparedQuery,
    edge_co_occurs_by_b: PreparedQuery,
    edge_supersedes_by_new: PreparedQuery,
    edge_supersedes_by_old: PreparedQuery,
    // Feedback
    feedback_put: PreparedQuery,
    feedback_list_all: PreparedQuery,
    // Intentions
    intention_put: PreparedQuery,
    intention_list: PreparedQuery,
    intention_list_all: PreparedQuery,
    intention_update_status: PreparedQuery,
    // Tool usage logging
    tool_usage_put: PreparedQuery,
    tool_usage_query: PreparedQuery,
    // Audit
    audit_put: PreparedQuery,
    // Sync/export list queries
    fold_list_all: PreparedQuery,
    temporal_list_all: PreparedQuery,
    // Warmth (Sprint 5)
    warmth_get: PreparedQuery,
    warmth_put: PreparedQuery,
    warmth_list_session: PreparedQuery,
    warmth_delete: PreparedQuery,
    // Rules (Sprint 5)
    rule_put_by_id: PreparedQuery,
    rule_put_by_family: PreparedQuery,
    rule_get: PreparedQuery,
    rule_list_family: PreparedQuery,
    // Derived cache (Sprint 5)
    derived_cache_get: PreparedQuery,
    derived_cache_put: PreparedQuery,
    derived_cache_clear: PreparedQuery,
    // Provenance (Sprint 5)
    provenance_put: PreparedQuery,
    provenance_get: PreparedQuery,
}

/// CQL storage backend.
pub struct CqlStorage {
    session: Arc<CqlSession>,
    stmts: PreparedStatements,
    keyspace: String,
}

impl CqlStorage {
    /// Connect to a Ferrosa/Cassandra cluster and prepare all statements.
    pub async fn connect(config: &FerrosaCqlConfig) -> anyhow::Result<Self> {
        let node_config = if config.contact_points.is_empty() {
            anyhow::bail!("no contact points configured");
        } else {
            let mut builder = NodeTcpConfigBuilder::new()
                .with_authenticator_provider(Arc::new(NoneAuthenticatorProvider));
            for cp in &config.contact_points {
                builder = builder.with_contact_point(cp.as_str().into());
            }
            builder.build().await?
        };

        let session = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            TcpSessionBuilder::new(RoundRobinLoadBalancingStrategy::new(), node_config).build(),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("CQL session build timed out (10s) — is Ferrosa running?")
        })??;

        let session = Arc::new(session);
        let ks = &config.keyspace;

        // Prepare all statements
        let stmts = PreparedStatements {
            memo_get: session
                .prepare(format!(
                    "SELECT result, result_embedding, hit_count, created_at, last_hit_at, expires_at \
                     FROM {ks}.memo_cache WHERE content_hash = ? AND model_version = ? AND tenant_id = ?"
                ))
                .await?,
            memo_touch: session
                .prepare(format!(
                    "UPDATE {ks}.memo_cache SET hit_count = hit_count + 1, last_hit_at = ? \
                     WHERE content_hash = ? AND model_version = ? AND tenant_id = ?"
                ))
                .await?,
            memo_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.memo_cache \
                     (content_hash, model_version, tenant_id, result, result_embedding, created_at, last_hit_at, hit_count, expires_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            plan_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.plan_state \
                     (session_id, tenant_id, depth, subtask_id, parent_subtask, goal_text, status, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            plan_get: session
                .prepare(format!(
                    "SELECT depth, subtask_id, parent_subtask, goal_text, status, outcome_summary, created_at, completed_at \
                     FROM {ks}.plan_state WHERE session_id = ? AND tenant_id = ?"
                ))
                .await?,
            plan_get_depth: session
                .prepare(format!(
                    "SELECT depth, subtask_id, parent_subtask, goal_text, status, outcome_summary, created_at, completed_at \
                     FROM {ks}.plan_state WHERE session_id = ? AND tenant_id = ? AND depth <= ?"
                ))
                .await?,
            plan_update: session
                .prepare(format!(
                    "UPDATE {ks}.plan_state SET status = ?, outcome_summary = ?, completed_at = ? \
                     WHERE session_id = ? AND tenant_id = ? AND depth = ? AND subtask_id = ?"
                ))
                .await?,
            fold_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.trajectory_folds \
                     (session_id, fold_id, tenant_id, depth, parent_fold_id, raw_trajectory, \
                      token_count, status, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            fold_get: session
                .prepare(format!(
                    "SELECT fold_id, depth, parent_fold_id, raw_trajectory, fold_summary, \
                     fold_embedding, token_count, compression_ratio, status, created_at, folded_at \
                     FROM {ks}.trajectory_folds WHERE session_id = ? AND tenant_id = ? AND fold_id = ?"
                ))
                .await?,
            fold_append: session
                .prepare(format!(
                    "UPDATE {ks}.trajectory_folds SET raw_trajectory = ?, token_count = ? \
                     WHERE session_id = ? AND tenant_id = ? AND fold_id = ?"
                ))
                .await?,
            fold_complete: session
                .prepare(format!(
                    "UPDATE {ks}.trajectory_folds \
                     SET status = ?, fold_summary = ?, fold_embedding = ?, compression_ratio = ?, folded_at = ? \
                     WHERE session_id = ? AND tenant_id = ? AND fold_id = ?"
                ))
                .await?,
            entity_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.entity_store \
                     (tenant_id, entity_id, session_id, entity_name, entity_type, \
                      context_snippet, confidence, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            entity_count: session
                .prepare(format!(
                    "SELECT entity_id FROM {ks}.entity_store WHERE tenant_id = ? AND session_id = ?"
                ))
                .await?,
            entity_list_session: session
                .prepare(format!(
                    "SELECT entity_id, entity_name, entity_type, source_fold_id, \
                     context_snippet, entity_embedding, confidence, state, created_at, \
                     description, description_embedding, tags, properties, content_hash, \
                     updated_at, scope, ingested_by_session \
                     FROM {ks}.entity_store WHERE tenant_id = ? AND session_id = ?"
                ))
                .await?,
            entity_list_all: session
                .prepare(format!(
                    "SELECT entity_id, session_id, entity_name, entity_type, source_fold_id, \
                     context_snippet, entity_embedding, confidence, state, created_at, \
                     description, description_embedding, tags, properties, content_hash, \
                     updated_at, scope, ingested_by_session \
                     FROM {ks}.entity_store WHERE tenant_id = ? ALLOW FILTERING"
                ))
                .await?,
            entity_update_state: session
                .prepare(format!(
                    "UPDATE {ks}.entity_store SET state = ? \
                     WHERE tenant_id = ? AND session_id = ? AND entity_id = ?"
                ))
                .await?,
            fold_count: session
                .prepare(format!(
                    "SELECT fold_id FROM {ks}.trajectory_folds WHERE tenant_id = ? AND session_id = ?"
                ))
                .await?,
            memo_count: session
                .prepare(format!(
                    "SELECT content_hash FROM {ks}.memo_cache WHERE tenant_id = ?"
                ))
                .await?,
            temporal_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.temporal_events \
                     (tenant_id, entity_id, event_time, event_id, fact_text, \
                      supersedes_id, valid_until, source_session, confidence) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            temporal_get_current: session
                .prepare(format!(
                    "SELECT event_time, event_id, fact_text, supersedes_id, source_session, confidence \
                     FROM {ks}.temporal_events WHERE tenant_id = ? AND entity_id = ? LIMIT 10"
                ))
                .await?,
            temporal_invalidate: session
                .prepare(format!(
                    "UPDATE {ks}.temporal_events SET valid_until = ? \
                     WHERE tenant_id = ? AND entity_id = ? AND event_time = ? AND event_id = ?"
                ))
                .await?,
            feedback_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.feedback_outcomes \
                     (tenant_id, session_id, query_id, program_type, task_complexity, \
                      succeeded, latency_ms, token_cost, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            feedback_list_all: session
                .prepare(format!(
                    "SELECT tenant_id, session_id, query_id, program_type, task_complexity, \
                     succeeded, latency_ms, token_cost, created_at \
                     FROM {ks}.feedback_outcomes"
                ))
                .await?,
            // Edges
            edge_folded_into: session
                .prepare(format!(
                    "INSERT INTO {ks}.folded_into \
                     (source_fold_id, target_fold_id, session_id, tenant_id, created_at) \
                     VALUES (?, ?, ?, ?, ?)"
                ))
                .await?,
            edge_mentioned_in: session
                .prepare(format!(
                    "INSERT INTO {ks}.mentioned_in \
                     (entity_id, fold_id, session_id, tenant_id, created_at) \
                     VALUES (?, ?, ?, ?, ?)"
                ))
                .await?,
            edge_co_occurs: session
                .prepare(format!(
                    "INSERT INTO {ks}.co_occurs_with \
                     (entity_a, entity_b, session_id, tenant_id, created_at, strength, last_reinforced) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            edge_supersedes: session
                .prepare(format!(
                    "INSERT INTO {ks}.supersedes \
                     (new_event_id, old_event_id, entity_id, tenant_id, created_at) \
                     VALUES (?, ?, ?, ?, ?)"
                ))
                .await?,
            // edge_list_* queries use dynamic queries in edge_list_session()
            // because ALLOW FILTERING with prepared statements is unreliable.
            // Entity neighbor queries (spreading activation)
            edge_mentioned_in_by_entity: session
                .prepare(format!(
                    "SELECT fold_id \
                     FROM {ks}.mentioned_in WHERE entity_id = ? AND tenant_id = ? ALLOW FILTERING"
                ))
                .await?,
            edge_co_occurs_by_a: session
                .prepare(format!(
                    "SELECT entity_b \
                     FROM {ks}.co_occurs_with WHERE entity_a = ? AND tenant_id = ? ALLOW FILTERING"
                ))
                .await?,
            edge_co_occurs_by_b: session
                .prepare(format!(
                    "SELECT entity_a \
                     FROM {ks}.co_occurs_with WHERE entity_b = ? AND tenant_id = ? ALLOW FILTERING"
                ))
                .await?,
            edge_supersedes_by_new: session
                .prepare(format!(
                    "SELECT old_event_id \
                     FROM {ks}.supersedes WHERE new_event_id = ? AND tenant_id = ? ALLOW FILTERING"
                ))
                .await?,
            edge_supersedes_by_old: session
                .prepare(format!(
                    "SELECT new_event_id \
                     FROM {ks}.supersedes WHERE old_event_id = ? AND tenant_id = ? ALLOW FILTERING"
                ))
                .await?,
            // Intentions (repo-scoped)
            intention_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.intentions \
                     (tenant_id, repo, intention_id, description, trigger_json, priority, \
                      status, created_at, triggered_at, completed_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            intention_list: session
                .prepare(format!(
                    "SELECT intention_id, repo, description, trigger_json, priority, status, \
                     created_at, triggered_at, completed_at \
                     FROM {ks}.intentions WHERE tenant_id = ? AND repo = ?"
                ))
                .await?,
            intention_list_all: session
                .prepare(format!(
                    "SELECT intention_id, repo, description, trigger_json, priority, status, \
                     created_at, triggered_at, completed_at \
                     FROM {ks}.intentions WHERE tenant_id = ? ALLOW FILTERING"
                ))
                .await?,
            intention_update_status: session
                .prepare(format!(
                    "UPDATE {ks}.intentions SET status = ?, triggered_at = ?, completed_at = ? \
                     WHERE tenant_id = ? AND repo = ? AND intention_id = ?"
                ))
                .await?,
            tool_usage_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.tool_usage_log \
                     (tenant_id, day, call_id, tool_name, repo, input_bytes, output_bytes, \
                      estimated_tokens, latency_ms, error) \
                     VALUES (?, ?, now(), ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            tool_usage_query: session
                .prepare(format!(
                    "SELECT tool_name, repo, input_bytes, output_bytes, estimated_tokens, \
                     latency_ms, error, dateOf(call_id) as created_at \
                     FROM {ks}.tool_usage_log WHERE tenant_id = ? AND day = ?"
                ))
                .await?,
            audit_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.audit_log \
                     (tenant_id, audit_id, operation, target_table, target_id, session_id, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            fold_list_all: session
                .prepare(format!(
                    "SELECT session_id, fold_id, depth, parent_fold_id, raw_trajectory, \
                     fold_summary, fold_embedding, token_count, compression_ratio, status, \
                     created_at, folded_at \
                     FROM {ks}.trajectory_folds WHERE tenant_id = ? ALLOW FILTERING"
                ))
                .await?,
            temporal_list_all: session
                .prepare(format!(
                    "SELECT entity_id, event_time, event_id, fact_text, supersedes_id, \
                     valid_until, source_session, confidence \
                     FROM {ks}.temporal_events WHERE tenant_id = ? ALLOW FILTERING"
                ))
                .await?,
            // Warmth (Sprint 5)
            warmth_get: session
                .prepare(format!(
                    "SELECT entity_id, session_id, warmth, pagerank, last_accessed_at, \
                     access_count, decay_zone, updated_at \
                     FROM {ks}.entity_warmth WHERE tenant_id = ? AND entity_id = ?"
                ))
                .await?,
            warmth_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.entity_warmth \
                     (tenant_id, entity_id, session_id, warmth, pagerank, reputation, last_accessed_at, \
                      access_count, decay_zone, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            warmth_list_session: session
                .prepare(format!(
                    "SELECT entity_id, session_id, warmth, pagerank, last_accessed_at, \
                     access_count, decay_zone, updated_at \
                     FROM {ks}.entity_warmth WHERE session_id = ?"
                ))
                .await?,
            warmth_delete: session
                .prepare(format!(
                    "DELETE FROM {ks}.entity_warmth WHERE tenant_id = ? AND entity_id = ?"
                ))
                .await?,
            // Rules (Sprint 5)
            rule_put_by_id: session
                .prepare(format!(
                    "INSERT INTO {ks}.rules_by_id \
                     (tenant_id, rule_id, version, name, family, state, rule_body, \
                      rule_weight, incremental, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            rule_put_by_family: session
                .prepare(format!(
                    "INSERT INTO {ks}.rules_by_family \
                     (tenant_id, family, state, rule_id, version, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            rule_get: session
                .prepare(format!(
                    "SELECT rule_id, version, name, family, state, rule_body, \
                     rule_weight, incremental, created_at, updated_at \
                     FROM {ks}.rules_by_id WHERE tenant_id = ? AND rule_id = ? LIMIT 1"
                ))
                .await?,
            rule_list_family: session
                .prepare(format!(
                    "SELECT rule_id, version, name, family, state, rule_body, \
                     rule_weight, incremental, created_at, updated_at \
                     FROM {ks}.rules_by_id WHERE tenant_id = ? ALLOW FILTERING"
                ))
                .await?,
            // Derived cache (Sprint 5)
            derived_cache_get: session
                .prepare(format!(
                    "SELECT seq, src_id, pred, dst_id, confidence, rule_id, computed_at \
                     FROM {ks}.derived_cache_by_query WHERE tenant_id = ? AND cache_key = ?"
                ))
                .await?,
            derived_cache_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.derived_cache_by_query \
                     (tenant_id, cache_key, seq, src_id, pred, dst_id, confidence, rule_id, computed_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            derived_cache_clear: session
                .prepare(format!(
                    "DELETE FROM {ks}.derived_cache_by_query WHERE tenant_id = ? AND cache_key = ?"
                ))
                .await?,
            // Provenance (Sprint 5)
            provenance_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.derivation_provenance \
                     (tenant_id, derived_edge_id, seq, parent_src, parent_pred, parent_dst, parent_kind) \
                     VALUES (?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            provenance_get: session
                .prepare(format!(
                    "SELECT seq, parent_src, parent_pred, parent_dst, parent_kind \
                     FROM {ks}.derivation_provenance WHERE tenant_id = ? AND derived_edge_id = ?"
                ))
                .await?,
        };

        tracing::info!(
            keyspace = ks,
            statements = 44,
            "CQL storage connected, all statements prepared"
        );

        Ok(Self {
            session,
            stmts,
            keyspace: ks.to_string(),
        })
    }

    /// Get a reference to the raw CQL session for ad-hoc queries.
    pub fn session(&self) -> &CqlSession {
        &self.session
    }

    /// Load entity types from the type registry table.
    /// Returns the default set if the table doesn't exist or is empty.
    pub async fn load_entity_types(&self) -> Vec<String> {
        let query = format!("SELECT type_name FROM {}.entity_types", self.keyspace);
        match self.session.query(query).await {
            Ok(frame) => {
                let rows = frame
                    .response_body()
                    .ok()
                    .and_then(|b| b.into_rows())
                    .unwrap_or_default();
                let mut types: Vec<String> = rows
                    .iter()
                    .filter_map(|r| r.r_by_name::<String>("type_name").ok())
                    .collect();
                if types.is_empty() {
                    return Self::default_entity_types();
                }
                types.sort();
                types
            }
            Err(_) => Self::default_entity_types(),
        }
    }

    /// Load edge types from the type registry table.
    pub async fn load_edge_types(&self) -> Vec<String> {
        let query = format!("SELECT type_name FROM {}.edge_types", self.keyspace);
        match self.session.query(query).await {
            Ok(frame) => {
                let rows = frame
                    .response_body()
                    .ok()
                    .and_then(|b| b.into_rows())
                    .unwrap_or_default();
                let mut types: Vec<String> = rows
                    .iter()
                    .filter_map(|r| r.r_by_name::<String>("type_name").ok())
                    .collect();
                types.sort();
                types
            }
            Err(_) => Vec::new(),
        }
    }

    pub fn default_entity_types() -> Vec<String> {
        [
            "person",
            "place",
            "event",
            "concept",
            "org",
            "bug",
            "decision",
            "pattern",
            "preference",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// Get the keyspace name this storage is connected to.
    pub fn keyspace(&self) -> &str {
        &self.keyspace
    }

    /// Helper: execute a prepared query and return rows.
    async fn query_rows(
        &self,
        stmt: &PreparedQuery,
        values: QueryValues,
    ) -> anyhow::Result<Vec<Row>> {
        let envelope = self.session.exec_with_values(stmt, values).await?;
        let body = envelope.response_body()?;
        Ok(body.into_rows().unwrap_or_default())
    }
}

impl Storage for CqlStorage {
    async fn memo_get(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<Option<MemoEntry>> {
        let rows = self
            .query_rows(
                &self.stmts.memo_get,
                query_values!(
                    content_hash.to_string(),
                    model_version.to_string(),
                    ctx.tenant_id
                ),
            )
            .await?;

        if let Some(row) = rows.into_iter().next() {
            let result: String = row.r_by_name("result")?;
            let hit_count: i64 = row.r_by_name("hit_count")?;
            let created_at: chrono::NaiveDateTime = row.r_by_name("created_at")?;

            Ok(Some(MemoEntry {
                content_hash: content_hash.to_string(),
                model_version: model_version.to_string(),
                result,
                result_embedding: row
                    .r_by_name::<cdrs_tokio::types::blob::Blob>("result_embedding")
                    .ok()
                    .map(|blob| blob.into_vec())
                    .filter(|v| !v.is_empty())
                    .map(|v| crate::vector::decode_vector(&v)),
                hit_count,
                created_at: created_at.and_utc(),
                last_hit_at: row
                    .r_by_name::<chrono::NaiveDateTime>("last_hit_at")
                    .ok()
                    .map(|t| t.and_utc()),
                expires_at: row
                    .r_by_name::<chrono::NaiveDateTime>("expires_at")
                    .ok()
                    .map(|t| t.and_utc()),
            }))
        } else {
            Ok(None)
        }
    }

    async fn memo_touch(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().naive_utc();
        self.session
            .exec_with_values(
                &self.stmts.memo_touch,
                query_values!(
                    now,
                    content_hash.to_string(),
                    model_version.to_string(),
                    ctx.tenant_id
                ),
            )
            .await?;
        Ok(())
    }

    async fn memo_put(&self, ctx: &TenantContext, entry: &MemoEntry) -> anyhow::Result<()> {
        let now = chrono::Utc::now().naive_utc();
        let expires: Option<chrono::NaiveDateTime> = entry.expires_at.map(|t| t.naive_utc());

        self.session
            .exec_with_values(
                &self.stmts.memo_put,
                query_values!(
                    entry.content_hash.clone(),
                    entry.model_version.clone(),
                    ctx.tenant_id,
                    entry.result.clone(),
                    entry.result_embedding.as_ref().map(|e| {
                        cdrs_tokio::types::blob::Blob::new(crate::vector::encode_vector(e))
                    }),
                    now,
                    now,  // last_hit_at = created_at initially
                    0i64, // hit_count
                    expires
                ),
            )
            .await?;
        Ok(())
    }

    async fn plan_put(&self, ctx: &TenantContext, node: &PlanNode) -> anyhow::Result<()> {
        let status = serde_json::to_string(&node.status)?
            .trim_matches('"')
            .to_string();

        self.session
            .exec_with_values(
                &self.stmts.plan_put,
                query_values!(
                    node.session_id,
                    ctx.tenant_id,
                    node.depth,
                    node.subtask_id.clone(),
                    node.parent_subtask.clone(),
                    node.goal_text.clone(),
                    status,
                    chrono::Utc::now().naive_utc()
                ),
            )
            .await?;
        Ok(())
    }

    async fn plan_get(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        max_depth: Option<i32>,
    ) -> anyhow::Result<Vec<PlanNode>> {
        let rows = if let Some(depth) = max_depth {
            self.query_rows(
                &self.stmts.plan_get_depth,
                query_values!(session_id, ctx.tenant_id, depth),
            )
            .await?
        } else {
            self.query_rows(
                &self.stmts.plan_get,
                query_values!(session_id, ctx.tenant_id),
            )
            .await?
        };

        let mut nodes = Vec::with_capacity(rows.len());
        for row in rows {
            let status_str: String = row.r_by_name("status")?;
            let status: PlanStatus =
                serde_json::from_str(&format!("\"{status_str}\"")).unwrap_or(PlanStatus::Pending);

            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();

            nodes.push(PlanNode {
                session_id,
                depth: row.r_by_name("depth")?,
                subtask_id: row.r_by_name("subtask_id")?,
                parent_subtask: row.r_by_name::<String>("parent_subtask").ok(),
                goal_text: row.r_by_name("goal_text")?,
                status,
                outcome_summary: row.r_by_name::<String>("outcome_summary").ok(),
                created_at: created.and_utc(),
                completed_at: row
                    .r_by_name::<chrono::NaiveDateTime>("completed_at")
                    .ok()
                    .map(|t| t.and_utc()),
            });
        }

        Ok(nodes)
    }

    async fn plan_update_status(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        depth: i32,
        subtask_id: &str,
        status: PlanStatus,
        outcome_summary: Option<&str>,
    ) -> anyhow::Result<()> {
        let status_str = serde_json::to_string(&status)?
            .trim_matches('"')
            .to_string();
        let completed = if status == PlanStatus::Complete || status == PlanStatus::Failed {
            Some(chrono::Utc::now().naive_utc())
        } else {
            None
        };

        self.session
            .exec_with_values(
                &self.stmts.plan_update,
                query_values!(
                    status_str,
                    outcome_summary.map(String::from),
                    completed,
                    session_id,
                    ctx.tenant_id,
                    depth,
                    subtask_id.to_string()
                ),
            )
            .await?;
        Ok(())
    }

    // --- Fold operations ---

    async fn fold_put(&self, ctx: &TenantContext, entry: &FoldEntry) -> anyhow::Result<()> {
        let status = serde_json::to_string(&entry.status)?
            .trim_matches('"')
            .to_string();

        self.session
            .exec_with_values(
                &self.stmts.fold_put,
                query_values!(
                    entry.session_id,
                    entry.fold_id,
                    ctx.tenant_id,
                    entry.depth,
                    entry.parent_fold_id,
                    entry.raw_trajectory.clone(),
                    entry.token_count,
                    status,
                    chrono::Utc::now().naive_utc()
                ),
            )
            .await?;
        Ok(())
    }

    async fn fold_get(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        fold_id: Uuid,
    ) -> anyhow::Result<Option<FoldEntry>> {
        let rows = self
            .query_rows(
                &self.stmts.fold_get,
                query_values!(session_id, ctx.tenant_id, fold_id),
            )
            .await?;

        if let Some(row) = rows.into_iter().next() {
            let status_str: String = row.r_by_name("status")?;
            let status: FoldStatus =
                serde_json::from_str(&format!("\"{status_str}\"")).unwrap_or(FoldStatus::Active);
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();

            Ok(Some(FoldEntry {
                session_id,
                fold_id,
                tenant_id: ctx.tenant_id,
                depth: row.r_by_name("depth")?,
                parent_fold_id: row.r_by_name::<Uuid>("parent_fold_id").ok(),
                raw_trajectory: row.r_by_name("raw_trajectory")?,
                fold_summary: row.r_by_name::<String>("fold_summary").ok(),
                fold_embedding: row
                    .r_by_name::<cdrs_tokio::types::blob::Blob>("fold_embedding")
                    .ok()
                    .map(|blob| blob.into_vec())
                    .filter(|v| !v.is_empty())
                    .map(|v| crate::vector::decode_vector(&v)),
                token_count: row.r_by_name("token_count")?,
                compression_ratio: row.r_by_name::<f64>("compression_ratio").ok(),
                status,
                created_at: created.and_utc(),
                folded_at: row
                    .r_by_name::<chrono::NaiveDateTime>("folded_at")
                    .ok()
                    .map(|t| t.and_utc()),
            }))
        } else {
            Ok(None)
        }
    }

    async fn fold_append(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        fold_id: Uuid,
        text: &str,
    ) -> anyhow::Result<()> {
        // Read current trajectory, append, write back
        // (CQL doesn't have string append — need read-modify-write)
        if let Some(fold) = self.fold_get(ctx, session_id, fold_id).await? {
            let new_trajectory = format!("{}\n{}", fold.raw_trajectory, text);
            let new_count = new_trajectory.split_whitespace().count() as i32;

            self.session
                .exec_with_values(
                    &self.stmts.fold_append,
                    query_values!(
                        new_trajectory,
                        new_count,
                        session_id,
                        ctx.tenant_id,
                        fold_id
                    ),
                )
                .await?;
        }
        Ok(())
    }

    async fn fold_complete(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        fold_id: Uuid,
        summary: &str,
        embedding: Vec<f32>,
        compression_ratio: f64,
    ) -> anyhow::Result<()> {
        let embedding_blob =
            cdrs_tokio::types::blob::Blob::new(crate::vector::encode_vector(&embedding));
        self.session
            .exec_with_values(
                &self.stmts.fold_complete,
                query_values!(
                    "folded".to_string(),
                    summary.to_string(),
                    embedding_blob,
                    compression_ratio,
                    chrono::Utc::now().naive_utc(),
                    session_id,
                    ctx.tenant_id,
                    fold_id
                ),
            )
            .await?;
        Ok(())
    }

    async fn fold_search(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        query_embedding: &[f32],
        k: usize,
        include_raw: bool,
    ) -> anyhow::Result<Vec<FoldSummary>> {
        // ANN query using ORDER BY fold_embedding ANN OF ?
        // Falls back to LIMIT-based if ANN query fails (no HNSW index)
        let query_blob =
            cdrs_tokio::types::blob::Blob::new(crate::vector::encode_vector(query_embedding));
        let query = format!(
            "SELECT fold_id, depth, fold_summary, token_count, raw_trajectory \
             FROM {}.trajectory_folds WHERE session_id = ? AND tenant_id = ? \
             ORDER BY fold_embedding ANN OF ? LIMIT ?",
            self.keyspace
        );
        let envelope = match self
            .session
            .query_with_values(
                query,
                query_values!(session_id, ctx.tenant_id, query_blob, k as i32),
            )
            .await
        {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "ANN query failed, falling back to LIMIT");
                let fallback = format!(
                    "SELECT fold_id, depth, fold_summary, token_count, raw_trajectory \
                     FROM {}.trajectory_folds WHERE session_id = ? AND tenant_id = ? LIMIT ?",
                    self.keyspace
                );
                self.session
                    .query_with_values(fallback, query_values!(session_id, ctx.tenant_id, k as i32))
                    .await?
            }
        };

        let rows = envelope.response_body()?.into_rows().unwrap_or_default();
        let mut results = Vec::new();
        for row in rows {
            if let Ok(summary) = row.r_by_name::<String>("fold_summary") {
                results.push(FoldSummary {
                    fold_id: row.r_by_name("fold_id")?,
                    depth: row.r_by_name("depth")?,
                    fold_summary: summary,
                    token_count: row.r_by_name("token_count")?,
                    similarity: None,
                    raw_trajectory: if include_raw {
                        row.r_by_name::<String>("raw_trajectory").ok()
                    } else {
                        None
                    },
                });
            }
        }
        Ok(results)
    }

    // --- Entity operations ---

    async fn entity_put(&self, ctx: &TenantContext, entry: &EntityEntry) -> anyhow::Result<()> {
        // Base INSERT with required fields only — avoids cdrs-tokio Option
        // serialization issues with Ferrosa's VECTOR columns.
        self.session
            .exec_with_values(
                &self.stmts.entity_put,
                query_values!(
                    ctx.tenant_id,
                    entry.entity_id,
                    entry.session_id,
                    entry.entity_name.clone(),
                    entry.entity_type.clone(),
                    entry.context_snippet.clone(),
                    entry.confidence as f32,
                    chrono::Utc::now().naive_utc()
                ),
            )
            .await?;

        // Set optional fields via UPDATE (CQL upsert semantics).
        if let Some(fold_id) = entry.source_fold_id {
            let q = format!(
                "UPDATE {}.entity_store SET source_fold_id = ? \
                 WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
                self.keyspace
            );
            let _ = self
                .session
                .query_with_values(
                    q,
                    query_values!(fold_id, ctx.tenant_id, entry.session_id, entry.entity_id),
                )
                .await;
        }
        if let Some(ref emb) = entry.entity_embedding {
            // cdrs-tokio doesn't support the VECTOR CQL type — Blob is rejected
            // as type mismatch. Use a raw CQL literal [f32, f32, ...] instead.
            let vec_literal: String = format!(
                "[{}]",
                emb.iter()
                    .map(|v| format!("{v:.8}"))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let q = format!(
                "UPDATE {ks}.entity_store SET entity_embedding = {vec_literal} \
                 WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
                ks = self.keyspace,
            );
            if let Err(e) = self
                .session
                .query_with_values(
                    q,
                    query_values!(ctx.tenant_id, entry.session_id, entry.entity_id),
                )
                .await
            {
                tracing::warn!(entity_id = %entry.entity_id, error = %e, "failed to store entity_embedding");
            }
        }

        // --- Rich entity fields (Sprint 1 slice 1b) ---
        // scope + updated_at are always set. ingested_by_session is optional
        // (populated for global-scope entities to record which session
        // originally ingested them).
        let scope_str = match entry.scope {
            EntityScope::Session => "session",
            EntityScope::Global => "global",
        };
        let updated_at_ndt = entry
            .updated_at
            .unwrap_or(entry.created_at)
            .naive_utc();
        let q = format!(
            "UPDATE {ks}.entity_store SET scope = ?, updated_at = ? \
             WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
            ks = self.keyspace,
        );
        if let Err(e) = self
            .session
            .query_with_values(
                q,
                query_values!(
                    scope_str.to_string(),
                    updated_at_ndt,
                    ctx.tenant_id,
                    entry.session_id,
                    entry.entity_id
                ),
            )
            .await
        {
            tracing::warn!(entity_id = %entry.entity_id, error = %e, "failed to store scope/updated_at");
        }

        if let Some(ingester) = entry.ingested_by_session {
            let q = format!(
                "UPDATE {ks}.entity_store SET ingested_by_session = ? \
                 WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
                ks = self.keyspace,
            );
            if let Err(e) = self
                .session
                .query_with_values(
                    q,
                    query_values!(
                        ingester,
                        ctx.tenant_id,
                        entry.session_id,
                        entry.entity_id
                    ),
                )
                .await
            {
                tracing::warn!(entity_id = %entry.entity_id, error = %e, "failed to store ingested_by_session");
            }
        }

        // Optional text fields.
        if let Some(ref desc) = entry.description {
            let q = format!(
                "UPDATE {ks}.entity_store SET description = ? \
                 WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
                ks = self.keyspace,
            );
            if let Err(e) = self
                .session
                .query_with_values(
                    q,
                    query_values!(
                        desc.clone(),
                        ctx.tenant_id,
                        entry.session_id,
                        entry.entity_id
                    ),
                )
                .await
            {
                tracing::warn!(entity_id = %entry.entity_id, error = %e, "failed to store description");
            }
        }

        if let Some(ref emb) = entry.description_embedding {
            let vec_literal: String = format!(
                "[{}]",
                emb.iter()
                    .map(|v| format!("{v:.8}"))
                    .collect::<Vec<_>>()
                    .join(",")
            );
            let q = format!(
                "UPDATE {ks}.entity_store SET description_embedding = {vec_literal} \
                 WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
                ks = self.keyspace,
            );
            if let Err(e) = self
                .session
                .query_with_values(
                    q,
                    query_values!(ctx.tenant_id, entry.session_id, entry.entity_id),
                )
                .await
            {
                tracing::warn!(entity_id = %entry.entity_id, error = %e, "failed to store description_embedding");
            }
        }

        if !entry.tags.is_empty() {
            let tags_json = serde_json::to_string(&entry.tags).unwrap_or_else(|_| "[]".into());
            let q = format!(
                "UPDATE {ks}.entity_store SET tags = ? \
                 WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
                ks = self.keyspace,
            );
            if let Err(e) = self
                .session
                .query_with_values(
                    q,
                    query_values!(
                        tags_json,
                        ctx.tenant_id,
                        entry.session_id,
                        entry.entity_id
                    ),
                )
                .await
            {
                tracing::warn!(entity_id = %entry.entity_id, error = %e, "failed to store tags");
            }
        }

        if !entry.properties.is_null() {
            let props_json =
                serde_json::to_string(&entry.properties).unwrap_or_else(|_| "{}".into());
            let q = format!(
                "UPDATE {ks}.entity_store SET properties = ? \
                 WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
                ks = self.keyspace,
            );
            if let Err(e) = self
                .session
                .query_with_values(
                    q,
                    query_values!(
                        props_json,
                        ctx.tenant_id,
                        entry.session_id,
                        entry.entity_id
                    ),
                )
                .await
            {
                tracing::warn!(entity_id = %entry.entity_id, error = %e, "failed to store properties");
            }
        }

        if let Some(ref hash) = entry.content_hash {
            let q = format!(
                "UPDATE {ks}.entity_store SET content_hash = ? \
                 WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
                ks = self.keyspace,
            );
            if let Err(e) = self
                .session
                .query_with_values(
                    q,
                    query_values!(
                        hash.clone(),
                        ctx.tenant_id,
                        entry.session_id,
                        entry.entity_id
                    ),
                )
                .await
            {
                tracing::warn!(entity_id = %entry.entity_id, error = %e, "failed to store content_hash");
            }
        }

        Ok(())
    }

    async fn entity_find_phonetic(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        name: &str,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        // TODO: use Ferrosa fts_match() when available in the cluster build.
        // Lightweight scan: only fetch columns needed for name matching.
        // Excludes context_snippet (~4KB) and entity_embedding (~3KB) per row.
        let query = format!(
            "SELECT entity_id, entity_name, entity_type, confidence, state, created_at \
             FROM {}.entity_store WHERE tenant_id = ? AND session_id = ? ALLOW FILTERING",
            self.keyspace
        );
        let envelope = self
            .session
            .query_with_values(query, query_values!(ctx.tenant_id, session_id))
            .await?;

        let rows = envelope.response_body()?.into_rows().unwrap_or_default();
        let lower = name.to_lowercase();

        // Collect matches with rank: 0=exact, 1=segment (after ::), 2=substring
        let mut scored: Vec<(u8, EntityEntry)> = Vec::new();
        for row in rows {
            let Ok(entity_name) = row.r_by_name::<String>("entity_name") else {
                continue;
            };
            let en = entity_name.to_lowercase();
            let rank = if en == lower {
                0 // exact match
            } else if en.split("::").any(|seg| seg == lower) {
                1 // segment match (e.g., "graph" matches "crate::graph")
            } else if en.split("::").any(|seg| seg.contains(&lower)) {
                2 // segment substring
            } else if en.contains(&lower) {
                3 // full-name substring
            } else {
                continue;
            };

            let Ok(entity_id) = row.r_by_name::<Uuid>("entity_id") else {
                continue;
            };
            let Ok(entity_type) = row.r_by_name::<String>("entity_type") else {
                continue;
            };
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();
            let state = row
                .r_by_name::<String>("state")
                .ok()
                .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
                .unwrap_or_default();

            scored.push((
                rank,
                EntityEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id,
                    session_id,
                    entity_name,
                    entity_type,
                    source_fold_id: None, // not fetched in lightweight query
                    context_snippet: String::new(), // not fetched
                    entity_embedding: None, // not fetched
                    confidence: f64::from(row.r_by_name::<f32>("confidence").unwrap_or(1.0)),
                    state,
                    created_at: created.and_utc(),
                    ..Default::default()
                },
            ));
        }

        scored.sort_by_key(|(rank, _)| *rank);
        Ok(scored.into_iter().map(|(_, e)| e).collect())
    }

    async fn entity_get_by_id(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        entity_id: Uuid,
    ) -> anyhow::Result<Option<EntityEntry>> {
        let query = format!(
            "SELECT entity_id, entity_name, entity_type, source_fold_id, \
             context_snippet, confidence, state, created_at, \
             description, description_embedding, tags, properties, content_hash, \
             updated_at, scope, ingested_by_session \
             FROM {}.entity_store WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
            self.keyspace
        );
        let envelope = self
            .session
            .query_with_values(query, query_values!(ctx.tenant_id, session_id, entity_id))
            .await?;
        let rows = envelope.response_body()?.into_rows().unwrap_or_default();
        if let Some(row) = rows.first() {
            let Ok(entity_name) = row.r_by_name::<String>("entity_name") else {
                return Ok(None);
            };
            let Ok(entity_type) = row.r_by_name::<String>("entity_type") else {
                return Ok(None);
            };
            let context_snippet = row
                .r_by_name::<String>("context_snippet")
                .unwrap_or_default();
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();
            let state = row
                .r_by_name::<String>("state")
                .ok()
                .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
                .unwrap_or_default();
            let (
                description,
                description_embedding,
                tags,
                properties,
                content_hash,
                updated_at,
                scope,
                ingested_by_session,
            ) = extract_rich_entity_fields(row);
            Ok(Some(EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id,
                entity_name,
                entity_type,
                source_fold_id: row.r_by_name::<Uuid>("source_fold_id").ok(),
                context_snippet,
                entity_embedding: None,
                confidence: f64::from(row.r_by_name::<f32>("confidence").unwrap_or(1.0)),
                state,
                created_at: created.and_utc(),
                description,
                description_embedding,
                tags,
                properties,
                content_hash,
                updated_at,
                scope,
                ingested_by_session,
            }))
        } else {
            Ok(None)
        }
    }

    async fn entity_get_batch(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        entity_ids: &[Uuid],
    ) -> anyhow::Result<Vec<EntityEntry>> {
        if entity_ids.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders: Vec<String> = entity_ids.iter().map(|_| "?".to_string()).collect();
        let query = format!(
            "SELECT entity_id, entity_name, entity_type, source_fold_id, \
                 context_snippet, confidence, state, created_at, \
                 description, description_embedding, tags, properties, content_hash, \
                 updated_at, scope, ingested_by_session \
                 FROM {}.entity_store WHERE tenant_id = ? AND session_id = ? AND entity_id IN ({})",
            self.keyspace,
            placeholders.join(", ")
        );

        // Prepare and bind values using SimpleValues
        let prepared = self.session.prepare(query).await?;
        let values: Vec<cdrs_tokio::types::value::Value> = std::iter::once(ctx.tenant_id)
            .chain(std::iter::once(session_id))
            .chain(entity_ids.iter().copied())
            .map(cdrs_tokio::types::value::Value::from)
            .collect();

        let envelope = self
            .session
            .exec_with_values(
                &prepared,
                cdrs_tokio::query::QueryValues::SimpleValues(values),
            )
            .await?;
        let rows = envelope.response_body()?.into_rows().unwrap_or_default();

        let mut results = Vec::new();
        for row in rows {
            let Ok(entity_id) = row.r_by_name::<Uuid>("entity_id") else {
                continue;
            };
            let Ok(entity_name) = row.r_by_name::<String>("entity_name") else {
                continue;
            };
            let Ok(entity_type) = row.r_by_name::<String>("entity_type") else {
                continue;
            };
            let context_snippet = row
                .r_by_name::<String>("context_snippet")
                .unwrap_or_default();
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();
            let state = row
                .r_by_name::<String>("state")
                .ok()
                .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
                .unwrap_or_default();
            let (
                description,
                description_embedding,
                tags,
                properties,
                content_hash,
                updated_at,
                scope,
                ingested_by_session,
            ) = extract_rich_entity_fields(&row);
            results.push(EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id,
                entity_name,
                entity_type,
                source_fold_id: row.r_by_name::<Uuid>("source_fold_id").ok(),
                context_snippet,
                entity_embedding: None,
                confidence: f64::from(row.r_by_name::<f32>("confidence").unwrap_or(1.0)),
                state,
                created_at: created.and_utc(),
                description,
                description_embedding,
                tags,
                properties,
                content_hash,
                updated_at,
                scope,
                ingested_by_session,
            });
        }
        Ok(results)
    }

    async fn entity_search_ann(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        query_embedding: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        // cdrs-tokio can't serialize VECTOR type — use CQL literal for the query vector.
        // Ferrosa also requires literal integer for LIMIT in ANN queries.
        let vec_literal: String = format!(
            "[{}]",
            query_embedding
                .iter()
                .map(|v| format!("{v:.8}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let query = format!(
            "SELECT entity_id, entity_name, entity_type, source_fold_id, \
             context_snippet, confidence, state, created_at \
             FROM {ks}.entity_store WHERE tenant_id = ? AND session_id = ? \
             ORDER BY entity_embedding ANN OF {vec_literal} LIMIT {k}",
            ks = self.keyspace,
        );
        let envelope = match self
            .session
            .query_with_values(query, query_values!(ctx.tenant_id, session_id))
            .await
        {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "entity ANN query failed");
                return Ok(Vec::new());
            }
        };
        let rows = envelope.response_body()?.into_rows().unwrap_or_default();
        let mut results = Vec::new();
        for row in rows {
            // Skip ghost rows with null required fields (P0 write-loss artifact).
            let Ok(entity_id) = row.r_by_name::<Uuid>("entity_id") else {
                continue;
            };
            let Ok(entity_name) = row.r_by_name::<String>("entity_name") else {
                continue;
            };
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();
            let state = row
                .r_by_name::<String>("state")
                .ok()
                .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
                .unwrap_or_default();
            results.push(EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id,
                entity_name,
                entity_type: row.r_by_name("entity_type").unwrap_or_default(),
                source_fold_id: row.r_by_name::<Uuid>("source_fold_id").ok(),
                context_snippet: row.r_by_name("context_snippet").unwrap_or_default(),
                entity_embedding: None,
                confidence: row
                    .r_by_name::<f32>("confidence")
                    .map(f64::from)
                    .unwrap_or(0.0),
                state,
                created_at: created.and_utc(),
                ..Default::default()
            });
        }
        Ok(results)
    }

    async fn entity_count(&self, ctx: &TenantContext, session_id: Uuid) -> anyhow::Result<usize> {
        // Client-side count: SELECT entity_id returns rows, count them.
        // Workaround for Ferrosa returning COUNT(*) column as "system.count".
        let rows = self
            .query_rows(
                &self.stmts.entity_count,
                query_values!(ctx.tenant_id, session_id),
            )
            .await?;
        Ok(rows.len())
    }

    async fn fold_count(&self, ctx: &TenantContext, session_id: Uuid) -> anyhow::Result<usize> {
        let rows = self
            .query_rows(
                &self.stmts.fold_count,
                query_values!(ctx.tenant_id, session_id),
            )
            .await?;
        Ok(rows.len())
    }

    async fn memo_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        let rows = self
            .query_rows(&self.stmts.memo_count, query_values!(ctx.tenant_id))
            .await?;
        Ok(rows.len())
    }

    async fn entity_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        let rows = self
            .query_rows(
                &self.stmts.entity_list_session,
                query_values!(ctx.tenant_id, session_id),
            )
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            // Skip rows with NULL required fields (ghost rows from bulk loads).
            let Ok(entity_id) = row.r_by_name::<Uuid>("entity_id") else {
                continue;
            };
            let Ok(entity_name) = row.r_by_name::<String>("entity_name") else {
                continue;
            };
            let Ok(entity_type) = row.r_by_name::<String>("entity_type") else {
                continue;
            };
            let context_snippet = row
                .r_by_name::<String>("context_snippet")
                .unwrap_or_default();
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();
            let state = row
                .r_by_name::<String>("state")
                .ok()
                .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
                .unwrap_or_default();
            let (
                description,
                description_embedding,
                tags,
                properties,
                content_hash,
                updated_at,
                scope,
                ingested_by_session,
            ) = extract_rich_entity_fields(&row);
            results.push(EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id,
                entity_name,
                entity_type,
                source_fold_id: row.r_by_name::<Uuid>("source_fold_id").ok(),
                context_snippet,
                entity_embedding: row
                    .r_by_name::<cdrs_tokio::types::blob::Blob>("entity_embedding")
                    .ok()
                    .map(|blob| blob.into_vec())
                    .filter(|v| !v.is_empty())
                    .map(|v| crate::vector::decode_vector(&v)),
                confidence: row
                    .r_by_name::<f32>("confidence")
                    .map(f64::from)
                    .unwrap_or(1.0),
                state,
                created_at: created.and_utc(),
                description,
                description_embedding,
                tags,
                properties,
                content_hash,
                updated_at,
                scope,
                ingested_by_session,
            });
        }
        Ok(results)
    }

    async fn entity_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<EntityEntry>> {
        let rows = self
            .query_rows(&self.stmts.entity_list_all, query_values!(ctx.tenant_id))
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();
            let state = row
                .r_by_name::<String>("state")
                .ok()
                .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
                .unwrap_or_default();
            let (
                description,
                description_embedding,
                tags,
                properties,
                content_hash,
                updated_at,
                scope,
                ingested_by_session,
            ) = extract_rich_entity_fields(&row);
            results.push(EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id: row.r_by_name("entity_id")?,
                session_id: row.r_by_name("session_id")?,
                entity_name: row.r_by_name("entity_name")?,
                entity_type: row.r_by_name("entity_type")?,
                source_fold_id: row.r_by_name::<Uuid>("source_fold_id").ok(),
                context_snippet: row.r_by_name("context_snippet")?,
                entity_embedding: row
                    .r_by_name::<cdrs_tokio::types::blob::Blob>("entity_embedding")
                    .ok()
                    .map(|blob| blob.into_vec())
                    .filter(|v| !v.is_empty())
                    .map(|v| crate::vector::decode_vector(&v)),
                confidence: row
                    .r_by_name::<f32>("confidence")
                    .map(f64::from)
                    .unwrap_or(1.0),
                state,
                created_at: created.and_utc(),
                description,
                description_embedding,
                tags,
                properties,
                content_hash,
                updated_at,
                scope,
                ingested_by_session,
            });
        }
        Ok(results)
    }

    async fn fold_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<FoldEntry>> {
        let rows = self
            .query_rows(&self.stmts.fold_list_all, query_values!(ctx.tenant_id))
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let status_str: String = row.r_by_name("status").unwrap_or_default();
            let status: FoldStatus =
                serde_json::from_str(&format!("\"{status_str}\"")).unwrap_or(FoldStatus::Active);
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();
            results.push(FoldEntry {
                session_id: row.r_by_name("session_id")?,
                fold_id: row.r_by_name("fold_id")?,
                tenant_id: ctx.tenant_id,
                depth: row.r_by_name("depth")?,
                parent_fold_id: row.r_by_name::<Uuid>("parent_fold_id").ok(),
                raw_trajectory: row.r_by_name("raw_trajectory").unwrap_or_default(),
                fold_summary: row.r_by_name::<String>("fold_summary").ok(),
                fold_embedding: row
                    .r_by_name::<cdrs_tokio::types::blob::Blob>("fold_embedding")
                    .ok()
                    .map(|blob| blob.into_vec())
                    .filter(|v| !v.is_empty())
                    .map(|v| crate::vector::decode_vector(&v)),
                token_count: row.r_by_name("token_count")?,
                compression_ratio: row.r_by_name::<f64>("compression_ratio").ok(),
                status,
                created_at: created.and_utc(),
                folded_at: row
                    .r_by_name::<chrono::NaiveDateTime>("folded_at")
                    .ok()
                    .map(|t| t.and_utc()),
            });
        }
        Ok(results)
    }

    async fn temporal_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<TemporalEvent>> {
        let rows = self
            .query_rows(&self.stmts.temporal_list_all, query_values!(ctx.tenant_id))
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let event_time: chrono::NaiveDateTime = row.r_by_name("event_time")?;
            results.push(TemporalEvent {
                tenant_id: ctx.tenant_id,
                entity_id: row.r_by_name("entity_id")?,
                event_time: event_time.and_utc(),
                event_id: row.r_by_name("event_id")?,
                fact_text: row.r_by_name("fact_text")?,
                supersedes_id: row.r_by_name::<Uuid>("supersedes_id").ok(),
                valid_until: row
                    .r_by_name::<chrono::NaiveDateTime>("valid_until")
                    .ok()
                    .map(|t| t.and_utc()),
                source_session: row.r_by_name("source_session")?,
                confidence: f64::from(row.r_by_name::<f32>("confidence").unwrap_or(1.0)),
            });
        }
        Ok(results)
    }

    async fn entity_update_state(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        state: MemoryState,
    ) -> anyhow::Result<()> {
        let state_str = state.to_string();
        // We need session_id for the partition key. Look up from entity_id via ALLOW FILTERING.
        // In a real deployment, the caller should provide session_id. For now, use a scan.
        let query = format!(
            "SELECT session_id FROM {}.entity_store WHERE tenant_id = ? AND entity_id = ? ALLOW FILTERING",
            self.keyspace
        );
        let envelope = self
            .session
            .query_with_values(query, query_values!(ctx.tenant_id, entity_id))
            .await?;
        let rows = envelope.response_body()?.into_rows().unwrap_or_default();
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("entity not found: {entity_id}"))?;
        let session_id: Uuid = row.r_by_name("session_id")?;

        self.session
            .exec_with_values(
                &self.stmts.entity_update_state,
                query_values!(state_str, ctx.tenant_id, session_id, entity_id),
            )
            .await?;
        Ok(())
    }

    // --- Temporal operations ---

    async fn temporal_put(&self, ctx: &TenantContext, event: &TemporalEvent) -> anyhow::Result<()> {
        self.session
            .exec_with_values(
                &self.stmts.temporal_put,
                query_values!(
                    ctx.tenant_id,
                    event.entity_id,
                    event.event_time.naive_utc(),
                    event.event_id,
                    event.fact_text.clone(),
                    event.supersedes_id,
                    event.valid_until.map(|t| t.naive_utc()),
                    event.source_session,
                    event.confidence as f32
                ),
            )
            .await?;
        Ok(())
    }

    async fn temporal_get_current(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> anyhow::Result<Option<TemporalEvent>> {
        let rows = self
            .query_rows(
                &self.stmts.temporal_get_current,
                query_values!(ctx.tenant_id, entity_id),
            )
            .await?;

        // Filter for valid_until IS NULL (current facts) — CQL can't filter on NULL
        for row in rows {
            if row
                .r_by_name::<chrono::NaiveDateTime>("valid_until")
                .is_err()
            {
                // NULL means this is the current fact
                let event_time: chrono::NaiveDateTime = row.r_by_name("event_time")?;
                return Ok(Some(TemporalEvent {
                    tenant_id: ctx.tenant_id,
                    entity_id,
                    event_time: event_time.and_utc(),
                    event_id: row.r_by_name("event_id")?,
                    fact_text: row.r_by_name("fact_text")?,
                    supersedes_id: row.r_by_name::<Uuid>("supersedes_id").ok(),
                    valid_until: None,
                    source_session: row.r_by_name("source_session")?,
                    confidence: f64::from(row.r_by_name::<f32>("confidence")?),
                }));
            }
        }
        Ok(None)
    }

    async fn temporal_invalidate(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        event_id: Uuid,
    ) -> anyhow::Result<()> {
        // Need event_time to update — fetch it first
        if let Some(event) = self
            .temporal_get_current(ctx, entity_id)
            .await?
            .filter(|e| e.event_id == event_id)
        {
            self.session
                .exec_with_values(
                    &self.stmts.temporal_invalidate,
                    query_values!(
                        chrono::Utc::now().naive_utc(),
                        ctx.tenant_id,
                        entity_id,
                        event.event_time.naive_utc(),
                        event_id
                    ),
                )
                .await?;
        }
        Ok(())
    }

    // --- Feedback operations ---

    async fn feedback_put(
        &self,
        ctx: &TenantContext,
        outcome: &FeedbackOutcome,
    ) -> anyhow::Result<()> {
        self.session
            .exec_with_values(
                &self.stmts.feedback_put,
                query_values!(
                    ctx.tenant_id,
                    outcome.session_id,
                    outcome.query_id,
                    outcome.program_type.clone(),
                    outcome.task_complexity.clone(),
                    outcome.succeeded,
                    outcome.latency_ms,
                    outcome.token_cost,
                    chrono::Utc::now().naive_utc()
                ),
            )
            .await?;
        Ok(())
    }

    async fn feedback_list_all(&self) -> anyhow::Result<Vec<FeedbackOutcome>> {
        let envelope = self.session.exec(&self.stmts.feedback_list_all).await?;
        let body = envelope.response_body()?;
        let rows = body.into_rows().unwrap_or_default();

        let mut outcomes = Vec::with_capacity(rows.len());
        for row in &rows {
            let tenant_id: Uuid = row
                .by_name("tenant_id")?
                .ok_or_else(|| anyhow::anyhow!("missing tenant_id"))?;
            let session_id: Uuid = row
                .by_name("session_id")?
                .ok_or_else(|| anyhow::anyhow!("missing session_id"))?;
            let query_id: Uuid = row
                .by_name("query_id")?
                .ok_or_else(|| anyhow::anyhow!("missing query_id"))?;
            let program_type: String = row
                .by_name("program_type")?
                .ok_or_else(|| anyhow::anyhow!("missing program_type"))?;
            let task_complexity: String = row
                .by_name("task_complexity")?
                .ok_or_else(|| anyhow::anyhow!("missing task_complexity"))?;
            let succeeded: bool = row
                .by_name("succeeded")?
                .ok_or_else(|| anyhow::anyhow!("missing succeeded"))?;
            let latency_ms: i32 = row
                .by_name("latency_ms")?
                .ok_or_else(|| anyhow::anyhow!("missing latency_ms"))?;
            let token_cost: i32 = row
                .by_name("token_cost")?
                .ok_or_else(|| anyhow::anyhow!("missing token_cost"))?;
            let created_at: chrono::NaiveDateTime = row
                .by_name("created_at")?
                .ok_or_else(|| anyhow::anyhow!("missing created_at"))?;

            outcomes.push(FeedbackOutcome {
                tenant_id,
                session_id,
                query_id,
                program_type,
                task_complexity,
                succeeded,
                latency_ms,
                token_cost,
                created_at: chrono::DateTime::from_naive_utc_and_offset(created_at, chrono::Utc),
            });
        }

        Ok(outcomes)
    }

    // --- Observability operations ---

    async fn memo_total_hits(&self, ctx: &TenantContext) -> anyhow::Result<i64> {
        // Client-side sum: fetch all hit_count values and sum.
        // Workaround for Ferrosa returning aggregate columns as "system.sum".
        let query = format!(
            "SELECT hit_count FROM {}.memo_cache WHERE tenant_id = ? ALLOW FILTERING",
            self.keyspace
        );
        let envelope = self
            .session
            .query_with_values(query, query_values!(ctx.tenant_id))
            .await?;
        let rows = envelope.response_body()?.into_rows().unwrap_or_default();
        let mut total: i64 = 0;
        for row in &rows {
            let hits: i64 = row.r_by_name("hit_count").unwrap_or(0);
            total += hits;
        }
        Ok(total)
    }

    async fn fold_count_by_status(
        &self,
        ctx: &TenantContext,
        status: crate::types::FoldStatus,
    ) -> anyhow::Result<usize> {
        let status_str = match status {
            crate::types::FoldStatus::Active => "active",
            crate::types::FoldStatus::Folded => "folded",
            crate::types::FoldStatus::Archived => "archived",
        };
        // Client-side count filtered by status.
        let query = format!(
            "SELECT status FROM {}.trajectory_folds WHERE tenant_id = ? ALLOW FILTERING",
            self.keyspace
        );
        let envelope = self
            .session
            .query_with_values(query, query_values!(ctx.tenant_id))
            .await?;
        let rows = envelope.response_body()?.into_rows().unwrap_or_default();
        let count = rows
            .iter()
            .filter(|r| {
                r.r_by_name::<String>("status")
                    .map(|s| s == status_str)
                    .unwrap_or(false)
            })
            .count();
        Ok(count)
    }

    async fn temporal_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        let query = format!(
            "SELECT event_id FROM {}.temporal_events WHERE tenant_id = ? ALLOW FILTERING",
            self.keyspace
        );
        let envelope = self
            .session
            .query_with_values(query, query_values!(ctx.tenant_id))
            .await?;
        let rows = envelope.response_body()?.into_rows().unwrap_or_default();
        Ok(rows.len())
    }

    async fn edge_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        let mut total: usize = 0;
        let pk_cols = [
            ("folded_into", "source_fold_id"),
            ("mentioned_in", "entity_id"),
            ("co_occurs_with", "entity_a"),
            ("supersedes", "new_event_id"),
        ];
        for (table, col) in &pk_cols {
            let query = format!(
                "SELECT {col} FROM {}.{table} WHERE tenant_id = ? ALLOW FILTERING",
                self.keyspace
            );
            let envelope = self
                .session
                .query_with_values(query, query_values!(ctx.tenant_id))
                .await?;
            let rows = envelope.response_body()?.into_rows().unwrap_or_default();
            total += rows.len();
        }
        Ok(total)
    }

    async fn delete_session(&self, ctx: &TenantContext, session_id: Uuid) -> anyhow::Result<usize> {
        // Delete from each table. CQL doesn't support cross-table transactions,
        // so we delete from each table individually. Session-scoped partition
        // keys make this efficient.
        let tables = [
            (
                "plan_state",
                format!(
                    "DELETE FROM {}.plan_state WHERE session_id = ? AND tenant_id = ?",
                    self.keyspace
                ),
            ),
            (
                "trajectory_folds",
                format!(
                    "DELETE FROM {}.trajectory_folds WHERE session_id = ? AND tenant_id = ?",
                    self.keyspace
                ),
            ),
        ];

        let mut count = 0;
        for (name, query) in &tables {
            match self
                .session
                .query_with_values(query.as_str(), query_values!(session_id, ctx.tenant_id))
                .await
            {
                Ok(_) => count += 1,
                Err(e) => {
                    tracing::warn!(table = name, error = %e, "delete_session: table delete failed")
                }
            }
        }
        // entity_store and feedback_outcomes have tenant_id as partition key,
        // not session_id — would need ALLOW FILTERING or secondary index.
        // For now, only session-partitioned tables are cleared.
        Ok(count)
    }

    // --- Edge operations ---

    async fn edge_folded_into(
        &self,
        ctx: &TenantContext,
        source_fold_id: Uuid,
        target_fold_id: Uuid,
        session_id: Uuid,
    ) -> anyhow::Result<()> {
        self.session
            .exec_with_values(
                &self.stmts.edge_folded_into,
                query_values!(
                    source_fold_id,
                    target_fold_id,
                    session_id,
                    ctx.tenant_id,
                    chrono::Utc::now().naive_utc()
                ),
            )
            .await?;
        tracing::debug!(%source_fold_id, %target_fold_id, "FOLDED_INTO edge created");
        Ok(())
    }

    async fn edge_mentioned_in(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        fold_id: Uuid,
        session_id: Uuid,
    ) -> anyhow::Result<()> {
        self.session
            .exec_with_values(
                &self.stmts.edge_mentioned_in,
                query_values!(
                    entity_id,
                    fold_id,
                    session_id,
                    ctx.tenant_id,
                    chrono::Utc::now().naive_utc()
                ),
            )
            .await?;
        tracing::debug!(%entity_id, %fold_id, "MENTIONED_IN edge created");
        Ok(())
    }

    async fn edge_co_occurs(
        &self,
        ctx: &TenantContext,
        entity_a: Uuid,
        entity_b: Uuid,
        session_id: Uuid,
        strength: f32,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().naive_utc();
        self.session
            .exec_with_values(
                &self.stmts.edge_co_occurs,
                query_values!(
                    entity_a,
                    entity_b,
                    session_id,
                    ctx.tenant_id,
                    now,
                    strength,
                    now
                ),
            )
            .await?;
        tracing::debug!(%entity_a, %entity_b, %strength, "CO_OCCURS_WITH edge created/reinforced");
        Ok(())
    }

    async fn edge_supersedes(
        &self,
        ctx: &TenantContext,
        new_event_id: Uuid,
        old_event_id: Uuid,
        entity_id: Uuid,
    ) -> anyhow::Result<()> {
        self.session
            .exec_with_values(
                &self.stmts.edge_supersedes,
                query_values!(
                    new_event_id,
                    old_event_id,
                    entity_id,
                    ctx.tenant_id,
                    chrono::Utc::now().naive_utc()
                ),
            )
            .await?;
        tracing::debug!(%new_event_id, %old_event_id, "SUPERSEDES edge created");
        Ok(())
    }

    async fn edge_prune_stale(
        &self,
        ctx: &TenantContext,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<usize> {
        // Read all CO_OCCURS edges for tenant, delete those with
        // last_reinforced < cutoff (or NULL last_reinforced).
        let query = format!(
            "SELECT entity_a, entity_b, last_reinforced \
             FROM {}.co_occurs_with WHERE tenant_id = ? ALLOW FILTERING",
            self.keyspace
        );
        let envelope = self
            .session
            .query_with_values(query, query_values!(ctx.tenant_id))
            .await?;
        let rows = envelope.response_body()?.into_rows().unwrap_or_default();

        let cutoff_naive = cutoff.naive_utc();
        let mut pruned = 0;
        for row in &rows {
            let stale = match row.r_by_name::<chrono::NaiveDateTime>("last_reinforced") {
                Ok(ts) => ts < cutoff_naive,
                Err(_) => true, // NULL last_reinforced = never reinforced
            };
            if stale {
                let a: Uuid = row.r_by_name("entity_a")?;
                let b: Uuid = row.r_by_name("entity_b")?;
                let del = format!(
                    "DELETE FROM {}.co_occurs_with WHERE entity_a = ? AND entity_b = ?",
                    self.keyspace
                );
                self.session
                    .query_with_values(del, query_values!(a, b))
                    .await?;
                pruned += 1;
            }
        }

        tracing::info!(pruned, total = rows.len(), "edge pruning complete");
        Ok(pruned)
    }

    async fn edge_decay_weights(&self, ctx: &TenantContext, factor: f64) -> anyhow::Result<usize> {
        let query = format!(
            "SELECT entity_a, entity_b, strength \
             FROM {}.co_occurs_with WHERE tenant_id = ? ALLOW FILTERING",
            self.keyspace
        );
        let envelope = self
            .session
            .query_with_values(query, query_values!(ctx.tenant_id))
            .await?;
        let rows = envelope.response_body()?.into_rows().unwrap_or_default();

        let mut decayed = 0;
        for row in &rows {
            let a: Uuid = row.r_by_name("entity_a")?;
            let b: Uuid = row.r_by_name("entity_b")?;
            let strength: f32 = row.r_by_name("strength").unwrap_or(1.0);
            let new_strength = (f64::from(strength) * factor) as f32;
            let update = format!(
                "UPDATE {}.co_occurs_with SET strength = ? \
                 WHERE entity_a = ? AND entity_b = ?",
                self.keyspace
            );
            self.session
                .query_with_values(update, query_values!(new_strength, a, b))
                .await?;
            decayed += 1;
        }

        tracing::info!(decayed, factor, "edge weight decay complete");
        Ok(decayed)
    }

    async fn edge_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<(Uuid, Uuid, String)>> {
        let mut edges = Vec::new();

        // Use prepare+execute to avoid paging bug where dynamic QUERY
        // returns only the first result page. Each table is best-effort.
        let edge_queries: &[(&str, &str, &str, &str)] = &[
            (
                "folded_into",
                "source_fold_id",
                "target_fold_id",
                "FOLDED_INTO",
            ),
            ("mentioned_in", "entity_id", "fold_id", "MENTIONED_IN"),
            ("co_occurs_with", "entity_a", "entity_b", "CO_OCCURS"),
        ];

        for (table, src_col, tgt_col, label) in edge_queries {
            let query = format!(
                "SELECT {src_col}, {tgt_col} FROM {}.{table} \
                 WHERE session_id = ? AND tenant_id = ? ALLOW FILTERING",
                self.keyspace
            );
            match self.session.prepare(query).await {
                Ok(prepared) => {
                    match self
                        .session
                        .exec_with_values(&prepared, query_values!(session_id, ctx.tenant_id))
                        .await
                    {
                        Ok(envelope) => {
                            let rows = envelope.response_body()?.into_rows().unwrap_or_default();
                            for row in rows {
                                if let (Ok(src), Ok(tgt)) = (
                                    row.r_by_name::<Uuid>(src_col),
                                    row.r_by_name::<Uuid>(tgt_col),
                                ) {
                                    edges.push((src, tgt, label.to_string()));
                                }
                            }
                        }
                        Err(e) => tracing::warn!(table, error = %e, "edge query failed"),
                    }
                }
                Err(e) => tracing::warn!(table, error = %e, "edge query prepare failed"),
            }
        }

        // SUPERSEDES edges (not session-scoped, return all for tenant)
        let query = format!(
            "SELECT new_event_id, old_event_id FROM {}.supersedes \
             WHERE tenant_id = ? ALLOW FILTERING",
            self.keyspace
        );
        if let Ok(prepared) = self.session.prepare(query).await
            && let Ok(envelope) = self
                .session
                .exec_with_values(&prepared, query_values!(ctx.tenant_id))
                .await
        {
            let rows = envelope.response_body()?.into_rows().unwrap_or_default();
            for row in rows {
                if let (Ok(src), Ok(tgt)) = (
                    row.r_by_name::<Uuid>("new_event_id"),
                    row.r_by_name::<Uuid>("old_event_id"),
                ) {
                    edges.push((src, tgt, "SUPERSEDES".into()));
                }
            }
        }

        Ok(edges)
    }

    #[allow(clippy::result_large_err)]
    async fn edge_list_all(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<(Uuid, Uuid, String)>> {
        let mut edges = Vec::new();

        let queries: &[(&str, &str, &str, &str)] = &[
            ("co_occurs_with", "entity_a", "entity_b", "CO_OCCURS"),
            ("mentioned_in", "entity_id", "fold_id", "MENTIONED_IN"),
            (
                "folded_into",
                "source_fold_id",
                "target_fold_id",
                "FOLDED_INTO",
            ),
            ("supersedes", "new_event_id", "old_event_id", "SUPERSEDES"),
        ];

        for &(table, src_col, tgt_col, edge_type) in queries {
            let query = format!(
                "SELECT {src_col}, {tgt_col} FROM {}.{table} WHERE tenant_id = ? ALLOW FILTERING",
                self.keyspace
            );
            if let Ok(prepared) = self.session().prepare(query).await
                && let Ok(frame) = self
                    .session()
                    .exec_with_values(&prepared, query_values!(ctx.tenant_id))
                    .await
                && let Ok(body) = frame.response_body()
                && let Some(rows) = body.into_rows()
            {
                for row in rows {
                    if let (Ok(a), Ok(b)) = (
                        row.r_by_name::<Uuid>(src_col),
                        row.r_by_name::<Uuid>(tgt_col),
                    ) {
                        edges.push((a, b, edge_type.into()));
                    }
                }
            }
        }

        Ok(edges)
    }

    async fn edge_list_for_entity(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> anyhow::Result<Vec<(Uuid, String)>> {
        let mut neighbors = Vec::new();

        // MENTIONED_IN edges (entity -> fold)
        let rows = self
            .query_rows(
                &self.stmts.edge_mentioned_in_by_entity,
                query_values!(entity_id, ctx.tenant_id),
            )
            .await?;
        for row in rows {
            let fold_id: Uuid = row.r_by_name("fold_id")?;
            neighbors.push((fold_id, "MENTIONED_IN".into()));
        }

        // CO_OCCURS_WITH edges (entity as entity_a)
        let rows = self
            .query_rows(
                &self.stmts.edge_co_occurs_by_a,
                query_values!(entity_id, ctx.tenant_id),
            )
            .await?;
        for row in rows {
            let other: Uuid = row.r_by_name("entity_b")?;
            neighbors.push((other, "CO_OCCURS".into()));
        }

        // CO_OCCURS_WITH edges (entity as entity_b)
        let rows = self
            .query_rows(
                &self.stmts.edge_co_occurs_by_b,
                query_values!(entity_id, ctx.tenant_id),
            )
            .await?;
        for row in rows {
            let other: Uuid = row.r_by_name("entity_a")?;
            neighbors.push((other, "CO_OCCURS".into()));
        }

        // SUPERSEDES edges (entity as new_event_id)
        let rows = self
            .query_rows(
                &self.stmts.edge_supersedes_by_new,
                query_values!(entity_id, ctx.tenant_id),
            )
            .await?;
        for row in rows {
            let old: Uuid = row.r_by_name("old_event_id")?;
            neighbors.push((old, "SUPERSEDES".into()));
        }

        // SUPERSEDES edges (entity as old_event_id)
        let rows = self
            .query_rows(
                &self.stmts.edge_supersedes_by_old,
                query_values!(entity_id, ctx.tenant_id),
            )
            .await?;
        for row in rows {
            let new_id: Uuid = row.r_by_name("new_event_id")?;
            neighbors.push((new_id, "SUPERSEDES".into()));
        }

        // Typed edges (contains, references, calls, depends_on, etc.)
        // Query the nil session (used by frg ingest) and the default session.
        let nil_session = Uuid::nil();
        let query = "SELECT src_id, edge_type, dst_id FROM agent_memory.typed_edges \
                     WHERE tenant_id = ? AND session_id = ?";
        let prepared = self.session.prepare(query).await?;
        let rows = self
            .query_rows(&prepared, query_values!(ctx.tenant_id, nil_session))
            .await
            .unwrap_or_default();
        for row in rows {
            let src: Uuid = match row.r_by_name("src_id") {
                Ok(v) => v,
                Err(_) => continue,
            };
            let dst: Uuid = match row.r_by_name("dst_id") {
                Ok(v) => v,
                Err(_) => continue,
            };
            let edge_type: String = row.r_by_name("edge_type").unwrap_or_default();
            if src == entity_id {
                neighbors.push((dst, edge_type));
            } else if dst == entity_id {
                neighbors.push((src, edge_type));
            }
        }

        Ok(neighbors)
    }

    // --- Intention operations ---

    async fn intention_put(
        &self,
        ctx: &TenantContext,
        intention: &crate::intention::Intention,
    ) -> anyhow::Result<()> {
        let trigger_json = serde_json::to_string(&intention.trigger)?;
        let priority_str = serde_json::to_string(&intention.priority)?
            .trim_matches('"')
            .to_string();
        let status_str = serde_json::to_string(&intention.status)?
            .trim_matches('"')
            .to_string();

        self.session
            .exec_with_values(
                &self.stmts.intention_put,
                query_values!(
                    ctx.tenant_id,
                    intention.repo.clone(),
                    intention.id,
                    intention.description.clone(),
                    trigger_json,
                    priority_str,
                    status_str,
                    intention.created_at.naive_utc(),
                    intention.triggered_at.map(|t| t.naive_utc()),
                    intention.completed_at.map(|t| t.naive_utc())
                ),
            )
            .await?;
        Ok(())
    }

    async fn intention_list(
        &self,
        ctx: &TenantContext,
        repo: &str,
    ) -> anyhow::Result<Vec<crate::intention::Intention>> {
        let rows = self
            .query_rows(
                &self.stmts.intention_list,
                query_values!(ctx.tenant_id, repo.to_string()),
            )
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let trigger_json: String = row.r_by_name("trigger_json")?;
            let trigger: crate::intention::IntentionTrigger = serde_json::from_str(&trigger_json)?;

            let priority_str: String = row.r_by_name("priority")?;
            let priority: crate::intention::Priority =
                serde_json::from_str(&format!("\"{priority_str}\""))
                    .unwrap_or(crate::intention::Priority::Normal);

            let status_str: String = row.r_by_name("status")?;
            let status: crate::intention::IntentionStatus =
                serde_json::from_str(&format!("\"{status_str}\""))
                    .unwrap_or(crate::intention::IntentionStatus::Pending);

            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();

            let repo_val: String = row.r_by_name("repo").unwrap_or_else(|_| repo.to_string());

            results.push(crate::intention::Intention {
                id: row.r_by_name("intention_id")?,
                repo: repo_val,
                description: row.r_by_name("description")?,
                trigger,
                priority,
                status,
                created_at: created.and_utc(),
                triggered_at: row
                    .r_by_name::<chrono::NaiveDateTime>("triggered_at")
                    .ok()
                    .map(|t| t.and_utc()),
                completed_at: row
                    .r_by_name::<chrono::NaiveDateTime>("completed_at")
                    .ok()
                    .map(|t| t.and_utc()),
            });
        }
        Ok(results)
    }

    async fn intention_list_all(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<crate::intention::Intention>> {
        let rows = self
            .query_rows(&self.stmts.intention_list_all, query_values!(ctx.tenant_id))
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let trigger_json: String = row.r_by_name("trigger_json")?;
            let trigger: crate::intention::IntentionTrigger = serde_json::from_str(&trigger_json)?;

            let priority_str: String = row.r_by_name("priority")?;
            let priority: crate::intention::Priority =
                serde_json::from_str(&format!("\"{priority_str}\""))
                    .unwrap_or(crate::intention::Priority::Normal);

            let status_str: String = row.r_by_name("status")?;
            let status: crate::intention::IntentionStatus =
                serde_json::from_str(&format!("\"{status_str}\""))
                    .unwrap_or(crate::intention::IntentionStatus::Pending);

            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();

            let repo_val: String = row.r_by_name("repo").unwrap_or_default();

            results.push(crate::intention::Intention {
                id: row.r_by_name("intention_id")?,
                repo: repo_val,
                description: row.r_by_name("description")?,
                trigger,
                priority,
                status,
                created_at: created.and_utc(),
                triggered_at: row
                    .r_by_name::<chrono::NaiveDateTime>("triggered_at")
                    .ok()
                    .map(|t| t.and_utc()),
                completed_at: row
                    .r_by_name::<chrono::NaiveDateTime>("completed_at")
                    .ok()
                    .map(|t| t.and_utc()),
            });
        }
        Ok(results)
    }

    async fn intention_update_status(
        &self,
        ctx: &TenantContext,
        repo: &str,
        id: Uuid,
        status: &str,
        triggered_at: Option<chrono::DateTime<chrono::Utc>>,
        completed_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> anyhow::Result<()> {
        self.session
            .exec_with_values(
                &self.stmts.intention_update_status,
                query_values!(
                    status.to_string(),
                    triggered_at.map(|t| t.naive_utc()),
                    completed_at.map(|t| t.naive_utc()),
                    ctx.tenant_id,
                    repo.to_string(),
                    id
                ),
            )
            .await?;
        Ok(())
    }

    // --- Tool usage logging ---

    async fn tool_usage_put(
        &self,
        ctx: &TenantContext,
        tool_name: &str,
        repo: &str,
        input_bytes: i32,
        output_bytes: i32,
        estimated_tokens: i32,
        latency_ms: i32,
        error: bool,
    ) -> anyhow::Result<()> {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        self.session
            .exec_with_values(
                &self.stmts.tool_usage_put,
                query_values!(
                    ctx.tenant_id,
                    today,
                    tool_name.to_string(),
                    repo.to_string(),
                    input_bytes,
                    output_bytes,
                    estimated_tokens,
                    latency_ms,
                    error
                ),
            )
            .await?;
        Ok(())
    }

    async fn tool_usage_query(
        &self,
        ctx: &TenantContext,
        day: &str,
    ) -> anyhow::Result<Vec<crate::types::ToolUsageRow>> {
        let rows = self
            .query_rows(
                &self.stmts.tool_usage_query,
                query_values!(ctx.tenant_id, day.to_string()),
            )
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            results.push(crate::types::ToolUsageRow {
                tool_name: row.r_by_name("tool_name")?,
                repo: row.r_by_name::<String>("repo").unwrap_or_default(),
                input_bytes: row.r_by_name("input_bytes")?,
                output_bytes: row.r_by_name("output_bytes")?,
                estimated_tokens: row.r_by_name("estimated_tokens")?,
                latency_ms: row.r_by_name("latency_ms")?,
                error: row.r_by_name("error")?,
                created_at: row
                    .r_by_name::<chrono::NaiveDateTime>("created_at")
                    .map(|t| t.and_utc())
                    .unwrap_or_else(|_| chrono::Utc::now()),
            });
        }
        Ok(results)
    }

    // --- Audit log operations ---

    async fn audit_put(&self, ctx: &TenantContext, entry: &AuditEntry) -> anyhow::Result<()> {
        self.session
            .exec_with_values(
                &self.stmts.audit_put,
                query_values!(
                    ctx.tenant_id,
                    entry.audit_id,
                    entry.operation.clone(),
                    entry.target_table.clone(),
                    entry.target_id.clone(),
                    entry.session_id,
                    entry.created_at.naive_utc()
                ),
            )
            .await?;
        Ok(())
    }

    // --- Warmth operations (Sprint 5) ---

    async fn warmth_get(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> anyhow::Result<Option<WarmthEntry>> {
        let rows = self
            .query_rows(
                &self.stmts.warmth_get,
                query_values!(ctx.tenant_id, entity_id),
            )
            .await?;

        if let Some(row) = rows.into_iter().next() {
            let last_accessed = row
                .r_by_name::<chrono::NaiveDateTime>("last_accessed_at")
                .unwrap_or_default();
            let updated = row
                .r_by_name::<chrono::NaiveDateTime>("updated_at")
                .unwrap_or_default();
            let zone_str: String = row.r_by_name("decay_zone").unwrap_or_default();

            Ok(Some(WarmthEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id: row.r_by_name("session_id")?,
                warmth: row.r_by_name("warmth")?,
                pagerank: row.r_by_name::<f64>("pagerank").unwrap_or(0.0),
                reputation: row.r_by_name::<f64>("reputation").unwrap_or(0.0),
                last_accessed_at: last_accessed.and_utc(),
                access_count: i64::from(row.r_by_name::<i32>("access_count").unwrap_or(0)),
                decay_zone: parse_decay_zone(&zone_str),
                updated_at: updated.and_utc(),
            }))
        } else {
            Ok(None)
        }
    }

    async fn warmth_put(&self, ctx: &TenantContext, entry: &WarmthEntry) -> anyhow::Result<()> {
        self.session
            .exec_with_values(
                &self.stmts.warmth_put,
                query_values!(
                    ctx.tenant_id,
                    entry.entity_id,
                    entry.session_id,
                    entry.warmth,
                    entry.pagerank,
                    entry.reputation,
                    entry.last_accessed_at.naive_utc(),
                    entry.access_count as i32,
                    entry.decay_zone.to_string(),
                    entry.updated_at.naive_utc()
                ),
            )
            .await?;
        Ok(())
    }

    async fn warmth_boost(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
        amount: f64,
        session_id: Uuid,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now();
        let entry = if let Some(existing) = self.warmth_get(ctx, entity_id).await? {
            WarmthEntry {
                warmth: existing.warmth + amount,
                access_count: existing.access_count + 1,
                last_accessed_at: now,
                updated_at: now,
                ..existing
            }
        } else {
            WarmthEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id,
                warmth: amount,
                pagerank: 0.0,
                reputation: 0.0,
                last_accessed_at: now,
                access_count: 1,
                decay_zone: DecayZone::Knowledge,
                updated_at: now,
            }
        };
        self.warmth_put(ctx, &entry).await
    }

    async fn warmth_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<WarmthEntry>> {
        let rows = self
            .query_rows(&self.stmts.warmth_list_session, query_values!(session_id))
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let last_accessed = row
                .r_by_name::<chrono::NaiveDateTime>("last_accessed_at")
                .unwrap_or_default();
            let updated = row
                .r_by_name::<chrono::NaiveDateTime>("updated_at")
                .unwrap_or_default();
            let zone_str: String = row.r_by_name("decay_zone").unwrap_or_default();

            results.push(WarmthEntry {
                tenant_id: ctx.tenant_id,
                entity_id: row.r_by_name("entity_id")?,
                session_id,
                warmth: row.r_by_name("warmth")?,
                pagerank: row.r_by_name::<f64>("pagerank").unwrap_or(0.0),
                reputation: row.r_by_name::<f64>("reputation").unwrap_or(0.0),
                last_accessed_at: last_accessed.and_utc(),
                access_count: i64::from(row.r_by_name::<i32>("access_count").unwrap_or(0)),
                decay_zone: parse_decay_zone(&zone_str),
                updated_at: updated.and_utc(),
            });
        }
        Ok(results)
    }

    async fn warmth_decay_all(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        elapsed_hours: f64,
    ) -> anyhow::Result<usize> {
        let entries = self.warmth_list_session(ctx, session_id).await?;
        let now = chrono::Utc::now();
        let mut pruned = 0;

        for entry in &entries {
            let multiplier = entry.decay_zone.decay_multiplier();
            let new_warmth = entry.warmth * (-0.1 * elapsed_hours * multiplier).exp();

            if new_warmth < 0.01 {
                // Below threshold: delete the row
                self.session
                    .exec_with_values(
                        &self.stmts.warmth_delete,
                        query_values!(ctx.tenant_id, entry.entity_id),
                    )
                    .await?;
                pruned += 1;
            } else {
                // Update with decayed warmth
                let updated = WarmthEntry {
                    warmth: new_warmth,
                    updated_at: now,
                    ..entry.clone()
                };
                self.warmth_put(ctx, &updated).await?;
            }
        }

        Ok(pruned)
    }

    // --- Rule registry operations (Sprint 5) ---

    async fn rule_put(&self, ctx: &TenantContext, entry: &RuleEntry) -> anyhow::Result<()> {
        let state_str = entry.state.to_string();
        let now = chrono::Utc::now().naive_utc();

        // Denormalized write: rules_by_id
        self.session
            .exec_with_values(
                &self.stmts.rule_put_by_id,
                query_values!(
                    ctx.tenant_id,
                    entry.rule_id.clone(),
                    entry.version,
                    entry.name.clone(),
                    entry.family.clone(),
                    state_str.clone(),
                    entry.rule_body.clone(),
                    entry.rule_weight,
                    entry.incremental,
                    entry.created_at.naive_utc(),
                    now
                ),
            )
            .await?;

        // Denormalized write: rules_by_family
        self.session
            .exec_with_values(
                &self.stmts.rule_put_by_family,
                query_values!(
                    ctx.tenant_id,
                    entry.family.clone(),
                    state_str,
                    entry.rule_id.clone(),
                    entry.version,
                    now
                ),
            )
            .await?;

        Ok(())
    }

    async fn rule_list_family(
        &self,
        ctx: &TenantContext,
        family: &str,
        state: RuleState,
    ) -> anyhow::Result<Vec<RuleEntry>> {
        let state_str = state.to_string();

        // Fetch all rules for tenant, filter client-side by family and state.
        // This is efficient for small rule sets (typically < 100 rules).
        let rows = self
            .query_rows(&self.stmts.rule_list_family, query_values!(ctx.tenant_id))
            .await?;

        let mut results = Vec::new();
        for row in rows {
            let row_family: String = row.r_by_name("family").unwrap_or_default();
            let row_state: String = row.r_by_name("state").unwrap_or_default();
            if row_family != family || row_state != state_str {
                continue;
            }
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();
            let updated = row
                .r_by_name::<chrono::NaiveDateTime>("updated_at")
                .unwrap_or_default();

            results.push(RuleEntry {
                tenant_id: ctx.tenant_id,
                rule_id: row.r_by_name("rule_id")?,
                version: row.r_by_name("version")?,
                name: row.r_by_name("name")?,
                family: row_family,
                state: parse_rule_state(&row_state),
                rule_body: row.r_by_name("rule_body")?,
                rule_weight: row.r_by_name::<f64>("rule_weight").unwrap_or(1.0),
                incremental: row.r_by_name::<bool>("incremental").unwrap_or(false),
                created_at: created.and_utc(),
                updated_at: updated.and_utc(),
            });
        }

        // Sort by version descending (CQL clustering is per-partition only)
        results.sort_by_key(|r| std::cmp::Reverse(r.version));
        Ok(results)
    }

    async fn rule_get(
        &self,
        ctx: &TenantContext,
        rule_id: &str,
    ) -> anyhow::Result<Option<RuleEntry>> {
        let rows = self
            .query_rows(
                &self.stmts.rule_get,
                query_values!(ctx.tenant_id, rule_id.to_string()),
            )
            .await?;

        if let Some(row) = rows.into_iter().next() {
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();
            let updated = row
                .r_by_name::<chrono::NaiveDateTime>("updated_at")
                .unwrap_or_default();
            let state_str: String = row.r_by_name("state").unwrap_or_default();

            Ok(Some(RuleEntry {
                tenant_id: ctx.tenant_id,
                rule_id: row.r_by_name("rule_id")?,
                version: row.r_by_name("version")?,
                name: row.r_by_name("name")?,
                family: row.r_by_name("family")?,
                state: parse_rule_state(&state_str),
                rule_body: row.r_by_name("rule_body")?,
                rule_weight: row.r_by_name::<f64>("rule_weight").unwrap_or(1.0),
                incremental: row.r_by_name::<bool>("incremental").unwrap_or(false),
                created_at: created.and_utc(),
                updated_at: updated.and_utc(),
            }))
        } else {
            Ok(None)
        }
    }

    // --- Derived cache operations (Sprint 5) ---

    async fn derived_cache_get(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
    ) -> anyhow::Result<Vec<DerivedFact>> {
        let rows = self
            .query_rows(
                &self.stmts.derived_cache_get,
                query_values!(ctx.tenant_id, cache_key.to_string()),
            )
            .await?;

        let mut facts = Vec::with_capacity(rows.len());
        for row in rows {
            let src_id: Uuid = row.r_by_name("src_id")?;
            let dst_id: Uuid = row.r_by_name("dst_id")?;

            facts.push(DerivedFact {
                src_id: src_id.to_string(),
                pred: row.r_by_name("pred")?,
                dst_id: dst_id.to_string(),
                confidence: row.r_by_name::<f64>("confidence").unwrap_or(1.0),
                rule_id: row.r_by_name("rule_id").unwrap_or_default(),
                support_count: 1,
                provenance: vec![],
            });
        }
        Ok(facts)
    }

    async fn derived_cache_put(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
        facts: &[DerivedFact],
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().naive_utc();

        for (idx, fact) in facts.iter().enumerate() {
            let src_uuid: Uuid = fact.src_id.parse().map_err(|e| {
                anyhow::anyhow!(
                    "derived_cache_put: invalid src_id UUID '{}': {}",
                    fact.src_id,
                    e
                )
            })?;
            let dst_uuid: Uuid = fact.dst_id.parse().map_err(|e| {
                anyhow::anyhow!(
                    "derived_cache_put: invalid dst_id UUID '{}': {}",
                    fact.dst_id,
                    e
                )
            })?;

            self.session
                .exec_with_values(
                    &self.stmts.derived_cache_put,
                    query_values!(
                        ctx.tenant_id,
                        cache_key.to_string(),
                        idx as i32,
                        src_uuid,
                        fact.pred.clone(),
                        dst_uuid,
                        fact.confidence,
                        fact.rule_id.clone(),
                        now
                    ),
                )
                .await?;
        }
        Ok(())
    }

    async fn derived_cache_clear(&self, ctx: &TenantContext, pred: &str) -> anyhow::Result<()> {
        // Delete the partition for this exact cache_key.
        // The `pred` parameter is used as cache_key in the derived_cache table.
        self.session
            .exec_with_values(
                &self.stmts.derived_cache_clear,
                query_values!(ctx.tenant_id, pred.to_string()),
            )
            .await?;
        tracing::debug!(pred, "derived cache cleared for key");
        Ok(())
    }

    // --- Provenance operations (Sprint 5) ---

    async fn provenance_put(
        &self,
        ctx: &TenantContext,
        derived_edge_id: &str,
        steps: &[ProvenanceStep],
    ) -> anyhow::Result<()> {
        for (idx, step) in steps.iter().enumerate() {
            self.session
                .exec_with_values(
                    &self.stmts.provenance_put,
                    query_values!(
                        ctx.tenant_id,
                        derived_edge_id.to_string(),
                        idx as i32,
                        step.parent_src.clone(),
                        step.parent_pred.clone(),
                        step.parent_dst.clone(),
                        step.parent_kind.clone()
                    ),
                )
                .await?;
        }
        Ok(())
    }

    async fn provenance_get(
        &self,
        ctx: &TenantContext,
        derived_edge_id: &str,
    ) -> anyhow::Result<Vec<ProvenanceStep>> {
        let rows = self
            .query_rows(
                &self.stmts.provenance_get,
                query_values!(ctx.tenant_id, derived_edge_id.to_string()),
            )
            .await?;

        let mut steps = Vec::with_capacity(rows.len());
        for row in rows {
            steps.push(ProvenanceStep {
                parent_src: row.r_by_name("parent_src")?,
                parent_pred: row.r_by_name("parent_pred")?,
                parent_dst: row.r_by_name("parent_dst")?,
                parent_kind: row.r_by_name("parent_kind")?,
            });
        }
        Ok(steps)
    }

    // --- Heat telemetry operations (Sprint 5) ---
    // Heat DDL (counter tables) not yet created — will be implemented in B10.
    // These are no-ops that log a debug message; heat is telemetry, not critical path.

    async fn heat_record(
        &self,
        _ctx: &TenantContext,
        pred: &str,
        _hit: bool,
        _compute_ms: Option<i64>,
    ) -> anyhow::Result<()> {
        tracing::debug!(pred, "heat_record: no-op (heat DDL pending B10)");
        Ok(())
    }

    async fn heat_get(
        &self,
        _ctx: &TenantContext,
        pred: &str,
        _days: u32,
    ) -> anyhow::Result<(i64, i64)> {
        tracing::debug!(pred, "heat_get: no-op (heat DDL pending B10)");
        Ok((0, 0))
    }

    async fn materialized_edge_put(
        &self,
        _ctx: &TenantContext,
        _edge: &MaterializedEdge,
    ) -> anyhow::Result<()> {
        anyhow::bail!("materialized_edge_put: CQL not yet implemented (B10)")
    }
    async fn materialized_edges_by_src(
        &self,
        _ctx: &TenantContext,
        _src_id: &str,
        _pred: Option<&str>,
    ) -> anyhow::Result<Vec<MaterializedEdge>> {
        anyhow::bail!("materialized_edges_by_src: CQL not yet implemented (B10)")
    }
    async fn materialized_edges_by_pred(
        &self,
        _ctx: &TenantContext,
        _pred: &str,
    ) -> anyhow::Result<Vec<MaterializedEdge>> {
        anyhow::bail!("materialized_edges_by_pred: CQL not yet implemented (B10)")
    }
    async fn materialized_edges_clear(
        &self,
        _ctx: &TenantContext,
        _pred: &str,
    ) -> anyhow::Result<()> {
        anyhow::bail!("materialized_edges_clear: CQL not yet implemented (B10)")
    }
    async fn promoted_predicate_get(
        &self,
        _ctx: &TenantContext,
        _pred: &str,
    ) -> anyhow::Result<Option<PromotedPredicate>> {
        anyhow::bail!("promoted_predicate_get: CQL not yet implemented (B10)")
    }
    async fn promoted_predicate_put(
        &self,
        _ctx: &TenantContext,
        _entry: &PromotedPredicate,
    ) -> anyhow::Result<()> {
        anyhow::bail!("promoted_predicate_put: CQL not yet implemented (B10)")
    }
    async fn promoted_predicate_list(
        &self,
        _ctx: &TenantContext,
    ) -> anyhow::Result<Vec<PromotedPredicate>> {
        anyhow::bail!("promoted_predicate_list: CQL not yet implemented (B10)")
    }

    // --- Typed edge operations ---

    async fn typed_edge_put(&self, ctx: &TenantContext, edge: &TypedEdge) -> anyhow::Result<()> {
        let query = "INSERT INTO agent_memory.typed_edges \
            (tenant_id, session_id, src_id, edge_type, dst_id, weight, metadata, created_at) \
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)";
        self.session
            .query_with_values(
                query,
                query_values!(
                    ctx.tenant_id,
                    edge.session_id,
                    edge.src_id,
                    edge.edge_type.clone(),
                    edge.dst_id,
                    edge.weight,
                    edge.metadata.clone().unwrap_or_default(),
                    edge.created_at.naive_utc()
                ),
            )
            .await?;
        Ok(())
    }

    async fn typed_edge_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<TypedEdge>> {
        let query = "SELECT src_id, edge_type, dst_id, weight, metadata, created_at \
            FROM agent_memory.typed_edges \
            WHERE tenant_id = ? AND session_id = ?";
        let prepared = self.session.prepare(query).await?;
        let rows = self
            .session
            .exec_with_values(&prepared, query_values!(ctx.tenant_id, session_id))
            .await?
            .response_body()?
            .into_rows()
            .unwrap_or_default();

        let mut edges = Vec::new();
        for row in rows {
            // Skip ghost rows with NULL required fields.
            let Ok(src_id) = row.r_by_name::<Uuid>("src_id") else {
                continue;
            };
            let Ok(dst_id) = row.r_by_name::<Uuid>("dst_id") else {
                continue;
            };
            let edge_type = row.r_by_name::<String>("edge_type").unwrap_or_default();
            if edge_type.is_empty() {
                continue;
            };
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();
            edges.push(TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id,
                edge_type,
                dst_id,
                weight: row.r_by_name::<f64>("weight").unwrap_or(1.0),
                metadata: row
                    .r_by_name::<String>("metadata")
                    .ok()
                    .filter(|s| !s.is_empty()),
                created_at: created.and_utc(),
            });
        }
        Ok(edges)
    }

    async fn typed_edge_list_from(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        src_id: Uuid,
    ) -> anyhow::Result<Vec<TypedEdge>> {
        let query = "SELECT src_id, edge_type, dst_id, weight, metadata, created_at \
            FROM agent_memory.typed_edges \
            WHERE tenant_id = ? AND session_id = ? AND src_id = ?";
        let prepared = self.session.prepare(query).await?;
        let rows = self
            .session
            .exec_with_values(&prepared, query_values!(ctx.tenant_id, session_id, src_id))
            .await?
            .response_body()?
            .into_rows()
            .unwrap_or_default();

        let mut edges = Vec::new();
        for row in rows {
            let created = row
                .r_by_name::<chrono::NaiveDateTime>("created_at")
                .unwrap_or_default();
            edges.push(TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id,
                edge_type: row.r_by_name::<String>("edge_type").unwrap_or_default(),
                dst_id: row.r_by_name("dst_id")?,
                weight: row.r_by_name::<f64>("weight").unwrap_or(1.0),
                metadata: row
                    .r_by_name::<String>("metadata")
                    .ok()
                    .filter(|s| !s.is_empty()),
                created_at: created.and_utc(),
            });
        }
        Ok(edges)
    }
}
