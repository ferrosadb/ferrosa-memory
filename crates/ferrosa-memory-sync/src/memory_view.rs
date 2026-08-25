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
//! never scanned: a page is a seek and a count is a server-side COUNT(*).

// This module reads rows through scylla 0.15's LegacySession API, the same
// choice cql_storage.rs made and for the same reason: the legacy API is
// deprecated upstream but has stable semantics, and migrating to the generic
// deserialization API is a separate piece of work across every call site.
// Scoped to the module rather than sprinkled per call, so the decision is
// stated once and a NEW deprecation still surfaces.
#![allow(deprecated)]
use anyhow::{Context, Result};
use std::sync::Arc;

use ferrosa_memory_core::tier_store::page_key;
use ferrosa_memory_core::tier_store::{
    CqlTierStore, EntitySource, TierStore, TierSummary, load_rules, summarise,
};
use ferrosa_memory_core::tiers::{Tier, TierRules};
use ferrosa_memory_core::types::TenantContext;
use scylla::SessionBuilder;
use uuid::Uuid;

// The tenant is a PARAMETER, not a constant.
//
// This read the task board's tenant (`Uuid::from_u128(1)`), because the board
// and the memory both live on this cluster and it looked like one thing. They
// are not one thing. On this machine the board's tenant holds 118 entities and
// no source rows at all, while the memory holds 79,284 entities and 69,683
// source rows under a tenant derived from the authenticated principal.
//
// Nothing failed. `summarise` counted the rows it was asked for, found none,
// and the phone would have shown a reachable memory plane with four tiers at
// zero — indistinguishable from a machine that has not been seeded. That is
// the same mistake `seed-tiers` made when it defaulted its tenant, and the
// same answer applies: a tenant is not something to guess.

/// Sorts this machine can actually perform.
///
/// `most used` and `most shared` are in the design ([D10]) and are NOT here:
/// retrieval counts and share counts are not recorded against a source row, so
/// offering them would be a control that silently does nothing.
///
/// [D10]: ../../../specs/knowledge-tiers/decisions.md
/// The orders a paged tier can actually be served in.
///
/// One. `title` was here and is gone: sorting by title means ordering the
/// whole tier before the first page can be drawn, which is the 15-second scan
/// that migration 055 exists to remove. The rows are stored in recency order
/// and a page is a seek along it; any other order is a different table, not a
/// different query.
///
/// It stays a list because a device reads it to decide which controls to
/// offer, and a device that is told there is one order will not draw a
/// chooser for orders that do not exist.
pub const AVAILABLE_SORTS: &[&str] = &["recent"];

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

/// A page of a tier, and where to resume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemPage {
    pub items: Vec<MemoryItem>,
    /// Where the next page starts, or `None` at the end of the tier.
    ///
    /// There is deliberately no `matched` total. Counting the matches in a
    /// tier means reading the tier, which is the 15-second scan this page
    /// exists to avoid, and a count that costs more than the page it labels is
    /// not worth its price. The map already reports each tier's size.
    pub next_cursor: Option<TierCursor>,
}

/// Where a tier's next page resumes: one cursor per root, because a tier is
/// the union of its roots and each is paged independently.
///
/// Opaque by intent. A device that understood the encoding could ask for a
/// cursor the store never issued, and a `page_key` is not a capability -- it
/// is a position in a partition the caller has already been shown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierCursor {
    per_root: std::collections::BTreeMap<String, String>,
}

impl TierCursor {
    pub fn get(&self, root: &str) -> Option<&str> {
        self.per_root.get(root).map(String::as_str)
    }

    pub fn set(&mut self, root: String, page_key: String) {
        self.per_root.insert(root, page_key);
    }

    pub fn is_empty(&self) -> bool {
        self.per_root.is_empty()
    }

