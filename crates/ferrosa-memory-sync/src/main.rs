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
    /// Generate a P2P device identity key file (MAAS-T-36). Prints the public
    /// key hex to register at the gateway (POST /v1/devices) + its fingerprint.
    #[cfg(feature = "webrtc-transport")]
    P2pKeygen {
        /// Where to write the key file (created 0600).
        #[arg(long)]
        out: std::path::PathBuf,
    },
    /// Approve one pending device using an already-approved device identity.
    #[cfg(feature = "webrtc-transport")]
    DeviceApprove {
        /// Gateway base URL.
        #[arg(long)]
        gateway: String,
        /// Already-approved device key file (from p2p-keygen).
        #[arg(long)]
        identity: std::path::PathBuf,
        /// Exact pending device id to approve.
        #[arg(long)]
        device_id: Uuid,
    },
    /// Offer + stream a sealed pack to a mutual contact through the MaaS
    /// broker (teacher side).
    #[cfg(feature = "webrtc-transport")]
    P2pShare {
        /// Gateway base URL (e.g. https://gw.example).
        #[arg(long)]
        gateway: String,
        /// Enrolled device key file. The ONLY credential — it signs every
        /// request, so no bearer key sits on this machine.
        #[arg(long)]
        identity: std::path::PathBuf,
        /// The learner's account id (must be a mutual contact).
        #[arg(long)]
        learner_account: Uuid,
        /// JSON file holding the TeacherSelection to pack.
        #[arg(long)]
        selection: std::path::PathBuf,
        /// Namespace label carried in pack provenance.
        #[arg(long, default_value = "default")]
        namespace: String,
    },
    /// Accept a pending offer and receive the pack into a landing directory
    /// (learner side).
    #[cfg(feature = "webrtc-transport")]
    P2pReceive {
        /// Gateway base URL.
        #[arg(long)]
        gateway: String,
        /// Enrolled device key file. The ONLY credential — it signs every
        /// request, so no bearer key sits on this machine.
        #[arg(long)]
        identity: std::path::PathBuf,
        /// Landing directory for applied packs (durable JSON; the
        /// storage-backed apply store is a separate packet).
        #[arg(long)]
        out_dir: std::path::PathBuf,
        /// Accept a specific session; defaults to the only pending offer
        /// (errors if there are zero or several — never guesses).
        #[arg(long)]
        session: Option<Uuid>,
    },
    /// Poll for mobile control offers addressed to this registered device,
    /// bind one direct signed WebRTC channel, and serve it until disconnect.
    #[cfg(feature = "webrtc-transport")]
    ControlListen {
        /// Gateway base URL.
        #[arg(long)]
        gateway: String,
        /// Enrolled device key file (from `fmem login` or `p2p-keygen`).
        ///
        /// The ONLY credential. This path no longer takes an API key: the
        /// identity signs every request, so there is no bearer secret sitting
        /// on the machine for an attacker to lift.
        #[arg(long)]
        identity: std::path::PathBuf,
        /// One absolute project directory available to the managed Codex CLI.
        #[arg(long)]
        workspace: std::path::PathBuf,
        /// Restrict durable control traffic to these CQL nodes. Repeatable.
        #[arg(long = "contact-point")]
        contact_points: Vec<String>,
        /// Use an already-current schema without issuing startup DDL.
        #[arg(long)]
        existing_schema: bool,
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
        #[cfg(feature = "webrtc-transport")]
        Command::P2pKeygen { out } => cmd_p2p_keygen(&out),
        #[cfg(feature = "webrtc-transport")]
        Command::DeviceApprove {
            gateway,
            identity,
            device_id,
        } => cmd_device_approve(&gateway, &identity, device_id).await,
        #[cfg(feature = "webrtc-transport")]
        Command::P2pShare {
            gateway,
            identity,
            learner_account,
            selection,
            namespace,
        } => cmd_p2p_share(&gateway, &identity, learner_account, &selection, &namespace).await,
        #[cfg(feature = "webrtc-transport")]
        Command::P2pReceive {
            gateway,
            identity,
            out_dir,
            session,
        } => cmd_p2p_receive(&gateway, &identity, &out_dir, session).await,
        #[cfg(feature = "webrtc-transport")]
        Command::ControlListen {
            gateway,
            identity,
            workspace,
            contact_points,
            existing_schema,
        } => {
            cmd_control_listen(
                &gateway,
                &identity,
                &workspace,
                &contact_points,
                existing_schema,
            )
            .await
        }
    }
}

