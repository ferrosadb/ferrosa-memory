//! Durable artifact blob/link writes and the bounded Mobile artifact projections.
//! Correctness: A checksum owns one immutable blob row, every upload owns a
//! separate semantic link, and only an authorized review can move a link from
//! pending to active.
//! Last revised: 2026-08-31
//! Last changed: Added checksum deduplication, reviewer authorization, and activation.
#![allow(deprecated)]

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use scylla::frame::response::result::CqlValue;
use scylla::{LegacySession, SessionBuilder};
use uuid::Uuid;

use ferrosa_memory_core::artifact::{ArtifactState, activation_allowed};

#[derive(Clone)]
pub struct StoredArtifact {
    pub artifact_id: String,
    pub display_name: String,
    pub checksum: String,
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub uploader_id: String,
    pub captured_path: String,
    pub host_id: String,
    pub host_label: String,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ArtifactRow {
    pub artifact_id: String,
    pub name: String,
    pub state: String,
    pub tags: Vec<String>,
}

pub struct ArtifactView {
    session: LegacySession,
    tenant_id: Uuid,
}

impl ArtifactView {
    pub async fn detail(&self, artifact_id: &str) -> Result<Option<ArtifactRow>> {
        let result = self.session.query_unpaged(
            "SELECT display_name, state FROM agent_memory.artifact_link WHERE tenant_id = ? AND artifact_id = ?",
            (self.tenant_id, artifact_id.to_owned()),
        ).await.context("reading artifact detail")?;
        let cols: std::collections::BTreeMap<_, _> = result
            .col_specs()
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name().to_owned(), i))
            .collect();
        let Some(row) = result.rows_or_empty().into_iter().next() else {
            return Ok(None);
        };
        let text = |name: &str| match row.columns.get(*cols.get(name)?)? {
            Some(scylla::frame::response::result::CqlValue::Text(v)) => Some(v.clone()),
            _ => None,
        };
        let mut artifact = ArtifactRow {
            artifact_id: artifact_id.to_owned(),
            name: text("display_name").unwrap_or_else(|| "artifact".to_owned()),
            state: text("state").unwrap_or_else(|| "pending".to_owned()),
            tags: Vec::new(),
        };
        let tags = self
            .session
            .query_unpaged(
                "SELECT tag FROM agent_memory.artifact_tag WHERE tenant_id = ? AND artifact_id = ? LIMIT 64",
                (self.tenant_id, artifact_id.to_owned()),
            )
            .await
            .context("reading artifact detail tags")?;
        let tag_cols: std::collections::BTreeMap<_, _> = tags
            .col_specs()
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name().to_owned(), i))
            .collect();
        artifact.tags = tags
            .rows_or_empty()
            .into_iter()
            .filter_map(|r| match r.columns.get(*tag_cols.get("tag")?)? {
                Some(scylla::frame::response::result::CqlValue::Text(v)) => Some(v.clone()),
                _ => None,
            })
            .collect();
        Ok(Some(artifact))
    }
    pub async fn connect(contact_points: &[String], tenant_id: Uuid) -> Result<Self> {
        anyhow::ensure!(
            !contact_points.is_empty(),
            "no contact points for the artifact store"
        );
        let session = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            SessionBuilder::new()
                .known_nodes(contact_points)
                .connection_timeout(std::time::Duration::from_secs(5))
                .build_legacy(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("artifact store did not answer within 10s"))?
        .context("connecting to artifact store")?;
        Ok(Self { session, tenant_id })
    }

    pub async fn persist_pending(&self, artifact: &StoredArtifact) -> Result<()> {
        let now = chrono::Utc::now();
        let blob_result = self.session.query_unpaged(
            "INSERT INTO agent_memory.artifact_blob (checksum, size_bytes, media_type, storage_locator, created_at, content) VALUES (?, ?, ?, ?, ?, ?) IF NOT EXISTS",
            (artifact.checksum.clone(), artifact.bytes.len() as i64, artifact.media_type.clone(),
             format!("cql:artifact_blob:{}", artifact.checksum), now, artifact.bytes.clone()),
        ).await.context("writing content-addressed artifact blob")?;
        if lwt_applied(blob_result)?.unwrap_or(true) {
            // The first writer owns the immutable bytes. A retry or a second
            // semantic link must not rewrite a potentially large blob row.
        } else {
            let existing = self
                .session
                .query_unpaged(
                    "SELECT size_bytes FROM agent_memory.artifact_blob WHERE checksum = ?",
                    (artifact.checksum.clone(),),
                )
                .await
                .context("checking existing artifact blob")?;
            let size = existing.rows_or_empty().into_iter().next().and_then(|row| {
                row.columns
                    .into_iter()
                    .next()
                    .and_then(|value| match value {
                        Some(CqlValue::BigInt(value)) => Some(value),
                        _ => None,
                    })
            });
            anyhow::ensure!(
                size == Some(artifact.bytes.len() as i64),
                "existing artifact blob checksum has a different size"
            );
        }
        self.session.query_unpaged(
            "INSERT INTO agent_memory.artifact_link (tenant_id, artifact_id, checksum, uploader_id, captured_path, host_id, host_label, captured_at, uploaded_at, state, display_name) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (self.tenant_id, artifact.artifact_id.clone(), artifact.checksum.clone(), artifact.uploader_id.clone(), artifact.captured_path.clone(), artifact.host_id.clone(), artifact.host_label.clone(), now, now, "pending", artifact.display_name.clone()),
        ).await.context("writing artifact link")?;
        self.session.query_unpaged(
            "INSERT INTO agent_memory.artifact_by_state (tenant_id, state, page_key, artifact_id, uploader_id, checksum) VALUES (?, ?, ?, ?, ?, ?)",
            (self.tenant_id, "pending", format!("{:013}-{}", now.timestamp_millis(), artifact.artifact_id), artifact.artifact_id.clone(), artifact.uploader_id.clone(), artifact.checksum.clone()),
        ).await.context("writing artifact pending index")?;
        for tag in artifact.tags.iter() {
            self.session.query_unpaged(
                "INSERT INTO agent_memory.artifact_tag (tenant_id, artifact_id, tag, source, immutable) VALUES (?, ?, ?, ?, ?)",
                (self.tenant_id, artifact.artifact_id.clone(), tag.clone(), "user", false),
            ).await.context("writing artifact tag")?;
        }
        self.session.query_unpaged(
            "INSERT INTO agent_memory.artifact_tag (tenant_id, artifact_id, tag, source, immutable) VALUES (?, ?, ?, ?, ?)",
            (self.tenant_id, artifact.artifact_id.clone(), "_sys_pending", "system", true),
        ).await.context("writing artifact pending tag")?;
        Ok(())
    }

    /// Activate one pending link after checking the server-side reviewer set.
    /// The caller supplies the authenticated device identity, never an
    /// identity copied from the request body.
    pub async fn activate(
        &self,
        artifact_id: &str,
        reviewer_id: &str,
        policy_expanding: bool,
        policy_change: bool,
    ) -> Result<bool> {
        let result = self.session.query_unpaged(
            "SELECT uploader_id, checksum, state FROM agent_memory.artifact_link WHERE tenant_id = ? AND artifact_id = ?",
            (self.tenant_id, artifact_id.to_owned()),
        ).await.context("reading artifact before activation")?;
        let cols = column_map(result.col_specs());
        let Some(row) = result.rows_or_empty().into_iter().next() else {
            return Ok(false);
        };
        let text = |name: &str| match row.columns.get(*cols.get(name)?)? {
            Some(CqlValue::Text(value)) => Some(value.clone()),
            _ => None,
        };
        let uploader_id = text("uploader_id").context("artifact uploader is missing")?;
        let checksum = text("checksum").context("artifact checksum is missing")?;
        let state = match text("state").as_deref() {
            Some("pending") | None => ArtifactState::Pending,
            Some("active") => ArtifactState::Active,
            Some("deleted") => ArtifactState::Deleted,
            Some(_) => ArtifactState::Deleted,
        };
        let mut authorized_reviewers = BTreeSet::new();
        if self.reviewer_exists(reviewer_id).await? {
            authorized_reviewers.insert(reviewer_id.to_owned());
        }
        activation_allowed(
            state,
            &uploader_id,
            reviewer_id,
            &authorized_reviewers,
            policy_expanding,
            policy_change,
        )
        .map_err(|error| anyhow::anyhow!("artifact activation denied: {error:?}"))?;

        let updated = self.session.query_unpaged(
            "UPDATE agent_memory.artifact_link SET state = ?, deleted_at = null WHERE tenant_id = ? AND artifact_id = ? IF state = ?",
            ("active", self.tenant_id, artifact_id.to_owned(), "pending"),
        ).await.context("activating artifact link")?;
        if !lwt_applied(updated)?.unwrap_or(true) {
            return Ok(false);
        }
        let page_key = format!(
            "{:013}-{}",
            chrono::Utc::now().timestamp_millis(),
            artifact_id
        );
        self.session.query_unpaged(
            "INSERT INTO agent_memory.artifact_by_state (tenant_id, state, page_key, artifact_id, uploader_id, checksum) VALUES (?, ?, ?, ?, ?, ?)",
            (self.tenant_id, "active", page_key, artifact_id.to_owned(), uploader_id, checksum),
        ).await.context("writing artifact active index")?;
        self.session.query_unpaged(
            "DELETE FROM agent_memory.artifact_tag WHERE tenant_id = ? AND artifact_id = ? AND tag = ?",
            (self.tenant_id, artifact_id.to_owned(), "_sys_pending"),
        ).await.context("removing artifact pending marker")?;
        self.session.query_unpaged(
            "INSERT INTO agent_memory.artifact_approval (tenant_id, artifact_id, approval_id, action, state, reviewer_id, decided_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            (self.tenant_id, artifact_id.to_owned(), Uuid::now_v7(), if policy_expanding { "policy_change" } else { "activate" }, "approved", reviewer_id.to_owned(), chrono::Utc::now()),
        ).await.context("recording artifact approval")?;
        Ok(true)
    }

    /// Returns pending metadata only for the uploader or a configured tenant
    /// reviewer. No blob content is read by this projection.
    pub async fn review_queue(&self, reviewer_id: &str, limit: i32) -> Result<Vec<ArtifactRow>> {
        let reviewer = self.reviewer_exists(reviewer_id).await?;
        let result = self.session.query_unpaged(
            "SELECT artifact_id, uploader_id FROM agent_memory.artifact_by_state WHERE tenant_id = ? AND state = ? LIMIT ?",
            (self.tenant_id, "pending", limit.clamp(1, 8)),
        ).await.context("reading pending artifact review queue")?;
        let cols = column_map(result.col_specs());
        let mut rows = Vec::new();
        for row in result.rows_or_empty() {
            let text = |name: &str| match row.columns.get(*cols.get(name)?)? {
                Some(CqlValue::Text(value)) => Some(value.clone()),
                _ => None,
            };
            let artifact_id = text("artifact_id").context("pending artifact id is missing")?;
            let uploader_id =
                text("uploader_id").context("pending artifact uploader is missing")?;
            if !reviewer && uploader_id != reviewer_id {
                continue;
            }
            if let Some(artifact) = self.detail(&artifact_id).await? {
                rows.push(artifact);
            }
        }
        Ok(rows)
    }

    async fn reviewer_exists(&self, reviewer_id: &str) -> Result<bool> {
        let result = self.session.query_unpaged(
            "SELECT reviewer_id FROM agent_memory.artifact_reviewer WHERE tenant_id = ? AND reviewer_id = ?",
            (self.tenant_id, reviewer_id.to_owned()),
        ).await.context("checking artifact reviewer authorization")?;
        Ok(!result.rows_or_empty().is_empty())
    }

    pub async fn overview(&self, limit: i32) -> Result<(usize, Vec<ArtifactRow>)> {
        let result = self.session.query_unpaged(
            "SELECT artifact_id, display_name, state FROM agent_memory.artifact_link WHERE tenant_id = ? LIMIT ?",
            (self.tenant_id, limit.clamp(1, 8)),
        ).await.context("reading artifact files")?;
        let cols: std::collections::BTreeMap<_, _> = result
            .col_specs()
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name().to_owned(), i))
            .collect();
        let mut rows = result
            .rows_or_empty()
            .into_iter()
            .filter_map(|row| {
                let text = |name: &str| match row.columns.get(*cols.get(name)?)? {
                    Some(scylla::frame::response::result::CqlValue::Text(v)) => Some(v.clone()),
                    _ => None,
                };
                Some(ArtifactRow {
                    artifact_id: text("artifact_id")?,
                    name: text("display_name").unwrap_or_else(|| "artifact".to_owned()),
                    state: text("state").unwrap_or_else(|| "pending".to_owned()),
                    tags: Vec::new(),
                })
            })
            .collect::<Vec<_>>();
        for row in &mut rows {
            let tags = self.session.query_unpaged(
                "SELECT tag FROM agent_memory.artifact_tag WHERE tenant_id = ? AND artifact_id = ? LIMIT 64",
                (self.tenant_id, row.artifact_id.clone()),
            ).await.context("reading artifact tags")?;
            let tag_cols: std::collections::BTreeMap<_, _> = tags
                .col_specs()
                .iter()
                .enumerate()
                .map(|(i, c)| (c.name().to_owned(), i))
                .collect();
            row.tags = tags
                .rows_or_empty()
                .into_iter()
                .filter_map(
                    |tag_row| match tag_row.columns.get(*tag_cols.get("tag")?)? {
                        Some(scylla::frame::response::result::CqlValue::Text(value)) => {
                            Some(value.clone())
                        }
                        _ => None,
                    },
                )
                .collect();
        }
        Ok((rows.len(), rows))
    }
}

fn column_map(
    specs: &[scylla::frame::response::result::ColumnSpec<'_>],
) -> std::collections::BTreeMap<String, usize> {
    specs
        .iter()
        .enumerate()
        .map(|(index, spec)| (spec.name().to_owned(), index))
        .collect()
}

fn lwt_applied(result: scylla::LegacyQueryResult) -> Result<Option<bool>> {
    let cols = column_map(result.col_specs());
    let Some(index) = cols.get("[applied]") else {
        return Ok(None);
    };
    let Some(row) = result.rows_or_empty().into_iter().next() else {
        return Ok(None);
    };
    match row.columns.get(*index).cloned().flatten() {
        Some(CqlValue::Boolean(value)) => Ok(Some(value)),
        _ => anyhow::bail!("conditional CQL [applied] value is not boolean"),
    }
}