    /// Wire form: `root\u{1}key` pairs joined by `\u{2}`.
    ///
    /// Control characters rather than a punctuation separator because a root
    /// is a path and a path may contain almost anything else. Roots and
    /// page_keys cannot contain these, so the encoding has no escape rules to
    /// get wrong.
    pub fn encode(&self) -> String {
        self.per_root
            .iter()
            .map(|(root, key)| format!("{root}\u{1}{key}"))
            .collect::<Vec<_>>()
            .join("\u{2}")
    }

    /// Parse a cursor a device sent back. A malformed cursor is refused rather
    /// than treated as "start from the beginning": silently restarting a list
    /// the operator was halfway down looks like the list looping.
    pub fn decode(raw: &str) -> Result<Self> {
        let mut per_root = std::collections::BTreeMap::new();
        for pair in raw.split('\u{2}') {
            if pair.is_empty() {
                continue;
            }
            let (root, key) = pair
                .split_once('\u{1}')
                .ok_or_else(|| anyhow::anyhow!("malformed memory cursor: no root/key separator"))?;
            anyhow::ensure!(!root.is_empty(), "malformed memory cursor: empty root");
            anyhow::ensure!(!key.is_empty(), "malformed memory cursor: empty page key");
            per_root.insert(root.to_owned(), key.to_owned());
        }
        Ok(Self { per_root })
    }
}

