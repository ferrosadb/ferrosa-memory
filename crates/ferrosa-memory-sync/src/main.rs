//! memory-sync — replicate memories between two Ferrosa CQL clusters.
//!
//! Reads all memory data for a tenant from the source cluster and upserts it
//! into the destination cluster. Idempotent: safe to re-run; CQL INSERT is
//! upsert-by-primary-key for all synced tables.
//!
//! # Usage
//!
//! ```sh
//! memory-sync --source remote.toml --dest local.toml --tenant-id <UUID>
//! memory-sync --source remote.toml --dest local.toml --tenant-id <UUID> --dry-run
//! ```
//!
//! Both config files use `ferrosa-memory.toml` format. Only the `[ferrosa]`
//! section (contact_points, keyspace) is required in each.

use anyhow::Context;
use clap::{Parser, Subcommand};
use ferrosa_memory_core::cql_storage::CqlStorage;
use ferrosa_memory_core::graph::{GraphClient, GraphConfig};
use ferrosa_memory_core::remote_identity::{InstanceId, PublicKeyFingerprint};
use ferrosa_memory_core::remotes::policy::{PolicyAction, PolicyFact, RemotePolicy};
use ferrosa_memory_core::remotes::types::{
    MemoryRemote, RemoteDeny, RemoteGrant, RemotePolicyFact, RemotePolicyKind, RemoteTrustClass,
};
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::types::{FoldStatus, TenantContext};
use futures_util::StreamExt;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[derive(Parser)]
#[command(about = "Sync memories between two Ferrosa CQL clusters")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Replicate all memories for a tenant from source to destination
    Sync {
        /// Path to source server config (ferrosa-memory.toml format)
        #[arg(long)]
        source: std::path::PathBuf,
        /// Path to destination server config (ferrosa-memory.toml format)
        #[arg(long)]
        dest: std::path::PathBuf,
        /// Tenant UUID to sync
        #[arg(long)]
        tenant_id: Uuid,
        /// Report what would be synced without writing to the destination
        #[arg(long)]
        dry_run: bool,
    },
    /// List tenant IDs present on a cluster (run this first to find your tenant UUID)
    Discover {
        /// Path to config for the cluster to inspect
        #[arg(long)]
        source: std::path::PathBuf,
    },
    /// Manage tenant-scoped remote-memory registry and policy facts
    Remote {
        /// Path to local cluster config (ferrosa-memory.toml format)
        #[arg(long)]
        config: std::path::PathBuf,
        /// Tenant UUID that owns the remote registry rows
        #[arg(long)]
        tenant_id: Uuid,
        #[command(subcommand)]
        action: RemoteCommand,
    },
}

#[derive(Subcommand)]
enum RemoteCommand {
    /// List registered remotes for this tenant
    List {
        /// Maximum rows to print
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Register or update a remote endpoint
    Add {
        #[arg(long)]
        remote_id: Uuid,
        #[arg(long)]
        name: String,
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        instance_id: Uuid,
        #[arg(long)]
        public_key_fingerprint: String,
        #[arg(long, default_value = "external")]
        trust_class: String,
    },
    /// Append/update one Datalog-backed policy fact for a remote
    UpdatePolicy {
        #[arg(long)]
        remote_id: Uuid,
        #[arg(long, default_value = "grant")]
        kind: String,
        #[arg(long, default_value = "read")]
        action: String,
        #[arg(long, default_value = "*")]
        namespace: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long, default_value_t = 1.0)]
        weight: f64,
    },
    /// Soft-disable a remote; provenance and policy rows are preserved
    Remove {
        #[arg(long)]
        remote_id: Uuid,
    },
    /// Show local registration health for a remote
    Health {
        #[arg(long)]
        remote_id: Uuid,
    },
    /// Show local protocol capabilities for a remote
    Capabilities {
        #[arg(long)]
        remote_id: Uuid,
    },
    /// Explain policy decisions for a remote
    ExplainPolicy {
        #[arg(long)]
        remote_id: Uuid,
        #[arg(long, default_value = "read")]
        action: String,
        #[arg(long, default_value = "knowledge")]
        namespace: String,
    },
}

