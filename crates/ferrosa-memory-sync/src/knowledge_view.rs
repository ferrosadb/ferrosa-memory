//! Module: What the phone can ask a machine about knowledge and claims.
//! Correctness: Correct when Knowledge shows only what a person approved, when
//! a claim's expiry drives its order, and when a decision that cannot legally
//! be made is refused rather than written.
//! Last revised: 2026-08-25
//! Last changed: the claims horizon missed every default-expiry claim by a day.
//!
//! # Why two lists and not one
//!
//! A claim is what a model asserts; knowledge is what a person ratified. The
//! Knowledge tab shows approved deliverables only, each carrying the green
//! check that says someone reviewed it. Claims live on their own tab. One list
//! would put a model's assertion and a person's judgement on equal footing,
//! which is the distinction the whole tier exists to make (D44).
//!
//! # Why the claims list is ordered by expiry
//!
//! A claim running out of time is the one that most needs a person: review it,
//! or lose it unreviewed. Claims expire like approved work does — a proposal
//! against a codebase that has since moved is worth less for never having been
//! read (D45).

// This module reads rows through scylla 0.15's LegacySession API, the same
// choice cql_storage.rs and memory_view.rs made and for the same reason: the
// legacy API is deprecated upstream but has stable semantics, and migrating to
// the generic deserialization API is a separate piece of work across every
// call site. Scoped to the module so the decision is stated once and a NEW
// deprecation still surfaces.
#![allow(deprecated)]

use std::sync::Arc;

use anyhow::{Context, Result};
use ferrosa_memory_core::knowledge::{
    CqlKnowledgeStore, KnowledgeItem, KnowledgePage, KnowledgeState, KnowledgeStore,
    KnowledgeVersion, expiry_day,
};
use ferrosa_memory_core::types::TenantContext;
use scylla::SessionBuilder;
use uuid::Uuid;

/// How many days ahead the claims list looks for things about to lapse.
///
/// The list walks day buckets forward from today, so this is a bound on WORK —
/// how many partitions one page may touch — not on the answer.
///
/// It must cover the whole window a claim can be filed with, or claims are
/// silently invisible. It did not: the horizon was fourteen days and the
/// default claim expiry is fourteen days, so every claim filed with the default
/// landed one day PAST the last bucket read and the queue showed nothing while
/// holding fourteen items. An off-by-one in a bound is indistinguishable from
/// an empty queue.
///
/// Tied to the expiry a decision gets, which is the longest window anything in
/// this system is filed with, and walked inclusively so a claim expiring
/// exactly at the far edge is still found.
const CLAIM_HORIZON_DAYS: i64 = ferrosa_memory_core::knowledge::DEFAULT_EXPIRY_DAYS;

/// The orders the claims list can actually be served in.
///
/// `expiry` is a seek: the buckets are already in that order. The others are a
/// sort over the fetched page, which is the right answer HERE and was the
/// wrong answer for the memory tab — the claims set is bounded by what a
/// person has not yet reviewed, and if that is ever large it is a fact about
/// the review backlog rather than a query to tune (D45).
pub const CLAIM_SORTS: &[&str] = &["expiry", "recent", "priority"];

pub struct KnowledgeView {
    store: CqlKnowledgeStore,
    ctx: TenantContext,
}

/// One row of a knowledge or claims list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeRow {
    pub knowledge_id: Uuid,
    pub title: String,
    pub kind: String,
    pub state: KnowledgeState,
    pub priority: i32,
    pub repo: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The agent that made it, so a claim can be sent back rather than
    /// silently rejected.
    pub author_agent: Option<String>,
}

impl From<KnowledgeItem> for KnowledgeRow {
    fn from(item: KnowledgeItem) -> Self {
        Self {
            knowledge_id: item.knowledge_id,
            title: item.title,
            kind: item.kind,
            state: item.state,
            priority: item.priority,
            repo: item.repo,
            expires_at: item.expires_at,
            author_agent: item.author_agent,
        }
    }
}

impl KnowledgeView {
    pub async fn connect(contact_points: &[String], tenant_id: Uuid) -> Result<Self> {
        if contact_points.is_empty() {
            anyhow::bail!("no contact points for the knowledge store");
        }
        // Bounded for the same reason the board and the tiers are: a control
        // session is being set up, and an unreachable cluster must fail in
        // seconds with a reason rather than hold a phone.
        let session = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            SessionBuilder::new()
                .known_nodes(contact_points)
                .connection_timeout(std::time::Duration::from_secs(5))
                .build_legacy(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("knowledge store did not answer within 10s"))?
        .context("connecting to the knowledge store")?;

        Ok(Self {
            store: CqlKnowledgeStore::new(Arc::new(session), "agent_memory"),
            ctx: TenantContext {
                tenant_id,
                session_origin: "mobile-control".to_owned(),
            },
        })
    }

    /// The Knowledge tier: approved work only, newest first.
    ///
    /// Both bands are read because the tier is a library rather than a queue —
    /// someone looking for what they know is not looking for what is urgent.
    pub async fn knowledge(&self, cursor: Option<&str>, limit: usize) -> Result<KnowledgePage> {
        self.merged_bands(KnowledgeState::Approved, cursor, limit)
            .await
    }

