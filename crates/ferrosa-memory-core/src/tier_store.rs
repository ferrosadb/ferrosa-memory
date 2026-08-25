//! Storage for the DIKW tier plane: where an entity came from, the rules that
//! turn a location into a tier, and the promotions that override them.
//!
//! The tier itself is never stored on the entity. It is recomputed from three
//! stored inputs — the entity's source path, the alias set, and the root rules
//! — so re-tiering a directory is one rule edit rather than a rewrite of every
//! row underneath it. A promotion is stored, because a person's decision is
//! not derivable from anything.
//!
//! ## The one cache, and why it is visible
//!
//! `entity_source.source_root` is a materialization of "resolve this path
//! against the alias set", kept so that "everything under `research/corpus`"
//! is an indexed read instead of a full scan. Editing an alias makes stored
//! roots stale. Rather than hide that, [`TierStore::restate_sources`]
//! re-resolves and reports how many rows moved — and a test holds the line
//! that an alias edit alone does NOT silently fix them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::cql_storage::{CqlSession, build_col_map, cql_get};
use crate::tiers::{Promotion, RootResolver, Tier, TierAssignment, TierRules};
use crate::types::{FactSet, TenantContext, Term};

/// Where one entity came from, and what that resolved to when it was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitySource {
    pub entity_id: Uuid,
    /// Which partition of `entity_store` holds it. An entity id alone cannot
    /// be read back, so without this nothing here can reach the entity.
    pub session_id: Uuid,
    /// Display cache, refreshed by `restate_sources`. Stale after a rename.
    pub title: String,
    /// As supplied by the caller. This is the truth; the rest is derived.
    pub source_path: String,
    /// The canonical root, or `None` when no alias covered the path.
    pub source_root: Option<String>,
    /// Which alias produced `source_root`. `None` when nothing matched.
    pub matched_alias: Option<String>,
    pub recorded_at: DateTime<Utc>,
}

/// What ingest supplies. The root, alias and timestamp are worked out here,
/// so a caller cannot record a resolution that disagrees with the alias set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceDraft {
    pub entity_id: Uuid,
    pub session_id: Uuid,
    pub title: String,
    pub source_path: String,
}

/// Two spellings of one location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootAlias {
    pub alias_prefix: String,
    pub canonical_root: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// A root's tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootRule {
    pub root: String,
    pub tier: Tier,
    pub created_by: String,
    /// Why this root earns this tier. Read by whoever questions it later.
    pub note: String,
    pub created_at: DateTime<Utc>,
}

/// A person overriding the derived tier for one entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierPromotion {
    pub entity_id: Uuid,
    pub tier: Tier,
    pub actor: String,
    pub reason: String,
    pub created_at: DateTime<Utc>,
}

/// What a restate actually did. Every field is a number someone can act on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestateReport {
    pub examined: usize,
    /// Rows whose resolved root changed and were rewritten.
    pub rewritten: usize,
    /// Rows whose path no alias covers. These have no root and no tier beyond
    /// the Data floor; a growing count means the alias set has a hole.
    pub unresolved: usize,
    /// The scan hit its limit, so `examined` is not the whole tenant.
    pub truncated: bool,
}

/// A bounded read of stored sources, honest about the bound.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourcePage {
    pub sources: Vec<EntitySource>,
    /// There is more behind the limit. Callers that summarise or reconcile
    /// must not report completeness when this is set.
    pub truncated: bool,
}

/// One page of a root's sources, newest first, and where to resume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootPage {
    pub sources: Vec<EntitySource>,
    /// The cursor for the NEXT page, or `None` at the end of the root.
    ///
    /// Opaque to the caller by intent: it is a `page_key`, and a device that
    /// treats it as one could construct a cursor the store never issued.
    pub next_cursor: Option<String>,
}

/// The clustering key of `entity_source_by_root`: `{millis:013}-{entity_id}`.
///
/// Unique by construction, and lexicographically ordered by time because the
/// milliseconds are zero-padded to a fixed width. Uniqueness is the point --
/// `recorded_at` alone repeats within a single ingest, and a cursor over tied
/// rows either repeats them or steps over them, the second of which loses rows
/// from a list that still looks complete.
///
/// Timestamps before 1970 produce a negative millisecond count, which does not
/// zero-pad into a sortable fixed width. That cannot arise from an ingest --
/// `recorded_at` is stamped at write time -- so it is a corrupt row rather
/// than a case to handle, and this says so instead of emitting a key that
/// silently sorts in the wrong place.
pub fn page_key(recorded_at: DateTime<Utc>, entity_id: Uuid) -> anyhow::Result<String> {
    let millis = recorded_at.timestamp_millis();
    anyhow::ensure!(
        millis >= 0,
        "source recorded_at {recorded_at} is before 1970; it cannot be ordered \
         in entity_source_by_root"
    );
    Ok(format!("{millis:013}-{entity_id}"))
}

pub trait TierStore: Send + Sync {
    fn record_source(
        &self,
        ctx: &TenantContext,
        draft: SourceDraft,
    ) -> impl std::future::Future<Output = anyhow::Result<EntitySource>> + Send;