#[derive(Default)]
struct SyncStats {
    entities: usize,
    folds: usize,
    temporal_events: usize,
    intentions: usize,
    feedback: usize,
    edges_folded_into: usize,
    edges_mentioned_in: usize,
    edges_co_occurs: usize,
    edges_supersedes: usize,
    errors: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    match args.command {
        Command::Discover { source } => cmd_discover(&source).await,
        Command::Sync {
            source,
            dest,
            tenant_id,
            dry_run,
        } => cmd_sync(&source, &dest, tenant_id, dry_run).await,
        Command::Remote {
            config,
            tenant_id,
            action,
        } => cmd_remote(&config, tenant_id, action).await,
    }
}

async fn connect_local(config_path: &std::path::Path) -> anyhow::Result<CqlStorage> {
    let config = ferrosa_memory_core::config::parse_config(
        &std::fs::read_to_string(config_path)
            .with_context(|| format!("reading {}", config_path.display()))?,
    )?;
    CqlStorage::connect(&config.ferrosa)
        .await
        .context("connecting to local cluster")
}

fn remote_policy_action(action: &str) -> anyhow::Result<PolicyAction> {
    match action {
        "read" => Ok(PolicyAction::Read),
        "detail_fetch" | "fetch_detail" | "detail" => Ok(PolicyAction::DetailFetch),
        "autocommit" | "auto_commit" => Ok(PolicyAction::Autocommit),
        "requires_activation" => Ok(PolicyAction::RequiresActivation),
        "should_consult" => Ok(PolicyAction::ShouldConsult),
        other => anyhow::bail!(
            "unknown policy action {other:?}; expected read, detail_fetch, autocommit, requires_activation, or should_consult"
        ),
    }
}

fn remote_policy_action_name(action: PolicyAction) -> &'static str {
    match action {
        PolicyAction::Read => "read",
        PolicyAction::DetailFetch => "detail_fetch",
        PolicyAction::Autocommit => "autocommit",
        PolicyAction::RequiresActivation => "requires_activation",
        PolicyAction::ShouldConsult => "should_consult",
    }
}

fn remote_policy_kind(
    kind: &str,
    action: &str,
    namespace: String,
) -> anyhow::Result<RemotePolicyKind> {
    match kind {
        "grant" | "allow" => Ok(RemotePolicyKind::Grant(RemoteGrant {
            namespace,
            grant: action.to_string(),
        })),
        "deny" | "block" => Ok(RemotePolicyKind::Deny(RemoteDeny {
            namespace,
            deny: action.to_string(),
        })),
        other => anyhow::bail!("unknown policy kind {other:?}; expected grant or deny"),
    }
}

fn remote_trust_class(value: &str) -> anyhow::Result<RemoteTrustClass> {
    match value {
        "personal" => Ok(RemoteTrustClass::Personal),
        "team" => Ok(RemoteTrustClass::Team),
        "partner" | "external" => Ok(RemoteTrustClass::Partner),
        "public" => Ok(RemoteTrustClass::Public),
        "archive" => Ok(RemoteTrustClass::Archive),
        other => anyhow::bail!(
            "unknown trust class {other:?}; expected personal, team, partner, public, external, or archive"
        ),
    }
}

