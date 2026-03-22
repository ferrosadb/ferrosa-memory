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
    // Feedback
    feedback_put: PreparedQuery,
    // Intentions
    intention_put: PreparedQuery,
    intention_list: PreparedQuery,
    intention_update_status: PreparedQuery,
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
                    "SELECT result, hit_count, created_at, last_hit_at, expires_at \
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
                     token_count, compression_ratio, status, created_at, folded_at \
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
                      source_fold_id, context_snippet, entity_embedding, confidence, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
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
                     context_snippet, confidence, state, created_at \
                     FROM {ks}.entity_store WHERE tenant_id = ? AND session_id = ?"
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
                     (entity_a, entity_b, session_id, tenant_id, created_at) \
                     VALUES (?, ?, ?, ?, ?)"
                ))
                .await?,
            edge_supersedes: session
                .prepare(format!(
                    "INSERT INTO {ks}.supersedes \
                     (new_event_id, old_event_id, entity_id, tenant_id, created_at) \
                     VALUES (?, ?, ?, ?, ?)"
                ))
                .await?,
            // Intentions
            intention_put: session
                .prepare(format!(
                    "INSERT INTO {ks}.intentions \
                     (tenant_id, intention_id, description, trigger_json, priority, \
                      status, created_at, triggered_at, completed_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
                ))
                .await?,
            intention_list: session
                .prepare(format!(
                    "SELECT intention_id, description, trigger_json, priority, status, \
                     created_at, triggered_at, completed_at \
                     FROM {ks}.intentions WHERE tenant_id = ?"
                ))
                .await?,
            intention_update_status: session
                .prepare(format!(
                    "UPDATE {ks}.intentions SET status = ?, triggered_at = ?, completed_at = ? \
                     WHERE tenant_id = ? AND intention_id = ?"
                ))
                .await?,
        };

        tracing::info!(
            keyspace = ks,
            statements = 27,
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
                result_embedding: None, // TODO: vector column read
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

            let created: chrono::NaiveDateTime = row.r_by_name("created_at")?;

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
            let created: chrono::NaiveDateTime = row.r_by_name("created_at")?;

            Ok(Some(FoldEntry {
                session_id,
                fold_id,
                tenant_id: ctx.tenant_id,
                depth: row.r_by_name("depth")?,
                parent_fold_id: row.r_by_name::<Uuid>("parent_fold_id").ok(),
                raw_trajectory: row.r_by_name("raw_trajectory")?,
                fold_summary: row.r_by_name::<String>("fold_summary").ok(),
                fold_embedding: None, // TODO: vector column read
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
        self.session
            .exec_with_values(
                &self.stmts.entity_put,
                query_values!(
                    ctx.tenant_id,
                    entry.entity_id,
                    entry.session_id,
                    entry.entity_name.clone(),
                    entry.entity_type.clone(),
                    entry.source_fold_id,
                    entry.context_snippet.clone(),
                    entry.entity_embedding.as_ref().map(|e| {
                        cdrs_tokio::types::blob::Blob::new(crate::vector::encode_vector(e))
                    }),
                    entry.confidence as f32,
                    chrono::Utc::now().naive_utc()
                ),
            )
            .await?;
        Ok(())
    }

    async fn entity_find_phonetic(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        name: &str,
    ) -> anyhow::Result<Option<EntityEntry>> {
        // TODO: use phonetic index query when Ferrosa supports it
        // Fallback: exact case-insensitive match via ALLOW FILTERING
        let query = format!(
            "SELECT entity_id, entity_name, entity_type, source_fold_id, \
             context_snippet, confidence, state, created_at \
             FROM {}.entity_store WHERE tenant_id = ? AND session_id = ? ALLOW FILTERING",
            self.keyspace
        );
        let envelope = self
            .session
            .query_with_values(query, query_values!(ctx.tenant_id, session_id))
            .await?;

        let rows = envelope.response_body()?.into_rows().unwrap_or_default();
        let lower = name.to_lowercase();
        for row in rows {
            let entity_name: String = row.r_by_name("entity_name")?;
            if entity_name.to_lowercase() == lower {
                let created: chrono::NaiveDateTime = row.r_by_name("created_at")?;
                let state = row
                    .r_by_name::<String>("state")
                    .ok()
                    .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
                    .unwrap_or_default();
                return Ok(Some(EntityEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id: row.r_by_name("entity_id")?,
                    session_id,
                    entity_name,
                    entity_type: row.r_by_name("entity_type")?,
                    source_fold_id: row.r_by_name::<Uuid>("source_fold_id").ok(),
                    context_snippet: row.r_by_name("context_snippet")?,
                    entity_embedding: None,
                    confidence: f64::from(row.r_by_name::<f32>("confidence")?),
                    state,
                    created_at: created.and_utc(),
                }));
            }
        }
        Ok(None)
    }

    async fn entity_search_ann(
        &self,
        ctx: &TenantContext,
        session_id: Uuid,
        query_embedding: &[f32],
        k: usize,
    ) -> anyhow::Result<Vec<EntityEntry>> {
        let query_blob =
            cdrs_tokio::types::blob::Blob::new(crate::vector::encode_vector(query_embedding));
        let query = format!(
            "SELECT entity_id, entity_name, entity_type, source_fold_id, \
             context_snippet, confidence, state, created_at \
             FROM {}.entity_store WHERE tenant_id = ? AND session_id = ? \
             ORDER BY entity_embedding ANN OF ? LIMIT ?",
            self.keyspace
        );
        let envelope = match self
            .session
            .query_with_values(
                query,
                query_values!(ctx.tenant_id, session_id, query_blob, k as i32),
            )
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
            let created: chrono::NaiveDateTime = row.r_by_name("created_at")?;
            let state = row
                .r_by_name::<String>("state")
                .ok()
                .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
                .unwrap_or_default();
            results.push(EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id: row.r_by_name("entity_id")?,
                session_id,
                entity_name: row.r_by_name("entity_name")?,
                entity_type: row.r_by_name("entity_type")?,
                source_fold_id: row.r_by_name::<Uuid>("source_fold_id").ok(),
                context_snippet: row.r_by_name("context_snippet")?,
                entity_embedding: None, // Don't return embedding bytes in search results
                confidence: f64::from(row.r_by_name::<f32>("confidence")?),
                state,
                created_at: created.and_utc(),
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
            let created: chrono::NaiveDateTime = row.r_by_name("created_at")?;
            let state = row
                .r_by_name::<String>("state")
                .ok()
                .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok())
                .unwrap_or_default();
            results.push(EntityEntry {
                tenant_id: ctx.tenant_id,
                entity_id: row.r_by_name("entity_id")?,
                session_id,
                entity_name: row.r_by_name("entity_name")?,
                entity_type: row.r_by_name("entity_type")?,
                source_fold_id: row.r_by_name::<Uuid>("source_fold_id").ok(),
                context_snippet: row.r_by_name("context_snippet")?,
                entity_embedding: None,
                confidence: f64::from(row.r_by_name::<f32>("confidence")?),
                state,
                created_at: created.and_utc(),
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
    ) -> anyhow::Result<()> {
        self.session
            .exec_with_values(
                &self.stmts.edge_co_occurs,
                query_values!(
                    entity_a,
                    entity_b,
                    session_id,
                    ctx.tenant_id,
                    chrono::Utc::now().naive_utc()
                ),
            )
            .await?;
        tracing::debug!(%entity_a, %entity_b, "CO_OCCURS_WITH edge created");
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
    ) -> anyhow::Result<Vec<crate::intention::Intention>> {
        let rows = self
            .query_rows(&self.stmts.intention_list, query_values!(ctx.tenant_id))
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

            let created: chrono::NaiveDateTime = row.r_by_name("created_at")?;

            results.push(crate::intention::Intention {
                id: row.r_by_name("intention_id")?,
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
                    id
                ),
            )
            .await?;
        Ok(())
    }
}