    fn source_of(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<EntitySource>>> + Send;

    fn sources_under_root(
        &self,
        ctx: &TenantContext,
        root: &str,
        limit: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<EntitySource>>> + Send;

    fn put_alias(
        &self,
        ctx: &TenantContext,
        alias: RootAlias,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    fn aliases(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<RootAlias>>> + Send;

    fn put_root_rule(
        &self,
        ctx: &TenantContext,
        rule: RootRule,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    fn root_rules(
        &self,
        ctx: &TenantContext,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<RootRule>>> + Send;

    fn promote(
        &self,
        ctx: &TenantContext,
        promotion: TierPromotion,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    fn promotion_of(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<TierPromotion>>> + Send;

    /// Every stored source for the tenant, up to `limit`.
    fn sources(
        &self,
        ctx: &TenantContext,
        limit: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<SourcePage>> + Send;

    /// Every promotion for the tenant, up to `limit`. Promotions are human
    /// decisions and stay few; the limit exists so a runaway cannot become an
    /// unbounded read.
    fn promotions(
        &self,
        ctx: &TenantContext,
        limit: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<Vec<TierPromotion>>> + Send;

    /// Re-resolve every stored source against the current alias set and
    /// rewrite the rows that moved. Bounded by `limit`, and says so when the
    /// bound bites.
    fn restate_sources(
        &self,
        ctx: &TenantContext,
        limit: usize,
    ) -> impl std::future::Future<Output = anyhow::Result<RestateReport>> + Send;
}

/// What a seed actually wrote, so a caller can report it rather than assume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeedReport {
    pub rules_written: usize,
    pub aliases_written: usize,
}

/// Write the builtin root rules, and aliases for one checkout of a repository.
///
/// Two halves, and only one of them is universal. The root -> tier MAPPING is
/// a decision about what kind of thing lives where, and travels with the
/// build. The ALIASES are filesystem paths, which are specific to a machine
/// and a person, so they are supplied rather than guessed.
///
/// Idempotent: both tables are keyed by their own primary key, so re-seeding
/// overwrites rather than duplicating. That matters because the obvious way
/// to fix a wrong alias is to run this again.
///
/// # Why `research_root` and not a scan
///
/// Walking the filesystem to find candidate roots would tie tiering to what
/// happens to be on this disk today, and a machine with a different layout
/// would silently tier the same content differently. A path stated once is a
/// decision; a path discovered is an accident.
pub async fn seed_tier_rules(
    store: &impl TierStore,
    ctx: &TenantContext,
    // Absolute path of the research checkout, e.g. /Users/you/src/research.
    research_root: &str,
    actor: &str,
) -> anyhow::Result<SeedReport> {
    let now = Utc::now();
    let mut report = SeedReport::default();

    for (root, tier) in TierRules::builtin().entries() {
        store
            .put_root_rule(
                ctx,
                RootRule {
                    root: root.clone(),
                    tier,
                    created_by: actor.to_owned(),
                    note: "builtin: ships with the build".to_owned(),
                    created_at: now,
                },
            )
            .await?;
        report.rules_written += 1;
    }

    // An identity alias for every builtin root, FIRST.
    //
    // A root rule with no alias that can produce it is a rule that can never
    // fire: the resolver only ever returns a canonical root some alias named,
    // so `session-capture -> data` sat there while 2,791 real paths beginning
    // `session-capture/` resolved to no root and counted as unclassified. The
    // rule looked correct in every listing of the rules. Every rule must be
    // reachable by at least one alias, which the test below holds.
    for (root, _) in TierRules::builtin().entries() {
        store
            .put_alias(
                ctx,
                RootAlias {
                    alias_prefix: root.clone(),
                    canonical_root: root,
                    created_by: actor.to_owned(),
                    created_at: now,
                },
            )
            .await?;
        report.aliases_written += 1;
    }

    let trimmed = research_root.trim_end_matches('/');
    // Each canonical root gets the concrete path it lives at here. The short
    // spelling is registered too, because a path that arrives from another
    // machine -- an imported packet, a hand-typed rule -- will not carry this
    // machine's home directory.
    let pairs = [
        ("research/corpus", format!("{trimmed}/corpus")),
        ("research/corpus", "research/corpus".to_owned()),
        ("research/skills", format!("{trimmed}/skills")),
        ("research/skills", "research/skills".to_owned()),
        ("research/rules", format!("{trimmed}/skills/rules")),
        ("research/rules", "research/rules".to_owned()),
    ];
    for (canonical, prefix) in pairs {
        store
            .put_alias(
                ctx,
                RootAlias {
                    alias_prefix: prefix,
                    canonical_root: canonical.to_owned(),
                    created_by: actor.to_owned(),
                    created_at: now,
                },
            )
            .await?;
        report.aliases_written += 1;
    }
    Ok(report)
}

/// Load the alias set and root rules as the resolver pair the tier logic wants.
///
/// One read of each, so a caller tiering many entities pays for the rules once.
pub async fn load_rules(
    store: &impl TierStore,
    ctx: &TenantContext,
) -> anyhow::Result<(RootResolver, TierRules)> {
    let aliases = store.aliases(ctx).await?;
    let rules = store.root_rules(ctx).await?;
    let resolver = RootResolver::new(
        aliases
            .into_iter()
            .map(|alias| (alias.alias_prefix, alias.canonical_root)),
    );
    let rules = TierRules::new(rules.into_iter().map(|rule| (rule.root, rule.tier)));
    Ok((resolver, rules))
}

/// The tier one entity currently has, recomputed from live rules.
pub async fn tier_of(
    store: &impl TierStore,
    ctx: &TenantContext,
    entity_id: Uuid,
) -> anyhow::Result<TierAssignment> {
    let (resolver, rules) = load_rules(store, ctx).await?;
    let source = store.source_of(ctx, entity_id).await?;
    let promotion = store
        .promotion_of(ctx, entity_id)
        .await?
        .map(|p| Promotion {
            tier: p.tier,
            by: p.actor,
            why: p.reason,
        });
    Ok(crate::tiers::resolve(
        source.as_ref().map(|s| s.source_path.as_str()),
        &resolver,
        &rules,
        promotion.as_ref(),
    ))
}

/// The base facts and rules that put tiers on the graph.
#[derive(Debug, Clone, Default)]
pub struct TierFacts {
    /// `source_root(E, "research/corpus")` for unpromoted entities, and
    /// `tier(E, "knowledge")` directly for promoted ones.
    pub facts: FactSet,
    /// The root -> tier program, ready to hand to the Datalog evaluator.
    pub rules: Vec<String>,
    /// Sources behind the limit were not loaded, so a share resolved from
    /// these facts is a share over part of the store.
    pub truncated: bool,
}

/// Load tiers into the graph as Datalog facts.
///
/// ## Why a promoted entity emits no `source_root`
///
/// A promotion can DEMOTE. If a promoted entity also emitted its root, the
/// root rule would derive a second `tier` fact for it, and a sharing floor
/// asks whether *any* tier fact clears the bar — so the demotion would be
/// defeated by the derivation it was meant to override. One entity, one tier
/// fact; the precedence lives here, matching [`crate::tiers::resolve`].
pub async fn load_tier_facts(
    store: &impl TierStore,
    ctx: &TenantContext,
    limit: usize,
) -> anyhow::Result<TierFacts> {
    let (_, rules) = load_rules(store, ctx).await?;
    let promotions = store.promotions(ctx, limit).await?;
    let promoted: HashMap<Uuid, Tier> = promotions
        .into_iter()
        .map(|promotion| (promotion.entity_id, promotion.tier))
        .collect();

    let page = store.sources(ctx, limit).await?;
    let mut facts = FactSet::new();
    for (entity_id, tier) in &promoted {
        facts.insert(
            "tier",
            vec![
                Term::Const(*entity_id),
                Term::ConstStr(tier.as_str().to_owned()),
            ],
        );
    }
    for source in page.sources {
        if promoted.contains_key(&source.entity_id) {
            continue;
        }
        let Some(root) = source.source_root else {
            continue;
        };
        facts.insert(
            "source_root",
            vec![Term::Const(source.entity_id), Term::ConstStr(root)],
        );
    }
    Ok(TierFacts {
        facts,
        rules: rules.as_datalog(),
        truncated: page.truncated,
    })
}

/// One row of the DIKW map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierCount {
    pub tier: Tier,
    pub count: usize,
    /// Which roots feed this tier, so the map can say where a number came from.
    pub roots: Vec<String>,
}

/// What the tier plane knows, counted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierSummary {
    /// Always four rows, in DIKW order, including tiers with nothing in them.
    /// A tier that vanishes when empty reads as a tier that does not exist.
    pub tiers: Vec<TierCount>,
    /// Sourced entities whose root matches no rule. The hole in the rule set.
    pub unclassified: usize,
    /// Rows in `entity_source`. Zero means nothing records a source yet, which
    /// is a different statement from "the store is empty" and the map must be
    /// able to say which one it is looking at.
    pub sourced: usize,
    pub truncated: bool,
}

/// Count what sits in each tier.
///
/// Counted here rather than by the database because the tier is derived: there
/// is no column to group by, and materialising one would be the second truth
/// that [`TierRules`] exists to avoid.
pub async fn summarise(
    store: &impl TierStore,
    ctx: &TenantContext,
    limit: usize,
) -> anyhow::Result<TierSummary> {
    let (_, rules) = load_rules(store, ctx).await?;
    let promoted: HashMap<Uuid, Tier> = store
        .promotions(ctx, limit)
        .await?
        .into_iter()
        .map(|promotion| (promotion.entity_id, promotion.tier))
        .collect();

    let page = store.sources(ctx, limit).await?;
    let mut counts: BTreeMap<Tier, usize> = BTreeMap::new();
    let mut roots: BTreeMap<Tier, BTreeSet<String>> = BTreeMap::new();
    let mut unclassified = 0;
    let sourced = page.sources.len();

    for source in page.sources {
        // A promotion decides on its own, exactly as in `resolve`.
        if let Some(tier) = promoted.get(&source.entity_id) {
            *counts.entry(*tier).or_default() += 1;
            continue;
        }
        match source
            .source_root
            .as_deref()
            .and_then(|root| rules.tier_of_root(root).map(|tier| (root, tier)))
        {
            Some((root, tier)) => {
                *counts.entry(tier).or_default() += 1;
                roots.entry(tier).or_default().insert(root.to_owned());
            }
            // No root, or a root no rule covers. Both are Data by the floor in
            // `resolve`, and both are counted as the hole they are.
            None => {
                *counts.entry(Tier::Data).or_default() += 1;
                unclassified += 1;
            }
        }
    }

    Ok(TierSummary {
        tiers: [Tier::Data, Tier::Information, Tier::Knowledge, Tier::Wisdom]
            .into_iter()
            .map(|tier| TierCount {
                tier,
                count: counts.get(&tier).copied().unwrap_or(0),
                roots: roots.get(&tier).into_iter().flatten().cloned().collect(),
            })
            .collect(),
        unclassified,
        sourced,
        truncated: page.truncated,
    })
}

/// Resolve a path against an alias set into the pair stored on the row.
fn resolved_pair(resolver: &RootResolver, path: &str) -> (Option<String>, Option<String>) {
    resolver
        .match_of(path)
        .map_or((None, None), |m| (Some(m.root), Some(m.alias_prefix)))
}

#[derive(Default)]
struct TenantTiers {
    sources: HashMap<Uuid, EntitySource>,
    aliases: HashMap<String, RootAlias>,
    rules: HashMap<String, RootRule>,
    promotions: HashMap<Uuid, TierPromotion>,
}

/// Deterministic in-memory implementation for domain tests.
#[derive(Default)]
pub struct InMemoryTierStore {
    tenants: Mutex<HashMap<Uuid, TenantTiers>>,
}

impl InMemoryTierStore {
    fn with<R>(&self, ctx: &TenantContext, f: impl FnOnce(&mut TenantTiers) -> R) -> R {
        let mut tenants = self.tenants.lock().expect("tier store mutex poisoned");
        f(tenants.entry(ctx.tenant_id).or_default())
    }

    fn resolver_of(tenant: &TenantTiers) -> RootResolver {
        RootResolver::new(
            tenant
                .aliases
                .values()
                .map(|alias| (alias.alias_prefix.clone(), alias.canonical_root.clone())),
        )
    }
}

impl TierStore for InMemoryTierStore {
    async fn record_source(
        &self,
        ctx: &TenantContext,
        draft: SourceDraft,
    ) -> anyhow::Result<EntitySource> {
        Ok(self.with(ctx, |tenant| {
            let resolver = Self::resolver_of(tenant);
            let (source_root, matched_alias) = resolved_pair(&resolver, &draft.source_path);
            let record = EntitySource {
                entity_id: draft.entity_id,
                session_id: draft.session_id,
                title: draft.title,
                source_path: draft.source_path,
                source_root,
                matched_alias,
                recorded_at: Utc::now(),
            };
            tenant.sources.insert(record.entity_id, record.clone());
            record
        }))
    }

    async fn source_of(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> anyhow::Result<Option<EntitySource>> {
        Ok(self.with(ctx, |tenant| tenant.sources.get(&entity_id).cloned()))
    }

    async fn sources_under_root(
        &self,
        ctx: &TenantContext,
        root: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<EntitySource>> {
        Ok(self.with(ctx, |tenant| {
            let mut found: Vec<EntitySource> = tenant
                .sources
                .values()
                .filter(|source| source.source_root.as_deref() == Some(root))
                .cloned()
                .collect();
            found.sort_by_key(|source| source.entity_id);
            found.truncate(limit);
            found
        }))
    }

    async fn put_alias(&self, ctx: &TenantContext, alias: RootAlias) -> anyhow::Result<()> {
        self.with(ctx, |tenant| {
            tenant.aliases.insert(alias.alias_prefix.clone(), alias);
        });
        Ok(())
    }

    async fn aliases(&self, ctx: &TenantContext) -> anyhow::Result<Vec<RootAlias>> {
        Ok(self.with(ctx, |tenant| {
            let mut all: Vec<RootAlias> = tenant.aliases.values().cloned().collect();
            all.sort_by(|a, b| a.alias_prefix.cmp(&b.alias_prefix));
            all
        }))
    }

    async fn put_root_rule(&self, ctx: &TenantContext, rule: RootRule) -> anyhow::Result<()> {
        self.with(ctx, |tenant| {
            tenant.rules.insert(rule.root.clone(), rule);
        });
        Ok(())
    }

    async fn root_rules(&self, ctx: &TenantContext) -> anyhow::Result<Vec<RootRule>> {
        Ok(self.with(ctx, |tenant| {
            let mut all: Vec<RootRule> = tenant.rules.values().cloned().collect();
            all.sort_by(|a, b| a.root.cmp(&b.root));
            all
        }))
    }

    async fn promote(&self, ctx: &TenantContext, promotion: TierPromotion) -> anyhow::Result<()> {
        self.with(ctx, |tenant| {
            tenant.promotions.insert(promotion.entity_id, promotion);
        });
        Ok(())
    }

    async fn promotion_of(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> anyhow::Result<Option<TierPromotion>> {
        Ok(self.with(ctx, |tenant| tenant.promotions.get(&entity_id).cloned()))
    }

    async fn sources(&self, ctx: &TenantContext, limit: usize) -> anyhow::Result<SourcePage> {
        Ok(self.with(ctx, |tenant| {
            let mut all: Vec<EntitySource> = tenant.sources.values().cloned().collect();
            all.sort_by_key(|source| source.entity_id);
            let truncated = all.len() > limit;
            all.truncate(limit);
            SourcePage {
                sources: all,
                truncated,
            }
        }))
    }

    async fn promotions(
        &self,
        ctx: &TenantContext,
        limit: usize,
    ) -> anyhow::Result<Vec<TierPromotion>> {
        Ok(self.with(ctx, |tenant| {
            let mut all: Vec<TierPromotion> = tenant.promotions.values().cloned().collect();
            all.sort_by_key(|promotion| promotion.entity_id);
            all.truncate(limit);
            all
        }))
    }

    async fn restate_sources(
        &self,
        ctx: &TenantContext,
        limit: usize,
    ) -> anyhow::Result<RestateReport> {
        let page = self.sources(ctx, limit).await?;
        Ok(self.with(ctx, |tenant| {
            let resolver = Self::resolver_of(tenant);
            let mut report = RestateReport {
                truncated: page.truncated,
                ..RestateReport::default()
            };
            for stale in page.sources {
                let Some(source) = tenant.sources.get_mut(&stale.entity_id) else {
                    continue;
                };
                let (root, alias) = resolved_pair(&resolver, &source.source_path);
                report.examined += 1;
                if root.is_none() {
                    report.unresolved += 1;
                }
                if source.source_root != root {
                    source.source_root = root;
                    source.matched_alias = alias;
                    report.rewritten += 1;
                }
            }
            report
        }))
    }
}

/// CQL-backed implementation against the tables in `ddl/054_knowledge_tiers.cql`.
pub struct CqlTierStore {
    session: Arc<CqlSession>,
    keyspace: String,
}

impl CqlTierStore {
    pub fn new(session: Arc<CqlSession>, keyspace: impl Into<String>) -> Self {
        Self {
            session,
            keyspace: keyspace.into(),
        }
    }

    async fn load_resolver(&self, ctx: &TenantContext) -> anyhow::Result<RootResolver> {
        let aliases = self.aliases(ctx).await?;
        Ok(RootResolver::new(
            aliases
                .into_iter()
                .map(|alias| (alias.alias_prefix, alias.canonical_root)),
        ))
    }

    async fn write_source(&self, ctx: &TenantContext, record: &EntitySource) -> anyhow::Result<()> {
        let query = format!(
            "INSERT INTO {}.entity_source \
             (tenant_id, entity_id, session_id, title, source_path, source_root, \
              matched_alias, recorded_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            self.keyspace
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    record.entity_id,
                    record.session_id,
                    record.title.as_str(),
                    record.source_path.as_str(),
                    record.source_root.as_deref(),
                    record.matched_alias.as_deref(),
                    record.recorded_at,
                ),
            )
            .await
            .context("writing entity source")?;

        self.write_source_by_root(ctx, record).await
    }

    /// The same fact, in the order the browse list reads it.
    ///
    /// Written after entity_source, not before: entity_source is the
    /// authority, and a crash between the two leaves this view missing a row
    /// that a rebuild can restore. The other order would leave a row here with
    /// no authority behind it.
    ///
    /// A source with no root is skipped. The partition key IS the root, so
    /// there is nowhere to put one -- and a sentinel partition would invent a
    /// root that the tier rules never assigned. Those rows stay reachable
    /// through entity_source, which is what `summarise` counts as
    /// unclassified.
    async fn write_source_by_root(
        &self,
        ctx: &TenantContext,
        record: &EntitySource,
    ) -> anyhow::Result<()> {
        let Some(root) = record.source_root.as_deref() else {
            return Ok(());
        };
        let query = format!(
            "INSERT INTO {}.entity_source_by_root \
             (tenant_id, source_root, page_key, entity_id, session_id, title, \
              source_path, matched_alias, recorded_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            self.keyspace
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    root,
                    page_key(record.recorded_at, record.entity_id)?,
                    record.entity_id,
                    record.session_id,
                    record.title.as_str(),
                    record.source_path.as_str(),
                    record.matched_alias.as_deref(),
                    record.recorded_at,
                ),
            )
            .await
            .context("writing entity source by root")?;
        Ok(())
    }

    /// One page of a root, newest first, resumable by cursor.
    ///
    /// `ORDER BY page_key DESC` is explicit because this engine accepts
    /// `CLUSTERING ORDER BY ... DESC` in the DDL and then ignores it (see
    /// ddl/055 and ../ferrosa/specs/bug-clustering-order-desc-ignored.md).
    /// Without the clause here the newest page would be the oldest rows, and
    /// nothing would report it.
    pub async fn sources_page(
        &self,
        ctx: &TenantContext,
        root: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<RootPage> {
        anyhow::ensure!(limit > 0, "sources_page: limit must be > 0");
        // One more than asked for, to learn whether a next page exists without
        // a second query and without reporting a cursor that leads nowhere.
        let probe = limit.saturating_add(1);
        let base = format!(
            "SELECT entity_id, session_id, title, source_path, source_root, \
             matched_alias, recorded_at, page_key FROM {}.entity_source_by_root \
             WHERE tenant_id = ? AND source_root = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = match cursor {
            Some(cursor) => {
                let query = format!("{base} AND page_key < ? ORDER BY page_key DESC LIMIT {probe}");
                self.session
                    .query_unpaged(query, (ctx.tenant_id, root, cursor))
                    .await
            }
            None => {
                let query = format!("{base} ORDER BY page_key DESC LIMIT {probe}");
                self.session
                    .query_unpaged(query, (ctx.tenant_id, root))
                    .await
            }
        }
        .context("reading a page of entity sources by root")?;

        let columns = build_col_map(result.col_specs());
        let mut sources = Vec::with_capacity(limit);
        let mut keys = Vec::with_capacity(limit);
        for row in result.rows_or_empty() {
            // The cursor is read from the row rather than recomputed from its
            // timestamp and id. Recomputing would agree until the day the key
            // format changes, and then hand out cursors that match nothing.
            keys.push(cql_get::<String>(&row, &columns, "page_key")?);
            sources.push(source_from_row(&row, &columns)?);
        }

        // The probe row is evidence of a next page, not part of this one.
        let has_more = sources.len() > limit;
        sources.truncate(limit);
        keys.truncate(limit);
        let next_cursor = if has_more { keys.last().cloned() } else { None };
        Ok(RootPage {
            sources,
            next_cursor,
        })
    }

    /// Populate `entity_source_by_root` from `entity_source`.
    ///
    /// Migration 055 creates an empty table; every row written before it
    /// existed lives only in `entity_source`. Without this the Memory tab
    /// would show four tiers of nothing on a cluster holding 69,683 rows --
    /// the same "reachable and empty" failure the wrong tenant produced, from
    /// a different cause.
    ///
    /// Idempotent: the destination key is derived from the row, so re-running
    /// rewrites each row onto itself. That makes it safe to re-run after a
    /// partial failure, which matters because this reads the whole table and a
    /// 69,683-row scan is a thing that gets interrupted.
    ///
    /// Rows with no `source_root` are counted, not written -- there is no
    /// partition for them -- so `skipped` is the honest difference between
    /// what was read and what the browse list can reach.
    pub async fn backfill_by_root(
        &self,
        ctx: &TenantContext,
        limit: usize,
    ) -> anyhow::Result<BackfillByRootReport> {
        let page = self.sources(ctx, limit).await?;
        let mut report = BackfillByRootReport {
            truncated: page.truncated,
            ..BackfillByRootReport::default()
        };
        for source in &page.sources {
            report.examined += 1;
            if source.source_root.is_none() {
                report.skipped_without_root += 1;
                continue;
            }
            self.write_source_by_root(ctx, source).await?;
            report.written += 1;
        }
        Ok(report)
    }
}

/// What a by-root backfill actually did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillByRootReport {
    pub examined: usize,
    pub written: usize,
    /// Read, but unreachable from the browse list: no root, so no partition.
    pub skipped_without_root: usize,
    /// The read hit its limit, so this is a prefix of the table and rerunning
    /// with a higher limit will find more.
    pub truncated: bool,
}

/// Read one `entity_source` row. Shared by the point read, the root query, and
/// the restate scan so all three agree on how a row deserializes.
fn source_from_row(
    row: &scylla::frame::response::result::Row,
    columns: &crate::cql_storage::ColMap,
) -> anyhow::Result<EntitySource> {
    Ok(EntitySource {
        entity_id: cql_get(row, columns, "entity_id")?,
        session_id: cql_get(row, columns, "session_id")?,
        title: cql_get::<String>(row, columns, "title").unwrap_or_default(),
        source_path: cql_get(row, columns, "source_path")?,
        source_root: cql_get::<String>(row, columns, "source_root").ok(),
        matched_alias: cql_get::<String>(row, columns, "matched_alias").ok(),
        recorded_at: cql_get(row, columns, "recorded_at")?,
    })
}

/// Parse a stored tier string, failing loud rather than defaulting.
///
/// A tier that does not parse means the row was written by something that
/// disagrees with this build about the tier vocabulary. Substituting Data
/// there would silently DOWNGRADE curated material, which is exactly the
/// direction that loses work.
fn parse_tier(raw: &str, context: &str) -> anyhow::Result<Tier> {
    Tier::parse(raw).ok_or_else(|| anyhow::anyhow!("unknown tier {raw:?} stored for {context}"))
}

impl TierStore for CqlTierStore {
    async fn record_source(
        &self,
        ctx: &TenantContext,
        draft: SourceDraft,
    ) -> anyhow::Result<EntitySource> {
        let resolver = self.load_resolver(ctx).await?;
        let (source_root, matched_alias) = resolved_pair(&resolver, &draft.source_path);
        let record = EntitySource {
            entity_id: draft.entity_id,
            session_id: draft.session_id,
            title: draft.title,
            source_path: draft.source_path,
            source_root,
            matched_alias,
            recorded_at: Utc::now(),
        };
        self.write_source(ctx, &record).await?;
        Ok(record)
    }

    async fn source_of(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> anyhow::Result<Option<EntitySource>> {
        let query = format!(
            "SELECT entity_id, session_id, title, source_path, source_root, matched_alias, \
             recorded_at \
             FROM {}.entity_source WHERE tenant_id = ? AND entity_id = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, entity_id))
            .await
            .context("reading entity source")?;
        let columns = build_col_map(result.col_specs());
        result
            .rows_or_empty()
            .into_iter()
            .next()
            .map(|row| source_from_row(&row, &columns))
            .transpose()
    }

    async fn sources_under_root(
        &self,
        ctx: &TenantContext,
        root: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<EntitySource>> {
        let query = format!(
            "SELECT entity_id, session_id, title, source_path, source_root, matched_alias, \
             recorded_at \
             FROM {}.entity_source WHERE tenant_id = ? AND source_root = ? LIMIT ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, root, i32::try_from(limit)?))
            .await
            .context("reading entity sources under a root")?;
        let columns = build_col_map(result.col_specs());
        result
            .rows_or_empty()
            .into_iter()
            .map(|row| source_from_row(&row, &columns))
            .collect()
    }

    async fn put_alias(&self, ctx: &TenantContext, alias: RootAlias) -> anyhow::Result<()> {
        let query = format!(
            "INSERT INTO {}.tier_root_alias \
             (tenant_id, alias_prefix, canonical_root, created_at, created_by) \
             VALUES (?, ?, ?, ?, ?)",
            self.keyspace
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    alias.alias_prefix.as_str(),
                    alias.canonical_root.as_str(),
                    alias.created_at,
                    alias.created_by.as_str(),
                ),
            )
            .await
            .context("writing tier root alias")?;
        Ok(())
    }

    async fn aliases(&self, ctx: &TenantContext) -> anyhow::Result<Vec<RootAlias>> {
        let query = format!(
            "SELECT alias_prefix, canonical_root, created_at, created_by \
             FROM {}.tier_root_alias WHERE tenant_id = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id,))
            .await
            .context("reading tier root aliases")?;
        let columns = build_col_map(result.col_specs());
        result
            .rows_or_empty()
            .into_iter()
            .map(|row| {
                Ok(RootAlias {
                    alias_prefix: cql_get(&row, &columns, "alias_prefix")?,
                    canonical_root: cql_get(&row, &columns, "canonical_root")?,
                    created_by: cql_get::<String>(&row, &columns, "created_by").unwrap_or_default(),
                    created_at: cql_get(&row, &columns, "created_at")?,
                })
            })
            .collect()
    }

    async fn put_root_rule(&self, ctx: &TenantContext, rule: RootRule) -> anyhow::Result<()> {
        let query = format!(
            "INSERT INTO {}.tier_root_rule \
             (tenant_id, root, tier, created_at, created_by, note) VALUES (?, ?, ?, ?, ?, ?)",
            self.keyspace
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    rule.root.as_str(),
                    rule.tier.as_str(),
                    rule.created_at,
                    rule.created_by.as_str(),
                    rule.note.as_str(),
                ),
            )
            .await
            .context("writing tier root rule")?;
        Ok(())
    }

    async fn root_rules(&self, ctx: &TenantContext) -> anyhow::Result<Vec<RootRule>> {
        let query = format!(
            "SELECT root, tier, created_at, created_by, note \
             FROM {}.tier_root_rule WHERE tenant_id = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id,))
            .await
            .context("reading tier root rules")?;
        let columns = build_col_map(result.col_specs());
        result
            .rows_or_empty()
            .into_iter()
            .map(|row| {
                let root: String = cql_get(&row, &columns, "root")?;
                let raw: String = cql_get(&row, &columns, "tier")?;
                let tier = parse_tier(&raw, &format!("root {root:?}"))?;
                Ok(RootRule {
                    root,
                    tier,
                    created_by: cql_get::<String>(&row, &columns, "created_by").unwrap_or_default(),
                    note: cql_get::<String>(&row, &columns, "note").unwrap_or_default(),
                    created_at: cql_get(&row, &columns, "created_at")?,
                })
            })
            .collect()
    }

    async fn promote(&self, ctx: &TenantContext, promotion: TierPromotion) -> anyhow::Result<()> {
        let query = format!(
            "INSERT INTO {}.tier_promotion \
             (tenant_id, entity_id, tier, actor, reason, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            self.keyspace
        );
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                query,
                (
                    ctx.tenant_id,
                    promotion.entity_id,
                    promotion.tier.as_str(),
                    promotion.actor.as_str(),
                    promotion.reason.as_str(),
                    promotion.created_at,
                ),
            )
            .await
            .context("writing tier promotion")?;
        Ok(())
    }

    async fn promotion_of(
        &self,
        ctx: &TenantContext,
        entity_id: Uuid,
    ) -> anyhow::Result<Option<TierPromotion>> {
        let query = format!(
            "SELECT entity_id, tier, actor, reason, created_at \
             FROM {}.tier_promotion WHERE tenant_id = ? AND entity_id = ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, entity_id))
            .await
            .context("reading tier promotion")?;
        let columns = build_col_map(result.col_specs());
        let Some(row) = result.rows_or_empty().into_iter().next() else {
            return Ok(None);
        };
        let raw: String = cql_get(&row, &columns, "tier")?;
        Ok(Some(TierPromotion {
            entity_id: cql_get(&row, &columns, "entity_id")?,
            tier: parse_tier(&raw, &format!("entity {entity_id}"))?,
            actor: cql_get::<String>(&row, &columns, "actor").unwrap_or_default(),
            reason: cql_get::<String>(&row, &columns, "reason").unwrap_or_default(),
            created_at: cql_get(&row, &columns, "created_at")?,
        }))
    }

    async fn sources(&self, ctx: &TenantContext, limit: usize) -> anyhow::Result<SourcePage> {
        // One over the limit, so a full page is distinguishable from a page
        // that happened to end exactly at the bound.
        let query = format!(
            "SELECT entity_id, session_id, title, source_path, source_root, matched_alias, \
             recorded_at \
             FROM {}.entity_source WHERE tenant_id = ? LIMIT ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, i32::try_from(limit + 1)?))
            .await
            .context("scanning entity sources")?;
        let columns = build_col_map(result.col_specs());
        let rows = result.rows_or_empty();
        let truncated = rows.len() > limit;
        let sources = rows
            .into_iter()
            .take(limit)
            .map(|row| source_from_row(&row, &columns))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(SourcePage { sources, truncated })
    }

    async fn promotions(
        &self,
        ctx: &TenantContext,
        limit: usize,
    ) -> anyhow::Result<Vec<TierPromotion>> {
        let query = format!(
            "SELECT entity_id, tier, actor, reason, created_at \
             FROM {}.tier_promotion WHERE tenant_id = ? LIMIT ?",
            self.keyspace
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, (ctx.tenant_id, i32::try_from(limit)?))
            .await
            .context("reading tier promotions")?;
        let columns = build_col_map(result.col_specs());
        result
            .rows_or_empty()
            .into_iter()
            .map(|row| {
                let entity_id: Uuid = cql_get(&row, &columns, "entity_id")?;
                let raw: String = cql_get(&row, &columns, "tier")?;
                Ok(TierPromotion {
                    entity_id,
                    tier: parse_tier(&raw, &format!("entity {entity_id}"))?,
                    actor: cql_get::<String>(&row, &columns, "actor").unwrap_or_default(),
                    reason: cql_get::<String>(&row, &columns, "reason").unwrap_or_default(),
                    created_at: cql_get(&row, &columns, "created_at")?,
                })
            })
            .collect()
    }

    async fn restate_sources(
        &self,
        ctx: &TenantContext,
        limit: usize,
    ) -> anyhow::Result<RestateReport> {
        let resolver = self.load_resolver(ctx).await?;
        let page = self.sources(ctx, limit).await?;
        let mut report = RestateReport {
            truncated: page.truncated,
            ..RestateReport::default()
        };
        for mut source in page.sources {
            let (root, alias) = resolved_pair(&resolver, &source.source_path);
            report.examined += 1;
            if root.is_none() {
                report.unresolved += 1;
            }
            if source.source_root != root {
                source.source_root = root;
                source.matched_alias = alias;
                self.write_source(ctx, &source).await?;
                report.rewritten += 1;
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiers::TierReason;

    fn tenant() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "tier-store-test".to_owned(),
        }
    }

    fn alias(prefix: &str, root: &str) -> RootAlias {
        RootAlias {
            alias_prefix: prefix.to_owned(),
            canonical_root: root.to_owned(),
            created_by: "ben".to_owned(),
            created_at: Utc::now(),
        }
    }

    fn rule(root: &str, tier: Tier) -> RootRule {
        RootRule {
            root: root.to_owned(),
            tier,
            created_by: "ben".to_owned(),
            note: "test".to_owned(),
            created_at: Utc::now(),
        }
    }

    /// A draft whose title is derived from the path, so a failing assertion
    /// names something recognisable.
    fn draft(entity_id: Uuid, path: &str) -> SourceDraft {
        SourceDraft {
            entity_id,
            session_id: Uuid::new_v4(),
            title: path.rsplit('/').next().unwrap_or(path).to_owned(),
            source_path: path.to_owned(),
        }
    }

    /// The store with the aliases and rules Ben described: two spellings of the
    /// research tree, corpus as Information, skills as Wisdom.
    async fn curated() -> (InMemoryTierStore, TenantContext) {
        let store = InMemoryTierStore::default();
        let ctx = tenant();
        for a in [
            alias("/Users/bkearns/src/research/corpus", "research/corpus"),
            alias("bkearns/research/corpus", "research/corpus"),
            alias("/Users/bkearns/src/research/skills", "research/skills"),
        ] {
            store.put_alias(&ctx, a).await.expect("alias");
        }
        for r in [
            rule("research/corpus", Tier::Information),
            rule("research/skills", Tier::Wisdom),
        ] {
            store.put_root_rule(&ctx, r).await.expect("rule");
        }
        (store, ctx)
    }

    #[tokio::test]
    async fn two_spellings_of_one_tree_land_in_one_tier() {
        let (store, ctx) = curated().await;
        let long = Uuid::new_v4();
        let short = Uuid::new_v4();
        store
            .record_source(
                &ctx,
                draft(long, "/Users/bkearns/src/research/corpus/rust/x.md"),
            )
            .await
            .expect("record");
        store
            .record_source(&ctx, draft(short, "bkearns/research/corpus/rust/x.md"))
            .await
            .expect("record");

        for id in [long, short] {
            let assignment = tier_of(&store, &ctx, id).await.expect("tier");
            assert_eq!(assignment.tier, Tier::Information, "entity {id}");
            assert_eq!(
                assignment.reason,
                TierReason::Root("research/corpus".to_owned())
            );
        }
    }

    #[tokio::test]
    async fn the_alias_that_fired_is_recorded() {
        let (store, ctx) = curated().await;
        let id = Uuid::new_v4();
        let record = store
            .record_source(&ctx, draft(id, "bkearns/research/corpus/rust/x.md"))
            .await
            .expect("record");
        // Without this, a mis-tier under two aliases pointing at one root is
        // unattributable: the root alone does not say which rule matched.
        assert_eq!(
            record.matched_alias.as_deref(),
            Some("bkearns/research/corpus")
        );
        assert_eq!(record.source_root.as_deref(), Some("research/corpus"));
    }

    #[tokio::test]
    async fn a_path_no_alias_covers_has_no_root_and_sits_at_data() {
        let (store, ctx) = curated().await;
        let id = Uuid::new_v4();
        let record = store
            .record_source(&ctx, draft(id, "/tmp/scratch/notes.md"))
            .await
            .expect("record");
        assert_eq!(record.source_root, None, "invented a root");
        assert_eq!(record.matched_alias, None);

        let assignment = tier_of(&store, &ctx, id).await.expect("tier");
        assert_eq!(assignment.tier, Tier::Data);
        assert_eq!(assignment.reason, TierReason::Default);
    }

    #[tokio::test]
    async fn a_promotion_outranks_the_root() {
        let (store, ctx) = curated().await;
        let id = Uuid::new_v4();
        store
            .record_source(
                &ctx,
                draft(id, "/Users/bkearns/src/research/corpus/rust/x.md"),
            )
            .await
            .expect("record");
        store
            .promote(
                &ctx,
                TierPromotion {
                    entity_id: id,
                    tier: Tier::Knowledge,
                    actor: "ben".to_owned(),
                    reason: "adjudicated".to_owned(),
                    created_at: Utc::now(),
                },
            )
            .await
            .expect("promote");

        let assignment = tier_of(&store, &ctx, id).await.expect("tier");
        assert_eq!(assignment.tier, Tier::Knowledge);
        assert_eq!(
            assignment.reason,
            TierReason::Promoted {
                by: "ben".to_owned(),
                why: "adjudicated".to_owned(),
            }
        );
    }

    /// Editing a rule re-tiers everything under the root, with no row rewrite.
    /// This is the whole reason the tier is derived rather than stored.
    #[tokio::test]
    async fn re_tiering_a_root_moves_everything_under_it() {
        let (store, ctx) = curated().await;
        let id = Uuid::new_v4();
        store
            .record_source(
                &ctx,
                draft(id, "/Users/bkearns/src/research/corpus/rust/x.md"),
            )
            .await
            .expect("record");
        assert_eq!(
            tier_of(&store, &ctx, id).await.expect("tier").tier,
            Tier::Information
        );

        store
            .put_root_rule(&ctx, rule("research/corpus", Tier::Knowledge))
            .await
            .expect("rule");
        assert_eq!(
            tier_of(&store, &ctx, id).await.expect("tier").tier,
            Tier::Knowledge,
            "the rule edit did not reach the entity"
        );
    }

    /// The stored root is a cache. An alias edit does NOT fix it, and this test
    /// says so out loud so nobody later assumes the index is self-healing.
    #[tokio::test]
    async fn an_alias_edit_leaves_the_index_stale_until_restated() {
        let (store, ctx) = curated().await;
        let id = Uuid::new_v4();
        store
            .record_source(
                &ctx,
                draft(id, "/Users/bkearns/src/research/corpus/rust/x.md"),
            )
            .await
            .expect("record");

        // Split the corpus tree: rust corpus becomes its own root.
        store
            .put_alias(
                &ctx,
                alias(
                    "/Users/bkearns/src/research/corpus/rust",
                    "research/corpus/rust",
                ),
            )
            .await
            .expect("alias");

        let indexed = store
            .sources_under_root(&ctx, "research/corpus/rust", 10)
            .await
            .expect("query");
        assert!(indexed.is_empty(), "index healed itself, which it cannot");

        let report = store.restate_sources(&ctx, 100).await.expect("restate");
        assert_eq!(report.examined, 1);
        assert_eq!(report.rewritten, 1);
        assert_eq!(report.unresolved, 0);
        assert!(!report.truncated);

        let indexed = store
            .sources_under_root(&ctx, "research/corpus/rust", 10)
            .await
            .expect("query");
        assert_eq!(indexed.len(), 1, "restate did not rewrite the row");
        assert_eq!(
            indexed[0].matched_alias.as_deref(),
            Some("/Users/bkearns/src/research/corpus/rust")
        );
    }

    #[tokio::test]
    async fn restate_counts_the_paths_no_alias_covers() {
        let (store, ctx) = curated().await;
        for path in ["/tmp/a.md", "/tmp/b.md"] {
            store
                .record_source(&ctx, draft(Uuid::new_v4(), path))
                .await
                .expect("record");
        }
        store
            .record_source(
                &ctx,
                draft(Uuid::new_v4(), "/Users/bkearns/src/research/skills/rust.md"),
            )
            .await
            .expect("record");

        let report = store.restate_sources(&ctx, 100).await.expect("restate");
        assert_eq!(report.examined, 3);
        assert_eq!(report.unresolved, 2, "a hole in the alias set went unseen");
        assert_eq!(report.rewritten, 0, "nothing moved, nothing to rewrite");
    }

    #[tokio::test]
    async fn restate_says_when_the_bound_bites() {
        let (store, ctx) = curated().await;
        for _ in 0..5 {
            store
                .record_source(&ctx, draft(Uuid::new_v4(), "/tmp/x.md"))
                .await
                .expect("record");
        }
        let report = store.restate_sources(&ctx, 2).await.expect("restate");
        assert_eq!(report.examined, 2);
        assert!(report.truncated, "a partial scan reported as complete");
    }

    #[tokio::test]
    async fn tenants_do_not_see_each_other_rules_or_sources() {
        let (store, ctx) = curated().await;
        let other = tenant();
        let id = Uuid::new_v4();
        store
            .record_source(
                &ctx,
                draft(id, "/Users/bkearns/src/research/skills/rust.md"),
            )
            .await
            .expect("record");

        assert!(
            store.source_of(&other, id).await.expect("read").is_none(),
            "source crossed a tenant boundary"
        );
        assert!(
            store.aliases(&other).await.expect("read").is_empty(),
            "aliases crossed a tenant boundary"
        );
        assert_eq!(
            tier_of(&store, &other, id).await.expect("tier").tier,
            Tier::Data
        );
    }

    /// The loader's precedence, and the reason it exists: a demotion has to
    /// beat the root rule it overrides, or a floor cannot be trusted.
    #[tokio::test]
    async fn a_demoted_skill_stops_clearing_the_floor() {
        use crate::sharing::ShareGrant;

        let (store, ctx) = curated().await;
        let kept = Uuid::new_v4();
        let demoted = Uuid::new_v4();
        for id in [kept, demoted] {
            store
                .record_source(
                    &ctx,
                    draft(id, "/Users/bkearns/src/research/skills/rust.md"),
                )
                .await
                .expect("record");
        }

        let loaded = load_tier_facts(&store, &ctx, 100).await.expect("facts");
        let shared = ShareGrant::new([kept, demoted], 0, Tier::Wisdom)
            .resolve(&derive(&loaded), 10_000)
            .expect("rules parse");
        assert!(
            shared.contains(&kept),
            "the skill root did not reach wisdom"
        );
        assert!(
            shared.contains(&demoted),
            "same root, same tier, before the demotion"
        );

        store
            .promote(
                &ctx,
                TierPromotion {
                    entity_id: demoted,
                    tier: Tier::Data,
                    actor: "ben".to_owned(),
                    reason: "unreviewed draft".to_owned(),
                    created_at: Utc::now(),
                },
            )
            .await
            .expect("promote");

        let loaded = load_tier_facts(&store, &ctx, 100).await.expect("facts");
        let shared = ShareGrant::new([kept, demoted], 0, Tier::Wisdom)
            .resolve(&derive(&loaded), 10_000)
            .expect("rules parse");
        assert!(shared.contains(&kept), "the demotion took the wrong entity");
        assert!(
            !shared.contains(&demoted),
            "the root rule defeated the demotion: a floor that a promotion \
             cannot lower is not a floor"
        );
    }

    /// Run the root -> tier program so the facts carry `tier`, the way a
    /// sharing grant will see them.
    fn derive(loaded: &TierFacts) -> FactSet {
        let rules: Vec<_> = loaded
            .rules
            .iter()
            .map(|rule| crate::datalog::parse_rule(rule).expect("registry rule must parse"))
            .collect();
        crate::datalog::evaluate(&rules, &loaded.facts, 64, 10_000).0
    }

    #[tokio::test]
    async fn the_map_counts_every_tier_including_the_empty_ones() {
        let (store, ctx) = curated().await;
        for path in [
            "/Users/bkearns/src/research/skills/rust.md",
            "/Users/bkearns/src/research/skills/elixir.md",
            "/Users/bkearns/src/research/corpus/rust/x.md",
            "/tmp/exhaust.md",
        ] {
            store
                .record_source(&ctx, draft(Uuid::new_v4(), path))
                .await
                .expect("record");
        }

        let map = summarise(&store, &ctx, 1_000).await.expect("summary");
        let by_tier = |want: Tier| {
            map.tiers
                .iter()
                .find(|row| row.tier == want)
                .unwrap_or_else(|| panic!("{want:?} missing from the map"))
        };
        assert_eq!(by_tier(Tier::Wisdom).count, 2);
        assert_eq!(by_tier(Tier::Information).count, 1);
        assert_eq!(by_tier(Tier::Data).count, 1, "the uncovered path");
        // Present with zero rather than absent: a tier that vanishes when
        // empty reads as a tier that does not exist.
        assert_eq!(by_tier(Tier::Knowledge).count, 0);
        assert_eq!(map.tiers.len(), 4);

        assert_eq!(map.unclassified, 1, "the hole in the rule set went unseen");
        assert_eq!(map.sourced, 4);
        assert!(!map.truncated);
        assert_eq!(by_tier(Tier::Wisdom).roots, vec!["research/skills"]);
    }

    /// Zero sourced entities and an empty store are different statements, and
    /// the map has to be able to tell them apart -- the first means nothing
    /// records a source yet, which is a build step, not an empty library.
    #[tokio::test]
    async fn an_unwired_ingest_reads_as_zero_sourced_not_as_an_empty_store() {
        let (store, ctx) = curated().await;
        let map = summarise(&store, &ctx, 1_000).await.expect("summary");
        assert_eq!(map.sourced, 0);
        assert_eq!(map.unclassified, 0);
        assert_eq!(map.tiers.len(), 4, "the map still has its shape");
        assert!(map.tiers.iter().all(|row| row.count == 0));
    }

    #[tokio::test]
    async fn a_promotion_moves_the_count_it_belongs_to() {
        let (store, ctx) = curated().await;
        let id = Uuid::new_v4();
        store
            .record_source(&ctx, draft(id, "/Users/bkearns/src/research/corpus/x.md"))
            .await
            .expect("record");
        store
            .promote(
                &ctx,
                TierPromotion {
                    entity_id: id,
                    tier: Tier::Wisdom,
                    actor: "ben".to_owned(),
                    reason: "curated".to_owned(),
                    created_at: Utc::now(),
                },
            )
            .await
            .expect("promote");

        let map = summarise(&store, &ctx, 1_000).await.expect("summary");
        let count = |want: Tier| {
            map.tiers
                .iter()
                .find(|row| row.tier == want)
                .map_or(0, |row| row.count)
        };
        assert_eq!(count(Tier::Wisdom), 1);
        assert_eq!(count(Tier::Information), 0, "counted in both places");
    }

    /// Seeding makes a real path resolve. Before this, the rules existed in
    /// the build and nowhere a query could see them.
    #[tokio::test]
    async fn seeding_makes_a_real_path_tier() {
        let store = InMemoryTierStore::default();
        let ctx = tenant();
        let report = seed_tier_rules(&store, &ctx, "/Users/bkearns/src/research", "ben")
            .await
            .expect("seed");
        assert!(report.rules_written >= 4);
        assert!(report.aliases_written >= 4);

        let skill = Uuid::new_v4();
        store
            .record_source(
                &ctx,
                draft(skill, "/Users/bkearns/src/research/skills/rust.md"),
            )
            .await
            .expect("record");
        assert_eq!(
            tier_of(&store, &ctx, skill).await.expect("tier").tier,
            Tier::Wisdom
        );

        let corpus = Uuid::new_v4();
        store
            .record_source(
                &ctx,
                draft(corpus, "/Users/bkearns/src/research/corpus/rust/x.md"),
            )
            .await
            .expect("record");
        assert_eq!(
            tier_of(&store, &ctx, corpus).await.expect("tier").tier,
            Tier::Information,
        );
    }

    /// A path from ANOTHER machine still tiers. An imported packet or a rule
    /// typed by hand will not carry this machine's home directory.
    #[tokio::test]
    async fn the_short_spelling_of_a_root_also_resolves() {
        let store = InMemoryTierStore::default();
        let ctx = tenant();
        seed_tier_rules(&store, &ctx, "/Users/bkearns/src/research", "ben")
            .await
            .expect("seed");

        let id = Uuid::new_v4();
        store
            .record_source(&ctx, draft(id, "research/skills/elixir.md"))
            .await
            .expect("record");
        assert_eq!(
            tier_of(&store, &ctx, id).await.expect("tier").tier,
            Tier::Wisdom
        );
    }

    /// The obvious way to fix a wrong alias is to run the seed again, so
    /// running it twice must not double anything.
    #[tokio::test]
    async fn seeding_twice_is_the_same_as_seeding_once() {
        let store = InMemoryTierStore::default();
        let ctx = tenant();
        seed_tier_rules(&store, &ctx, "/Users/bkearns/src/research", "ben")
            .await
            .expect("a");
        let after_one = store.aliases(&ctx).await.expect("read").len();
        seed_tier_rules(&store, &ctx, "/Users/bkearns/src/research", "ben")
            .await
            .expect("b");
        assert_eq!(after_one, store.aliases(&ctx).await.expect("read").len());
    }

    /// The deeper root must win over the shallower one it sits inside, or
    /// every rule file tiers as a skill.
    #[tokio::test]
    async fn the_rules_directory_beats_the_skills_directory_it_sits_in() {
        let store = InMemoryTierStore::default();
        let ctx = tenant();
        seed_tier_rules(&store, &ctx, "/Users/bkearns/src/research", "ben")
            .await
            .expect("seed");

        let id = Uuid::new_v4();
        let record = store
            .record_source(
                &ctx,
                draft(id, "/Users/bkearns/src/research/skills/rules/safety.md"),
            )
            .await
            .expect("record");
        assert_eq!(record.source_root.as_deref(), Some("research/rules"));
    }

    /// Every rule the seed writes must be REACHABLE.
    ///
    /// A root rule whose canonical root no alias produces can never fire. That
    /// shipped: `session-capture -> data` was written and looked correct in
    /// every listing of the rules, while 2,791 real paths beginning
    /// `session-capture/` resolved to no root at all and counted as
    /// unclassified.
    #[tokio::test]
    async fn every_seeded_rule_is_reachable_by_an_alias() {
        let store = InMemoryTierStore::default();
        let ctx = tenant();
        seed_tier_rules(&store, &ctx, "/Users/bkearns/src/research", "ben")
            .await
            .expect("seed");
        let (resolver, rules) = load_rules(&store, &ctx).await.expect("rules");

        for (root, _) in TierRules::builtin().entries() {
            let probe = format!("{root}/something.md");
            assert_eq!(
                resolver.root_of(&probe).as_deref(),
                Some(root.as_str()),
                "no alias can produce the root {root:?}, so its rule can never fire",
            );
            assert!(
                rules.tier_of_root(&root).is_some(),
                "rule for {root:?} missing"
            );
        }
    }

    /// The shape session capture actually takes on a real store.
    #[tokio::test]
    async fn a_session_capture_path_tiers_as_data() {
        let store = InMemoryTierStore::default();
        let ctx = tenant();
        seed_tier_rules(&store, &ctx, "/Users/bkearns/src/research", "ben")
            .await
            .expect("seed");

        let id = Uuid::new_v4();
        let record = store
            .record_source(
                &ctx,
                draft(id, "session-capture/Users/bkearns/src/ferrosa-suite"),
            )
            .await
            .expect("record");
        assert_eq!(record.source_root.as_deref(), Some("session-capture"));
        assert_eq!(
            tier_of(&store, &ctx, id).await.expect("tier").tier,
            Tier::Data
        );
    }

    #[tokio::test]
    async fn a_tier_string_this_build_does_not_know_is_refused() {
        // The CQL reader's guard, exercised directly: an unknown tier must not
        // silently become Data, because that downgrades curated material.
        assert!(parse_tier("wisdom", "root \"x\"").is_ok());
        let err = parse_tier("gnosis", "root \"x\"").expect_err("accepted an unknown tier");
        assert!(
            err.to_string().contains("gnosis"),
            "the error must name the value: {err}"
        );
    }

    /// The property the whole design rests on: a cursor over a page_key never
    /// repeats a row and never steps over one, INCLUDING when many rows share
    /// a millisecond. `recorded_at` alone cannot do this, which is why the key
    /// is synthetic.
    #[test]
    fn paging_a_tied_millisecond_loses_nothing() {
        let tied = DateTime::from_timestamp_millis(1_767_225_600_000).expect("epoch millis");
        // Ten rows in one millisecond, then three in later ones -- the shape
        // an ingest of a directory actually produces.
        let mut keys: Vec<String> = (0..10)
            .map(|i| page_key(tied, Uuid::from_u128(i + 1)).expect("key"))
            .chain((1..=3).map(|i| {
                let later =
                    DateTime::from_timestamp_millis(1_767_225_600_000 + i).expect("epoch millis");
                page_key(later, Uuid::from_u128(100 + i as u128)).expect("key")
            }))
            .collect();
        assert_eq!(
            keys.iter().collect::<std::collections::HashSet<_>>().len(),
            13,
            "a tied millisecond must still produce distinct keys"
        );

        // Newest first, as the query orders them.
        keys.sort();
        keys.reverse();

        // Walk it in pages of five the way sources_page does: take a page,
        // then ask for everything strictly less than its last key.
        let mut seen: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let page: Vec<String> = keys
                .iter()
                .filter(|key| match &cursor {
                    None => true,
                    Some(cursor) => *key < cursor,
                })
                .take(5)
                .cloned()
                .collect();
            if page.is_empty() {
                break;
            }
            cursor = page.last().cloned();
            seen.extend(page);
        }

        assert_eq!(seen, keys, "paging must reproduce the order exactly");
        assert_eq!(
            seen.iter().collect::<std::collections::HashSet<_>>().len(),
            13,
            "no row may be served twice"
        );
    }

    /// Zero-padding is what makes a lexicographic compare a chronological one.
    /// Without it "9999" sorts above "10000" and the list silently reorders
    /// around a digit boundary.
    #[test]
    fn page_keys_sort_chronologically_across_a_digit_boundary() {
        let id = Uuid::from_u128(1);
        let early = page_key(
            DateTime::from_timestamp_millis(999_999_999_999).expect("epoch millis"),
            id,
        )
        .expect("key");
        let late = page_key(
            DateTime::from_timestamp_millis(1_000_000_000_000).expect("epoch millis"),
            id,
        )
        .expect("key");
        assert!(early < late, "{early} should sort below {late}");
    }

    /// A timestamp that cannot be ordered is a corrupt row, and saying so
    /// beats emitting a key that sorts in the wrong place forever.
    #[test]
    fn a_pre_epoch_timestamp_is_refused_rather_than_mis_sorted() {
        let before = DateTime::from_timestamp_millis(-1).expect("epoch millis");
        assert!(page_key(before, Uuid::from_u128(1)).is_err());
    }
}