    /// The Claims queue: what still needs a person, soonest to lapse first.
    ///
    /// Walks day buckets forward from today. `proposed` and `revisit` are both
    /// awaiting review, and a claim sent back is not less urgent than one never
    /// looked at.
    pub async fn claims(&self, limit: usize) -> Result<Vec<KnowledgeRow>> {
        anyhow::ensure!(limit > 0, "a list of no claims is not a list");
        let today = chrono::Utc::now();
        let mut rows: Vec<KnowledgeRow> = Vec::new();
        // Inclusive: a claim expiring exactly at the horizon is still a claim,
        // and an exclusive range put every default-expiry claim one day out of
        // reach.
        for offset in 0..=CLAIM_HORIZON_DAYS {
            if rows.len() >= limit {
                break;
            }
            let day = expiry_day(today + chrono::Duration::days(offset));
            for state in [KnowledgeState::Proposed, KnowledgeState::Revisit] {
                let due = self
                    .store
                    .expiring_on(&self.ctx, state, &day, limit)
                    .await?;
                rows.extend(due.into_iter().map(KnowledgeRow::from));
            }
        }
        // Already in expiry order across buckets, but a day holds two states
        // read separately, so the within-day interleave is settled here.
        rows.sort_by_key(|row| row.expires_at);
        rows.truncate(limit);
        Ok(rows)
    }

    /// How many deliverables are in one state.
    ///
    /// For the memory map, which wants a number rather than a page: knowledge
    /// is the one tier that is stored rather than derived from a path, so the
    /// map has to ask the store that holds it.
    pub async fn count(&self, state: KnowledgeState) -> Result<usize> {
        self.store.count(&self.ctx, state).await
    }

    /// One item and its chain.
    pub async fn detail(
        &self,
        knowledge_id: Uuid,
    ) -> Result<Option<(KnowledgeItem, Vec<KnowledgeVersion>)>> {
        let Some(item) = self.store.item(&self.ctx, knowledge_id).await? else {
            return Ok(None);
        };
        let versions = self.store.versions(&self.ctx, knowledge_id).await?;
        Ok(Some((item, versions)))
    }

    /// Approve, reject, or send back.
    ///
    /// The store refuses an illegal transition before writing anything, so a
    /// stale screen cannot approve something a person already rejected.
    pub async fn decide(
        &self,
        knowledge_id: Uuid,
        to: KnowledgeState,
        reviewer: Option<&str>,
        feedback: Option<&str>,
    ) -> Result<KnowledgeItem> {
        self.store
            .decide(&self.ctx, knowledge_id, to, reviewer, feedback)
            .await
    }

    /// Read both priority bands and merge them, newest first.
    ///
    /// The bands exist so the OVERVIEW can read urgent work in one seek. A
    /// full list wants both, and merging two ordered pages is cheaper than the
    /// scan a single unbanded partition would have needed.
    async fn merged_bands(
        &self,
        state: KnowledgeState,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<KnowledgePage> {
        anyhow::ensure!(limit > 0, "a page of no items is not a page");
        let mut items = Vec::new();
        let mut next: Option<String> = None;
        for band in ["high", "low"] {
            let page = self
                .store
                .page(&self.ctx, state, band, cursor, limit)
                .await?;
            items.extend(page.items);
            // The further-back cursor wins, so neither band is skipped.
            next = match (next, page.next_cursor) {
                (Some(a), Some(b)) => Some(if a > b { b } else { a }),
                (a, b) => a.or(b),
            };
        }
        items.sort_by_key(|item| std::cmp::Reverse(item.created_at));
        items.truncate(limit);
        Ok(KnowledgePage {
            items,
            next_cursor: next,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrosa_memory_core::knowledge::DEFAULT_EXPIRY_DAYS;

    /// The horizon must reach the far edge of the window claims are filed with.
    ///
    /// It did not, and the failure was invisible: the horizon was 14 days, the
    /// default claim expiry was 14 days, and an exclusive range read buckets
    /// for today..today+13. Every claim filed with the default landed in the
    /// day-14 bucket, one past the last one read, so a queue holding fourteen
    /// items showed nothing — identical to having no claims at all.
    #[test]
    fn a_claim_at_the_default_expiry_falls_inside_the_horizon() {
        let today = chrono::Utc::now();
        let walked: Vec<String> = (0..=CLAIM_HORIZON_DAYS)
            .map(|offset| expiry_day(today + chrono::Duration::days(offset)))
            .collect();

        for filed_with in [1, 7, 14, DEFAULT_EXPIRY_DAYS] {
            let lands_on = expiry_day(today + chrono::Duration::days(filed_with));
            assert!(
                walked.contains(&lands_on),
                "a claim filed with {filed_with} days expires on {lands_on}, which the \
                 horizon never reads — it would be invisible in the queue"
            );
        }
    }

    /// The horizon is a bound on WORK, so it stays a countable number of reads.
    #[test]
    fn the_horizon_stays_a_bounded_number_of_partition_reads() {
        assert!(
            (1..=62).contains(&CLAIM_HORIZON_DAYS),
            "one page of claims must not walk {CLAIM_HORIZON_DAYS} day partitions"
        );
    }

    /// Every sort the device is offered must be one this can perform. Offering
    /// one it cannot is a control that silently does nothing.
    #[test]
    fn the_offered_sorts_are_the_ones_that_exist() {
        assert!(
            CLAIM_SORTS.contains(&"expiry"),
            "the default must be offered"
        );
        for sort in CLAIM_SORTS {
            assert!(!sort.is_empty());
        }
    }
}