async fn cmd_remote(
    config_path: &std::path::Path,
    tenant_id: Uuid,
    action: RemoteCommand,
) -> anyhow::Result<()> {
    let storage = connect_local(config_path).await?;
    let ctx = TenantContext {
        tenant_id,
        session_origin: "memory-sync-remote".into(),
    };

    match action {
        RemoteCommand::List { limit } => {
            let remotes = storage.remote_list(&ctx, limit).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "remotes": remotes }))?
            );
        }
        RemoteCommand::Add {
            remote_id,
            name,
            endpoint,
            instance_id,
            public_key_fingerprint,
            trust_class,
        } => {
            if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
                anyhow::bail!("remote endpoint must start with http:// or https://");
            }
            if endpoint.contains('@') {
                anyhow::bail!("remote endpoint must not contain credentials");
            }
            let remote = MemoryRemote {
                remote_id,
                name,
                endpoint,
                instance_id: InstanceId(instance_id),
                public_key_fingerprint: PublicKeyFingerprint(public_key_fingerprint),
                trust_class: remote_trust_class(&trust_class)?,
                enabled: true,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            };
            storage.remote_put(&ctx, &remote).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "remote_id": remote_id, "stored": true })
                )?
            );
        }
        RemoteCommand::UpdatePolicy {
            remote_id,
            kind,
            action,
            namespace,
            reason: _,
            weight: _,
        } => {
            let action = remote_policy_action(&action)?;
            let kind = remote_policy_kind(&kind, remote_policy_action_name(action), namespace)?;
            let fact = RemotePolicyFact {
                remote_id,
                fact_id: Uuid::new_v4(),
                kind,
                created_at: chrono::Utc::now(),
                expires_at: None,
            };
            storage.remote_policy_put(&ctx, &fact).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "remote_id": remote_id, "fact_id": fact.fact_id })
                )?
            );
        }
        RemoteCommand::Remove { remote_id } => {
            let mut remote = storage
                .remote_get(&ctx, remote_id)
                .await?
                .context("unknown remote_id")?;
            remote.enabled = false;
            remote.updated_at = chrono::Utc::now();
            storage.remote_put(&ctx, &remote).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "remote_id": remote_id, "disabled": true, "preserved_provenance": true })
                )?
            );
        }
        RemoteCommand::Health { remote_id } => {
            let remote = storage
                .remote_get(&ctx, remote_id)
                .await?
                .context("unknown remote_id")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "remote_id": remote_id,
                    "registered": true,
                    "enabled": remote.enabled,
                    "endpoint": remote.endpoint,
                    "status": if remote.enabled { "configured" } else { "disabled" }
                }))?
            );
        }
        RemoteCommand::Capabilities { remote_id } => {
            let remote = storage
                .remote_get(&ctx, remote_id)
                .await?
                .context("unknown remote_id")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "remote_id": remote_id,
                    "enabled": remote.enabled,
                    "capabilities": ["teach_query_stream", "pull_preview", "pull_commit", "remote_detail", "archive_detail"],
                    "supports_policy_explain": true,
                    "supports_provenance": true
                }))?
            );
        }
        RemoteCommand::ExplainPolicy {
            remote_id,
            action,
            namespace,
        } => {
            let remote = storage
                .remote_get(&ctx, remote_id)
                .await?
                .context("unknown remote_id")?;
            let rows = storage.remote_policy_list(&ctx, remote_id).await?;
            let mut facts = vec![PolicyFact::remote(remote.name.clone())];
            for row in &rows {
                if let Some(fact) = policy_fact_from_row(&remote.name, row)? {
                    facts.push(fact);
                }
            }
            let policy = RemotePolicy::from_facts(facts);
            let item = ferrosa_memory_core::remotes::policy::PolicyItem::new(
                "memory_sync_cli_probe",
                namespace.clone(),
            );
            let decision = match remote_policy_action(&action)? {
                PolicyAction::Read => policy.can_query(&remote.name, &namespace),
                PolicyAction::DetailFetch => policy.can_fetch_detail(&remote.name, &item),
                PolicyAction::Autocommit => policy.can_autocommit(&remote.name, &item),
                PolicyAction::RequiresActivation => policy.requires_activation(&remote.name, &item),
                PolicyAction::ShouldConsult => policy.should_consult(&remote.name, &namespace),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "remote_id": remote_id,
                    "remote_name": remote.name,
                    "action": action,
                    "namespace": namespace,
                    "allowed": decision.allowed,
                    "explanation": decision.explanation,
                    "reasons": decision.reasons.iter().map(|reason| serde_json::json!({
                        "code": reason.code,
                        "fact": reason.fact,
                        "message": reason.message,
                    })).collect::<Vec<_>>(),
                    "policy_fact_count": rows.len(),
                }))?
            );
        }
    }
    Ok(())
}

fn policy_fact_from_row(
    remote_name: &str,
    row: &RemotePolicyFact,
) -> anyhow::Result<Option<PolicyFact>> {
    match &row.kind {
        RemotePolicyKind::Grant(grant) => match grant.grant.as_str() {
            "trusted_for" => Ok(Some(PolicyFact::trusted_for(
                remote_name,
                grant.namespace.clone(),
            ))),
            "fallback_enabled" => Ok(Some(PolicyFact::fallback_enabled(
                remote_name,
                grant.namespace.clone(),
            ))),
            action => Ok(Some(PolicyFact::grant(
                remote_name,
                remote_policy_action(action)?,
                grant.namespace.clone(),
            ))),
        },
        RemotePolicyKind::Deny(deny) => match deny.deny.as_str() {
            "not_trusted_for" => Ok(Some(PolicyFact::not_trusted_for(
                remote_name,
                deny.namespace.clone(),
            ))),
            action => Ok(Some(PolicyFact::deny(
                remote_name,
                remote_policy_action(action)?,
                deny.namespace.clone(),
            ))),
        },
    }
}

