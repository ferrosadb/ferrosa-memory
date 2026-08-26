//! Module: Knowledge as a lifecycle — claims, approval, expiry, demotion.
//! Correctness: Correct when only legal transitions are accepted, when a
//! demotion says why it happened, and when the derived keys a queue is read by
//! agree with the rows written into it.
//! Last revised: 2026-08-25
//! Last changed: Created, from decisions D16-D49.
//!
//! Every other tier is DERIVED from a source path and never stored, so
//! re-tiering a directory is one rule edit. Knowledge cannot work that way: a
//! deliverable is Knowledge because a person curated it and because it is
//! still true, so its tier depends on its STATE. This module is that state.
//!
//! The distinction the whole tier rests on: a **claim** is what a model
//! asserts, and **knowledge** is what a person ratified.

use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::cql_storage::{build_col_map, cql_get};
use crate::tiers::Tier;

/// Where a deliverable is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeState {
    /// An agent proposed it. A claim, not yet knowledge.
    Proposed,
    /// Sent back with feedback: directionally right, needs more.
    Revisit,
    /// A person reviewed it and stands behind it.
    Approved,
    /// Its expiry passed.
    Expired,
    /// A newer version was approved in its place.
    Superseded,
    /// A person turned it down.
    Rejected,
}

impl KnowledgeState {
    /// The wire and storage spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Revisit => "revisit",
            Self::Approved => "approved",
            Self::Expired => "expired",
            Self::Superseded => "superseded",
            Self::Rejected => "rejected",
        }
    }

    /// Read a stored state back. `None` for anything this build does not know,
    /// which is a row written by a different version rather than a value to
    /// guess at.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "proposed" => Some(Self::Proposed),
            "revisit" => Some(Self::Revisit),
            "approved" => Some(Self::Approved),
            "expired" => Some(Self::Expired),
            "superseded" => Some(Self::Superseded),
            "rejected" => Some(Self::Rejected),
            _ => None,
        }
    }

    /// Whether this state still needs a person.
    ///
    /// What the Approvals queue and the Claims tab are both asking.
    pub fn awaits_a_person(&self) -> bool {
        matches!(self, Self::Proposed | Self::Revisit)
    }

    /// Whether the Knowledge tier shows it.
    ///
    /// Approved only (D44). A claim on the same list as ratified knowledge
    /// would put a model's assertion and a person's judgement on equal
    /// footing, which is the distinction the tier exists to make.
    pub fn is_knowledge(&self) -> bool {
        matches!(self, Self::Approved)
    }
}

/// Why an item left Knowledge, and where it lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Demotion {
    /// Approved, then its expiry passed. "At this time this was the current
    /// state of our knowledge" is a true and useful record.
    WasTrueThen,
    /// A claim nobody reviewed before it lapsed. Effort went into it and no
    /// one judged it, so its value is unknown rather than nil.
    NobodyLooked,
    /// A person turned it down.
    Refused,
}

impl Demotion {
    /// The tier it lands in.
    ///
    /// **Rejection is the only signal strong enough to demote to Data** (D46).
    /// A person saying no is a judgement; time passing is not. So the other
    /// two land in Information, and stay readable.
    pub fn lands_in(&self) -> Tier {
        match self {
            Self::WasTrueThen | Self::NobodyLooked => Tier::Information,
            Self::Refused => Tier::Data,
        }
    }

    /// The stored spelling, kept distinct so the three reasons stay legible
    /// once they are all sitting in Information.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WasTrueThen => "was_true_then",
            Self::NobodyLooked => "nobody_looked",
            Self::Refused => "refused",
        }
    }
}

/// A transition that was refused, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IllegalTransition {
    pub from: KnowledgeState,
    pub to: KnowledgeState,
    pub reason: &'static str,
}

impl std::fmt::Display for IllegalTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} cannot become {}: {}",
            self.from.as_str(),
            self.to.as_str(),
            self.reason
        )
    }
}

/// Whether a state change is allowed.
///
/// Enumerated rather than left to callers, because the states carry meaning a
/// person relies on: an approved item bears a green check saying someone
/// reviewed it, and a path that reaches Approved without a person passing
/// through would make that mark a lie.
pub fn transition(
    from: KnowledgeState,
    to: KnowledgeState,
) -> Result<KnowledgeState, IllegalTransition> {
    use KnowledgeState::*;
    let allowed = match (from, to) {
        // A person reviews a claim, or sends it back, or turns it down.
        (Proposed | Revisit, Approved | Revisit | Rejected) => true,
        // Retraction: a person can turn down something already approved.
        (Approved, Rejected) => true,
        // Time, and newer versions.
        (Approved | Proposed | Revisit, Expired) => true,
        (Approved, Superseded) => true,
        _ => false,
    };
    if allowed {
        return Ok(to);
    }
    Err(IllegalTransition {
        from,
        to,
        reason: match (from, to) {
            (Expired | Superseded | Rejected, _) => "it has already left the lifecycle",
            (_, Proposed) => "nothing returns to proposed; sending back uses revisit",
            (Proposed | Revisit, Superseded) => "only an approved version can be superseded",
            _ => "not a transition this lifecycle has",
        },
    })
}

/// Which band the overview reads it in.
///
/// Two bands by recency IS the overview's order — new-high, older-high,
/// new-low, old-low (D23) — so this is a partition key rather than a sort.
pub fn priority_band(priority: i32) -> &'static str {
    if priority >= 50 { "high" } else { "low" }
}

/// The day bucket an expiry falls in, as `YYYY-MM-DD`.
///
/// The sweep reads today's bucket rather than everything ever approved, and a
/// sweeper that missed days catches up by reading those days.
pub fn expiry_day(expires_at: DateTime<Utc>) -> String {
    expires_at.format("%Y-%m-%d").to_string()
}

/// How long an approval lasts unless someone says otherwise (D27).
pub const DEFAULT_EXPIRY_DAYS: i64 = 30;

