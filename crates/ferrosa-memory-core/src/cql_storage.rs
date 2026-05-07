//! CQL storage backend using scylla 0.15 (canonical ScyllaDB/Cassandra driver).
//!
//! Implements the [`Storage`] trait against a real Ferrosa/Cassandra cluster.
//! All queries use prepared statements with parameterized bindings (STRIDE T4).
//! Every query includes `tenant_id` from auth context (STRIDE I1).
//!
//! p1-22: migrated from cdrs-tokio fork → scylla 0.15. Uses LegacySession
//! and LegacyQueryResult for minimal churn on row-reading call sites.

// The entire module uses the scylla 0.15 LegacySession API intentionally.
// The legacy API is deprecated upstream but provides stable semantics until
// we migrate to the new generic deserialization API in a follow-up.
#![allow(deprecated)]

use std::collections::HashMap;
use std::sync::Arc;

use scylla::frame::response::cql_to_rust::FromCqlVal;
use scylla::frame::response::result::{CqlValue, Row};
use scylla::prepared_statement::PreparedStatement;
use scylla::{LegacySession, SessionBuilder};
use serde_json::json;
use uuid::Uuid;

use crate::config::FerrosaCqlConfig;
use crate::context_segment::{ContextSegment, TemporalEdge};
use crate::storage::Storage;
use crate::types::*;

// ---------------------------------------------------------------------------
// Compatibility shim: by-name column access for scylla LegacyQueryResult rows
// ---------------------------------------------------------------------------
//
// scylla's `Row` is positional-only (`columns: Vec<Option<CqlValue>>`).
// The column-name → index mapping comes from `LegacyQueryResult::col_specs()`.
// We extract the mapping once per result and pass it alongside rows via
// `CqlResultSet`.

/// Ordered column-name mapping extracted from a `LegacyQueryResult`.
pub type ColMap = HashMap<String, usize>;

/// Build a `ColMap` from a slice of `ColumnSpec`s.
pub fn build_col_map(specs: &[scylla::frame::response::result::ColumnSpec<'_>]) -> ColMap {
    specs
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name().to_owned(), i))
        .collect()
}

/// Read a typed value from a `Row` by column name.
///
/// Returns `Err` if the column is absent from the result metadata or if the
/// CQL value cannot be converted to `T`. The error message includes the column
/// name and the conversion error for easy diagnosis.
pub fn cql_get<T>(row: &Row, col_map: &ColMap, name: &str) -> anyhow::Result<T>
where
    T: FromCqlVal<Option<CqlValue>>,
{
    let idx = col_map
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("column '{}' not in result set", name))?;
    let val = row
        .columns
        .get(*idx)
        .ok_or_else(|| anyhow::anyhow!("column index {} out of range for '{}'", idx, name))?
        .clone();
    T::from_cql(val).map_err(|e| anyhow::anyhow!("column '{}': {}", name, e))
}

fn sprint1_seed_insert_statements(ks: &str) -> (String, String) {
    let entity_q = format!(
        "INSERT INTO {ks}.entity_types (type_name, description, created_at) \
         VALUES (?, ?, ?)"
    );
    let edge_q = format!(
        "INSERT INTO {ks}.edge_types (type_name, description, src_types, dst_types, created_at) \
         VALUES (?, ?, ?, ?, ?)"
    );
    (entity_q, edge_q)
}

/// Execute a SELECT or DML and return `(col_map, rows)`.
///
/// Convenience wrapper around `execute_unpaged` + col_map extraction.
#[allow(unused_macros)]
macro_rules! exec_rows {
    ($session:expr, $stmt:expr, $values:expr) => {{
        #[allow(deprecated)]
        let result = $session.execute_unpaged($stmt, $values).await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        (col_map, rows)
    }};
}

/// Execute a raw (non-prepared) SELECT and return `(col_map, rows)`.
#[allow(unused_macros)]
macro_rules! query_rows {
    ($session:expr, $query:expr, $values:expr) => {{
        #[allow(deprecated)]
        let result = $session.query_unpaged($query, $values).await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        (col_map, rows)
    }};
}

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

fn parse_approval_decision(s: &str) -> ApprovalDecision {
    match s {
        "approved" => ApprovalDecision::Approved,
        "rejected" => ApprovalDecision::Rejected,
        _ => ApprovalDecision::Proposed,
    }
}

fn parse_claim_status(s: &str) -> ClaimStatus {
    match s {
        "approved" => ClaimStatus::Approved,
        "rejected" => ClaimStatus::Rejected,
        _ => ClaimStatus::Proposed,
    }
}

fn parse_alias_scope_kind(s: &str) -> AliasScopeKind {
    match s {
        "session" => AliasScopeKind::Session,
        "workspace" => AliasScopeKind::Workspace,
        _ => AliasScopeKind::Global,
    }
}

fn render_vector_literal(values: &[f32]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|v| format!("{v:.8}"))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn build_fold_ann_search_query(
    keyspace: &str,
    query_embedding: &[f32],
    k: usize,
) -> (String, usize) {
    let vec_literal = render_vector_literal(query_embedding);
    (
        format!(
            "SELECT fold_id, depth, fold_summary, token_count, raw_trajectory \
             FROM {keyspace}.trajectory_folds WHERE session_id = ? AND tenant_id = ? \
             ORDER BY fold_embedding ANN OF {vec_literal} LIMIT {k}"
        ),
        2,
    )
}

fn tokenize_context_terms(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter_map(|part| {
            let term = part.trim().to_ascii_lowercase();
            if term.len() >= 3 { Some(term) } else { None }
        })
        .collect()
}

fn context_segment_from_row(
    ctx: &TenantContext,
    row: &Row,
    col_map: &ColMap,
) -> anyhow::Result<ContextSegment> {
    let embedding = cql_get::<Vec<u8>>(row, col_map, "segment_embedding")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| crate::vector::decode_vector(&v));
    Ok(ContextSegment {
        tenant_id: cql_get(row, col_map, "tenant_id").unwrap_or(ctx.tenant_id),
        session_id: cql_get(row, col_map, "session_id")?,
        segment_id: cql_get(row, col_map, "segment_id")?,
        source_session: cql_get(row, col_map, "source_session")?,
        source_fold_id: cql_get::<Uuid>(row, col_map, "source_fold_id").ok(),
        conversation_id: cql_get(row, col_map, "conversation_id")?,
        segment_index: cql_get(row, col_map, "segment_index")?,
        start_turn: cql_get(row, col_map, "start_turn")?,
        end_turn: cql_get(row, col_map, "end_turn")?,
        start_time: cql_get::<chrono::DateTime<chrono::Utc>>(row, col_map, "start_time").ok(),
        end_time: cql_get::<chrono::DateTime<chrono::Utc>>(row, col_map, "end_time").ok(),
        segment_text: cql_get(row, col_map, "segment_text")?,
        segment_summary: cql_get::<String>(row, col_map, "segment_summary").ok(),
        bm25_text: cql_get(row, col_map, "bm25_text")?,
        segment_embedding: embedding,
        token_count: cql_get(row, col_map, "token_count")?,
        content_hash: cql_get(row, col_map, "content_hash")?,
        prev_segment_id: cql_get::<Uuid>(row, col_map, "prev_segment_id").ok(),
        next_segment_id: cql_get::<Uuid>(row, col_map, "next_segment_id").ok(),
        created_at: cql_get::<chrono::DateTime<chrono::Utc>>(row, col_map, "created_at")
            .unwrap_or_else(|_| chrono::Utc::now()),
    })
}

fn temporal_edge_from_row(
    ctx: &TenantContext,
    row: &Row,
    col_map: &ColMap,
) -> anyhow::Result<TemporalEdge> {
    Ok(TemporalEdge {
        tenant_id: cql_get(row, col_map, "tenant_id").unwrap_or(ctx.tenant_id),
        session_id: cql_get(row, col_map, "session_id")?,
        src_id: cql_get(row, col_map, "src_id")?,
        edge_type: cql_get(row, col_map, "edge_type")?,
        dst_id: cql_get(row, col_map, "dst_id")?,
        relation_time: cql_get(row, col_map, "relation_time")?,
        ordinal: cql_get(row, col_map, "ordinal")?,
        metadata: cql_get(row, col_map, "metadata").unwrap_or_default(),
        created_at: cql_get::<chrono::DateTime<chrono::Utc>>(row, col_map, "created_at")
            .unwrap_or_else(|_| chrono::Utc::now()),
    })
}