async fn cmd_sync(
    source: &std::path::Path,
    dest: &std::path::Path,
    tenant_id: Uuid,
    dry_run: bool,
) -> anyhow::Result<()> {
    let src_config = ferrosa_memory_core::config::parse_config(
        &std::fs::read_to_string(source)
            .with_context(|| format!("reading {}", source.display()))?,
    )?;
    let dst_config = ferrosa_memory_core::config::parse_config(
        &std::fs::read_to_string(dest).with_context(|| format!("reading {}", dest.display()))?,
    )?;

    tracing::info!(
        source = ?src_config.ferrosa.contact_points,
        dest   = ?dst_config.ferrosa.contact_points,
        %tenant_id,
        dry_run,
        "memory-sync starting"
    );

    let src = CqlStorage::connect(&src_config.ferrosa)
        .await
        .context("connecting to source cluster")?;
    let dst = CqlStorage::connect(&dst_config.ferrosa)
        .await
        .context("connecting to destination cluster")?;
    let dst_graph = GraphClient::connect(&GraphConfig {
        http_url: dst_config.graph.http_url.clone(),
        username: dst_config.graph.username.clone(),
        password: dst_config.graph.password.clone(),
        keyspace: dst_config.ferrosa.keyspace.clone(),
    })
    .await
    .context("connecting to destination graph endpoint")?;

    let ctx = TenantContext {
        tenant_id,
        session_origin: "memory-sync".into(),
    };
    let stats = sync_all(&src, &dst, &dst_graph, &ctx, dry_run).await?;

    tracing::info!(
        entities = stats.entities,
        folds = stats.folds,
        temporal_events = stats.temporal_events,
        intentions = stats.intentions,
        feedback = stats.feedback,
        edges_folded_into = stats.edges_folded_into,
        edges_mentioned_in = stats.edges_mentioned_in,
        edges_co_occurs = stats.edges_co_occurs,
        edges_supersedes = stats.edges_supersedes,
        errors = stats.errors,
        "sync complete"
    );
    if dry_run {
        tracing::info!("dry-run: no writes were performed");
    }
    if stats.errors > 0 {
        tracing::warn!(count = stats.errors, "some records failed to sync");
    }
    Ok(())
}

/// Discover tenant IDs present on a cluster.
async fn cmd_discover(config_path: &std::path::Path) -> anyhow::Result<()> {
    use ferrosa_memory_core::cql_storage::{build_col_map, cql_get};
    use futures_util::StreamExt;
    use std::collections::BTreeSet;

    let config = ferrosa_memory_core::config::parse_config(
        &std::fs::read_to_string(config_path)
            .with_context(|| format!("reading {}", config_path.display()))?,
    )?;

    tracing::info!(contact_points = ?config.ferrosa.contact_points, "connecting");
    let storage = CqlStorage::connect(&config.ferrosa)
        .await
        .context("connecting to cluster")?;

    let ks = storage.keyspace();
    let mut tenants: BTreeSet<Uuid> = BTreeSet::new();

    // Sample each table that has tenant_id as a top-level column
    let queries = [
        format!("SELECT tenant_id FROM {ks}.entity_store LIMIT 1000 ALLOW FILTERING"),
        format!("SELECT tenant_id FROM {ks}.feedback_outcomes LIMIT 1000"),
        format!("SELECT tenant_id FROM {ks}.intentions LIMIT 1000"),
        format!("SELECT tenant_id FROM {ks}.temporal_events LIMIT 1000 ALLOW FILTERING"),
    ];

    for query in &queries {
        #[allow(deprecated)]
        match storage.session().query_iter(query.to_owned(), ()).await {
            Ok(mut iter) => {
                let col_map = build_col_map(iter.get_column_specs());
                while let Some(row) = iter.next().await {
                    match row {
                        Ok(row) => {
                            if let Ok(tid) = cql_get::<Uuid>(&row, &col_map, "tenant_id") {
                                tenants.insert(tid);
                            }
                        }
                        Err(e) => tracing::warn!(query, %e, "row decode failed"),
                    }
                }
            }
            Err(e) => tracing::warn!(query, %e, "query failed"),
        }
    }

    if tenants.is_empty() {
        println!("No tenants found — cluster may be empty.");
    } else {
        println!(
            "Tenants found on {}:",
            config.ferrosa.contact_points.join(", ")
        );
        for t in &tenants {
            println!("  {t}");
        }
    }

    Ok(())
}