/// The ordering key within a bucket.
///
/// `{millis:013}-{knowledge_id}`, unique by construction. Timestamps alone are
/// not: several deliverables can land in one millisecond, and a cursor over
/// tied rows steps over them — losing rows from a list that still looks
/// complete.
pub fn page_key(at: DateTime<Utc>, knowledge_id: Uuid) -> anyhow::Result<String> {
    let millis = at.timestamp_millis();
    anyhow::ensure!(
        millis >= 0,
        "a knowledge timestamp before 1970 cannot be ordered: {at}"
    );
    Ok(format!("{millis:013}-{knowledge_id}"))
}

/// One deliverable, across all its revisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeItem {
    pub knowledge_id: Uuid,
    pub title: String,
    /// pull_request | presentation | report | ... Text because the set grows
    /// by rule, and a schema change per deliverable type is what stops people
    /// adding one (D22).
    pub kind: String,
    pub state: KnowledgeState,
    pub current_version: Option<i32>,
    /// A claim carries one from creation; approval RESETS it (D44).
    pub expires_at: Option<DateTime<Utc>>,
    /// So it can be sent back with feedback rather than silently rejected.
    /// The session too, because the agent may be gone by review time and its
    /// session is what a replacement picks up (D24).
    pub author_agent: Option<String>,
    pub author_session: Option<Uuid>,
    pub task_id: Option<String>,
    pub priority: i32,
    pub repo: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub reviewed_by: Option<String>,
    pub reviewed_at: Option<DateTime<Utc>>,
}

/// One revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeVersion {
    pub version: i32,
    /// For a pull request this is the URL and nothing is stored — Ferrosa
    /// holds no token and takes no action on GitHub (D19).
    pub body_url: Option<String>,
    pub artifact_id: Option<Uuid>,
    pub summary: Option<String>,
    pub version_state: String,
    /// What sent the PREVIOUS version back, so this one can be read against
    /// what was asked for (D36).
    pub feedback: Option<String>,
    pub created_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub approved_at: Option<DateTime<Utc>>,
}

/// What an agent supplies when it proposes a deliverable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimDraft {
    pub title: String,
    pub kind: String,
    pub body_url: Option<String>,
    pub summary: Option<String>,
    pub author_agent: Option<String>,
    pub author_session: Option<Uuid>,
    pub task_id: Option<String>,
    pub priority: i32,
    pub repo: Option<String>,
    /// How long the CLAIM has before it lapses unread. Not the approval
    /// expiry -- approval resets it.
    pub expires_in_days: i64,
}

/// A page of a queue, and where to resume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnowledgePage {
    pub items: Vec<KnowledgeItem>,
    pub next_cursor: Option<String>,
}

/// Everything the knowledge lifecycle stores.
pub trait KnowledgeStore: Send + Sync {
    /// Record an agent's proposal. Lands as `proposed` with an expiry already
    /// set, and appears while it is still being made (D37).
    fn propose(
        &self,
        ctx: &crate::types::TenantContext,
        draft: ClaimDraft,
    ) -> impl std::future::Future<Output = anyhow::Result<KnowledgeItem>> + Send;

    /// Move an item to a new state, recording who decided and why.
    ///
    /// Refuses an illegal transition rather than writing it: the states carry
    /// meaning a person relies on.
    fn decide(
        &self,
        ctx: &crate::types::TenantContext,
        knowledge_id: Uuid,
        to: KnowledgeState,
        reviewer: Option<&str>,
        feedback: Option<&str>,
    ) -> impl std::future::Future<Output = anyhow::Result<KnowledgeItem>> + Send;