#[cfg(feature = "webrtc-transport")]
fn cmd_p2p_keygen(out: &std::path::Path) -> anyhow::Result<()> {
    use ferrosa_memory_sync::peer_cli;
    let identity = peer_cli::keygen(out)?;
    let public = identity.public_identity();
    let public_hex: String = public
        .public_key
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    println!("device key written: {}", out.display());
    println!("instance_id:        {}", public.instance_id.0);
    println!("public_key (register this at POST /v1/devices): {public_hex}");
    println!("fingerprint:        {}", public.public_key_fingerprint.0);
    Ok(())
}

#[cfg(feature = "webrtc-transport")]
async fn cmd_device_approve(
    gateway: &str,
    identity_path: &std::path::Path,
    target_device_id: Uuid,
) -> anyhow::Result<()> {
    use ferrosa_memory_sync::peer_cli;
    use ferrosa_memory_sync::signaling_client::HttpSignalingClient;

    let identity = std::sync::Arc::new(peer_cli::load_identity(identity_path)?);
    let fingerprint = identity.public_identity().public_key_fingerprint.0;
    // The approving device signs for itself. It already had to prove possession
    // of an enrolled key to be allowed to vouch at all, so an API key here was
    // a second, weaker credential doing no additional work.
    let api = HttpSignalingClient::with_credential(
        gateway,
        ferrosa_memory_sync::signaling_client::Credential::device(std::sync::Arc::clone(&identity)),
    );
    let account = api.whoami().await?;
    let devices = api.devices().await?;
    let approver = devices
        .iter()
        .find(|device| device.fingerprint == fingerprint && device.revoked_at.is_none())
        .context("the supplied identity is not a live registered device")?;
    let pending = api.pending_devices().await?;
    let target = pending
        .iter()
        .find(|device| device.device_id == target_device_id)
        .context("the exact target device is not pending")?;
    let message = format!(
        "maas-device-approval:v1:{}:{}",
        account.account_id, target.fingerprint
    );
    let signature = identity.sign_bytes(message.as_bytes());
    let signature_hex: String = signature
        .0
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    api.approve_device(target.device_id, approver.device_id, &signature_hex)
        .await?;
    println!(
        "approved device {} ({}) with enrolled device {}",
        target.device_id, target.label, approver.device_id
    );
    Ok(())
}

#[cfg(feature = "webrtc-transport")]
async fn cmd_p2p_share(
    gateway: &str,
    identity_path: &std::path::Path,
    learner_account: Uuid,
    selection_path: &std::path::Path,
    namespace: &str,
) -> anyhow::Result<()> {
    use chrono::Utc;
    use ferrosa_memory_sync::pack::{CipherSuite, PackProvenanceEnvelope};
    use ferrosa_memory_sync::peer_cli;
    use ferrosa_memory_sync::peer_session::{PeerSessionConfig, run_teacher_session};
    use ferrosa_memory_sync::replication::{PackBuildParams, TeacherSelection};
    use ferrosa_memory_sync::signaling_client::{Credential, HttpSignalingClient, SignalingApi};

    let identity = std::sync::Arc::new(peer_cli::load_identity(identity_path)?);
    let selection: TeacherSelection = serde_json::from_slice(&std::fs::read(selection_path)?)?;
    let api = HttpSignalingClient::with_credential(
        gateway,
        Credential::device(std::sync::Arc::clone(&identity)),
    );
    let public = identity.public_identity();

    let pack_id = Uuid::new_v4();
    let session_id = api
        .offer(learner_account, pack_id, &public.public_key_fingerprint.0)
        .await?;
    println!("offer created: session {session_id} pack {pack_id}");
    println!("waiting for the learner to accept…");

    let created = Utc::now();
    let mut params = PackBuildParams {
        pack_id,
        pack_version: 1,
        cipher_suite: CipherSuite::Aes256Gcm,
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        embedding_model: String::new(),
        embedding_dim: 0,
        summary_first: false,
        created_at: created,
        ttl_expires_at: None,
        provenance: PackProvenanceEnvelope {
            teacher_instance_id: public.instance_id,
            // Overwritten with the broker-vouched pair inside the driver.
            teacher_fingerprint: public.public_key_fingerprint.clone(),
            learner_fingerprint: public.public_key_fingerprint.clone(),
            request_id: None,
            source_namespace: namespace.to_string(),
        },
    };
    let report = run_teacher_session(
        &api,
        &identity,
        session_id,
        &selection,
        &mut params,
        &PeerSessionConfig::default(),
    )
    .await?;
    println!(
        "pack {pack_id} delivered and applied (dropped_edges={}, dropped_temporal={})",
        report.dropped_edges, report.dropped_temporal
    );
    Ok(())
}