async fn sync_all(
    src: &CqlStorage,
    dst: &CqlStorage,
    dst_graph: &GraphClient,
    ctx: &TenantContext,
    dry_run: bool,
) -> anyhow::Result<SyncStats> {
    let mut stats = SyncStats::default();

    // --- Entities ---
    let entities = src
        .entity_list_all(ctx)
        .await
        .context("listing entities from source")?;
    stats.entities = entities.len();
    tracing::info!(count = entities.len(), "syncing entities");
    if !dry_run {
        for e in &entities {
            if let Err(err) = dst.entity_put(ctx, e).await {
                tracing::warn!(entity_id = %e.entity_id, %err, "entity sync failed");
                stats.errors += 1;
            }
        }
    }

    // --- Folds ---
    let folds = src
        .fold_list_all(ctx)
        .await
        .context("listing folds from source")?;
    stats.folds = folds.len();
    tracing::info!(count = folds.len(), "syncing folds");
    if !dry_run {
        for fold in &folds {
            if let Err(err) = sync_fold(dst, ctx, fold).await {
                tracing::warn!(fold_id = %fold.fold_id, %err, "fold sync failed");
                stats.errors += 1;
            }
        }
    }

    // --- Temporal events ---
    let events = src
        .temporal_list_all(ctx)
        .await
        .context("listing temporal events from source")?;
    stats.temporal_events = events.len();
    tracing::info!(count = events.len(), "syncing temporal events");
    if !dry_run {
        for ev in &events {
            if let Err(err) = dst.temporal_put(ctx, ev).await {
                tracing::warn!(entity_id = %ev.entity_id, event_id = %ev.event_id, %err, "temporal event sync failed");
                stats.errors += 1;
            }
        }
    }

    // --- Intentions ---
    let intentions = src
        .intention_list_all(ctx)
        .await
        .context("listing intentions from source")?;
    stats.intentions = intentions.len();
    tracing::info!(count = intentions.len(), "syncing intentions");
    if !dry_run {
        for intention in &intentions {
            if let Err(err) = dst.intention_put(ctx, intention).await {
                tracing::warn!(intention_id = %intention.id, %err, "intention sync failed");
                stats.errors += 1;
            }
        }
    }

    // --- Feedback outcomes ---
    // feedback_list_all is cross-tenant; filter to the target tenant here.
    let all_feedback = src
        .feedback_list_all()
        .await
        .context("listing feedback from source")?;
    let feedback: Vec<_> = all_feedback
        .into_iter()
        .filter(|f| f.tenant_id == ctx.tenant_id)
        .collect();
    stats.feedback = feedback.len();
    tracing::info!(count = feedback.len(), "syncing feedback outcomes");
    if !dry_run {
        for outcome in &feedback {
            let outcome_ctx = TenantContext {
                tenant_id: outcome.tenant_id,
                session_origin: "memory-sync".into(),
            };
            if let Err(err) = dst.feedback_put(&outcome_ctx, outcome).await {
                tracing::warn!(query_id = %outcome.query_id, %err, "feedback sync failed");
                stats.errors += 1;
            }
        }
    }

    // --- Edges ---
    sync_edges(src, dst_graph, ctx, dry_run, &mut stats).await?;

    Ok(stats)
}

/// Sync a single fold: INSERT base row, then UPDATE summary/embedding for Folded folds.
async fn sync_fold(
    dst: &CqlStorage,
    ctx: &TenantContext,
    fold: &ferrosa_memory_core::types::FoldEntry,
) -> anyhow::Result<()> {
    dst.fold_put(ctx, fold).await?;

    if fold.status == FoldStatus::Folded
        && let (Some(summary), Some(embedding)) =
            (fold.fold_summary.as_deref(), fold.fold_embedding.as_deref())
    {
        dst.fold_complete(
            ctx,
            fold.session_id,
            fold.fold_id,
            summary,
            embedding.to_vec(),
            fold.compression_ratio.unwrap_or(1.0),
        )
        .await?;
    }
    Ok(())
}