fn rule_entry_from_row(
    ctx: &TenantContext,
    row: &Row,
    col_map: &ColMap,
) -> anyhow::Result<RuleEntry> {
    let created = cql_get::<chrono::DateTime<chrono::Utc>>(row, col_map, "created_at")
        .unwrap_or_else(|e| {
            tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
            chrono::DateTime::UNIX_EPOCH
        });
    let updated = cql_get::<chrono::DateTime<chrono::Utc>>(row, col_map, "updated_at")
        .unwrap_or_else(|e| {
            tracing::warn!(col = "updated_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
            chrono::DateTime::UNIX_EPOCH
        });
    let state_str: String = cql_get(row, col_map, "state").unwrap_or_default();

    Ok(RuleEntry {
        tenant_id: ctx.tenant_id,
        rule_id: cql_get(row, col_map, "rule_id")?,
        version: cql_get(row, col_map, "version")?,
        name: cql_get(row, col_map, "name")?,
        family: cql_get(row, col_map, "family")?,
        state: parse_rule_state(&state_str),
        rule_body: cql_get(row, col_map, "rule_body")?,
        rule_weight: cql_get::<f64>(row, col_map, "rule_weight").unwrap_or(1.0),
        incremental: cql_get::<bool>(row, col_map, "incremental").unwrap_or(false),
        created_at: created,
        updated_at: updated,
    })
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
    col_map: &ColMap,
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
    let description = cql_get::<String>(row, col_map, "description").ok();
    let description_embedding = cql_get::<Vec<u8>>(row, col_map, "description_embedding")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| crate::vector::decode_vector(&v));
    let tags = cql_get::<String>(row, col_map, "tags")
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default();
    let properties = cql_get::<String>(row, col_map, "properties")
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let content_hash = cql_get::<String>(row, col_map, "content_hash").ok();
    let updated_at = cql_get::<chrono::DateTime<chrono::Utc>>(row, col_map, "updated_at").ok();
    let scope = cql_get::<String>(row, col_map, "scope")
        .ok()
        .and_then(|s| match s.as_str() {
            "global" => Some(EntityScope::Global),
            "session" => Some(EntityScope::Session),
            _ => None,
        })
        .unwrap_or_default();
    let ingested_by_session = cql_get::<Uuid>(row, col_map, "ingested_by_session").ok();
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

/// Type alias for the scylla legacy session used throughout this crate.
///
/// `LegacySession` provides `execute_unpaged` / `query_unpaged` that return
/// `LegacyQueryResult`, which is the lowest-churn migration path from
/// cdrs-tokio's frame-based API.
pub type CqlSession = LegacySession;

/// Build a new CQL session with the given username/password against the
/// contact points in `config`. Shared by `CqlStorage::connect` (runtime
/// session) and `connect_admin_session` (short-lived migration session).
pub async fn connect_session(
    config: &FerrosaCqlConfig,
    username: &str,
    password: &str,
) -> anyhow::Result<Arc<CqlSession>> {
    if config.contact_points.is_empty() {
        anyhow::bail!("no contact points configured");
    }

    let session = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        SessionBuilder::new()
            .known_nodes(&config.contact_points)
            .user(username, password)
            .connection_timeout(std::time::Duration::from_secs(10))
            .build_legacy(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("CQL session build timed out (10s) — is Ferrosa running?"))??;

    Ok(Arc::new(session))
}

/// Build a short-lived session for schema migrations. Uses
/// `admin_username`/`admin_password` when set; otherwise falls back to the
/// runtime `username`/`password` (auth-disabled clusters or deployments
/// where the runtime user already has DDL rights).
pub async fn connect_admin_session(config: &FerrosaCqlConfig) -> anyhow::Result<Arc<CqlSession>> {
    let (user, pass) = match (&config.admin_username, &config.admin_password) {
        (Some(u), Some(p)) => (u.as_str(), p.as_str()),
        _ => (config.username.as_str(), config.password.as_str()),
    };
    connect_session(config, user, pass).await
}

/// Prepared statement cache for all table operations.
struct PreparedStatements {
    // Memo
    memo_get: PreparedStatement,
    memo_touch: PreparedStatement,
    memo_put: PreparedStatement,
    // Plan
    plan_put: PreparedStatement,
    plan_get: PreparedStatement,
    plan_get_depth: PreparedStatement,
    plan_update: PreparedStatement,
    // Fold
    fold_put: PreparedStatement,
    fold_get: PreparedStatement,
    fold_append: PreparedStatement,
    fold_complete: PreparedStatement,
    // Entity
    entity_put: PreparedStatement,
    entity_count: PreparedStatement,
    entity_list_session: PreparedStatement,
    entity_list_all: PreparedStatement,
    entity_update_state: PreparedStatement,
    // Count queries for stats
    fold_count: PreparedStatement,
    memo_count: PreparedStatement,
    // Temporal
    temporal_put: PreparedStatement,
    temporal_get_current: PreparedStatement,
    temporal_invalidate: PreparedStatement,
    // Entity neighbor queries (spreading activation)
    edge_mentioned_in_by_entity: PreparedStatement,
    edge_co_occurs_by_a: PreparedStatement,
    edge_co_occurs_by_b: PreparedStatement,
    edge_supersedes_by_new: PreparedStatement,
    edge_supersedes_by_old: PreparedStatement,
    // Feedback
    feedback_put: PreparedStatement,
    feedback_list_all: PreparedStatement,
    // Intentions
    intention_put: PreparedStatement,
    intention_list: PreparedStatement,
    intention_list_all: PreparedStatement,
    intention_update_status: PreparedStatement,
    // Tool usage logging
    tool_usage_put: PreparedStatement,
    tool_usage_query: PreparedStatement,
    // Audit
    audit_put: PreparedStatement,
    // Sync/export list queries
    fold_list_all: PreparedStatement,
    temporal_list_all: PreparedStatement,
    // Warmth (Sprint 5)
    warmth_get: PreparedStatement,
    warmth_put: PreparedStatement,
    warmth_list_session: PreparedStatement,
    warmth_delete: PreparedStatement,
    // Rules (Sprint 5)
    rule_put_by_id: PreparedStatement,
    rule_put_by_family: PreparedStatement,
    rule_put_active_by_state: PreparedStatement,
    rule_get: PreparedStatement,
    rule_get_version: PreparedStatement,
    rule_list_family: PreparedStatement,
    rule_list_active: PreparedStatement,
    // Derived cache (Sprint 5)
    derived_cache_get: PreparedStatement,
    derived_cache_put: PreparedStatement,
    derived_cache_clear: PreparedStatement,
    // TTL tracking (Sprint 6)
    derived_cache_ttl_track_put: PreparedStatement,
    derived_cache_ttl_track_get: PreparedStatement,
    // Provenance (Sprint 5)
    provenance_put: PreparedStatement,
    provenance_get: PreparedStatement,
    // Confidence scores
    confidence_put: PreparedStatement,
    confidence_get: PreparedStatement,
}

/// CQL storage backend.
pub struct CqlStorage {
    session: Arc<CqlSession>,
    stmts: PreparedStatements,
    keyspace: String,
}

impl CqlStorage {
    fn graph_write_error(op: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "{op} must go through a GraphClient-backed storage adapter; direct graph-table writes are disabled"
        )
    }

    /// Connect to a Ferrosa/Cassandra cluster and prepare all statements.
    pub async fn connect(config: &FerrosaCqlConfig) -> anyhow::Result<Self> {
        let session = connect_session(config, &config.username, &config.password).await?;
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
                     description, tags, properties, content_hash, \
                     updated_at, scope, ingested_by_session \
                     FROM {ks}.entity_store WHERE tenant_id = ? AND session_id = ?"
                ))
                .await?,
            entity_list_all: session
                .prepare(format!(
                    "SELECT entity_id, session_id, entity_name, entity_type, source_fold_id, \
                     context_snippet, entity_embedding, confidence, state, created_at, \
                     description, tags, properties, content_hash, \
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
            rule_put_active_by_state: session
                .prepare(format!(
                    "INSERT INTO {ks}.rules_active_by_state \
                     (tenant_id, state, family, rule_id, version, updated_at) \
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
            rule_get_version: session
                .prepare(format!(
                    "SELECT rule_id, version, name, family, state, rule_body, \
                     rule_weight, incremental, created_at, updated_at \
                     FROM {ks}.rules_by_id WHERE tenant_id = ? AND rule_id = ? AND version = ? LIMIT 1"
                ))
                .await?,
            rule_list_family: session
                .prepare(format!(
                    "SELECT rule_id, version \
                     FROM {ks}.rules_by_family WHERE tenant_id = ? AND family = ? AND state = ?"
                ))
                .await?,
            rule_list_active: session
                .prepare(format!(
                    "SELECT family, rule_id, version \
                     FROM {ks}.rules_active_by_state WHERE tenant_id = ? AND state = ?"
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
            // TTL tracking (Sprint 6)
            derived_cache_ttl_track_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.derived_cache_ttl_track \
                     (tenant_id, cache_key, seq, src_id, pred, dst_id, ttl_seconds, rule_id, computed_at, next_maintenance) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            derived_cache_ttl_track_get: session
                .prepare(format!(
                    "SELECT ttl_seconds, next_maintenance FROM {ks}.derived_cache_ttl_track \
                     WHERE tenant_id = ? AND cache_key = ? AND seq = ?"
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
            confidence_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.confidence_scores \
                     (entity_id, fact_hash, confidence, source_count, last_confirmed_at, contradiction_count) \
                     VALUES (?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            confidence_get: session
                .prepare(format!(
                    "SELECT confidence, source_count, last_confirmed_at, contradiction_count \
                     FROM {ks}.confidence_scores WHERE entity_id = ? AND fact_hash = ?"
                ))
                .await?,
        };

        tracing::info!(
            keyspace = ks,
            statements = 46,
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
        #[allow(deprecated)]
        match self.session.query_unpaged(query, &[] as &[&str]).await {
            Ok(result) => {
                let col_map = build_col_map(result.col_specs());
                let rows = result.rows_or_empty();
                let mut types: Vec<String> = rows
                    .iter()
                    .filter_map(|r| cql_get::<String>(r, &col_map, "type_name").ok())
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
        #[allow(deprecated)]
        match self.session.query_unpaged(query, &[] as &[&str]).await {
            Ok(result) => {
                let col_map = build_col_map(result.col_specs());
                let rows = result.rows_or_empty();
                let mut types: Vec<String> = rows
                    .iter()
                    .filter_map(|r| cql_get::<String>(r, &col_map, "type_name").ok())
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
            "skill",
            "tag",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// Get the keyspace name this storage is connected to.
    pub fn keyspace(&self) -> &str {
        &self.keyspace
    }

    /// Idempotently register the Sprint 1 entity and edge types in the type
    /// registry. Safe to run on every startup — CQL INSERT on the primary
    /// key is upsert, so re-running is a no-op for existing entries.
    ///
    /// Adds:
    /// - entity_types: `skill`, `tag`
    /// - edge_types: `TAGGED_AS` (entity → tag), `PARENT_TAG` (tag → tag
    ///   hierarchy), `REQUIRES` (skill prerequisite)
    ///
    /// `SUPERSEDES` was already seeded in DDL 019; versions of skills link
    /// via that edge type.
    pub async fn seed_sprint1_types(&self) -> anyhow::Result<()> {
        let ks = &self.keyspace;

        // Entity types (upsert).
        let entity_seeds: &[(&str, &str)] = &[
            (
                "skill",
                "A methodology or procedure with structured steps (e.g. TDD, STRIDE threat modeling). Retrieved for context-aware suggestions.",
            ),
            (
                "tag",
                "A classification label. Tags form a hierarchy via PARENT_TAG edges; entities attach via TAGGED_AS.",
            ),
        ];
        let (entity_q, edge_q) = sprint1_seed_insert_statements(ks);
        let entity_writes = entity_seeds.iter().map(|(name, desc)| {
            let q = entity_q.clone();
            let name = name.to_string();
            let desc = desc.to_string();
            async move {
                let created_at = chrono::Utc::now();
                #[allow(deprecated)]
                let res = self
                    .session
                    .query_unpaged(q, (name.clone(), desc, created_at))
                    .await;
                if let Err(e) = res {
                    tracing::warn!(type_name = %name, error = %e, "seed_sprint1_types: entity insert failed");
                }
            }
        });

        // Edge types (upsert).
        let edge_seeds: &[(&str, &str, &str, &str)] = &[
            (
                "TAGGED_AS",
                "Entity belongs to a tag. Source: any entity. Destination: tag.",
                "",
                "tag",
            ),
            (
                "PARENT_TAG",
                "Tag is a sub-category of another tag (hierarchy DAG). Source and destination: tag.",
                "tag",
                "tag",
            ),
            (
                "REQUIRES",
                "Source skill has a prerequisite skill. Source: skill. Destination: skill.",
                "skill",
                "skill",
            ),
        ];
        let edge_writes = edge_seeds.iter().map(|(name, desc, src, dst)| {
            let q = edge_q.clone();
            let name = name.to_string();
            let desc = desc.to_string();
            let src = src.to_string();
            let dst = dst.to_string();
            async move {
                let created_at = chrono::Utc::now();
                #[allow(deprecated)]
                let res = self
                    .session
                    .query_unpaged(q, (name.clone(), desc, src, dst, created_at))
                    .await;
                if let Err(e) = res {
                    tracing::warn!(edge_type = %name, error = %e, "seed_sprint1_types: edge insert failed");
                }
            }
        });

        // Run every seed upsert concurrently — each is an idempotent
        // single-partition write against a distinct primary key, so
        // there's no ordering requirement. Serially they were ~5 × RTT;
        // this collapses the whole seed to a single RTT window.
        tokio::join!(
            futures_util::future::join_all(entity_writes),
            futures_util::future::join_all(edge_writes),
        );

        Ok(())
    }

    /// Helper: execute a prepared statement and return `(col_map, rows)`.
    #[allow(deprecated)]
    async fn exec_prepared_rows(
        &self,
        stmt: &PreparedStatement,
        values: impl scylla::serialize::row::SerializeRow,
    ) -> anyhow::Result<(ColMap, Vec<Row>)> {
        let result = self.session.execute_unpaged(stmt, values).await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        Ok((col_map, rows))
    }

    async fn query_count_allow_filtering(
        &self,
        table: &str,
        ctx: &TenantContext,
    ) -> anyhow::Result<usize> {
        let query = format!(
            "SELECT COUNT(*) FROM {}.{} WHERE tenant_id = ? ALLOW FILTERING",
            self.keyspace, table
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query.clone(), (ctx.tenant_id,))
            .await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        let Some(row) = rows.first() else {
            return Ok(0);
        };
        if let Ok(count) = cql_get::<i64>(row, &col_map, "count") {
            return Ok(count as usize);
        }
        if let Ok(count) = cql_get::<i64>(row, &col_map, "system.count") {
            return Ok(count as usize);
        }
        anyhow::bail!("COUNT(*) response for {table} did not expose a readable count column");
    }

    /// Update only the entity embedding + updated_at for an existing row.
    ///
    /// This avoids the broader entity_put upsert path when running embedding
    /// backfills, so later schema fields are not rewritten from stale entity
    /// snapshots.
    pub async fn entity_update_embedding(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        entity_id: Uuid,
        embedding: &[f32],
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<()> {
        let vec_literal: String = format!(
            "[{}]",
            embedding
                .iter()
                .map(|v| format!("{v:.8}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let embedding_q = format!(
            "UPDATE {ks}.entity_store SET entity_embedding = {vec_literal} \
             WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
            ks = self.keyspace,
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(embedding_q, (ctx.tenant_id, session_id, entity_id))
            .await?;
        let updated_at_q = format!(
            "UPDATE {ks}.entity_store SET updated_at = ? \
             WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
            ks = self.keyspace,
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                updated_at_q,
                (updated_at, ctx.tenant_id, session_id, entity_id),
            )
            .await?;
        Ok(())
    }

    /// List all memo cache rows for a tenant (batch/export use only).
    pub async fn memo_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<MemoEntry>> {
        let query = format!(
            "SELECT content_hash, model_version, result, result_embedding, hit_count, created_at, last_hit_at, expires_at \
             FROM {}.memo_cache WHERE tenant_id = ? ALLOW FILTERING",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self.session.query_unpaged(query, (ctx.tenant_id,)).await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let created_at = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    chrono::DateTime::UNIX_EPOCH
                });
            results.push(MemoEntry {
                content_hash: cql_get(&row, &col_map, "content_hash")?,
                model_version: cql_get(&row, &col_map, "model_version")?,
                result: cql_get(&row, &col_map, "result")?,
                result_embedding: cql_get::<Vec<u8>>(&row, &col_map, "result_embedding")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|v| crate::vector::decode_vector(&v)),
                hit_count: cql_get(&row, &col_map, "hit_count").unwrap_or(0),
                created_at,
                last_hit_at: cql_get::<chrono::DateTime<chrono::Utc>>(
                    &row,
                    &col_map,
                    "last_hit_at",
                )
                .ok(),
                expires_at: cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "expires_at")
                    .ok(),
            });
        }
        Ok(results)
    }

    /// Targeted fold embedding rewrite for migration/backfill jobs.
    pub async fn fold_update_embedding(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        fold_id: Uuid,
        embedding: &[f32],
    ) -> anyhow::Result<()> {
        let vec_literal = render_vector_literal(embedding);
        let query = format!(
            "UPDATE {}.trajectory_folds SET fold_embedding = {} \
             WHERE session_id = ? AND tenant_id = ? AND fold_id = ?",
            self.keyspace, vec_literal
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(query, (session_id, ctx.tenant_id, fold_id))
            .await?;
        Ok(())
    }

    /// Targeted memo embedding rewrite for migration/backfill jobs.
    pub async fn memo_update_embedding(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
        embedding: &[f32],
    ) -> anyhow::Result<()> {
        let vec_literal = render_vector_literal(embedding);
        let query = format!(
            "UPDATE {}.memo_cache SET result_embedding = {} \
             WHERE content_hash = ? AND model_version = ? AND tenant_id = ?",
            self.keyspace, vec_literal
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                query,
                (
                    content_hash.to_string(),
                    model_version.to_string(),
                    ctx.tenant_id,
                ),
            )
            .await?;
        Ok(())
    }
}

impl Storage for CqlStorage {
    async fn memo_get(
        &self,
        ctx: &TenantContext,
        content_hash: &str,
        model_version: &str,
    ) -> anyhow::Result<Option<MemoEntry>> {
        let (col_map, rows) = self
            .exec_prepared_rows(
                &self.stmts.memo_get,
                (
                    content_hash.to_string(),
                    model_version.to_string(),
                    ctx.tenant_id,
                ),
            )
            .await?;

        if let Some(row) = rows.into_iter().next() {
            let result: String = cql_get(&row, &col_map, "result")?;
            let hit_count: i64 = cql_get(&row, &col_map, "hit_count")?;
            let created_at: chrono::DateTime<chrono::Utc> = cql_get(&row, &col_map, "created_at")?;

            Ok(Some(MemoEntry {
                content_hash: content_hash.to_string(),
                model_version: model_version.to_string(),
                result,
                result_embedding: cql_get::<Vec<u8>>(&row, &col_map, "result_embedding")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|v| crate::vector::decode_vector(&v)),
                hit_count,
                created_at,
                last_hit_at: cql_get::<chrono::DateTime<chrono::Utc>>(
                    &row,
                    &col_map,
                    "last_hit_at",
                )
                .ok(),
                expires_at: cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "expires_at")
                    .ok(),
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
        let now = chrono::Utc::now();
        self.exec_prepared_rows(
            &self.stmts.memo_touch,
            (
                now,
                content_hash.to_string(),
                model_version.to_string(),
                ctx.tenant_id,
            ),
        )
        .await?;
        Ok(())
    }

    async fn memo_put(&self, ctx: &TenantContext, entry: &MemoEntry) -> anyhow::Result<()> {
        let now = chrono::Utc::now();
        let expires: Option<chrono::DateTime<chrono::Utc>> = entry.expires_at;

        self.exec_prepared_rows(
            &self.stmts.memo_put,
            (
                entry.content_hash.clone(),
                entry.model_version.clone(),
                ctx.tenant_id,
                entry.result.clone(),
                entry
                    .result_embedding
                    .as_ref()
                    .map(|e| crate::vector::encode_vector(e)),
                now,
                now,  // last_hit_at = created_at initially
                0i64, // hit_count
                expires,
            ),
        )
        .await?;
        Ok(())
    }

    async fn plan_put(&self, ctx: &TenantContext, node: &PlanNode) -> anyhow::Result<()> {
        let status = serde_json::to_string(&node.status)?
            .trim_matches('"')
            .to_string();

        self.exec_prepared_rows(
            &self.stmts.plan_put,
            (
                node.session_id,
                ctx.tenant_id,
                node.depth,
                node.subtask_id.clone(),
                node.parent_subtask.clone(),
                node.goal_text.clone(),
                status,
                chrono::Utc::now(),
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
        let (col_map, rows) = if let Some(depth) = max_depth {
            self.exec_prepared_rows(
                &self.stmts.plan_get_depth,
                (session_id, ctx.tenant_id, depth),
            )
            .await?
        } else {
            self.exec_prepared_rows(&self.stmts.plan_get, (session_id, ctx.tenant_id))
                .await?
        };

        let mut nodes = Vec::with_capacity(rows.len());
        for row in rows {
            let status_str: String = cql_get(&row, &col_map, "status")?;
            let status: PlanStatus =
                serde_json::from_str(&format!("\"{status_str}\"")).unwrap_or(PlanStatus::Pending);

            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    chrono::DateTime::UNIX_EPOCH
                });

            nodes.push(PlanNode {
                session_id,
                depth: cql_get(&row, &col_map, "depth")?,
                subtask_id: cql_get(&row, &col_map, "subtask_id")?,
                parent_subtask: cql_get::<String>(&row, &col_map, "parent_subtask").ok(),
                goal_text: cql_get(&row, &col_map, "goal_text")?,
                status,
                outcome_summary: cql_get::<String>(&row, &col_map, "outcome_summary").ok(),
                created_at: created,
                completed_at: cql_get::<chrono::DateTime<chrono::Utc>>(
                    &row,
                    &col_map,
                    "completed_at",
                )
                .ok(),
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
        let completed: Option<chrono::DateTime<chrono::Utc>> =
            if status == PlanStatus::Complete || status == PlanStatus::Failed {
                Some(chrono::Utc::now())
            } else {
                None
            };

        self.exec_prepared_rows(
            &self.stmts.plan_update,
            (
                status_str,
                outcome_summary.map(String::from),
                completed,
                session_id,
                ctx.tenant_id,
                depth,
                subtask_id.to_string(),
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

        self.exec_prepared_rows(
            &self.stmts.fold_put,
            (
                entry.session_id,
                entry.fold_id,
                ctx.tenant_id,
                entry.depth,
                entry.parent_fold_id,
                entry.raw_trajectory.clone(),
                entry.token_count,
                status,
                chrono::Utc::now(),
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
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.fold_get, (session_id, ctx.tenant_id, fold_id))
            .await?;

        if let Some(row) = rows.into_iter().next() {
            let status_str: String = cql_get(&row, &col_map, "status")?;
            let status: FoldStatus =
                serde_json::from_str(&format!("\"{status_str}\"")).unwrap_or(FoldStatus::Active);
            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    chrono::DateTime::UNIX_EPOCH
                });

            Ok(Some(FoldEntry {
                session_id,
                fold_id,
                tenant_id: ctx.tenant_id,
                depth: cql_get(&row, &col_map, "depth")?,
                parent_fold_id: cql_get::<Uuid>(&row, &col_map, "parent_fold_id").ok(),
                raw_trajectory: cql_get(&row, &col_map, "raw_trajectory")?,
                fold_summary: cql_get::<String>(&row, &col_map, "fold_summary").ok(),
                fold_embedding: cql_get::<Vec<u8>>(&row, &col_map, "fold_embedding")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|v| crate::vector::decode_vector(&v)),
                token_count: cql_get(&row, &col_map, "token_count")?,
                compression_ratio: cql_get::<f64>(&row, &col_map, "compression_ratio").ok(),
                status,
                created_at: created,
                folded_at: cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "folded_at")
                    .ok(),
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

            self.exec_prepared_rows(
                &self.stmts.fold_append,
                (
                    new_trajectory,
                    new_count,
                    session_id,
                    ctx.tenant_id,
                    fold_id,
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
        let embedding_bytes = crate::vector::encode_vector(&embedding);
        self.exec_prepared_rows(
            &self.stmts.fold_complete,
            (
                "folded".to_string(),
                summary.to_string(),
                embedding_bytes,
                compression_ratio,
                chrono::Utc::now(),
                session_id,
                ctx.tenant_id,
                fold_id,
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
        // ANN query using ORDER BY fold_embedding ANN OF [..] LIMIT {k}.
        // Ferrosa vector ANN expects a vector literal; binding Vec<u8> serializes
        // as Blob and produces warning-noisy fallback on live smoke tests.
        let (query, _bind_count) = build_fold_ann_search_query(&self.keyspace, query_embedding, k);
        #[allow(deprecated)]
        let (col_map, rows) = match self
            .session
            .query_unpaged(query, (session_id, ctx.tenant_id))
            .await
        {
            Ok(result) => {
                let col_map = build_col_map(result.col_specs());
                let rows = result.rows_or_empty();
                (col_map, rows)
            }
            Err(e) => {
                tracing::warn!(error = %e, "ANN query failed, falling back to LIMIT");
                let fallback = format!(
                    "SELECT fold_id, depth, fold_summary, token_count, raw_trajectory \
                     FROM {}.trajectory_folds WHERE session_id = ? AND tenant_id = ? LIMIT {}",
                    self.keyspace, k
                );
                #[allow(deprecated)]
                let result = self
                    .session
                    .query_unpaged(fallback, (session_id, ctx.tenant_id))
                    .await?;
                let col_map = build_col_map(result.col_specs());
                let rows = result.rows_or_empty();
                (col_map, rows)
            }
        };

        let mut results = Vec::new();
        for row in rows {
            if let Ok(summary) = cql_get::<String>(&row, &col_map, "fold_summary") {
                results.push(FoldSummary {
                    fold_id: cql_get(&row, &col_map, "fold_id")?,
                    depth: cql_get(&row, &col_map, "depth")?,
                    fold_summary: summary,
                    token_count: cql_get(&row, &col_map, "token_count")?,
                    similarity: None,
                    raw_trajectory: if include_raw {
                        cql_get::<String>(&row, &col_map, "raw_trajectory").ok()
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
        // Base INSERT with required fields only — avoids Option serialization
        // issues with Ferrosa's VECTOR columns.
        self.exec_prepared_rows(
            &self.stmts.entity_put,
            (
                ctx.tenant_id,
                entry.entity_id,
                entry.session_id,
                entry.entity_name.clone(),
                entry.entity_type.clone(),
                entry.context_snippet.clone(),
                entry.confidence as f32,
                chrono::Utc::now(),
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
            #[allow(deprecated)]
            let _ = self
                .session
                .query_unpaged(
                    q,
                    (fold_id, ctx.tenant_id, entry.session_id, entry.entity_id),
                )
                .await;
        }
        if let Some(ref emb) = entry.entity_embedding {
            // Ferrosa requires a CQL literal [f32, f32, ...] for VECTOR columns.
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
            #[allow(deprecated)]
            if let Err(e) = self
                .session
                .query_unpaged(q, (ctx.tenant_id, entry.session_id, entry.entity_id))
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
        let updated_at = entry.updated_at.unwrap_or(entry.created_at);
        let q = format!(
            "UPDATE {ks}.entity_store SET scope = ?, updated_at = ? \
             WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
            ks = self.keyspace,
        );
        #[allow(deprecated)]
        if let Err(e) = self
            .session
            .query_unpaged(
                q,
                (
                    scope_str.to_string(),
                    updated_at,
                    ctx.tenant_id,
                    entry.session_id,
                    entry.entity_id,
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
            #[allow(deprecated)]
            if let Err(e) = self
                .session
                .query_unpaged(
                    q,
                    (ingester, ctx.tenant_id, entry.session_id, entry.entity_id),
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
            #[allow(deprecated)]
            if let Err(e) = self
                .session
                .query_unpaged(
                    q,
                    (
                        desc.clone(),
                        ctx.tenant_id,
                        entry.session_id,
                        entry.entity_id,
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
            #[allow(deprecated)]
            if let Err(e) = self
                .session
                .query_unpaged(q, (ctx.tenant_id, entry.session_id, entry.entity_id))
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
            #[allow(deprecated)]
            if let Err(e) = self
                .session
                .query_unpaged(
                    q,
                    (tags_json, ctx.tenant_id, entry.session_id, entry.entity_id),
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
            #[allow(deprecated)]
            if let Err(e) = self
                .session
                .query_unpaged(
                    q,
                    (props_json, ctx.tenant_id, entry.session_id, entry.entity_id),
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
            #[allow(deprecated)]
            if let Err(e) = self
                .session
                .query_unpaged(
                    q,
                    (
                        hash.clone(),
                        ctx.tenant_id,
                        entry.session_id,
                        entry.entity_id,
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
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, session_id))
            .await?;

        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        let lower = name.to_lowercase();

        // Collect matches with rank: 0=exact, 1=segment (after ::), 2=substring
        let mut scored: Vec<(u8, EntityEntry)> = Vec::new();
        for row in rows {
            let Ok(entity_name) = cql_get::<String>(&row, &col_map, "entity_name") else {
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

            let Ok(entity_id) = cql_get::<Uuid>(&row, &col_map, "entity_id") else {
                continue;
            };
            let Ok(entity_type) = cql_get::<String>(&row, &col_map, "entity_type") else {
                continue;
            };
            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            let state = cql_get::<String>(&row, &col_map, "state")
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
                    confidence: f64::from(
                        cql_get::<f32>(&row, &col_map, "confidence").unwrap_or(1.0),
                    ),
                    state,
                    created_at: created,
                    ..Default::default()
                },
            ));
        }

        scored.sort_by_key(|(rank, _)| *rank);
        Ok(scored.into_iter().map(|(_, e)| e).collect())
    }

    async fn entity_find_by_exact_name(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        name: &str,
        entity_type: &str,
    ) -> anyhow::Result<Option<EntityEntry>> {
        // Exact lookup keyed on (tenant_id, session_id, entity_name,
        // entity_type). `idx_entity_name_phonetic` (ddl/002) gives the 2i
        // on `entity_name`; the entity_type + session + tenant predicates
        // are added under ALLOW FILTERING so we only carry back the one
        // row the caller cares about. This replaces the fuzzy
        // `entity_find_phonetic` scan as the by-name idempotency key for
        // writers like `ingest_skill`.
        let query = format!(
            "SELECT entity_id FROM {}.entity_store \
             WHERE tenant_id = ? AND session_id = ? \
               AND entity_name = ? AND entity_type = ? \
             ALLOW FILTERING",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, session_id, name, entity_type))
            .await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let entity_id = cql_get::<Uuid>(row, &col_map, "entity_id")
            .map_err(|e| anyhow::anyhow!("entity_find_by_exact_name row missing entity_id: {e}"))?;
        // Delegate to the full-row read so callers see the same shape as
        // entity_get_by_id without duplicating the column list here.
        self.entity_get_by_id(ctx, session_id, entity_id).await
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
             description, tags, properties, content_hash, \
             updated_at, scope, ingested_by_session \
             FROM {}.entity_store WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, session_id, entity_id))
            .await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        if let Some(row) = rows.first() {
            let Ok(entity_name) = cql_get::<String>(row, &col_map, "entity_name") else {
                return Ok(None);
            };
            let Ok(entity_type) = cql_get::<String>(row, &col_map, "entity_type") else {
                return Ok(None);
            };
            let context_snippet =
                cql_get::<String>(row, &col_map, "context_snippet").map_err(|e| {
                    anyhow::anyhow!("required column `context_snippet` read failed: {e}")
                })?;
            let created = cql_get::<chrono::DateTime<chrono::Utc>>(row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            let state = cql_get::<String>(row, &col_map, "state")
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
            ) = extract_rich_entity_fields(row, &col_map);
            Ok(Some(EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id,
                entity_name,
                entity_type,
                source_fold_id: cql_get::<Uuid>(row, &col_map, "source_fold_id").ok(),
                context_snippet,
                entity_embedding: None,
                confidence: f64::from(cql_get::<f32>(row, &col_map, "confidence").unwrap_or(1.0)),
                state,
                created_at: created,
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

        // Scylla's SerializeRow trait doesn't support dynamic-length parameter lists.
        // Issue individual point lookups and merge — all are primary-key reads, O(1) each.
        let mut results = Vec::new();
        for &eid in entity_ids {
            if let Some(entry) = self.entity_get_by_id(ctx, session_id, eid).await? {
                results.push(entry);
            }
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
        #[allow(deprecated)]
        let ann_result = match self
            .session
            .query_unpaged(query, (ctx.tenant_id, session_id))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "entity ANN query failed");
                return Ok(Vec::new());
            }
        };
        let col_map = build_col_map(ann_result.col_specs());
        let rows = ann_result.rows_or_empty();
        let mut results = Vec::new();
        for row in rows {
            // Skip ghost rows with null required fields (P0 write-loss artifact).
            let Ok(entity_id) = cql_get::<Uuid>(&row, &col_map, "entity_id") else {
                continue;
            };
            let Ok(entity_name) = cql_get::<String>(&row, &col_map, "entity_name") else {
                continue;
            };
            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            let state = cql_get::<String>(&row, &col_map, "state")
                .ok()
                .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
                .unwrap_or_default();
            results.push(EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id,
                entity_name,
                entity_type: cql_get(&row, &col_map, "entity_type").unwrap_or_default(),
                source_fold_id: cql_get::<Uuid>(&row, &col_map, "source_fold_id").ok(),
                context_snippet: cql_get(&row, &col_map, "context_snippet").unwrap_or_default(),
                entity_embedding: None,
                confidence: cql_get::<f32>(&row, &col_map, "confidence")
                    .map(f64::from)
                    .unwrap_or(0.0),
                state,
                created_at: created,
                ..Default::default()
            });
        }
        Ok(results)
    }

    async fn entity_count(&self, ctx: &TenantContext, session_id: Uuid) -> anyhow::Result<usize> {
        // Client-side count: SELECT entity_id returns rows, count them.
        // Workaround for Ferrosa returning COUNT(*) column as "system.count".
        let (_col_map, rows) = self
            .exec_prepared_rows(&self.stmts.entity_count, (ctx.tenant_id, session_id))
            .await?;
        Ok(rows.len())
    }

    async fn fold_count(&self, ctx: &TenantContext, session_id: Uuid) -> anyhow::Result<usize> {
        let (_col_map, rows) = self
            .exec_prepared_rows(&self.stmts.fold_count, (ctx.tenant_id, session_id))
            .await?;
        Ok(rows.len())
    }

    async fn memo_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        let (_col_map, rows) = self
            .exec_prepared_rows(&self.stmts.memo_count, (ctx.tenant_id,))
            .await?;
        Ok(rows.len())
    }

    async fn entity_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.entity_list_session, (ctx.tenant_id, session_id))
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            // Skip rows with NULL required fields (ghost rows from bulk loads).
            let Ok(entity_id) = cql_get::<Uuid>(&row, &col_map, "entity_id") else {
                continue;
            };
            let Ok(entity_name) = cql_get::<String>(&row, &col_map, "entity_name") else {
                continue;
            };
            let Ok(entity_type) = cql_get::<String>(&row, &col_map, "entity_type") else {
                continue;
            };
            let context_snippet =
                cql_get::<String>(&row, &col_map, "context_snippet").map_err(|e| {
                    anyhow::anyhow!("required column `context_snippet` read failed: {e}")
                })?;
            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            let state = cql_get::<String>(&row, &col_map, "state")
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
            ) = extract_rich_entity_fields(&row, &col_map);
            results.push(EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id,
                entity_name,
                entity_type,
                source_fold_id: cql_get::<Uuid>(&row, &col_map, "source_fold_id").ok(),
                context_snippet,
                entity_embedding: cql_get::<Vec<u8>>(&row, &col_map, "entity_embedding")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|v| crate::vector::decode_vector(&v)),
                confidence: cql_get::<f32>(&row, &col_map, "confidence")
                    .map(f64::from)
                    .unwrap_or(1.0),
                state,
                created_at: created,
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

    async fn entity_counts_by_type_and_state(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<EntityTypeStateCount>> {
        let query = format!(
            "SELECT entity_type, state FROM {}.entity_store WHERE tenant_id = ? AND session_id = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, session_id))
            .await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        let mut counts: std::collections::BTreeMap<(String, String), usize> =
            std::collections::BTreeMap::new();
        for row in rows {
            let entity_type = cql_get::<String>(&row, &col_map, "entity_type").unwrap_or_default();
            let state = cql_get::<String>(&row, &col_map, "state")
                .unwrap_or_else(|_| MemoryState::default().to_string());
            *counts.entry((entity_type, state)).or_insert(0) += 1;
        }
        Ok(counts
            .into_iter()
            .map(|((entity_type, state), count)| EntityTypeStateCount {
                entity_type,
                state: serde_json::from_str(&format!("\"{state}\""))
                    .unwrap_or_else(|_| MemoryState::default()),
                count,
            })
            .collect())
    }

    async fn entity_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<EntityEntry>> {
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.entity_list_all, (ctx.tenant_id,))
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            let state = cql_get::<String>(&row, &col_map, "state")
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
            ) = extract_rich_entity_fields(&row, &col_map);
            results.push(EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id: cql_get(&row, &col_map, "entity_id")?,
                session_id: cql_get(&row, &col_map, "session_id")?,
                entity_name: cql_get(&row, &col_map, "entity_name")?,
                entity_type: cql_get(&row, &col_map, "entity_type")?,
                source_fold_id: cql_get::<Uuid>(&row, &col_map, "source_fold_id").ok(),
                context_snippet: cql_get(&row, &col_map, "context_snippet")?,
                entity_embedding: cql_get::<Vec<u8>>(&row, &col_map, "entity_embedding")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|v| crate::vector::decode_vector(&v)),
                confidence: cql_get::<f32>(&row, &col_map, "confidence")
                    .map(f64::from)
                    .unwrap_or(1.0),
                state,
                created_at: created,
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
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.fold_list_all, (ctx.tenant_id,))
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let status_str: String = cql_get(&row, &col_map, "status").unwrap_or_default();
            let status: FoldStatus =
                serde_json::from_str(&format!("\"{status_str}\"")).unwrap_or(FoldStatus::Active);
            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            results.push(FoldEntry {
                session_id: cql_get(&row, &col_map, "session_id")?,
                fold_id: cql_get(&row, &col_map, "fold_id")?,
                tenant_id: ctx.tenant_id,
                depth: cql_get(&row, &col_map, "depth")?,
                parent_fold_id: cql_get::<Uuid>(&row, &col_map, "parent_fold_id").ok(),
                raw_trajectory: cql_get(&row, &col_map, "raw_trajectory").unwrap_or_default(),
                fold_summary: cql_get::<String>(&row, &col_map, "fold_summary").ok(),
                fold_embedding: cql_get::<Vec<u8>>(&row, &col_map, "fold_embedding")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(|v| crate::vector::decode_vector(&v)),
                token_count: cql_get(&row, &col_map, "token_count")?,
                compression_ratio: cql_get::<f64>(&row, &col_map, "compression_ratio").ok(),
                status,
                created_at: created,
                folded_at: cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "folded_at")
                    .ok(),
            });
        }
        Ok(results)
    }

    async fn temporal_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<TemporalEvent>> {
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.temporal_list_all, (ctx.tenant_id,))
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let event_time: chrono::DateTime<chrono::Utc> = cql_get(&row, &col_map, "event_time")?;
            results.push(TemporalEvent {
                tenant_id: ctx.tenant_id,
                entity_id: cql_get(&row, &col_map, "entity_id")?,
                event_time,
                event_id: cql_get(&row, &col_map, "event_id")?,
                fact_text: cql_get(&row, &col_map, "fact_text")?,
                supersedes_id: cql_get::<Uuid>(&row, &col_map, "supersedes_id").ok(),
                valid_until: cql_get::<chrono::DateTime<chrono::Utc>>(
                    &row,
                    &col_map,
                    "valid_until",
                )
                .ok(),
                source_session: cql_get(&row, &col_map, "source_session")?,
                confidence: f64::from(cql_get::<f32>(&row, &col_map, "confidence").unwrap_or(1.0)),
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
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, entity_id))
            .await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("entity not found: {entity_id}"))?;
        let session_id: Uuid = cql_get(&row, &col_map, "session_id")?;

        self.session
            .execute_unpaged(
                &self.stmts.entity_update_state,
                (state_str, ctx.tenant_id, session_id, entity_id),
            )
            .await?;
        Ok(())
    }

    async fn entity_delete(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        entity_id: Uuid,
    ) -> anyhow::Result<bool> {
        let exists = self
            .entity_get_by_id(ctx, session_id, entity_id)
            .await?
            .is_some();
        if !exists {
            return Ok(false);
        }
        let query = format!(
            "DELETE FROM {}.entity_store WHERE tenant_id = ? AND session_id = ? AND entity_id = ?",
            self.keyspace
        );
        self.session
            .query_unpaged(query, (ctx.tenant_id, session_id, entity_id))
            .await?;
        Ok(true)
    }

    // --- Temporal operations ---

    async fn temporal_put(&self, ctx: &TenantContext, event: &TemporalEvent) -> anyhow::Result<()> {
        self.session
            .execute_unpaged(
                &self.stmts.temporal_put,
                (
                    ctx.tenant_id,
                    event.entity_id,
                    event.event_time,
                    event.event_id,
                    event.fact_text.clone(),
                    event.supersedes_id,
                    event.valid_until,
                    event.source_session,
                    event.confidence as f32,
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
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.temporal_get_current, (ctx.tenant_id, entity_id))
            .await?;

        // Filter for valid_until IS NULL (current facts) — CQL can't filter on NULL
        for row in rows {
            if cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "valid_until").is_err() {
                // NULL means this is the current fact
                let event_time: chrono::DateTime<chrono::Utc> =
                    cql_get(&row, &col_map, "event_time")?;
                return Ok(Some(TemporalEvent {
                    tenant_id: ctx.tenant_id,
                    entity_id,
                    event_time,
                    event_id: cql_get(&row, &col_map, "event_id")?,
                    fact_text: cql_get(&row, &col_map, "fact_text")?,
                    supersedes_id: cql_get::<Uuid>(&row, &col_map, "supersedes_id").ok(),
                    valid_until: None,
                    source_session: cql_get(&row, &col_map, "source_session")?,
                    confidence: f64::from(cql_get::<f32>(&row, &col_map, "confidence")?),
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
                .execute_unpaged(
                    &self.stmts.temporal_invalidate,
                    (
                        chrono::Utc::now(),
                        ctx.tenant_id,
                        entity_id,
                        event.event_time,
                        event_id,
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
            .execute_unpaged(
                &self.stmts.feedback_put,
                (
                    ctx.tenant_id,
                    outcome.session_id,
                    outcome.query_id,
                    outcome.program_type.clone(),
                    outcome.task_complexity.clone(),
                    outcome.succeeded,
                    outcome.latency_ms,
                    outcome.token_cost,
                    chrono::Utc::now(),
                ),
            )
            .await?;
        Ok(())
    }

    async fn feedback_list_all(&self) -> anyhow::Result<Vec<FeedbackOutcome>> {
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.feedback_list_all, ())
            .await?;

        let mut outcomes = Vec::with_capacity(rows.len());
        for row in &rows {
            let tenant_id: Uuid = cql_get(row, &col_map, "tenant_id")?;
            let session_id: Uuid = cql_get(row, &col_map, "session_id")?;
            let query_id: Uuid = cql_get(row, &col_map, "query_id")?;
            let program_type: String = cql_get(row, &col_map, "program_type")?;
            let task_complexity: String = cql_get(row, &col_map, "task_complexity")?;
            let succeeded: bool = cql_get(row, &col_map, "succeeded")?;
            let latency_ms: i32 = cql_get(row, &col_map, "latency_ms")?;
            let token_cost: i32 = cql_get(row, &col_map, "token_cost")?;
            let created_at: chrono::DateTime<chrono::Utc> = cql_get(row, &col_map, "created_at")?;

            outcomes.push(FeedbackOutcome {
                tenant_id,
                session_id,
                query_id,
                program_type,
                task_complexity,
                succeeded,
                latency_ms,
                token_cost,
                created_at,
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
        #[allow(deprecated)]
        let result = self.session.query_unpaged(query, (ctx.tenant_id,)).await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        let mut total: i64 = 0;
        for row in &rows {
            let hits: i64 = cql_get(row, &col_map, "hit_count").unwrap_or(0);
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
        #[allow(deprecated)]
        let result = self.session.query_unpaged(query, (ctx.tenant_id,)).await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        let count = rows
            .iter()
            .filter(|r| {
                cql_get::<String>(r, &col_map, "status")
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
        #[allow(deprecated)]
        let result = self.session.query_unpaged(query, (ctx.tenant_id,)).await?;
        let _col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        Ok(rows.len())
    }

    async fn edge_count(&self, ctx: &TenantContext) -> anyhow::Result<usize> {
        let mut total = 0usize;
        for table in [
            "co_occurs_with",
            "mentioned_in",
            "folded_into",
            "supersedes",
            "typed_edges",
        ] {
            total += self.query_count_allow_filtering(table, ctx).await?;
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
                .query_unpaged(query.as_str(), (session_id, ctx.tenant_id))
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
        _ctx: &TenantContext,
        _source_fold_id: Uuid,
        _target_fold_id: Uuid,
        _session_id: Uuid,
    ) -> anyhow::Result<()> {
        Err(Self::graph_write_error("edge_folded_into"))
    }

    async fn edge_mentioned_in(
        &self,
        _ctx: &TenantContext,
        _entity_id: Uuid,
        _fold_id: Uuid,
        _session_id: Uuid,
    ) -> anyhow::Result<()> {
        Err(Self::graph_write_error("edge_mentioned_in"))
    }

    async fn edge_co_occurs(
        &self,
        _ctx: &TenantContext,
        _entity_a: Uuid,
        _entity_b: Uuid,
        _session_id: Uuid,
        _strength: f32,
    ) -> anyhow::Result<()> {
        Err(Self::graph_write_error("edge_co_occurs"))
    }

    async fn edge_supersedes(
        &self,
        _ctx: &TenantContext,
        _new_event_id: Uuid,
        _old_event_id: Uuid,
        _entity_id: Uuid,
    ) -> anyhow::Result<()> {
        Err(Self::graph_write_error("edge_supersedes"))
    }

    async fn edge_prune_stale(
        &self,
        _ctx: &TenantContext,
        _cutoff: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<usize> {
        Err(Self::graph_write_error("edge_prune_stale"))
    }

    async fn edge_decay_weights(
        &self,
        _ctx: &TenantContext,
        _factor: f64,
    ) -> anyhow::Result<usize> {
        Err(Self::graph_write_error("edge_decay_weights"))
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
                "SELECT {src_col}, {tgt_col}, tenant_id, session_id FROM {}.{table}",
                self.keyspace
            );
            match self.session.prepare(query).await {
                Ok(prepared) => match self.exec_prepared_rows(&prepared, ()).await {
                    Ok((col_map, rows)) => {
                        for row in rows {
                            let row_tenant = cql_get::<Uuid>(&row, &col_map, "tenant_id");
                            let row_session = cql_get::<Uuid>(&row, &col_map, "session_id");
                            if row_tenant.as_ref().ok() != Some(&ctx.tenant_id)
                                || row_session.as_ref().ok() != Some(&session_id)
                            {
                                continue;
                            }
                            if let (Ok(src), Ok(tgt)) = (
                                cql_get::<Uuid>(&row, &col_map, src_col),
                                cql_get::<Uuid>(&row, &col_map, tgt_col),
                            ) {
                                edges.push((src, tgt, label.to_string()));
                            }
                        }
                    }
                    Err(e) => tracing::warn!(table, error = %e, "edge query failed"),
                },
                Err(e) => tracing::warn!(table, error = %e, "edge query prepare failed"),
            }
        }

        // SUPERSEDES edges (not session-scoped, return all for tenant)
        let query = format!(
            "SELECT new_event_id, old_event_id FROM {}.supersedes \
             WHERE tenant_id = ? ALLOW FILTERING",
            self.keyspace
        );
        #[allow(deprecated)]
        if let Ok(prepared) = self.session.prepare(query).await {
            #[allow(deprecated)]
            if let Ok(result) = self
                .session
                .execute_unpaged(&prepared, (ctx.tenant_id,))
                .await
            {
                let sup_col_map = build_col_map(result.col_specs());
                let rows = result.rows_or_empty();
                for row in rows {
                    if let (Ok(src), Ok(tgt)) = (
                        cql_get::<Uuid>(&row, &sup_col_map, "new_event_id"),
                        cql_get::<Uuid>(&row, &sup_col_map, "old_event_id"),
                    ) {
                        edges.push((src, tgt, "SUPERSEDES".into()));
                    }
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
            #[allow(deprecated)]
            if let Ok(prepared) = self.session.prepare(query).await {
                #[allow(deprecated)]
                if let Ok(result) = self
                    .session
                    .execute_unpaged(&prepared, (ctx.tenant_id,))
                    .await
                {
                    let ea_col_map = build_col_map(result.col_specs());
                    let rows = result.rows_or_empty();
                    for row in rows {
                        if let (Ok(a), Ok(b)) = (
                            cql_get::<Uuid>(&row, &ea_col_map, src_col),
                            cql_get::<Uuid>(&row, &ea_col_map, tgt_col),
                        ) {
                            edges.push((a, b, edge_type.into()));
                        }
                    }
                } // end if execute_unpaged
            } // end if prepare
        } // end for

        Ok(edges)
    }

    async fn edge_list_for_entity(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> anyhow::Result<Vec<(Uuid, String)>> {
        let mut neighbors = Vec::new();

        // MENTIONED_IN edges (entity -> fold)
        let (col_map, rows) = self
            .exec_prepared_rows(
                &self.stmts.edge_mentioned_in_by_entity,
                (entity_id, ctx.tenant_id),
            )
            .await?;
        for row in rows {
            let fold_id: Uuid = cql_get(&row, &col_map, "fold_id")?;
            neighbors.push((fold_id, "MENTIONED_IN".into()));
        }

        // CO_OCCURS_WITH edges (entity as entity_a)
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.edge_co_occurs_by_a, (entity_id, ctx.tenant_id))
            .await?;
        for row in rows {
            let other: Uuid = cql_get(&row, &col_map, "entity_b")?;
            neighbors.push((other, "CO_OCCURS".into()));
        }

        // CO_OCCURS_WITH edges (entity as entity_b)
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.edge_co_occurs_by_b, (entity_id, ctx.tenant_id))
            .await?;
        for row in rows {
            let other: Uuid = cql_get(&row, &col_map, "entity_a")?;
            neighbors.push((other, "CO_OCCURS".into()));
        }

        // SUPERSEDES edges (entity as new_event_id)
        let (col_map, rows) = self
            .exec_prepared_rows(
                &self.stmts.edge_supersedes_by_new,
                (entity_id, ctx.tenant_id),
            )
            .await?;
        for row in rows {
            let old: Uuid = cql_get(&row, &col_map, "old_event_id")?;
            neighbors.push((old, "SUPERSEDES".into()));
        }

        // SUPERSEDES edges (entity as old_event_id)
        let (col_map, rows) = self
            .exec_prepared_rows(
                &self.stmts.edge_supersedes_by_old,
                (entity_id, ctx.tenant_id),
            )
            .await?;
        for row in rows {
            let new_id: Uuid = cql_get(&row, &col_map, "new_event_id")?;
            neighbors.push((new_id, "SUPERSEDES".into()));
        }

        // Typed edges (contains, references, calls, depends_on, etc.)
        // Query the nil session (used by frg ingest) and the default session.
        let nil_session = Uuid::nil();
        let query = format!(
            "SELECT src_id, edge_type, dst_id FROM {}.typed_edges \
             WHERE tenant_id = ? AND session_id = ?",
            self.keyspace
        );
        let prepared = self.session.prepare(query).await?;
        let (col_map, rows) = self
            .exec_prepared_rows(&prepared, (ctx.tenant_id, nil_session))
            .await
            .unwrap_or_default();
        for row in rows {
            let src: Uuid = match cql_get(&row, &col_map, "src_id") {
                Ok(v) => v,
                Err(_) => continue,
            };
            let dst: Uuid = match cql_get(&row, &col_map, "dst_id") {
                Ok(v) => v,
                Err(_) => continue,
            };
            let edge_type: String = cql_get(&row, &col_map, "edge_type").unwrap_or_default();
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
            .execute_unpaged(
                &self.stmts.intention_put,
                (
                    ctx.tenant_id,
                    intention.repo.clone(),
                    intention.id,
                    intention.description.clone(),
                    trigger_json,
                    priority_str,
                    status_str,
                    intention.created_at,
                    intention.triggered_at,
                    intention.completed_at,
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
        let (col_map, rows) = self
            .exec_prepared_rows(
                &self.stmts.intention_list,
                (ctx.tenant_id, repo.to_string()),
            )
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let trigger_json: String = cql_get(&row, &col_map, "trigger_json")?;
            let trigger: crate::intention::IntentionTrigger = serde_json::from_str(&trigger_json)?;

            let priority_str: String = cql_get(&row, &col_map, "priority")?;
            let priority: crate::intention::Priority =
                serde_json::from_str(&format!("\"{priority_str}\""))
                    .unwrap_or(crate::intention::Priority::Normal);

            let status_str: String = cql_get(&row, &col_map, "status")?;
            let status: crate::intention::IntentionStatus =
                serde_json::from_str(&format!("\"{status_str}\""))
                    .unwrap_or(crate::intention::IntentionStatus::Pending);

            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });

            let repo_val: String =
                cql_get(&row, &col_map, "repo").unwrap_or_else(|_| repo.to_string());

            results.push(crate::intention::Intention {
                id: cql_get(&row, &col_map, "intention_id")?,
                repo: repo_val,
                description: cql_get(&row, &col_map, "description")?,
                trigger,
                priority,
                status,
                created_at: created,
                triggered_at: cql_get::<chrono::DateTime<chrono::Utc>>(
                    &row,
                    &col_map,
                    "triggered_at",
                )
                .ok(),
                completed_at: cql_get::<chrono::DateTime<chrono::Utc>>(
                    &row,
                    &col_map,
                    "completed_at",
                )
                .ok(),
            });
        }
        Ok(results)
    }

    async fn intention_list_all(
        &self,
        ctx: &TenantContext,
    ) -> anyhow::Result<Vec<crate::intention::Intention>> {
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.intention_list_all, (ctx.tenant_id,))
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let trigger_json: String = cql_get(&row, &col_map, "trigger_json")?;
            let trigger: crate::intention::IntentionTrigger = serde_json::from_str(&trigger_json)?;

            let priority_str: String = cql_get(&row, &col_map, "priority")?;
            let priority: crate::intention::Priority =
                serde_json::from_str(&format!("\"{priority_str}\""))
                    .unwrap_or(crate::intention::Priority::Normal);

            let status_str: String = cql_get(&row, &col_map, "status")?;
            let status: crate::intention::IntentionStatus =
                serde_json::from_str(&format!("\"{status_str}\""))
                    .unwrap_or(crate::intention::IntentionStatus::Pending);

            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });

            let repo_val: String = cql_get(&row, &col_map, "repo").unwrap_or_default();

            results.push(crate::intention::Intention {
                id: cql_get(&row, &col_map, "intention_id")?,
                repo: repo_val,
                description: cql_get(&row, &col_map, "description")?,
                trigger,
                priority,
                status,
                created_at: created,
                triggered_at: cql_get::<chrono::DateTime<chrono::Utc>>(
                    &row,
                    &col_map,
                    "triggered_at",
                )
                .ok(),
                completed_at: cql_get::<chrono::DateTime<chrono::Utc>>(
                    &row,
                    &col_map,
                    "completed_at",
                )
                .ok(),
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
            .execute_unpaged(
                &self.stmts.intention_update_status,
                (
                    status.to_string(),
                    triggered_at,
                    completed_at,
                    ctx.tenant_id,
                    repo.to_string(),
                    id,
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
            .execute_unpaged(
                &self.stmts.tool_usage_put,
                (
                    ctx.tenant_id,
                    today,
                    tool_name.to_string(),
                    repo.to_string(),
                    input_bytes,
                    output_bytes,
                    estimated_tokens,
                    latency_ms,
                    error,
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
        let (col_map, rows) = self
            .exec_prepared_rows(
                &self.stmts.tool_usage_query,
                (ctx.tenant_id, day.to_string()),
            )
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            results.push(crate::types::ToolUsageRow {
                tool_name: cql_get(&row, &col_map, "tool_name")?,
                repo: cql_get::<String>(&row, &col_map, "repo").unwrap_or_default(),
                input_bytes: cql_get(&row, &col_map, "input_bytes")?,
                output_bytes: cql_get(&row, &col_map, "output_bytes")?,
                estimated_tokens: cql_get(&row, &col_map, "estimated_tokens")?,
                latency_ms: cql_get(&row, &col_map, "latency_ms")?,
                error: cql_get(&row, &col_map, "error")?,
                created_at: cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                    .unwrap_or_else(|_| chrono::Utc::now()),
            });
        }
        Ok(results)
    }

    // --- Audit log operations ---

    async fn audit_put(&self, ctx: &TenantContext, entry: &AuditEntry) -> anyhow::Result<()> {
        self.session
            .execute_unpaged(
                &self.stmts.audit_put,
                (
                    ctx.tenant_id,
                    entry.audit_id,
                    entry.operation.clone(),
                    entry.target_table.clone(),
                    entry.target_id.clone(),
                    entry.session_id,
                    entry.created_at,
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
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.warmth_get, (ctx.tenant_id, entity_id))
            .await?;

        if let Some(row) = rows.into_iter().next() {
            let last_accessed = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "last_accessed_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "last_accessed_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            let updated = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "updated_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "updated_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            let zone_str: String = cql_get(&row, &col_map, "decay_zone").unwrap_or_default();

            Ok(Some(WarmthEntry {
                tenant_id: ctx.tenant_id,
                entity_id,
                session_id: cql_get(&row, &col_map, "session_id")?,
                warmth: cql_get(&row, &col_map, "warmth")?,
                pagerank: cql_get::<f64>(&row, &col_map, "pagerank").unwrap_or(0.0),
                reputation: cql_get::<f64>(&row, &col_map, "reputation").unwrap_or(0.0),
                last_accessed_at: last_accessed,
                access_count: i64::from(
                    cql_get::<i32>(&row, &col_map, "access_count").unwrap_or(0),
                ),
                decay_zone: parse_decay_zone(&zone_str),
                updated_at: updated,
            }))
        } else {
            Ok(None)
        }
    }

    async fn warmth_put(&self, ctx: &TenantContext, entry: &WarmthEntry) -> anyhow::Result<()> {
        self.session
            .execute_unpaged(
                &self.stmts.warmth_put,
                (
                    ctx.tenant_id,
                    entry.entity_id,
                    entry.session_id,
                    entry.warmth,
                    entry.pagerank,
                    entry.reputation,
                    entry.last_accessed_at,
                    entry.access_count as i32,
                    entry.decay_zone.to_string(),
                    entry.updated_at,
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
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.warmth_list_session, (session_id,))
            .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in rows {
            let last_accessed = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "last_accessed_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "last_accessed_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            let updated = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "updated_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "updated_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            let zone_str: String = cql_get(&row, &col_map, "decay_zone").unwrap_or_default();

            results.push(WarmthEntry {
                tenant_id: ctx.tenant_id,
                entity_id: cql_get(&row, &col_map, "entity_id")?,
                session_id,
                warmth: cql_get(&row, &col_map, "warmth")?,
                pagerank: cql_get::<f64>(&row, &col_map, "pagerank").unwrap_or(0.0),
                reputation: cql_get::<f64>(&row, &col_map, "reputation").unwrap_or(0.0),
                last_accessed_at: last_accessed,
                access_count: i64::from(
                    cql_get::<i32>(&row, &col_map, "access_count").unwrap_or(0),
                ),
                decay_zone: parse_decay_zone(&zone_str),
                updated_at: updated,
            });
        }
        Ok(results)
    }

    async fn warmth_delete(&self, ctx: &TenantContext, entity_id: Uuid) -> anyhow::Result<()> {
        self.session
            .execute_unpaged(&self.stmts.warmth_delete, (ctx.tenant_id, entity_id))
            .await?;
        Ok(())
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
                    .execute_unpaged(&self.stmts.warmth_delete, (ctx.tenant_id, entry.entity_id))
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
        let now = chrono::Utc::now();

        // Denormalized write: rules_by_id
        self.session
            .execute_unpaged(
                &self.stmts.rule_put_by_id,
                (
                    ctx.tenant_id,
                    entry.rule_id.clone(),
                    entry.version,
                    entry.name.clone(),
                    entry.family.clone(),
                    state_str.clone(),
                    entry.rule_body.clone(),
                    entry.rule_weight,
                    entry.incremental,
                    entry.created_at,
                    now,
                ),
            )
            .await?;

        // Denormalized write: rules_by_family
        self.session
            .execute_unpaged(
                &self.stmts.rule_put_by_family,
                (
                    ctx.tenant_id,
                    entry.family.clone(),
                    state_str.clone(),
                    entry.rule_id.clone(),
                    entry.version,
                    now,
                ),
            )
            .await?;
        self.session
            .execute_unpaged(
                &self.stmts.rule_put_active_by_state,
                (
                    ctx.tenant_id,
                    state_str,
                    entry.family.clone(),
                    entry.rule_id.clone(),
                    entry.version,
                    now,
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
        let (col_map, rows) = self
            .exec_prepared_rows(
                &self.stmts.rule_list_family,
                (ctx.tenant_id, family.to_string(), state_str.clone()),
            )
            .await?;

        let mut results = Vec::new();
        for row in rows {
            let rule_id: String = cql_get(&row, &col_map, "rule_id")?;
            let version: i32 = cql_get(&row, &col_map, "version")?;
            let (full_col_map, full_rows) = self
                .exec_prepared_rows(
                    &self.stmts.rule_get_version,
                    (ctx.tenant_id, rule_id, version),
                )
                .await?;
            if let Some(full_row) = full_rows.into_iter().next() {
                results.push(rule_entry_from_row(ctx, &full_row, &full_col_map)?);
            }
        }

        results.sort_by_key(|r| std::cmp::Reverse(r.version));
        Ok(results)
    }

    async fn rule_list_active(
        &self,
        ctx: &TenantContext,
        state: RuleState,
    ) -> anyhow::Result<Vec<RuleEntry>> {
        let state_str = state.to_string();
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.rule_list_active, (ctx.tenant_id, state_str))
            .await?;

        let mut results = Vec::new();
        for row in rows {
            let rule_id: String = cql_get(&row, &col_map, "rule_id")?;
            let version: i32 = cql_get(&row, &col_map, "version")?;
            let (full_col_map, full_rows) = self
                .exec_prepared_rows(
                    &self.stmts.rule_get_version,
                    (ctx.tenant_id, rule_id, version),
                )
                .await?;
            if let Some(full_row) = full_rows.into_iter().next() {
                results.push(rule_entry_from_row(ctx, &full_row, &full_col_map)?);
            }
        }

        results.sort_by(|a, b| {
            a.family
                .cmp(&b.family)
                .then_with(|| b.version.cmp(&a.version))
                .then_with(|| a.rule_id.cmp(&b.rule_id))
        });
        Ok(results)
    }

    async fn rule_get(
        &self,
        ctx: &TenantContext,
        rule_id: &str,
    ) -> anyhow::Result<Option<RuleEntry>> {
        let (col_map, rows) = self
            .exec_prepared_rows(&self.stmts.rule_get, (ctx.tenant_id, rule_id.to_string()))
            .await?;

        if let Some(row) = rows.into_iter().next() {
            Ok(Some(rule_entry_from_row(ctx, &row, &col_map)?))
        } else {
            Ok(None)
        }
    }

    async fn approval_append(
        &self,
        ctx: &TenantContext,
        entry: &ApprovalEntry,
    ) -> anyhow::Result<()> {
        let query = format!(
            "INSERT INTO {}.approvals_by_target \
             (tenant_id, artifact_kind, artifact_ref, created_at, approval_id, decision, review_note, reviewer, scope, workspace_scope, session_scope, mirror_entity_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            self.keyspace
        );
        self.session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    entry.artifact_kind.to_string(),
                    entry.artifact_ref.clone(),
                    entry.created_at,
                    entry.approval_id,
                    entry.decision.to_string(),
                    entry.review_note.clone().unwrap_or_default(),
                    entry.reviewer.clone(),
                    entry.scope.clone(),
                    entry.workspace_scope.clone().unwrap_or_default(),
                    entry.session_scope,
                    entry.mirror_entity_id,
                ),
            )
            .await?;
        Ok(())
    }

    async fn approval_list(
        &self,
        ctx: &TenantContext,
        artifact_kind: &str,
        artifact_ref: &str,
    ) -> anyhow::Result<Vec<ApprovalEntry>> {
        let query = format!(
            "SELECT artifact_kind, artifact_ref, created_at, approval_id, decision, review_note, reviewer, scope, workspace_scope, session_scope, mirror_entity_id \
             FROM {}.approvals_by_target WHERE tenant_id = ? AND artifact_kind = ? AND artifact_ref = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let _qr = self
            .session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    artifact_kind.to_string(),
                    artifact_ref.to_string(),
                ),
            )
            .await?;
        let col_map = build_col_map(_qr.col_specs());
        let rows = _qr.rows_or_empty();

        let mut results = Vec::new();
        for row in rows {
            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_default();
            results.push(ApprovalEntry {
                tenant_id: ctx.tenant_id,
                approval_id: cql_get(&row, &col_map, "approval_id")?,
                artifact_kind: crate::expert_system::parse_artifact_kind(
                    &cql_get::<String>(&row, &col_map, "artifact_kind").unwrap_or_default(),
                )?,
                artifact_ref: cql_get(&row, &col_map, "artifact_ref")?,
                decision: parse_approval_decision(
                    &cql_get::<String>(&row, &col_map, "decision").unwrap_or_default(),
                ),
                review_note: cql_get::<String>(&row, &col_map, "review_note")
                    .ok()
                    .filter(|value| !value.is_empty()),
                reviewer: cql_get(&row, &col_map, "reviewer")?,
                scope: cql_get(&row, &col_map, "scope")?,
                workspace_scope: cql_get::<String>(&row, &col_map, "workspace_scope")
                    .ok()
                    .filter(|value| !value.is_empty()),
                session_scope: cql_get::<Uuid>(&row, &col_map, "session_scope").ok(),
                mirror_entity_id: cql_get(&row, &col_map, "mirror_entity_id")?,
                created_at: created,
            });
        }
        results.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.approval_id.cmp(&left.approval_id))
        });
        Ok(results)
    }

    async fn approval_latest(
        &self,
        ctx: &TenantContext,
        artifact_kind: &str,
        artifact_ref: &str,
    ) -> anyhow::Result<Option<ApprovalEntry>> {
        Ok(self
            .approval_list(ctx, artifact_kind, artifact_ref)
            .await?
            .into_iter()
            .next())
    }

    async fn alias_put(&self, ctx: &TenantContext, entry: &AliasEntry) -> anyhow::Result<()> {
        let query = format!(
            "INSERT INTO {}.aliases_by_name \
             (tenant_id, alias_name, scope_kind, scope_ref, alias_id, canonical_tool, parameter_map, fixed_arguments, args_templates, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            self.keyspace
        );
        self.session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    entry.alias_name.clone(),
                    entry.scope_kind.to_string(),
                    entry.scope_ref.clone(),
                    entry.alias_id,
                    entry.canonical_tool.clone(),
                    entry.parameter_map.to_string(),
                    entry.fixed_arguments.to_string(),
                    entry.args_templates.to_string(),
                    entry.status.to_string(),
                    entry.created_at,
                    entry.updated_at,
                ),
            )
            .await?;
        Ok(())
    }

    async fn alias_list(
        &self,
        ctx: &TenantContext,
        alias_name: &str,
    ) -> anyhow::Result<Vec<AliasEntry>> {
        let query = format!(
            "SELECT alias_name, scope_kind, scope_ref, alias_id, canonical_tool, parameter_map, fixed_arguments, args_templates, status, created_at, updated_at \
             FROM {}.aliases_by_name WHERE tenant_id = ? AND alias_name = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let _qr = self
            .session
            .query_unpaged(query, (ctx.tenant_id, alias_name.to_string()))
            .await?;
        let col_map = build_col_map(_qr.col_specs());
        let rows = _qr.rows_or_empty();

        let mut results = Vec::new();
        for row in rows {
            results.push(AliasEntry {
                tenant_id: ctx.tenant_id,
                alias_id: cql_get(&row, &col_map, "alias_id")?,
                alias_name: cql_get(&row, &col_map, "alias_name")?,
                scope_kind: parse_alias_scope_kind(
                    &cql_get::<String>(&row, &col_map, "scope_kind").unwrap_or_default(),
                ),
                scope_ref: cql_get(&row, &col_map, "scope_ref")?,
                canonical_tool: cql_get(&row, &col_map, "canonical_tool")?,
                parameter_map: cql_get::<String>(&row, &col_map, "parameter_map")
                    .ok()
                    .and_then(|value| serde_json::from_str(&value).ok())
                    .unwrap_or_else(|| json!({})),
                fixed_arguments: cql_get::<String>(&row, &col_map, "fixed_arguments")
                    .ok()
                    .and_then(|value| serde_json::from_str(&value).ok())
                    .unwrap_or_else(|| json!({})),
                args_templates: cql_get::<String>(&row, &col_map, "args_templates")
                    .ok()
                    .and_then(|value| serde_json::from_str(&value).ok())
                    .unwrap_or_else(|| json!({})),
                status: parse_claim_status(
                    &cql_get::<String>(&row, &col_map, "status").unwrap_or_default(),
                ),
                created_at: cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                    .unwrap_or_default(),
                updated_at: cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "updated_at")
                    .unwrap_or_default(),
            });
        }
        results.sort_by_key(|row| std::cmp::Reverse(row.updated_at));
        Ok(results)
    }

    // --- Derived cache operations (Sprint 5) ---

    async fn derived_cache_get(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
    ) -> anyhow::Result<Vec<DerivedFact>> {
        let (col_map, rows) = self
            .exec_prepared_rows(
                &self.stmts.derived_cache_get,
                (ctx.tenant_id, cache_key.to_string()),
            )
            .await?;

        let mut facts = Vec::with_capacity(rows.len());
        for row in rows {
            let src_id: Uuid = cql_get(&row, &col_map, "src_id")?;
            let dst_id: Uuid = cql_get(&row, &col_map, "dst_id")?;

            facts.push(DerivedFact {
                src_id: src_id.to_string(),
                pred: cql_get(&row, &col_map, "pred")?,
                dst_id: dst_id.to_string(),
                confidence: cql_get::<f64>(&row, &col_map, "confidence").unwrap_or(1.0),
                rule_id: cql_get(&row, &col_map, "rule_id").unwrap_or_default(),
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
        let now = chrono::Utc::now();

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
                .execute_unpaged(
                    &self.stmts.derived_cache_put,
                    (
                        ctx.tenant_id,
                        cache_key.to_string(),
                        idx as i32,
                        src_uuid,
                        fact.pred.clone(),
                        dst_uuid,
                        fact.confidence,
                        fact.rule_id.clone(),
                        now,
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
            .execute_unpaged(
                &self.stmts.derived_cache_clear,
                (ctx.tenant_id, pred.to_string()),
            )
            .await?;
        tracing::debug!(pred, "derived cache cleared for key");
        Ok(())
    }

    async fn derived_cache_list_all(
        &self,
        ctx: &TenantContext,
        limit: usize,
    ) -> anyhow::Result<Vec<crate::types::DerivedFactRow>> {
        let query = format!(
            "SELECT cache_key, seq, src_id, pred, dst_id, confidence, rule_id, computed_at \
             FROM {}.derived_cache_by_query \
             WHERE tenant_id = ? LIMIT {} ALLOW FILTERING",
            self.keyspace, limit
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query.clone(), (ctx.tenant_id,))
            .await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();

        let mut results: Vec<crate::types::DerivedFactRow> = Vec::new();
        for row in rows {
            let cache_key: Option<String> = cql_get::<String>(&row, &col_map, "cache_key").ok();
            let _seq: i32 = cql_get(&row, &col_map, "seq").unwrap_or_default();
            let src_id: Uuid = cql_get(&row, &col_map, "src_id").unwrap_or_default();
            let pred: String = cql_get(&row, &col_map, "pred").unwrap_or_default();
            let dst_id: Uuid = cql_get(&row, &col_map, "dst_id").unwrap_or_default();
            let confidence: f64 = cql_get(&row, &col_map, "confidence").unwrap_or_default();
            let rule_id: String = cql_get(&row, &col_map, "rule_id").unwrap_or_default();
            let computed_at: Option<chrono::DateTime<chrono::Utc>> =
                cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "computed_at").ok();

            results.push(crate::types::DerivedFactRow {
                source_id: src_id.to_string(),
                predicate: pred,
                target_id: dst_id.to_string(),
                confidence,
                rule_id,
                cache_key,
                computed_at: computed_at.map(|dt| dt.to_string()).unwrap_or_default(),
            });
        }
        Ok(results)
    }

    // --- TTL tracking (Sprint 6) ---

    async fn derived_cache_ttl_track_put(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
        facts: &[crate::types::TtlTrackEntry],
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now();

        for fact in facts {
            let src_uuid: Uuid = fact.src_id.parse().map_err(|e| {
                anyhow::anyhow!(
                    "ttl_track_put: invalid src_id UUID '{}': {}",
                    fact.src_id,
                    e
                )
            })?;
            let dst_uuid: Uuid = fact.dst_id.parse().map_err(|e| {
                anyhow::anyhow!(
                    "ttl_track_put: invalid dst_id UUID '{}': {}",
                    fact.dst_id,
                    e
                )
            })?;

            self.session
                .execute_unpaged(
                    &self.stmts.derived_cache_ttl_track_put,
                    (
                        ctx.tenant_id,
                        cache_key.to_string(),
                        fact.seq,
                        src_uuid,
                        fact.pred.clone(),
                        dst_uuid,
                        fact.ttl_seconds,
                        fact.rule_id.clone(),
                        now,
                        fact.next_maintenance.clone(),
                    ),
                )
                .await?;
        }
        Ok(())
    }

    async fn derived_cache_ttl_track_get(
        &self,
        ctx: &TenantContext,
        cache_key: &str,
    ) -> anyhow::Result<Vec<(i32, i32)>> {
        let (col_map, rows) = self
            .exec_prepared_rows(
                &self.stmts.derived_cache_ttl_track_get,
                (ctx.tenant_id, cache_key.to_string()),
            )
            .await?;

        let mut entries: Vec<(i32, i32)> = Vec::new();
        for row in rows {
            let seq: i32 = cql_get(&row, &col_map, "seq").unwrap_or_default();
            let ttl_seconds: i32 = cql_get(&row, &col_map, "ttl_seconds").unwrap_or_default();
            entries.push((seq, ttl_seconds));
        }
        Ok(entries)
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
                .execute_unpaged(
                    &self.stmts.provenance_put,
                    (
                        ctx.tenant_id,
                        derived_edge_id.to_string(),
                        idx as i32,
                        step.parent_src.clone(),
                        step.parent_pred.clone(),
                        step.parent_dst.clone(),
                        step.parent_kind.clone(),
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
        let (col_map, rows) = self
            .exec_prepared_rows(
                &self.stmts.provenance_get,
                (ctx.tenant_id, derived_edge_id.to_string()),
            )
            .await?;

        let mut steps = Vec::with_capacity(rows.len());
        for row in rows {
            steps.push(ProvenanceStep {
                parent_src: cql_get(&row, &col_map, "parent_src")?,
                parent_pred: cql_get(&row, &col_map, "parent_pred")?,
                parent_dst: cql_get(&row, &col_map, "parent_dst")?,
                parent_kind: cql_get(&row, &col_map, "parent_kind")?,
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

    async fn typed_edge_put(&self, _ctx: &TenantContext, _edge: &TypedEdge) -> anyhow::Result<()> {
        Err(Self::graph_write_error("typed_edge_put"))
    }

    async fn typed_edge_list_session(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
    ) -> anyhow::Result<Vec<TypedEdge>> {
        let query = format!(
            "SELECT src_id, edge_type, dst_id, weight, metadata, created_at \
             FROM {}.typed_edges \
             WHERE tenant_id = ? AND session_id = ?",
            self.keyspace
        );
        let prepared = self.session.prepare(query).await?;
        #[allow(deprecated)]
        let _qr = self
            .session
            .execute_unpaged(&prepared, (ctx.tenant_id, session_id))
            .await?;
        let col_map = build_col_map(_qr.col_specs());
        let rows = _qr.rows_or_empty();

        let mut edges = Vec::new();
        for row in rows {
            // Skip ghost rows with NULL required fields.
            let Ok(src_id) = cql_get::<Uuid>(&row, &col_map, "src_id") else {
                continue;
            };
            let Ok(dst_id) = cql_get::<Uuid>(&row, &col_map, "dst_id") else {
                continue;
            };
            let edge_type = cql_get::<String>(&row, &col_map, "edge_type").unwrap_or_default();
            if edge_type.is_empty() {
                continue;
            };
            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            edges.push(TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id,
                edge_type,
                dst_id,
                weight: cql_get::<f64>(&row, &col_map, "weight").unwrap_or(1.0),
                metadata: cql_get::<String>(&row, &col_map, "metadata")
                    .ok()
                    .filter(|s| !s.is_empty()),
                created_at: created,
            });
        }
        Ok(edges)
    }

    async fn typed_edge_list_all(&self, ctx: &TenantContext) -> anyhow::Result<Vec<TypedEdge>> {
        let query = format!(
            "SELECT src_id, edge_type, dst_id, weight, metadata, created_at, session_id \
             FROM {}.typed_edges WHERE tenant_id = ? ALLOW FILTERING",
            self.keyspace
        );
        let prepared = self.session.prepare(query).await?;
        #[allow(deprecated)]
        let _qr = self
            .session
            .execute_unpaged(&prepared, (ctx.tenant_id,))
            .await?;
        let col_map = build_col_map(_qr.col_specs());
        let rows = _qr.rows_or_empty();

        let mut edges = Vec::new();
        for row in rows {
            let Ok(src_id) = cql_get::<Uuid>(&row, &col_map, "src_id") else {
                continue;
            };
            let Ok(dst_id) = cql_get::<Uuid>(&row, &col_map, "dst_id") else {
                continue;
            };
            let edge_type = cql_get::<String>(&row, &col_map, "edge_type").unwrap_or_default();
            if edge_type.is_empty() {
                continue;
            }
            let session_id = cql_get::<Uuid>(&row, &col_map, "session_id").unwrap_or(Uuid::nil());
            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            edges.push(TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id,
                edge_type,
                dst_id,
                weight: cql_get::<f64>(&row, &col_map, "weight").unwrap_or(1.0),
                metadata: cql_get::<String>(&row, &col_map, "metadata")
                    .ok()
                    .filter(|s| !s.is_empty()),
                created_at: created,
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
        let query = format!(
            "SELECT src_id, edge_type, dst_id, weight, metadata, created_at \
             FROM {}.typed_edges \
             WHERE tenant_id = ? AND session_id = ? AND src_id = ?",
            self.keyspace
        );
        let prepared = self.session.prepare(query).await?;
        #[allow(deprecated)]
        let _qr = self
            .session
            .execute_unpaged(&prepared, (ctx.tenant_id, session_id, src_id))
            .await?;
        let col_map = build_col_map(_qr.col_specs());
        let rows = _qr.rows_or_empty();

        let mut edges = Vec::new();
        for row in rows {
            let created = cql_get::<chrono::DateTime<chrono::Utc>>(&row, &col_map, "created_at")
                .unwrap_or_else(|e| {
                    tracing::warn!(col = "created_at", err = %e, "row has null/corrupt timestamp; defaulting to epoch");
                    Default::default()
                });
            edges.push(TypedEdge {
                tenant_id: ctx.tenant_id,
                session_id,
                src_id,
                edge_type: cql_get::<String>(&row, &col_map, "edge_type").unwrap_or_default(),
                dst_id: cql_get(&row, &col_map, "dst_id")?,
                weight: cql_get::<f64>(&row, &col_map, "weight").unwrap_or(1.0),
                metadata: cql_get::<String>(&row, &col_map, "metadata")
                    .ok()
                    .filter(|s| !s.is_empty()),
                created_at: created,
            });
        }
        Ok(edges)
    }

    async fn typed_edge_delete(
        &self,
        _ctx: &TenantContext,
        _session_id: Uuid,
        _src_id: Uuid,
        _edge_type: &str,
        _dst_id: Uuid,
    ) -> anyhow::Result<bool> {
        Err(Self::graph_write_error("typed_edge_delete"))
    }

    async fn context_segment_put(
        &self,
        ctx: &TenantContext,
        segment: &ContextSegment,
    ) -> anyhow::Result<()> {
        let q = format!(
            "INSERT INTO {ks}.context_segments (tenant_id, session_id, segment_id, \
             source_session, source_fold_id, conversation_id, segment_index, start_turn, \
             end_turn, start_time, end_time, segment_text, bm25_text, token_count, \
             content_hash, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            ks = self.keyspace
        );
        self.session
            .query_unpaged(
                q,
                (
                    ctx.tenant_id,
                    segment.session_id,
                    segment.segment_id,
                    segment.source_session,
                    segment.source_fold_id,
                    segment.conversation_id.clone(),
                    segment.segment_index,
                    segment.start_turn,
                    segment.end_turn,
                    segment.start_time,
                    segment.end_time,
                    segment.segment_text.clone(),
                    segment.bm25_text.clone(),
                    segment.token_count,
                    segment.content_hash.clone(),
                    segment.created_at,
                ),
            )
            .await?;

        if segment.segment_summary.is_some()
            || segment.prev_segment_id.is_some()
            || segment.next_segment_id.is_some()
        {
            let q = format!(
                "UPDATE {ks}.context_segments SET segment_summary = ?, \
                 prev_segment_id = ?, next_segment_id = ? \
                 WHERE tenant_id = ? AND session_id = ? AND segment_id = ?",
                ks = self.keyspace
            );
            self.session
                .query_unpaged(
                    q,
                    (
                        segment.segment_summary.clone(),
                        segment.prev_segment_id,
                        segment.next_segment_id,
                        ctx.tenant_id,
                        segment.session_id,
                        segment.segment_id,
                    ),
                )
                .await?;
        }

        if let Some(embedding) = &segment.segment_embedding {
            let vec_literal = render_vector_literal(embedding);
            let q = format!(
                "UPDATE {ks}.context_segments SET segment_embedding = {vec_literal} \
                 WHERE tenant_id = ? AND session_id = ? AND segment_id = ?",
                ks = self.keyspace
            );
            self.session
                .query_unpaged(q, (ctx.tenant_id, segment.session_id, segment.segment_id))
                .await?;
        }

        let terms = tokenize_context_terms(&segment.bm25_text);
        let doc_len = terms.len() as i32;
        let mut counts: HashMap<String, i32> = HashMap::new();
        for term in terms {
            *counts.entry(term).or_insert(0) += 1;
        }
        let q = format!(
            "INSERT INTO {ks}.context_segment_terms \
             (tenant_id, session_id, term, segment_id, tf, doc_len) \
             VALUES (?, ?, ?, ?, ?, ?)",
            ks = self.keyspace
        );
        for (term, tf) in counts {
            self.session
                .query_unpaged(
                    q.clone(),
                    (
                        ctx.tenant_id,
                        segment.session_id,
                        term,
                        segment.segment_id,
                        tf,
                        doc_len,
                    ),
                )
                .await?;
        }
        Ok(())
    }

    async fn context_segment_get(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        segment_id: Uuid,
    ) -> anyhow::Result<Option<ContextSegment>> {
        let q = format!(
            "SELECT * FROM {ks}.context_segments \
             WHERE tenant_id = ? AND session_id = ? AND segment_id = ?",
            ks = self.keyspace
        );
        let result = self
            .session
            .query_unpaged(q, (ctx.tenant_id, session_id, segment_id))
            .await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        rows.first()
            .map(|row| context_segment_from_row(ctx, row, &col_map))
            .transpose()
    }

    async fn context_segment_get_by_hash(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        content_hash: &str,
    ) -> anyhow::Result<Option<ContextSegment>> {
        let q = format!(
            "SELECT * FROM {ks}.context_segments \
             WHERE tenant_id = ? AND session_id = ? AND content_hash = ? \
             LIMIT 1 ALLOW FILTERING",
            ks = self.keyspace
        );
        let result = self
            .session
            .query_unpaged(q, (ctx.tenant_id, session_id, content_hash.to_string()))
            .await?;
        let col_map = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        rows.first()
            .map(|row| context_segment_from_row(ctx, row, &col_map))
            .transpose()
    }

    async fn context_segment_search_bm25(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        query: &str,
        k: usize,
    ) -> anyhow::Result<Vec<ContextSegment>> {
        let mut scores: HashMap<Uuid, i32> = HashMap::new();
        let term_q = format!(
            "SELECT segment_id, tf FROM {ks}.context_segment_terms \
             WHERE tenant_id = ? AND session_id = ? AND term = ?",
            ks = self.keyspace
        );
        for term in tokenize_context_terms(query) {
            let result = self
                .session
                .query_unpaged(term_q.clone(), (ctx.tenant_id, session_id, term))
                .await?;
            let col_map = build_col_map(result.col_specs());
            for row in result.rows_or_empty() {
                let id: Uuid = cql_get(&row, &col_map, "segment_id")?;
                let tf: i32 = cql_get(&row, &col_map, "tf").unwrap_or(1);
                *scores.entry(id).or_insert(0) += tf;
            }
        }
        let mut ranked: Vec<(Uuid, i32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut segments = Vec::new();
        for (segment_id, _) in ranked.into_iter().take(k) {
            if let Some(segment) = self
                .context_segment_get(ctx, session_id, segment_id)
                .await?
            {
                segments.push(segment);
            }
        }
        Ok(segments)
    }

    async fn context_segment_search_ann(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        query_embedding: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<ContextSegment>> {
        let vec_literal = render_vector_literal(query_embedding);
        let q = format!(
            "SELECT * FROM {ks}.context_segments \
             WHERE tenant_id = ? AND session_id = ? \
             ORDER BY segment_embedding ANN OF {vec_literal} LIMIT {k}",
            ks = self.keyspace,
            k = k
        );
        let result = match self
            .session
            .query_unpaged(q, (ctx.tenant_id, session_id))
            .await
        {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(error = %e, "context segment ANN query failed, falling back to session scan");
                let fallback = format!(
                    "SELECT * FROM {ks}.context_segments \
                     WHERE tenant_id = ? AND session_id = ? LIMIT {k}",
                    ks = self.keyspace,
                    k = k
                );
                self.session
                    .query_unpaged(fallback, (ctx.tenant_id, session_id))
                    .await?
            }
        };
        let col_map = build_col_map(result.col_specs());
        result
            .rows_or_empty()
            .into_iter()
            .map(|row| context_segment_from_row(ctx, &row, &col_map))
            .collect()
    }

    async fn temporal_edge_put(
        &self,
        ctx: &TenantContext,
        edge: &TemporalEdge,
    ) -> anyhow::Result<()> {
        let q = format!(
            "INSERT INTO {ks}.temporal_edges \
             (tenant_id, session_id, src_id, edge_type, dst_id, relation_time, ordinal, metadata, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            ks = self.keyspace
        );
        self.session
            .query_unpaged(
                q,
                (
                    ctx.tenant_id,
                    edge.session_id,
                    edge.src_id,
                    edge.edge_type.clone(),
                    edge.dst_id,
                    edge.relation_time,
                    edge.ordinal,
                    edge.metadata.clone(),
                    edge.created_at,
                ),
            )
            .await?;
        Ok(())
    }

    async fn temporal_edge_list_from(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        src_id: Uuid,
        edge_type: &str,
    ) -> anyhow::Result<Vec<TemporalEdge>> {
        let q = format!(
            "SELECT * FROM {ks}.temporal_edges \
             WHERE tenant_id = ? AND session_id = ? AND src_id = ? AND edge_type = ?",
            ks = self.keyspace
        );
        let result = self
            .session
            .query_unpaged(
                q,
                (ctx.tenant_id, session_id, src_id, edge_type.to_string()),
            )
            .await?;
        let col_map = build_col_map(result.col_specs());
        let mut edges: Vec<TemporalEdge> = result
            .rows_or_empty()
            .into_iter()
            .map(|row| temporal_edge_from_row(ctx, &row, &col_map))
            .collect::<anyhow::Result<_>>()?;
        edges.sort_by_key(|edge| edge.ordinal);
        Ok(edges)
    }

    async fn confidence_put(
        &self,
        _ctx: &TenantContext,
        score: &crate::types::ConfidenceScore,
    ) -> anyhow::Result<()> {
        self.exec_prepared_rows(
            &self.stmts.confidence_put,
            (
                score.entity_id,
                score.fact_hash.clone(),
                score.confidence,
                score.source_count,
                score.last_confirmed_at,
                score.contradiction_count,
            ),
        )
        .await?;
        Ok(())
    }

    async fn confidence_get(
        &self,
        _ctx: &TenantContext,
        entity_id: Uuid,
        fact_hash: &str,
    ) -> anyhow::Result<Option<crate::types::ConfidenceScore>> {
        let (col_map, rows) = self
            .exec_prepared_rows(
                &self.stmts.confidence_get,
                (entity_id, fact_hash.to_string()),
            )
            .await?;
        if let Some(row) = rows.into_iter().next() {
            let confidence: f64 = cql_get(&row, &col_map, "confidence")?;
            let source_count: i32 = cql_get(&row, &col_map, "source_count")?;
            let last_confirmed_at: chrono::DateTime<chrono::Utc> =
                cql_get(&row, &col_map, "last_confirmed_at")?;
            let contradiction_count: i32 = cql_get(&row, &col_map, "contradiction_count")?;
            Ok(Some(crate::types::ConfidenceScore {
                entity_id,
                fact_hash: fact_hash.to_string(),
                confidence,
                source_count,
                last_confirmed_at,
                contradiction_count,
            }))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod cql_storage_tests {
    use super::*;

    #[test]
    fn sprint1_seed_insert_statements_bind_created_at_timestamp() {
        let (entity_q, edge_q) = sprint1_seed_insert_statements("agent_memory");

        assert!(
            entity_q.contains("created_at)"),
            "entity seed query should write created_at: {entity_q}"
        );
        assert!(
            entity_q.contains("VALUES (?, ?, ?)"),
            "entity seed query should bind created_at as a timestamp parameter: {entity_q}"
        );
        assert!(
            edge_q.contains("VALUES (?, ?, ?, ?, ?)"),
            "edge seed query should bind created_at as a timestamp parameter: {edge_q}"
        );
        assert!(
            !entity_q.contains("now()") && !edge_q.contains("now()"),
            "seed queries must not rely on Ferrosa CQL now() coercion"
        );
    }

    #[test]
    fn fold_ann_search_query_uses_vector_literal_not_blob_parameter() {
        let (query, bind_count) = build_fold_ann_search_query("agent_memory", &[0.25, -1.5], 7);

        assert!(
            query.contains("ORDER BY fold_embedding ANN OF [0.25000000,-1.50000000] LIMIT 7"),
            "fold ANN query should render a vector literal: {query}"
        );
        assert!(
            !query.contains("ANN OF ?"),
            "fold ANN query must not bind the vector as a blob: {query}"
        );
        assert_eq!(
            bind_count, 2,
            "only session_id and tenant_id should be bound after rendering the vector literal"
        );
    }
}