#[cfg(feature = "webrtc-transport")]
async fn cmd_p2p_receive(
    gateway: &str,
    identity_path: &std::path::Path,
    out_dir: &std::path::Path,
    session: Option<Uuid>,
) -> anyhow::Result<()> {
    use ferrosa_memory_sync::peer_cli::{self, DirPackApplyStore};
    use ferrosa_memory_sync::peer_session::{PeerSessionConfig, run_learner_session};
    use ferrosa_memory_sync::signaling_client::{Credential, HttpSignalingClient, SignalingApi};

    let identity = std::sync::Arc::new(peer_cli::load_identity(identity_path)?);
    let api = HttpSignalingClient::with_credential(
        gateway,
        Credential::device(std::sync::Arc::clone(&identity)),
    );

    let session_id = match session {
        Some(s) => s,
        None => {
            let pending = api.pending_offers().await?;
            match pending.as_slice() {
                [only] => {
                    println!(
                        "accepting the pending offer {} from {} (pack {})",
                        only.session_id, only.teacher_account, only.pack_id
                    );
                    only.session_id
                }
                [] => anyhow::bail!("no pending offers"),
                many => anyhow::bail!(
                    "{} pending offers — pass --session to pick one: {:?}",
                    many.len(),
                    many.iter().map(|o| o.session_id).collect::<Vec<_>>()
                ),
            }
        }
    };

    let store = DirPackApplyStore::open(out_dir)?;
    let health = run_learner_session(
        &api,
        &identity,
        session_id,
        store,
        session_id,
        &PeerSessionConfig::default(),
    )
    .await?;
    println!(
        "pack applied into {} (frames={}, applied={})",
        out_dir.display(),
        health.frames_received,
        health.packs_applied
    );
    Ok(())
}

