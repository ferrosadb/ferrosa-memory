//! Module: What the phone can ask a machine about its memory tiers.
//! Correctness: Correct when a number on the map came from rows that exist,
//! when a tier with nothing in it still appears, and when a sort the machine
//! cannot actually perform is never offered.
//! Last revised: 2026-08-24
//! Last changed: new — the DIKW map and a tier's contents.
//!
//! # Why the counting happens here and not in CQL
//!
//! A tier is derived from a source root through the rule table, so there is no
//! column to `GROUP BY`. Materialising one would be the second truth that
//! `TierRules` exists to avoid. The rows are read and counted in process,
//! bounded by [`SCAN_LIMIT`], and every answer says whether the bound bit.

// This module reads rows through scylla 0.15's LegacySession API, the same
// choice cql_storage.rs made and for the same reason: the legacy API is
// deprecated upstream but has stable semantics, and migrating to the generic
// deserialization API is a separate piece of work across every call site.
// Scoped to the module rather than sprinkled per call, so the decision is
// stated once and a NEW deprecation still surfaces.
#![allow(deprecated)]
use anyhow::{Context, Result};
use std::sync::Arc;

use ferrosa_memory_core::tier_store::{
    CqlTierStore, EntitySource, TierStore, TierSummary, load_rules, summarise,
};
use ferrosa_memory_core::tiers::{Tier, TierRules};
use ferrosa_memory_core::types::TenantContext;
use scylla::SessionBuilder;
use uuid::Uuid;

/// The single-user tenant, matching `task_board::TENANT_ID`.
const TENANT_ID: Uuid = Uuid::from_u128(1);

/// How many source rows one answer will read.
///
/// Not a page size — the page is much smaller. This is the bound on the scan
/// that produces a count, and a summary that hit it says so rather than
/// reporting a number that looks complete.
const SCAN_LIMIT: usize = 50_000;

/// Sorts this machine can actually perform.
///
/// `most used` and `most shared` are in the design ([D10]) and are NOT here:
/// retrieval counts and share counts are not recorded against a source row, so
/// offering them would be a control that silently does nothing.
///
/// [D10]: ../../../specs/knowledge-tiers/decisions.md
pub const AVAILABLE_SORTS: &[&str] = &["recent", "title"];

pub struct MemoryView {
    store: CqlTierStore,
    ctx: TenantContext,
}

/// One row of a tier's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryItem {
    pub entity_id: Uuid,
    pub session_id: Uuid,
    pub title: String,
    pub tier: Tier,
    pub source_path: String,
    pub source_root: Option<String>,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
}

/// A page of a tier, and how much of the tier it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemPage {
    pub items: Vec<MemoryItem>,
    /// How many matched, before paging. The page is a window on this.
    pub matched: usize,
    /// The scan hit [`SCAN_LIMIT`], so `matched` is a floor and not a total.
    pub truncated: bool,
}

impl MemoryView {
    pub async fn connect(contact_points: &[String]) -> Result<Self> {
        if contact_points.is_empty() {
            anyhow::bail!("no contact points for the memory store");
        }
        // Bounded for the same reason the board is: a control session is being
        // set up, and an unreachable cluster must fail in seconds with a reason
        // rather than hold a phone.
        let session = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            SessionBuilder::new()
                .known_nodes(contact_points)
                .connection_timeout(std::time::Duration::from_secs(5))
                .build_legacy(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("memory store did not answer within 10s"))?
        .context("connecting to the memory store")?;

        Ok(Self {
            store: CqlTierStore::new(Arc::new(session), "agent_memory"),
            ctx: TenantContext {
                tenant_id: TENANT_ID,
                session_origin: "mobile-control".to_owned(),
            },
        })
    }

    /// The DIKW map.
    pub async fn map(&self) -> Result<TierSummary> {
        summarise(&self.store, &self.ctx, SCAN_LIMIT).await
    }

    /// One tier's contents, optionally filtered by a search term.
    ///
    /// Search is a case-insensitive substring of the title or the source path.
    /// Deliberately not the semantic search the memory system has: this list is
    /// how someone finds a thing they already know the name of, and a ranked
    /// semantic answer would reorder the list under them while they select.
    pub async fn items(
        &self,
        tier: Tier,
        query: Option<&str>,
        sort: &str,
        offset: usize,
        limit: usize,
    ) -> Result<ItemPage> {
        let (_, rules) = load_rules(&self.store, &self.ctx).await?;
        let promoted = self.promoted_tiers().await?;
        let page = self.store.sources(&self.ctx, SCAN_LIMIT).await?;

        let needle = query.map(str::to_lowercase);
        let mut matched: Vec<MemoryItem> = page
            .sources
            .into_iter()
            .map(|source| {
                let item_tier = tier_of_source(&source, &promoted, &rules);
                (source, item_tier)
            })
            .filter(|(_, item_tier)| *item_tier == tier)
            .map(|(source, item_tier)| MemoryItem {
                entity_id: source.entity_id,
                session_id: source.session_id,
                title: source.title,
                tier: item_tier,
                source_path: source.source_path,
                source_root: source.source_root,
                recorded_at: source.recorded_at,
            })
            .filter(|item| match &needle {
                None => true,
                Some(needle) => {
                    item.title.to_lowercase().contains(needle)
                        || item.source_path.to_lowercase().contains(needle)
                }
            })
            .collect();

        sort_items(&mut matched, sort);
        let total = matched.len();
        let items = matched.into_iter().skip(offset).take(limit).collect();
        Ok(ItemPage {
            items,
            matched: total,
            truncated: page.truncated,
        })
    }

    async fn promoted_tiers(&self) -> Result<std::collections::HashMap<Uuid, Tier>> {
        Ok(self
            .store
            .promotions(&self.ctx, SCAN_LIMIT)
            .await?
            .into_iter()
            .map(|promotion| (promotion.entity_id, promotion.tier))
            .collect())
    }
}

/// A source row's tier, with promotion taking precedence exactly as it does in
/// `tiers::resolve` and in `summarise`. Three call sites, one rule.
fn tier_of_source(
    source: &EntitySource,
    promoted: &std::collections::HashMap<Uuid, Tier>,
    rules: &TierRules,
) -> Tier {
    if let Some(tier) = promoted.get(&source.entity_id) {
        return *tier;
    }
    source
        .source_root
        .as_deref()
        .and_then(|root| rules.tier_of_root(root))
        .unwrap_or(Tier::Data)
}

/// Order a tier's contents.
///
/// An unrecognised sort falls back to `recent` rather than erroring, because
/// the caller is a phone whose build may be older than this one — but the
/// frame states which sorts exist, so a UI has no reason to send another.
fn sort_items(items: &mut [MemoryItem], sort: &str) {
    match sort {
        "title" => items.sort_by_key(|item| item.title.to_lowercase()),
        // Newest first, by the timestamp on the row. Reversing the scan order
        // would have been reverse-id order wearing the word "recent".
        _ => items.sort_by_key(|item| std::cmp::Reverse(item.recorded_at)),
    }
}
