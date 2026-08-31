//! Durable artifact blob/link writes and the bounded Mobile overview projection.
#![allow(deprecated)]

use anyhow::{Context, Result};
use scylla::{LegacySession, SessionBuilder};
use uuid::Uuid;

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
                "SELECT tag FROM agent_memory.artifact_tag WHERE tenant_id = ? AND artifact_id = ?",
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
        self.session.query_unpaged(
            "INSERT INTO agent_memory.artifact_blob (checksum, size_bytes, media_type, storage_locator, created_at, content) VALUES (?, ?, ?, ?, ?, ?)",
            (artifact.checksum.clone(), artifact.bytes.len() as i64, artifact.media_type.clone(),
             format!("cql:artifact_blob:{}", artifact.checksum), now, artifact.bytes.clone()),
        ).await.context("writing content-addressed artifact blob")?;
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
                "SELECT tag FROM agent_memory.artifact_tag WHERE tenant_id = ? AND artifact_id = ?",
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