#[cfg(feature = "webrtc-transport")]
async fn cmd_control_listen(
    gateway: &str,
    identity_path: &std::path::Path,
    workspace: &std::path::Path,
    contact_points: &[String],
    existing_schema: bool,
) -> anyhow::Result<()> {
    use std::{sync::Arc, time::Duration};

    use ferrosa_memory_core::config::load_config_with_dbaas;
    use ferrosa_memory_core::control_store::{ControlEventDraft, ControlStore, CqlControlStore};
    use ferrosa_memory_core::types::TenantContext;
    use ferrosa_memory_sync::codex_runtime::{CodexTmuxConfig, CodexTmuxRuntime};
    use ferrosa_memory_sync::control_session::{
        ControlRuntimeDispatcher, ControlSessionConfig, run_control_server_session,
    };
    use ferrosa_memory_sync::peer_cli;
    use ferrosa_memory_sync::signaling_client::{
        ControlSignalingApi, Credential, HttpSignalingClient,
    };

    let identity = Arc::new(peer_cli::load_identity(identity_path)?);
    let public = identity.public_identity();
    let fingerprint = public.public_key_fingerprint.0;
    // Device-signed, not API-key. The gateway resolves the account from the
    // signature, so the listener carries nothing that would still be useful to
    // someone who copied it off this disk.
    let api =
        HttpSignalingClient::with_credential(gateway, Credential::device(Arc::clone(&identity)));
    let config = ControlSessionConfig::default();
    let mut memory_config = load_config_with_dbaas()
        .context("loading Ferrosa Memory config for durable mobile control")?;
    if !contact_points.is_empty() {
        memory_config.ferrosa.contact_points = contact_points.to_vec();
    }
    let store = if existing_schema {
        CqlControlStore::connect_existing(&memory_config.ferrosa).await
    } else {
        CqlControlStore::connect(&memory_config.ferrosa).await
    }
    .context("connecting durable mobile control store")?;
    let control_store = Arc::new(store);
    let runtime = CodexTmuxRuntime::new(CodexTmuxConfig::new(workspace, fingerprint.clone()))
        .context("configuring Codex tmux-light runtime")?;
    let dispatcher = ControlRuntimeDispatcher::new(Arc::clone(&control_store), runtime);
    println!("control listener device fingerprint: {fingerprint}");
    println!("managed Codex workspace: {}", workspace.display());
    println!("polling {gateway} for device-targeted control offers");

    loop {
        let pending = api.control_pending_offers(&fingerprint).await?;
        if pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        for offer in pending {
            println!(
                "accepting control session {} from controller device {}",
                offer.session_id, offer.controller_device_id
            );
            let mut channel = match run_control_server_session(
                &api,
                &identity,
                offer.session_id,
                &config,
            )
            .await
            {
                Ok(channel) => channel,
                Err(error) => {
                    tracing::warn!(
                        session_id = %offer.session_id,
                        error = %error,
                        "control session bind failed"
                    );
                    continue;
                }
            };
            println!("control session {} bound directly", offer.session_id);
            let tenant = TenantContext {
                tenant_id: offer.account_id,
                session_origin: format!("mobile-control:{}", offer.session_id),
            };
            let cursor = control_store
                .reserve_cursor_block(&tenant, &fingerprint, 64)
                .await
                .context("reserving durable mobile control cursor block")?
                .start;
            control_store
                .append_event(
                    &tenant,
                    &fingerprint,
                    ControlEventDraft {
                        cursor,
                        event_id: Uuid::now_v7(),
                        command_id: None,
                        kind: "heartbeat".to_owned(),
                        payload: serde_json::json!({
                            "session_id": offer.session_id,
                            "controller_device_id": offer.controller_device_id,
                        }),
                        created_at: chrono::Utc::now(),
                    },
                )
                .await
                .context("persisting control-session heartbeat")?;
            loop {
                let frame = match channel.recv_text().await {
                    Ok(frame) => frame,
                    Err(error) => {
                        tracing::info!(
                            session_id = %offer.session_id,
                            error = %error,
                            "control session disconnected"
                        );
                        break;
                    }
                };
                match dispatcher.reply(&tenant, &fingerprint, &frame).await {
                    Ok(Some(reply)) => {
                        if let Err(error) = channel.send_text(&reply).await {
                            tracing::info!(
                                session_id = %offer.session_id,
                                error = %error,
                                "control pong send failed"
                            );
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            session_id = %offer.session_id,
                            error = %error,
                            "invalid control application frame"
                        );
                        let _ = channel.close().await;
                        break;
                    }
                }
            }
        }
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

#[cfg(all(test, feature = "webrtc-transport"))]
mod cli_tests {
    use super::*;

    #[test]
    fn control_listen_cli_contract() {
        let args = Args::try_parse_from([
            "memory-sync",
            "control-listen",
            "--gateway",
            "https://gateway.example",
            "--api-key",
            "secret",
            "--identity",
            "/tmp/device.json",
            "--workspace",
            "/tmp/project",
            "--contact-point",
            "127.0.0.1:19044",
            "--existing-schema",
        ])
        .expect("parse control-listen");

        assert!(matches!(
            args.command,
            Command::ControlListen { workspace, contact_points, existing_schema, .. }
                if workspace == std::path::Path::new("/tmp/project")
                    && contact_points == vec!["127.0.0.1:19044"]
                    && existing_schema
        ));
    }
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