impl MemoryView {
    pub async fn connect(contact_points: &[String], tenant_id: Uuid) -> Result<Self> {
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
                tenant_id,
                session_origin: "mobile-control".to_owned(),
            },
        })
    }

    /// The DIKW map.
    pub async fn map(&self) -> Result<TierSummary> {
        // The limit is vestigial: summarise counts per root and reads no
        // source rows at all. Passing a scan size here would suggest it still
        // scans.
        summarise(&self.store, &self.ctx, 0).await
    }

    /// One page of a tier, newest first, resumable by cursor.
    ///
    /// This used to read the whole tenant partition, derive every row's tier,
    /// sort, and keep a window of twenty. Measured on 69,683 rows that was
    /// 15.0 s per page, paid again for every page, plus a second scan the same
    /// size for promotions -- and sustained scans of one partition were
    /// starving control-session writes on the same keyspace. Migration 055
    /// stores the same facts partitioned by root and ordered by recency, so a
    /// page is a seek: measured 375 ms for the largest tier and 1 ms for the
    /// smallest.
    ///
    /// A tier is the union of its roots, so this pages each root and merges
    /// them. `page_key` embeds the timestamp, so comparing keys across roots
    /// IS comparing recency, and the merge needs no second sort key.
    ///
    /// Search is a case-insensitive substring of the title or the source path,
    /// applied to the rows on the page. It is deliberately not the semantic
    /// search the memory system has: that is a ranked top-50 window, which is
    /// the right shape for "find the thing about X" and the wrong shape for
    /// paging a list of 63,163.
    pub async fn items(
        &self,
        tier: Tier,
        query: Option<&str>,
        cursor: Option<&TierCursor>,
        limit: usize,
    ) -> Result<ItemPage> {
        anyhow::ensure!(limit > 0, "a page of no items is not a page");
        let (_, rules) = load_rules(&self.store, &self.ctx).await?;
        let promoted = self.promoted_tiers().await?;

        // The roots this tier is made of. A tier with none has no rows to
        // page, which is a real answer and not an error.
        let roots: Vec<String> = rules
            .entries()
            .into_iter()
            .filter(|(_, root_tier)| *root_tier == tier)
            .map(|(root, _)| root)
            .collect();
        if roots.is_empty() {
            return Ok(ItemPage::default());
        }

        // Ask each root for a whole page. Any single root could supply the
        // entire merged page, so asking for less would under-fill it.
        let mut candidates: Vec<(String, MemoryItem)> = Vec::new();
        for root in &roots {
            let from = cursor.and_then(|cursor| cursor.get(root));
            let page = self
                .store
                .sources_page(&self.ctx, root, from, limit)
                .await?;
            for source in page.sources {
                let key = page_key(source.recorded_at, source.entity_id)?;
                candidates.push((key, self.item_of(source, &promoted, &rules)));
            }
        }

        // Newest first across roots.
        candidates.sort_by(|left, right| right.0.cmp(&left.0));

        let needle = query.map(str::to_lowercase);
        let mut next = TierCursor::default();
        let mut items = Vec::with_capacity(limit);
        let mut consumed = 0usize;
        for (key, item) in candidates {
            if consumed == limit {
                break;
            }
            consumed += 1;
            // The cursor advances over every row READ, matched or not.
            // Advancing only over matches would re-read the filtered rows on
            // the next page and never make progress through a tier where
            // nothing matches.
            if let Some(root) = item.source_root.clone() {
                next.set(root, key);
            }
            let keep = match &needle {
                None => true,
                Some(needle) => {
                    item.title.to_lowercase().contains(needle)
                        || item.source_path.to_lowercase().contains(needle)
                }
            };
            if keep {
                items.push(item);
            }
        }

        Ok(ItemPage {
            items,
            // A full page read means there is more behind it. This is the
            // truthful signal available without counting the tier, which is
            // the scan this change exists to remove.
            next_cursor: (consumed == limit).then_some(next),
        })
    }

    /// One source row as a list item, with promotion applied exactly as
    /// `summarise` applies it.
    fn item_of(
        &self,
        source: EntitySource,
        promoted: &std::collections::HashMap<Uuid, Tier>,
        rules: &TierRules,
    ) -> MemoryItem {
        let tier = tier_of_source(&source, promoted, rules);
        MemoryItem {
            entity_id: source.entity_id,
            session_id: source.session_id,
            title: source.title,
            tier,
            source_path: source.source_path,
            source_root: source.source_root,
            recorded_at: source.recorded_at,
        }
    }

    async fn promoted_tiers(&self) -> Result<std::collections::HashMap<Uuid, Tier>> {
        Ok(self
            .store
            .promotions(
                &self.ctx,
                ferrosa_memory_core::tier_store::PROMOTION_READ_LIMIT,
            )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A root is a path, and a path can contain almost any punctuation. The
    /// separators are control characters precisely so this round-trips without
    /// escape rules -- including the roots that look like they would break it.
    #[test]
    fn a_cursor_round_trips_through_awkward_roots() {
        let mut cursor = TierCursor::default();
        cursor.set(
            "/Users/bkearns/src/research/skills".to_owned(),
            "1767225600000-00000000-0000-0000-0000-000000000001".to_owned(),
        );
        cursor.set(
            "a root, with a comma: and a colon".to_owned(),
            "1767225600001-00000000-0000-0000-0000-000000000002".to_owned(),
        );
        let decoded = TierCursor::decode(&cursor.encode()).expect("decodes");
        assert_eq!(decoded, cursor);
    }

    /// The empty cursor is the first page, not an error.
    #[test]
    fn an_empty_cursor_decodes_to_nothing() {
        let decoded = TierCursor::decode("").expect("decodes");
        assert!(decoded.is_empty());
    }

    /// Refused, NOT silently treated as the first page. Restarting a list the
    /// operator is halfway down looks like the list looping, and looping is a
    /// far harder thing to report than an error is.
    #[test]
    fn a_malformed_cursor_is_refused_rather_than_restarted() {
        assert!(TierCursor::decode("no-separator-here").is_err());
        assert!(TierCursor::decode("\u{1}key-with-no-root").is_err());
        assert!(TierCursor::decode("root-with-no-key\u{1}").is_err());
    }

    /// The frame advertises what the store can serve. A device offering a
    /// `title` order would be offering a scan.
    #[test]
    fn only_the_orders_a_seek_can_serve_are_advertised() {
        assert_eq!(AVAILABLE_SORTS, &["recent"]);
    }
}