/// Sync all four edge tables using raw CQL reads from source + typed writes to destination.
///
/// Edge created_at and last_reinforced timestamps are reset to sync time (this is the
/// behaviour of the Storage trait write methods — edge timestamps are used only for pruning,
/// not memory recall, so this is acceptable).
async fn sync_edges(
    src: &CqlStorage,
    dst_graph: &GraphClient,
    ctx: &TenantContext,
    dry_run: bool,
    stats: &mut SyncStats,
) -> anyhow::Result<()> {
    use ferrosa_memory_core::cql_storage::cql_get;

    let keyspace = src.keyspace();

    // folded_into
    {
        let query = format!(
            "SELECT source_fold_id, target_fold_id, session_id \
             FROM {keyspace}.folded_into WHERE tenant_id = ? ALLOW FILTERING"
        );
        let (col_map, rows) = raw_query(src, &query, ctx.tenant_id).await?;
        stats.edges_folded_into = rows.len();
        tracing::info!(count = rows.len(), "syncing folded_into edges");
        if !dry_run {
            for row in &rows {
                let src_id: Uuid = cql_get::<Uuid>(row, &col_map, "source_fold_id")?;
                let tgt_id: Uuid = cql_get::<Uuid>(row, &col_map, "target_fold_id")?;
                let sid: Uuid = cql_get::<Uuid>(row, &col_map, "session_id")?;
                if let Err(err) = dst_graph
                    .put_folded_into_edge(ctx.tenant_id, sid, src_id, tgt_id)
                    .await
                {
                    tracing::warn!(%err, "folded_into edge sync failed");
                    stats.errors += 1;
                }
            }
        }
    }

    // mentioned_in
    {
        let query = format!(
            "SELECT entity_id, fold_id, session_id \
             FROM {keyspace}.mentioned_in WHERE tenant_id = ? ALLOW FILTERING"
        );
        let (col_map, rows) = raw_query(src, &query, ctx.tenant_id).await?;
        stats.edges_mentioned_in = rows.len();
        tracing::info!(count = rows.len(), "syncing mentioned_in edges");
        if !dry_run {
            for row in &rows {
                let entity_id: Uuid = cql_get::<Uuid>(row, &col_map, "entity_id")?;
                let fold_id: Uuid = cql_get::<Uuid>(row, &col_map, "fold_id")?;
                let sid: Uuid = cql_get::<Uuid>(row, &col_map, "session_id")?;
                if let Err(err) = dst_graph
                    .put_mentioned_in_edge(ctx.tenant_id, sid, entity_id, fold_id)
                    .await
                {
                    tracing::warn!(%err, "mentioned_in edge sync failed");
                    stats.errors += 1;
                }
            }
        }
    }

    // co_occurs_with (preserves strength)
    {
        let query = format!(
            "SELECT entity_a, entity_b, session_id, strength \
             FROM {keyspace}.co_occurs_with WHERE tenant_id = ? ALLOW FILTERING"
        );
        let (col_map, rows) = raw_query(src, &query, ctx.tenant_id).await?;
        stats.edges_co_occurs = rows.len();
        tracing::info!(count = rows.len(), "syncing co_occurs_with edges");
        if !dry_run {
            for row in &rows {
                let a: Uuid = cql_get::<Uuid>(row, &col_map, "entity_a")?;
                let b: Uuid = cql_get::<Uuid>(row, &col_map, "entity_b")?;
                let sid: Uuid = cql_get::<Uuid>(row, &col_map, "session_id")?;
                let strength: f32 = cql_get::<f32>(row, &col_map, "strength").unwrap_or(1.0);
                if let Err(err) = dst_graph
                    .put_co_occurs_edge(ctx.tenant_id, sid, a, b, strength)
                    .await
                {
                    tracing::warn!(%err, "co_occurs_with edge sync failed");
                    stats.errors += 1;
                }
            }
        }
    }

    // supersedes
    {
        let query = format!(
            "SELECT new_event_id, old_event_id, entity_id \
             FROM {keyspace}.supersedes WHERE tenant_id = ? ALLOW FILTERING"
        );
        let (col_map, rows) = raw_query(src, &query, ctx.tenant_id).await?;
        stats.edges_supersedes = rows.len();
        tracing::info!(count = rows.len(), "syncing supersedes edges");
        if !dry_run {
            for row in &rows {
                let new_id: Uuid = cql_get::<Uuid>(row, &col_map, "new_event_id")?;
                let old_id: Uuid = cql_get::<Uuid>(row, &col_map, "old_event_id")?;
                let entity_id: Uuid = cql_get::<Uuid>(row, &col_map, "entity_id")?;
                if let Err(err) = dst_graph
                    .put_supersedes_edge(ctx.tenant_id, entity_id, new_id, old_id)
                    .await
                {
                    tracing::warn!(%err, "supersedes edge sync failed");
                    stats.errors += 1;
                }
            }
        }
    }

    Ok(())
}