    /// One item.
    fn item(
        &self,
        ctx: &crate::types::TenantContext,
        knowledge_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<KnowledgeItem>>> + Send;

    /// Its version chain, oldest first.
    ///
    /// Deliberately unlimited. The chain grows one entry per review cycle, so
    /// it is bounded by human effort rather than by corpus size — measured at
    /// 0.9 ms to read five hundred versions of one item, against 0.2 ms for
    /// the two point reads the hot path actually uses via `current_version`.
    ///
    /// A LIMIT here would be a bound on a RESULT, which in this codebase means
    /// the caller silently sees a partial history. If a chain ever does grow
    /// past what a person could have produced, that is a runaway writer worth
    /// hearing about, not a page size worth tuning.
    fn versions(
        &self,
        ctx: &crate::types::TenantContext,
        knowledge_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<KnowledgeVersion>>> + Send;

    /// A page of one state and band, newest first.
    fn page(
        &self,
        ctx: &crate::types::TenantContext,
        state: KnowledgeState,
        band: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<KnowledgePage>> + Send;

    /// How many items are in one state, across both priority bands.
    ///
    /// Counted rather than paged. The memory tab wants a number, and walking
    /// pages to get one reads every row of a queue that can hold hundreds.
    fn count(
        &self,
        ctx: &crate::types::TenantContext,
        state: KnowledgeState,
    ) -> impl std::future::Future<Output = anyhow::Result<usize>> + Send;

    /// What lapses on one day, for the sweep.
    fn expiring_on(
        &self,
        ctx: &crate::types::TenantContext,
        state: KnowledgeState,
        day: &str,
        limit: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<KnowledgeItem>>> + Send;
}

/// The reference implementation, and what the unit tests run against.
///
/// Holds the same shape the CQL store writes: an item map, a version chain,
/// and the queues keyed by (state, band). Keeping the queues explicit rather
/// than deriving them on read is deliberate -- it is the CQL layout, so a
/// caller cannot pass here and fail there.
#[derive(Default)]
pub struct InMemoryKnowledgeStore {
    tenants: std::sync::Mutex<std::collections::HashMap<Uuid, TenantKnowledge>>,
}

#[derive(Default)]
struct TenantKnowledge {
    items: std::collections::HashMap<Uuid, KnowledgeItem>,
    versions: std::collections::HashMap<Uuid, Vec<KnowledgeVersion>>,
    /// (state, band) -> page_key -> id. A BTreeMap so a page is ordered.
    by_state: std::collections::HashMap<(String, String), std::collections::BTreeMap<String, Uuid>>,
    /// (state, day) -> page_key -> id.
    by_expiry:
        std::collections::HashMap<(String, String), std::collections::BTreeMap<String, Uuid>>,
}

impl InMemoryKnowledgeStore {
    fn with<T>(
        &self,
        ctx: &crate::types::TenantContext,
        f: impl FnOnce(&mut TenantKnowledge) -> T,
    ) -> T {
        let mut tenants = self.tenants.lock().expect("knowledge store poisoned");
        f(tenants.entry(ctx.tenant_id).or_default())
    }

    /// Take the item out of the queues it is currently in.
    ///
    /// The queues are keyed BY state, so a state change is a MOVE (D43).
    /// Inserting without removing leaves the item in two queues at once, which
    /// is the fault already fixed once in `entity_source_by_root`.
    fn unindex(tenant: &mut TenantKnowledge, item: &KnowledgeItem, key: &str) {
        let band = priority_band(item.priority).to_owned();
        if let Some(queue) = tenant
            .by_state
            .get_mut(&(item.state.as_str().to_owned(), band))
        {
            queue.remove(key);
        }
        if let Some(expires) = item.expires_at
            && let Some(queue) = tenant
                .by_expiry
                .get_mut(&(item.state.as_str().to_owned(), expiry_day(expires)))
        {
            queue.remove(key);
        }
    }

    fn index(tenant: &mut TenantKnowledge, item: &KnowledgeItem, key: &str) {
        let band = priority_band(item.priority).to_owned();
        tenant
            .by_state
            .entry((item.state.as_str().to_owned(), band))
            .or_default()
            .insert(key.to_owned(), item.knowledge_id);
        if let Some(expires) = item.expires_at {
            tenant
                .by_expiry
                .entry((item.state.as_str().to_owned(), expiry_day(expires)))
                .or_default()
                .insert(key.to_owned(), item.knowledge_id);
        }
    }
}

impl KnowledgeStore for InMemoryKnowledgeStore {
    async fn propose(
        &self,
        ctx: &crate::types::TenantContext,
        draft: ClaimDraft,
    ) -> anyhow::Result<KnowledgeItem> {
        let now = Utc::now();
        let knowledge_id = Uuid::now_v7();
        // A claim expires too: an unreviewed proposal against a codebase that
        // has moved is worth less for never having been read (D44).
        let expires_at = now + chrono::Duration::days(draft.expires_in_days);
        let item = KnowledgeItem {
            knowledge_id,
            title: draft.title,
            kind: draft.kind,
            state: KnowledgeState::Proposed,
            current_version: Some(1),
            expires_at: Some(expires_at),
            author_agent: draft.author_agent,
            author_session: draft.author_session,
            task_id: draft.task_id,
            priority: draft.priority,
            repo: draft.repo,
            created_at: now,
            updated_at: now,
            reviewed_by: None,
            reviewed_at: None,
        };
        let key = page_key(now, knowledge_id)?;
        let version = KnowledgeVersion {
            version: 1,
            body_url: draft.body_url,
            artifact_id: None,
            summary: draft.summary,
            version_state: "pending".to_owned(),
            feedback: None,
            created_by: item.author_agent.clone(),
            created_at: now,
            approved_at: None,
        };
        self.with(ctx, |tenant| {
            tenant.items.insert(knowledge_id, item.clone());
            tenant.versions.insert(knowledge_id, vec![version]);
            Self::index(tenant, &item, &key);
        });
        Ok(item)
    }

    async fn decide(
        &self,
        ctx: &crate::types::TenantContext,
        knowledge_id: Uuid,
        to: KnowledgeState,
        reviewer: Option<&str>,
        feedback: Option<&str>,
    ) -> anyhow::Result<KnowledgeItem> {
        let now = Utc::now();
        self.with(ctx, |tenant| {
            let item = tenant
                .items
                .get(&knowledge_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no knowledge item {knowledge_id}"))?;
            transition(item.state, to).map_err(|e| anyhow::anyhow!("{e}"))?;

            let key = page_key(item.created_at, knowledge_id)?;
            Self::unindex(tenant, &item, &key);

            let mut moved = item;
            moved.state = to;
            moved.updated_at = now;
            if to == KnowledgeState::Approved {
                moved.reviewed_by = reviewer.map(str::to_owned);
                moved.reviewed_at = Some(now);
                // Approval RESETS the expiry rather than granting it: the
                // claim already had one (D44, D27).
                moved.expires_at = Some(now + chrono::Duration::days(DEFAULT_EXPIRY_DAYS));
            }
            if to == KnowledgeState::Rejected || to == KnowledgeState::Revisit {
                moved.reviewed_by = reviewer.map(str::to_owned);
                moved.reviewed_at = Some(now);
            }
            if let Some(feedback) = feedback
                && let Some(chain) = tenant.versions.get_mut(&knowledge_id)
                && let Some(last) = chain.last_mut()
            {
                last.feedback = Some(feedback.to_owned());
            }
            Self::index(tenant, &moved, &key);
            tenant.items.insert(knowledge_id, moved.clone());
            Ok(moved)
        })
    }

    async fn item(
        &self,
        ctx: &crate::types::TenantContext,
        knowledge_id: Uuid,
    ) -> anyhow::Result<Option<KnowledgeItem>> {
        Ok(self.with(ctx, |tenant| tenant.items.get(&knowledge_id).cloned()))
    }

    async fn versions(
        &self,
        ctx: &crate::types::TenantContext,
        knowledge_id: Uuid,
    ) -> anyhow::Result<Vec<KnowledgeVersion>> {
        Ok(self.with(ctx, |tenant| {
            tenant
                .versions
                .get(&knowledge_id)
                .cloned()
                .unwrap_or_default()
        }))
    }

    async fn page(
        &self,
        ctx: &crate::types::TenantContext,
        state: KnowledgeState,
        band: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<KnowledgePage> {
        anyhow::ensure!(limit > 0, "a page of no items is not a page");
        Ok(self.with(ctx, |tenant| {
            let queue = tenant
                .by_state
                .get(&(state.as_str().to_owned(), band.to_owned()));
            let mut keys: Vec<String> = queue
                .map(|q| q.keys().cloned().collect())
                .unwrap_or_default();
            // Newest first, matching ORDER BY page_key DESC.
            keys.sort_by(|a, b| b.cmp(a));
            let start = cursor.map_or(0, |c| {
                keys.iter()
                    .position(|k| k.as_str() < c)
                    .unwrap_or(keys.len())
            });
            let window: Vec<String> = keys.into_iter().skip(start).take(limit + 1).collect();
            let has_more = window.len() > limit;
            let page: Vec<String> = window.into_iter().take(limit).collect();
            let next_cursor = has_more.then(|| page.last().cloned()).flatten();
            let items = page
                .iter()
                .filter_map(|k| queue.and_then(|q| q.get(k)))
                .filter_map(|id| tenant.items.get(id).cloned())
                .collect();
            KnowledgePage { items, next_cursor }
        }))
    }

    async fn count(
        &self,
        ctx: &crate::types::TenantContext,
        state: KnowledgeState,
    ) -> anyhow::Result<usize> {
        let tenants = self.tenants.lock().expect("knowledge store poisoned");
        Ok(tenants.get(&ctx.tenant_id).map_or(0, |tenant| {
            tenant
                .items
                .values()
                .filter(|item| item.state == state)
                .count()
        }))
    }

    async fn expiring_on(
        &self,
        ctx: &crate::types::TenantContext,
        state: KnowledgeState,
        day: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<KnowledgeItem>> {
        anyhow::ensure!(limit > 0, "a sweep of nothing is not a sweep");
        Ok(self.with(ctx, |tenant| {
            tenant
                .by_expiry
                .get(&(state.as_str().to_owned(), day.to_owned()))
                .map(|q| {
                    q.values()
                        .filter_map(|id| tenant.items.get(id).cloned())
                        .take(limit)
                        .collect()
                })
                .unwrap_or_default()
        }))
    }
}

/// The CQL implementation, mirroring [`InMemoryKnowledgeStore`].
///
/// Columns are read BY NAME through `build_col_map`/`cql_get`, never by
/// position. A positional reader is one added column away from silently
/// shifting every field after it — a fault that cost real debugging in the
/// task board on the same day this was written.
pub struct CqlKnowledgeStore {
    session: Arc<crate::cql_storage::CqlSession>,
    keyspace: String,
}

impl CqlKnowledgeStore {
    pub fn new(session: Arc<crate::cql_storage::CqlSession>, keyspace: impl Into<String>) -> Self {
        Self {
            session,
            keyspace: keyspace.into(),
        }
    }

    /// Put the item into the queues its current state belongs to.
    async fn index(
        &self,
        ctx: &crate::types::TenantContext,
        item: &KnowledgeItem,
    ) -> anyhow::Result<()> {
        let key = page_key(item.created_at, item.knowledge_id)?;
        let band = priority_band(item.priority);
        let query = format!(
            "INSERT INTO {}.knowledge_by_state \
             (tenant_id, state, priority_band, page_key, knowledge_id, title, kind, \
              priority, repo, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            self.keyspace
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    item.state.as_str(),
                    band,
                    key.as_str(),
                    item.knowledge_id,
                    item.title.as_str(),
                    item.kind.as_str(),
                    item.priority,
                    item.repo.as_deref(),
                    item.expires_at,
                ),
            )
            .await
            .context("indexing knowledge by state")?;

        if let Some(expires) = item.expires_at {
            // Ordered by EXPIRY, not creation, so walking day buckets forward
            // is already the Claims tab's default sort (D45).
            let expiry_key = page_key(expires, item.knowledge_id)?;
            let query = format!(
                "INSERT INTO {}.knowledge_by_expiry \
                 (tenant_id, state, expiry_day, page_key, knowledge_id, title, kind, \
                  priority, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                self.keyspace
            );
            #[allow(deprecated)]
            self.session
                .query_unpaged(
                    query,
                    (
                        ctx.tenant_id,
                        item.state.as_str(),
                        expiry_day(expires),
                        expiry_key.as_str(),
                        item.knowledge_id,
                        item.title.as_str(),
                        item.kind.as_str(),
                        item.priority,
                        expires,
                    ),
                )
                .await
                .context("indexing knowledge by expiry")?;
        }
        Ok(())
    }

    /// Take the item OUT of the queues its old state put it in.
    ///
    /// Both queues are partitioned by state, so a state change is a MOVE. An
    /// insert without this leaves the item in two queues at once — the fault
    /// already fixed once in `entity_source_by_root`, and demonstrated against
    /// these very tables before they were written to (D43).
    async fn unindex(
        &self,
        ctx: &crate::types::TenantContext,
        item: &KnowledgeItem,
    ) -> anyhow::Result<()> {
        let key = page_key(item.created_at, item.knowledge_id)?;
        let query = format!(
            "DELETE FROM {}.knowledge_by_state \
             WHERE tenant_id = ? AND state = ? AND priority_band = ? AND page_key = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    item.state.as_str(),
                    priority_band(item.priority),
                    key.as_str(),
                ),
            )
            .await
            .context("removing knowledge from its old state queue")?;

        if let Some(expires) = item.expires_at {
            let expiry_key = page_key(expires, item.knowledge_id)?;
            let query = format!(
                "DELETE FROM {}.knowledge_by_expiry \
                 WHERE tenant_id = ? AND state = ? AND expiry_day = ? AND page_key = ?",
                self.keyspace
            );
            #[allow(deprecated)]
            self.session
                .query_unpaged(
                    query,
                    (
                        ctx.tenant_id,
                        item.state.as_str(),
                        expiry_day(expires),
                        expiry_key.as_str(),
                    ),
                )
                .await
                .context("removing knowledge from its old expiry bucket")?;
        }
        Ok(())
    }

    async fn write_item(
        &self,
        ctx: &crate::types::TenantContext,
        item: &KnowledgeItem,
    ) -> anyhow::Result<()> {
        let query = format!(
            "INSERT INTO {}.knowledge_item \
             (tenant_id, knowledge_id, title, kind, state, current_version, expires_at, \
              author_agent, author_session, task_id, priority, repo, created_at, \
              updated_at, reviewed_by, reviewed_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            self.keyspace
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    item.knowledge_id,
                    item.title.as_str(),
                    item.kind.as_str(),
                    item.state.as_str(),
                    item.current_version,
                    item.expires_at,
                    item.author_agent.as_deref(),
                    item.author_session,
                    item.task_id.as_deref(),
                    item.priority,
                    item.repo.as_deref(),
                    item.created_at,
                    item.updated_at,
                    item.reviewed_by.as_deref(),
                    item.reviewed_at,
                ),
            )
            .await
            .context("writing a knowledge item")?;
        Ok(())
    }
}

impl KnowledgeStore for CqlKnowledgeStore {
    async fn propose(
        &self,
        ctx: &crate::types::TenantContext,
        draft: ClaimDraft,
    ) -> anyhow::Result<KnowledgeItem> {
        let now = Utc::now();
        let knowledge_id = Uuid::now_v7();
        let item = KnowledgeItem {
            knowledge_id,
            title: draft.title,
            kind: draft.kind,
            state: KnowledgeState::Proposed,
            current_version: Some(1),
            // A claim expires too (D44).
            expires_at: Some(now + chrono::Duration::days(draft.expires_in_days)),
            author_agent: draft.author_agent,
            author_session: draft.author_session,
            task_id: draft.task_id,
            priority: draft.priority,
            repo: draft.repo,
            created_at: now,
            updated_at: now,
            reviewed_by: None,
            reviewed_at: None,
        };
        self.write_item(ctx, &item).await?;

        let query = format!(
            "INSERT INTO {}.knowledge_version \
             (tenant_id, knowledge_id, version, body_url, artifact_id, summary, \
              version_state, feedback, created_by, created_at, approved_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            self.keyspace
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    knowledge_id,
                    1i32,
                    draft.body_url.as_deref(),
                    None::<Uuid>,
                    draft.summary.as_deref(),
                    "pending",
                    None::<&str>,
                    item.author_agent.as_deref(),
                    now,
                    None::<DateTime<Utc>>,
                ),
            )
            .await
            .context("writing the first knowledge version")?;

        self.index(ctx, &item).await?;
        Ok(item)
    }

    async fn decide(
        &self,
        ctx: &crate::types::TenantContext,
        knowledge_id: Uuid,
        to: KnowledgeState,
        reviewer: Option<&str>,
        feedback: Option<&str>,
    ) -> anyhow::Result<KnowledgeItem> {
        let item = self
            .item(ctx, knowledge_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no knowledge item {knowledge_id}"))?;
        // Refused before anything is written: the states carry meaning a
        // person relies on.
        transition(item.state, to).map_err(|e| anyhow::anyhow!("{e}"))?;

        // OUT of the old partitions before INTO the new ones. Both queues are
        // keyed by state, so this is a move (D43).
        self.unindex(ctx, &item).await?;

        let now = Utc::now();
        let mut moved = item;
        moved.state = to;
        moved.updated_at = now;
        if matches!(
            to,
            KnowledgeState::Approved | KnowledgeState::Rejected | KnowledgeState::Revisit
        ) {
            moved.reviewed_by = reviewer.map(str::to_owned);
            moved.reviewed_at = Some(now);
        }
        if to == KnowledgeState::Approved {
            // RESETS rather than grants: the claim already had one.
            moved.expires_at = Some(now + chrono::Duration::days(DEFAULT_EXPIRY_DAYS));
        }
        self.write_item(ctx, &moved).await?;
        self.index(ctx, &moved).await?;

        if let Some(feedback) = feedback
            && let Some(version) = moved.current_version
        {
            let query = format!(
                "UPDATE {}.knowledge_version SET feedback = ? \
                 WHERE tenant_id = ? AND knowledge_id = ? AND version = ?",
                self.keyspace
            );
            #[allow(deprecated)]
            self.session
                .query_unpaged(query, (feedback, ctx.tenant_id, knowledge_id, version))
                .await
                .context("recording review feedback")?;
        }
        Ok(moved)
    }

    async fn item(
        &self,
        ctx: &crate::types::TenantContext,
        knowledge_id: Uuid,
    ) -> anyhow::Result<Option<KnowledgeItem>> {
        let query = format!(
            "SELECT knowledge_id, title, kind, state, current_version, expires_at, \
             author_agent, author_session, task_id, priority, repo, created_at, \
             updated_at, reviewed_by, reviewed_at FROM {}.knowledge_item \
             WHERE tenant_id = ? AND knowledge_id = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, knowledge_id))
            .await
            .context("reading a knowledge item")?;
        let columns = build_col_map(result.col_specs());
        result
            .rows_or_empty()
            .into_iter()
            .next()
            .map(|row| item_from_row(&row, &columns))
            .transpose()
    }

    async fn versions(
        &self,
        ctx: &crate::types::TenantContext,
        knowledge_id: Uuid,
    ) -> anyhow::Result<Vec<KnowledgeVersion>> {
        let query = format!(
            "SELECT version, body_url, artifact_id, summary, version_state, feedback, \
             created_by, created_at, approved_at FROM {}.knowledge_version \
             WHERE tenant_id = ? AND knowledge_id = ? ORDER BY version ASC",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, knowledge_id))
            .await
            .context("reading a knowledge version chain")?;
        let columns = build_col_map(result.col_specs());
        result
            .rows_or_empty()
            .into_iter()
            .map(|row| {
                Ok(KnowledgeVersion {
                    version: cql_get(&row, &columns, "version")?,
                    body_url: cql_get::<String>(&row, &columns, "body_url").ok(),
                    artifact_id: cql_get(&row, &columns, "artifact_id").ok(),
                    summary: cql_get::<String>(&row, &columns, "summary").ok(),
                    version_state: cql_get::<String>(&row, &columns, "version_state")
                        .unwrap_or_else(|_| "pending".to_owned()),
                    feedback: cql_get::<String>(&row, &columns, "feedback").ok(),
                    created_by: cql_get::<String>(&row, &columns, "created_by").ok(),
                    created_at: cql_get(&row, &columns, "created_at")?,
                    approved_at: cql_get(&row, &columns, "approved_at").ok(),
                })
            })
            .collect()
    }

    async fn page(
        &self,
        ctx: &crate::types::TenantContext,
        state: KnowledgeState,
        band: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<KnowledgePage> {
        anyhow::ensure!(limit > 0, "a page of no items is not a page");
        // One more than asked, to learn whether a next page exists without a
        // second query and without handing out a cursor that leads nowhere.
        let probe = limit.saturating_add(1);
        let base = format!(
            "SELECT knowledge_id, page_key FROM {}.knowledge_by_state \
             WHERE tenant_id = ? AND state = ? AND priority_band = ?",
            self.keyspace
        );
        // ORDER BY explicitly: this engine accepts CLUSTERING ORDER BY DESC in
        // DDL and then ignores it, so the newest page would silently be the
        // oldest rows.
        #[allow(deprecated)]
        let result = match cursor {
            Some(cursor) => {
                let query = format!("{base} AND page_key < ? ORDER BY page_key DESC LIMIT {probe}");
                self.session
                    .query_unpaged(query, (ctx.tenant_id, state.as_str(), band, cursor))
                    .await
            }
            None => {
                let query = format!("{base} ORDER BY page_key DESC LIMIT {probe}");
                self.session
                    .query_unpaged(query, (ctx.tenant_id, state.as_str(), band))
                    .await
            }
        }
        .context("reading a page of knowledge")?;

        let columns = build_col_map(result.col_specs());
        let mut ids = Vec::with_capacity(limit);
        let mut keys = Vec::with_capacity(limit);
        for row in result.rows_or_empty() {
            ids.push(cql_get::<Uuid>(&row, &columns, "knowledge_id")?);
            keys.push(cql_get::<String>(&row, &columns, "page_key")?);
        }
        let has_more = ids.len() > limit;
        ids.truncate(limit);
        keys.truncate(limit);

        let mut items = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(item) = self.item(ctx, id).await? {
                items.push(item);
            }
        }
        Ok(KnowledgePage {
            items,
            next_cursor: has_more.then(|| keys.last().cloned()).flatten(),
        })
    }

    async fn count(
        &self,
        ctx: &crate::types::TenantContext,
        state: KnowledgeState,
    ) -> anyhow::Result<usize> {
        // One COUNT per band, which is two seeks rather than a scan. The bands
        // exist so the overview can read urgent work in one seek; a total wants
        // both, and adding them is cheaper than an unbanded partition.
        let mut total = 0usize;
        for band in ["high", "low"] {
            #[allow(deprecated)]
            let result = self
                .session
                .query_unpaged(
                    format!(
                        "SELECT COUNT(*) FROM {}.knowledge_by_state \
                         WHERE tenant_id = ? AND state = ? AND priority_band = ?",
                        self.keyspace
                    ),
                    (ctx.tenant_id, state.as_str(), band),
                )
                .await
                .context("counting knowledge in a state")?;
            let counted = result
                .rows_or_empty()
                .into_iter()
                .next()
                .and_then(|row| row.columns.first().cloned().flatten())
                .and_then(|value| value.as_bigint())
                .unwrap_or(0);
            total += usize::try_from(counted).unwrap_or(0);
        }
        Ok(total)
    }

    async fn expiring_on(
        &self,
        ctx: &crate::types::TenantContext,
        state: KnowledgeState,
        day: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<KnowledgeItem>> {
        anyhow::ensure!(limit > 0, "a sweep of nothing is not a sweep");
        let query = format!(
            "SELECT knowledge_id FROM {}.knowledge_by_expiry \
             WHERE tenant_id = ? AND state = ? AND expiry_day = ? \
             ORDER BY page_key ASC LIMIT {limit}",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, state.as_str(), day))
            .await
            .context("reading a day of expiring knowledge")?;
        let columns = build_col_map(result.col_specs());
        let ids: Vec<Uuid> = result
            .rows_or_empty()
            .into_iter()
            .map(|row| cql_get::<Uuid>(&row, &columns, "knowledge_id"))
            .collect::<anyhow::Result<_>>()?;
        let mut items = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(item) = self.item(ctx, id).await? {
                items.push(item);
            }
        }
        Ok(items)
    }
}

/// Read one `knowledge_item` row. Shared by every reader so they agree.
fn item_from_row(
    row: &scylla::frame::response::result::Row,
    columns: &crate::cql_storage::ColMap,
) -> anyhow::Result<KnowledgeItem> {
    let raw_state: String = cql_get(row, columns, "state")?;
    Ok(KnowledgeItem {
        knowledge_id: cql_get(row, columns, "knowledge_id")?,
        title: cql_get::<String>(row, columns, "title").unwrap_or_default(),
        kind: cql_get::<String>(row, columns, "kind").unwrap_or_default(),
        // A state this build does not know is a row written by another
        // version. Substituting Proposed would put someone else's approved
        // work back in the review queue.
        state: KnowledgeState::parse(&raw_state)
            .ok_or_else(|| anyhow::anyhow!("unknown knowledge state {raw_state:?}"))?,
        current_version: cql_get::<i32>(row, columns, "current_version").ok(),
        expires_at: cql_get(row, columns, "expires_at").ok(),
        author_agent: cql_get::<String>(row, columns, "author_agent").ok(),
        author_session: cql_get(row, columns, "author_session").ok(),
        task_id: cql_get::<String>(row, columns, "task_id").ok(),
        priority: cql_get::<i32>(row, columns, "priority").unwrap_or(50),
        repo: cql_get::<String>(row, columns, "repo").ok(),
        created_at: cql_get(row, columns, "created_at")?,
        updated_at: cql_get(row, columns, "updated_at")?,
        reviewed_by: cql_get::<String>(row, columns, "reviewed_by").ok(),
        reviewed_at: cql_get(row, columns, "reviewed_at").ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The green check has to mean something. A path that reaches Approved
    /// without a person passing through would make the mark a lie.
    #[test]
    fn a_claim_becomes_knowledge_only_by_review() {
        use KnowledgeState::*;
        for from in [Proposed, Revisit] {
            assert!(transition(from, Approved).is_ok(), "{from:?} -> approved");
            assert!(transition(from, Rejected).is_ok(), "{from:?} -> rejected");
            assert!(transition(from, Revisit).is_ok(), "{from:?} -> revisit");
        }
        for from in [Expired, Superseded, Rejected] {
            assert!(transition(from, Approved).is_err(), "{from:?} -> approved");
        }
    }

    /// Rejection is reachable after approval too — a retraction.
    #[test]
    fn an_approved_item_can_still_be_retracted() {
        assert!(transition(KnowledgeState::Approved, KnowledgeState::Rejected).is_ok());
    }

    /// Sending back uses revisit. Returning to proposed would erase that a
    /// person had already looked once.
    #[test]
    fn nothing_returns_to_proposed() {
        use KnowledgeState::*;
        for from in [Proposed, Revisit, Approved, Expired, Superseded, Rejected] {
            let err = transition(from, Proposed).expect_err("must be refused");
            assert_eq!(err.to, Proposed);
        }
    }

    /// Superseding is what a newer APPROVED version does to the one it
    /// replaced, so nothing else can be superseded.
    #[test]
    fn only_approved_work_is_superseded() {
        use KnowledgeState::*;
        assert!(transition(Approved, Superseded).is_ok());
        assert!(transition(Proposed, Superseded).is_err());
        assert!(transition(Revisit, Superseded).is_err());
    }

    /// A terminal state is terminal, and says so rather than failing silently.
    #[test]
    fn a_terminal_state_explains_itself() {
        let err = transition(KnowledgeState::Rejected, KnowledgeState::Approved)
            .expect_err("rejected is terminal");
        assert!(
            err.to_string().contains("already left the lifecycle"),
            "got {err}"
        );
    }

    /// D46, and the reason the rule collapsed to one sentence: a person saying
    /// no is a judgement, time passing is not.
    #[test]
    fn only_refusal_demotes_to_data() {
        assert_eq!(Demotion::Refused.lands_in(), Tier::Data);
        assert_eq!(Demotion::WasTrueThen.lands_in(), Tier::Information);
        assert_eq!(Demotion::NobodyLooked.lands_in(), Tier::Information);
    }

    /// The three reasons must stay distinguishable once they are all sitting
    /// in Information. "This was true then", "nobody got to it" and "a person
    /// refused this" read very differently a year later.
    #[test]
    fn every_demotion_reason_is_recorded_distinctly() {
        let all = [
            Demotion::WasTrueThen,
            Demotion::NobodyLooked,
            Demotion::Refused,
        ];
        let names: std::collections::HashSet<_> = all.iter().map(|d| d.as_str()).collect();
        assert_eq!(names.len(), 3, "two reasons share a name");
    }

    /// Only approved work is Knowledge (D44).
    #[test]
    fn the_knowledge_tier_shows_approved_work_only() {
        use KnowledgeState::*;
        assert!(Approved.is_knowledge());
        for other in [Proposed, Revisit, Expired, Superseded, Rejected] {
            assert!(!other.is_knowledge(), "{other:?} is not knowledge");
        }
    }

    /// The claims queue is what still needs a person.
    #[test]
    fn claims_are_what_awaits_a_person() {
        use KnowledgeState::*;
        assert!(Proposed.awaits_a_person());
        assert!(Revisit.awaits_a_person());
        for done in [Approved, Expired, Superseded, Rejected] {
            assert!(!done.awaits_a_person(), "{done:?}");
        }
    }

    #[test]
    fn the_priority_band_splits_at_fifty() {
        assert_eq!(priority_band(50), "high");
        assert_eq!(priority_band(49), "low");
        assert_eq!(priority_band(100), "high");
        assert_eq!(priority_band(0), "low");
        assert_eq!(priority_band(-1), "low", "a negative priority is not high");
    }

    #[test]
    fn the_expiry_day_is_the_bucket_a_sweep_reads() {
        let at = DateTime::from_timestamp_millis(1_767_225_600_000).expect("epoch millis");
        assert_eq!(expiry_day(at), at.format("%Y-%m-%d").to_string());
        assert_eq!(expiry_day(at).len(), 10, "YYYY-MM-DD");
    }

    /// The property the queues rest on: two items in one millisecond get
    /// different keys, so a cursor cannot step over one.
    #[test]
    fn page_keys_are_unique_within_a_millisecond() {
        let at = DateTime::from_timestamp_millis(1_767_225_600_000).expect("epoch millis");
        let a = page_key(at, Uuid::from_u128(1)).expect("key");
        let b = page_key(at, Uuid::from_u128(2)).expect("key");
        assert_ne!(a, b);
    }

    /// Zero padding is what makes a lexicographic compare a chronological one.
    #[test]
    fn page_keys_sort_chronologically_across_a_digit_boundary() {
        let id = Uuid::from_u128(1);
        let early = page_key(
            DateTime::from_timestamp_millis(999_999_999_999).expect("millis"),
            id,
        )
        .expect("key");
        let late = page_key(
            DateTime::from_timestamp_millis(1_000_000_000_000).expect("millis"),
            id,
        )
        .expect("key");
        assert!(early < late, "{early} should sort below {late}");
    }

    #[test]
    fn a_pre_epoch_timestamp_is_refused_rather_than_mis_sorted() {
        let before = DateTime::from_timestamp_millis(-1).expect("millis");
        assert!(page_key(before, Uuid::from_u128(1)).is_err());
    }

    #[test]
    fn every_state_round_trips() {
        use KnowledgeState::*;
        for state in [Proposed, Revisit, Approved, Expired, Superseded, Rejected] {
            assert_eq!(KnowledgeState::parse(state.as_str()), Some(state));
        }
        assert_eq!(KnowledgeState::parse("nonsense"), None);
    }

    fn ctx() -> crate::types::TenantContext {
        crate::types::TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "knowledge-test".to_owned(),
        }
    }

    fn draft(title: &str, priority: i32) -> ClaimDraft {
        ClaimDraft {
            title: title.to_owned(),
            kind: "pull_request".to_owned(),
            body_url: Some("https://github.com/ferrosadb/ferrosa-memory/pull/1".to_owned()),
            summary: Some("a summary".to_owned()),
            author_agent: Some("claude".to_owned()),
            author_session: Some(Uuid::new_v4()),
            task_id: Some("t_0d313bb0".to_owned()),
            priority,
            repo: Some("ferrosa-memory".to_owned()),
            expires_in_days: 7,
        }
    }

    /// THE regression this store is written around: the queues are keyed by
    /// state, so approving must MOVE the row. Same fault already fixed once in
    /// entity_source_by_root, where a re-rooted item was listed under two
    /// tiers at the same time.
    #[tokio::test]
    async fn approving_moves_an_item_out_of_the_claims_queue() {
        let store = InMemoryKnowledgeStore::default();
        let ctx = ctx();
        let item = store
            .propose(&ctx, draft("a deck", 80))
            .await
            .expect("propose");

        let claims = store
            .page(&ctx, KnowledgeState::Proposed, "high", None, 10)
            .await
            .expect("claims");
        assert_eq!(claims.items.len(), 1, "the claim is in the claims queue");

        store
            .decide(
                &ctx,
                item.knowledge_id,
                KnowledgeState::Approved,
                Some("ben"),
                None,
            )
            .await
            .expect("approve");

        let claims = store
            .page(&ctx, KnowledgeState::Proposed, "high", None, 10)
            .await
            .expect("claims");
        let knowledge = store
            .page(&ctx, KnowledgeState::Approved, "high", None, 10)
            .await
            .expect("knowledge");
        assert_eq!(claims.items.len(), 0, "it must LEAVE the claims queue");
        assert_eq!(
            knowledge.items.len(),
            1,
            "and appear in knowledge exactly once"
        );
    }

    /// Approval resets the expiry rather than granting it -- the claim already
    /// had one, because an unreviewed proposal goes stale too (D44).
    #[tokio::test]
    async fn approval_resets_the_expiry_a_claim_already_had() {
        let store = InMemoryKnowledgeStore::default();
        let ctx = ctx();
        let claim = store
            .propose(&ctx, draft("a report", 60))
            .await
            .expect("propose");
        let claim_expiry = claim.expires_at.expect("a claim expires too");

        let approved = store
            .decide(
                &ctx,
                claim.knowledge_id,
                KnowledgeState::Approved,
                Some("ben"),
                None,
            )
            .await
            .expect("approve");
        let approved_expiry = approved.expires_at.expect("approval sets one");
        assert!(
            approved_expiry > claim_expiry,
            "approval must push the expiry out, not inherit the claim's"
        );
        assert_eq!(approved.reviewed_by.as_deref(), Some("ben"));
        assert!(
            approved.reviewed_at.is_some(),
            "the green check needs a time"
        );
    }

    /// An illegal transition is refused rather than written.
    #[tokio::test]
    async fn an_illegal_transition_is_refused_and_changes_nothing() {
        let store = InMemoryKnowledgeStore::default();
        let ctx = ctx();
        let item = store
            .propose(&ctx, draft("a deck", 80))
            .await
            .expect("propose");
        store
            .decide(
                &ctx,
                item.knowledge_id,
                KnowledgeState::Rejected,
                Some("ben"),
                None,
            )
            .await
            .expect("reject");

        let err = store
            .decide(
                &ctx,
                item.knowledge_id,
                KnowledgeState::Approved,
                Some("ben"),
                None,
            )
            .await
            .expect_err("rejected is terminal");
        assert!(format!("{err:#}").contains("already left the lifecycle"));

        let still = store
            .item(&ctx, item.knowledge_id)
            .await
            .expect("read")
            .expect("exists");
        assert_eq!(
            still.state,
            KnowledgeState::Rejected,
            "the state did not move"
        );
    }

    /// Sending back carries the feedback onto the version, so the next one can
    /// be read against what was asked for (D36).
    #[tokio::test]
    async fn sending_back_records_the_feedback_on_the_version() {
        let store = InMemoryKnowledgeStore::default();
        let ctx = ctx();
        let item = store
            .propose(&ctx, draft("a deck", 80))
            .await
            .expect("propose");
        store
            .decide(
                &ctx,
                item.knowledge_id,
                KnowledgeState::Revisit,
                Some("ben"),
                Some("directionally right, needs the Q3 numbers"),
            )
            .await
            .expect("send back");
        let chain = store
            .versions(&ctx, item.knowledge_id)
            .await
            .expect("versions");
        assert_eq!(
            chain[0].feedback.as_deref(),
            Some("directionally right, needs the Q3 numbers")
        );
    }

    /// The bands are read separately, which is what makes the overview two
    /// seeks instead of a sort.
    #[tokio::test]
    async fn high_and_low_priority_claims_are_separate_pages() {
        let store = InMemoryKnowledgeStore::default();
        let ctx = ctx();
        store
            .propose(&ctx, draft("urgent", 90))
            .await
            .expect("propose");
        store
            .propose(&ctx, draft("whenever", 10))
            .await
            .expect("propose");

        let high = store
            .page(&ctx, KnowledgeState::Proposed, "high", None, 10)
            .await
            .expect("high");
        let low = store
            .page(&ctx, KnowledgeState::Proposed, "low", None, 10)
            .await
            .expect("low");
        assert_eq!(high.items.len(), 1);
        assert_eq!(low.items.len(), 1);
        assert_eq!(high.items[0].title, "urgent");
        assert_eq!(low.items[0].title, "whenever");
    }

    /// The sweep reads one day, not everything ever approved.
    #[tokio::test]
    async fn the_sweep_reads_only_its_day() {
        let store = InMemoryKnowledgeStore::default();
        let ctx = ctx();
        let claim = store
            .propose(&ctx, draft("a deck", 80))
            .await
            .expect("propose");
        let day = expiry_day(claim.expires_at.expect("claims expire"));

        let due = store
            .expiring_on(&ctx, KnowledgeState::Proposed, &day, 100)
            .await
            .expect("sweep");
        assert_eq!(due.len(), 1, "found in its own day bucket");

        let other = store
            .expiring_on(&ctx, KnowledgeState::Proposed, "1999-01-01", 100)
            .await
            .expect("sweep");
        assert!(other.is_empty(), "a different day is a different partition");
    }

    /// A claim that lapses unreviewed leaves the expiry bucket it was in, or
    /// the sweep would keep finding it every day forever.
    #[tokio::test]
    async fn expiring_a_claim_takes_it_out_of_the_sweep() {
        let store = InMemoryKnowledgeStore::default();
        let ctx = ctx();
        let claim = store
            .propose(&ctx, draft("a deck", 80))
            .await
            .expect("propose");
        let day = expiry_day(claim.expires_at.expect("claims expire"));

        store
            .decide(
                &ctx,
                claim.knowledge_id,
                KnowledgeState::Expired,
                None,
                None,
            )
            .await
            .expect("expire");

        let due = store
            .expiring_on(&ctx, KnowledgeState::Proposed, &day, 100)
            .await
            .expect("sweep");
        assert!(
            due.is_empty(),
            "an expired claim is no longer pending expiry"
        );
    }
}
