//! Module: Read this machine's classification rules for the control channel.
//! Correctness: Correct when a rule nothing can reach is reported as
//! unreachable rather than as merely unused, when an alias pointing at no rule
//! is reported separately, and when a count nobody took is absent rather than
//! zero.
//! Last revised: 2026-08-26
//! Last changed: New.
//!
//! # Why this is a reader and not a second implementation
//!
//! The judgement — which rules are reachable, which aliases dangle, how a rule
//! reads as datalog — lives in `ferrosa_memory_core::rules_view` and is tested
//! there without a cluster. This file only fetches rows and hands them over.
//! Two copies of "is this rule reachable" would eventually disagree, and the
//! one people see would be whichever this file happened to implement.
//!
//! # Why unreachable is computed here and not in the app
//!
//! The phone cannot tell "this rule has no aliases" from "aliases were not
//! sent". A reachability the machine states is a fact; one the app infers from
//! an empty list is a guess that reads identically.

use std::sync::Arc;

use anyhow::{Context, Result};
use ferrosa_memory_core::config::FerrosaCqlConfig;
use ferrosa_memory_core::cql_storage::CqlStorage;
use ferrosa_memory_core::rules_view::{
    DanglingAlias, TierRuleRow, dangling_aliases, tier_rule_rows,
};
use ferrosa_memory_core::tier_store::{CqlTierStore, MAX_ROOTS_COUNTED, TierStore};
use ferrosa_memory_core::types::TenantContext;
use scylla::SessionBuilder;
use uuid::Uuid;

/// What one rule has actually classified.
pub struct RuleYield {
    /// The rule, as the listing knows it.
    pub rule: TierRuleRow,
    /// A page of the items it classified, newest first.
    pub items: Vec<ferrosa_memory_core::tier_store::EntitySource>,
    /// Where to resume, or `None` at the end.
    pub next_cursor: Option<String>,
}

/// Everything the Rules tab needs for one machine.
pub struct RulesSnapshot {
    pub rules: Vec<TierRuleRow>,
    pub dangling: Vec<DanglingAlias>,
    /// Active, approved expert-system rules. These are a different population
    /// from tier rules and must be shown as such rather than silently omitted.
    pub registry: Vec<ferrosa_memory_core::datalog::EffectiveRuleEntry>,
}

pub struct RulesView {
    store: CqlTierStore,
    registry_store: CqlStorage,
    ctx: TenantContext,
}

impl RulesView {
    pub async fn connect(contact_points: &[String], tenant_id: Uuid) -> Result<Self> {
        if contact_points.is_empty() {
            anyhow::bail!("no contact points for the rules store");
        }
        // Bounded on the same terms as the knowledge and board views: a
        // control session is being set up, and an unreachable cluster must
        // fail in seconds with a reason rather than hold a phone open.
        // build_legacy is deprecated upstream and still what every other store
        // in this crate uses. Annotated rather than migrated: switching one
        // call site to the new deserialization API would leave this store
        // reading rows differently from the ones beside it, and that is a
        // change to make deliberately across the crate, not incidentally here.
        #[allow(deprecated)]
        let session = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            SessionBuilder::new()
                .known_nodes(contact_points)
                .connection_timeout(std::time::Duration::from_secs(5))
                .build_legacy(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("rules store did not answer within 10s"))?
        .context("connecting to the rules store")?;

        let session = Arc::new(session);
        let registry_store = CqlStorage::connect(&FerrosaCqlConfig {
            contact_points: contact_points.to_vec(),
            keyspace: "agent_memory".into(),
            replication_factor: 1,
            consistency: "local_quorum".into(),
            username: String::new(),
            password: String::new(),
            admin_username: None,
            admin_password: None,
            tls_ca_path: None,
            tls_skip_hostname_verify: false,
        })
        .await
        .context("connecting to the rule registry")?;
        Ok(Self {
            store: CqlTierStore::new(session, "agent_memory"),
            registry_store,
            ctx: TenantContext {
                tenant_id,
                session_origin: "mobile-control".to_owned(),
            },
        })
    }

    /// The rules and the aliases that reach them.
    ///
    /// One read of each table, joined in memory. Both are small — they are
    /// per-root, not per-item — so this does not need paging the way the
    /// corpus does.
    pub async fn snapshot(&self) -> Result<RulesSnapshot> {
        let rules = self
            .store
            .root_rules(&self.ctx)
            .await
            .context("reading the tier rules")?;
        let aliases = self
            .store
            .aliases(&self.ctx)
            .await
            .context("reading the root aliases")?;

        let registry = ferrosa_memory_core::datalog::load_effective_rule_entries(
            &self.registry_store,
            &self.ctx,
            None,
        )
        .await
        .context("reading active registry rules")?;
        Ok(RulesSnapshot {
            rules: tier_rule_rows(&rules, &aliases),
            dangling: dangling_aliases(&rules, &aliases),
            registry,
        })
    }

    /// What one rule has classified, a page at a time.
    ///
    /// Paged rather than counted: a rule's yield is per-ITEM, unlike the rule
    /// list itself, so it is the one part of this screen that can be large. A
    /// count alone would also be the wrong answer — the question behind
    /// "delete everything this classified" is *which things*, and only a list
    /// answers that.
    pub async fn rule_yield(
        &self,
        root: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Option<RuleYield>> {
        let snapshot = self.snapshot().await?;
        // The rule is looked up rather than trusted from the caller: a root
        // the device names but the store does not have is a stale screen, and
        // answering with an empty page would read as "this rule classified
        // nothing" rather than "this rule is gone".
        let Some(rule) = snapshot.rules.into_iter().find(|row| row.root == root) else {
            return Ok(None);
        };
        let page = self
            .store
            .sources_page(&self.ctx, root, cursor, limit)
            .await
            .with_context(|| format!("reading what {root} classified"))?;
        Ok(Some(RuleYield {
            rule,
            items: page.sources,
            next_cursor: page.next_cursor,
        }))
    }

    /// How many items each rule has classified.
    ///
    /// Deliberately NOT part of `snapshot`. Counting is one round trip per
    /// root and took 3.3 s on this store, which the device spent looking at a
    /// spinner with a list already available behind it. The list answers "what
    /// rules exist"; the counts answer "how much did each do", and the second
    /// question must never delay the first.
    ///
    /// A root whose count fails is OMITTED, never zero. Past the bound nothing
    /// is counted at all rather than some — a subset of counts rendered beside
    /// uncounted rules reads as "those classified nothing".
    pub async fn counts(&self) -> std::collections::BTreeMap<String, usize> {
        let mut counts = std::collections::BTreeMap::new();
        let Ok(rules) = self.store.root_rules(&self.ctx).await else {
            tracing::warn!("could not read the rules to count them");
            return counts;
        };
        if rules.len() > MAX_ROOTS_COUNTED {
            tracing::warn!(
                roots = rules.len(),
                "past {MAX_ROOTS_COUNTED} roots; rule yields are not counted"
            );
            return counts;
        }
        for rule in rules {
            match self.store.count_under_root(&self.ctx, &rule.root).await {
                Ok(count) => {
                    counts.insert(rule.root, count as usize);
                }
                Err(error) => {
                    tracing::warn!(root = %rule.root, %error, "could not count a rule's yield");
                }
            }
        }
        counts
    }
}