/// Execute a raw CQL query filtered by tenant_id and return (col_map, rows).
async fn raw_query(
    storage: &CqlStorage,
    query: &str,
    tenant_id: Uuid,
) -> anyhow::Result<(
    ferrosa_memory_core::cql_storage::ColMap,
    Vec<scylla::frame::response::result::Row>,
)> {
    #[allow(deprecated)]
    let mut iter = storage
        .session()
        .query_iter(query.to_string(), (tenant_id,))
        .await
        .with_context(|| format!("raw query failed: {query}"))?;
    let col_map = ferrosa_memory_core::cql_storage::build_col_map(iter.get_column_specs());
    let mut rows = Vec::new();
    while let Some(row) = iter.next().await {
        rows.push(row.with_context(|| format!("raw query row decode failed: {query}"))?);
    }
    Ok((col_map, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_cli_parses_list_and_add_commands() {
        let tenant_id = Uuid::new_v4();
        let remote_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();

        let list = Args::try_parse_from([
            "memory-sync",
            "remote",
            "--config",
            "local.toml",
            "--tenant-id",
            &tenant_id.to_string(),
            "list",
            "--limit",
            "10",
        ])
        .unwrap();
        assert!(matches!(
            list.command,
            Command::Remote {
                action: RemoteCommand::List { limit: 10 },
                ..
            }
        ));

        let add = Args::try_parse_from([
            "memory-sync",
            "remote",
            "--config",
            "local.toml",
            "--tenant-id",
            &tenant_id.to_string(),
            "add",
            "--remote-id",
            &remote_id.to_string(),
            "--name",
            "gpu",
            "--endpoint",
            "https://gpu.example/mcp",
            "--instance-id",
            &instance_id.to_string(),
            "--public-key-fingerprint",
            "ed25519:gpu",
            "--trust-class",
            "personal",
        ])
        .unwrap();
        assert!(matches!(
            add.command,
            Command::Remote {
                action: RemoteCommand::Add { name, endpoint, .. },
                ..
            } if name == "gpu" && endpoint == "https://gpu.example/mcp"
        ));
    }

    #[test]
    fn remote_cli_parses_policy_and_explain_namespace() {
        let tenant_id = Uuid::new_v4();
        let remote_id = Uuid::new_v4();

        let update = Args::try_parse_from([
            "memory-sync",
            "remote",
            "--config",
            "local.toml",
            "--tenant-id",
            &tenant_id.to_string(),
            "update-policy",
            "--remote-id",
            &remote_id.to_string(),
            "--kind",
            "grant",
            "--action",
            "autocommit",
            "--namespace",
            "knowledge",
        ])
        .unwrap();
        assert!(matches!(
            update.command,
            Command::Remote {
                action: RemoteCommand::UpdatePolicy { action, namespace, .. },
                ..
            } if action == "autocommit" && namespace == "knowledge"
        ));

        let explain = Args::try_parse_from([
            "memory-sync",
            "remote",
            "--config",
            "local.toml",
            "--tenant-id",
            &tenant_id.to_string(),
            "explain-policy",
            "--remote-id",
            &remote_id.to_string(),
            "--action",
            "should_consult",
            "--namespace",
            "gpu_builds",
        ])
        .unwrap();
        assert!(matches!(
            explain.command,
            Command::Remote {
                action: RemoteCommand::ExplainPolicy { action, namespace, .. },
                ..
            } if action == "should_consult" && namespace == "gpu_builds"
        ));
    }
}
