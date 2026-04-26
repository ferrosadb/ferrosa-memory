//! # ferrosa-memory-batch
//!
//! Nightly batch job for routing guideline refinement (ADR-002).
//! Also provides data migration utilities.
//!
//! ## Usage
//!
//! ```sh
//! # Run guideline refinement (default)
//! ferrosa-memory-batch
//!
//! # Migrate all entities to the configured default session_id
//! ferrosa-memory-batch migrate-session
//!
//! # With config
//! FERROSA_MEMORY_CONFIG=./ferrosa-memory.toml ferrosa-memory-batch
//! ```

use ferrosa_memory_core::batch;
use ferrosa_memory_core::cql_storage::CqlStorage;
use ferrosa_memory_core::storage::Storage;
use ferrosa_memory_core::types::TenantContext;
use futures_util::future::join_all;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

fn batch_cql_config(
    config: &ferrosa_memory_core::config::Config,
) -> ferrosa_memory_core::config::FerrosaCqlConfig {
    let mut cql = config.ferrosa.clone();
    if let (Some(admin_username), Some(admin_password)) =
        (cql.admin_username.clone(), cql.admin_password.clone())
    {
        cql.username = admin_username;
        cql.password = admin_password;
    }
    cql
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = match ferrosa_memory_core::config::load_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("config not found ({e}), using defaults");
            ferrosa_memory_core::config::parse_config(
                "[ferrosa]\ncontact_points = [\"localhost:9042\"]\n",
            )?
        }
    };

    let subcommand = std::env::args().nth(1).unwrap_or_default();

    match subcommand.as_str() {
        "migrate-session" => migrate_session(&config).await,
        "retype-entities" => retype_entities(&config).await,
        "rename-entities" => rename_entities(&config).await,
        "backfill-rich-entities" => backfill_rich_entities(&config).await,
        _ => run_guidelines(&config).await,
    }
}

/// Migrate all entities to the configured default session_id.
///
/// Reads all entities for the tenant, re-inserts with the target session_id,
/// then deletes the old session partitions.
async fn migrate_session(config: &ferrosa_memory_core::config::Config) -> anyhow::Result<()> {
    let target_sid = config
        .server
        .session_id
        .as_ref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| anyhow::anyhow!("no session_id configured in [server]"))?;

    let tenant_id = config
        .server
        .tenant_id
        .as_ref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| anyhow::anyhow!("no tenant_id configured in [server]"))?;

    let ctx = TenantContext {
        tenant_id,
        session_origin: "batch-migrate".into(),
    };

    tracing::info!(
        tenant_id = %tenant_id,
        target_session_id = %target_sid,
        "starting session migration"
    );

    let storage = CqlStorage::connect(&batch_cql_config(config)).await?;
    tracing::info!("connected to CQL cluster");

    // Read all entities for this tenant
    let entities = storage.entity_list_all(&ctx).await?;
    tracing::info!(count = entities.len(), "loaded entities");

    let mut migrated = 0;
    let mut skipped = 0;
    let mut old_sessions: std::collections::HashSet<Uuid> = std::collections::HashSet::new();

    for entity in &entities {
        if entity.session_id == target_sid {
            skipped += 1;
            continue;
        }

        old_sessions.insert(entity.session_id);

        // Re-insert with target session_id
        let mut migrated_entity = entity.clone();
        migrated_entity.session_id = target_sid;
        storage.entity_put(&ctx, &migrated_entity).await?;
        migrated += 1;
    }

    tracing::info!(
        migrated = migrated,
        skipped = skipped,
        old_sessions = old_sessions.len(),
        "entity migration complete"
    );

    // Delete old session partitions
    for old_sid in &old_sessions {
        match storage.delete_session(&ctx, *old_sid).await {
            Ok(n) => tracing::info!(session_id = %old_sid, deleted = n, "cleaned old session"),
            Err(e) => {
                tracing::warn!(session_id = %old_sid, error = %e, "failed to clean old session")
            }
        }
    }

    tracing::info!("migration complete");
    Ok(())
}

/// Re-classify entity types using two-tier NER (heuristic + LLM).
///
/// Pass 1: Fast heuristic NER resolves obvious cases (acronyms, org suffixes,
///         known tools, common names).
/// Pass 2: Entities still typed "concept" after pass 1 are sent to the local
///         Ollama model (qwen3.5:27b) for LLM-backed classification.
///
/// Only changes entities currently typed as "concept" — types explicitly set
/// by the user or LLM are preserved.
async fn retype_entities(config: &ferrosa_memory_core::config::Config) -> anyhow::Result<()> {
    let tenant_id = config
        .server
        .tenant_id
        .as_ref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| anyhow::anyhow!("no tenant_id configured in [server]"))?;

    let ctx = TenantContext {
        tenant_id,
        session_origin: "batch-retype".into(),
    };

    let storage = CqlStorage::connect(&batch_cql_config(config)).await?;
    tracing::info!("connected to CQL cluster");

    let entities = storage.entity_list_all(&ctx).await?;
    let concept_count = entities
        .iter()
        .filter(|e| e.entity_type == "concept")
        .count();
    tracing::info!(
        total = entities.len(),
        concepts = concept_count,
        "loaded entities"
    );

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let ollama_url = &config.embeddings.ollama_base_url;
    let llm_model = "qwen3.5:27b";

    let mut retyped_heuristic = 0;
    let mut retyped_llm = 0;
    let mut skipped = 0;

    for entity in &entities {
        if entity.entity_type != "concept" {
            skipped += 1;
            continue;
        }

        let new_type = ferrosa_memory_core::ner::classify_entity(
            &http,
            ollama_url,
            llm_model,
            &entity.entity_name,
            &entity.context_snippet,
        )
        .await;

        if new_type == "concept" {
            skipped += 1;
            continue;
        }

        // Track whether heuristic or LLM resolved it.
        let heuristic = ferrosa_memory_core::smart_ingest::infer_entity_type(&entity.entity_name);
        if heuristic != "concept" {
            retyped_heuristic += 1;
        } else {
            retyped_llm += 1;
        }

        let mut updated = entity.clone();
        updated.entity_type = new_type.clone();
        storage.entity_put(&ctx, &updated).await?;

        tracing::info!(
            name = %entity.entity_name,
            old_type = "concept",
            new_type,
            "retyped entity"
        );
    }

    tracing::info!(retyped_heuristic, retyped_llm, skipped, "retype complete");
    Ok(())
}

/// Re-extract entity names using three-tier NER for entities with
/// sentence-fragment names (>5 words).
async fn rename_entities(config: &ferrosa_memory_core::config::Config) -> anyhow::Result<()> {
    let tenant_id = config
        .server
        .tenant_id
        .as_ref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| anyhow::anyhow!("no tenant_id configured in [server]"))?;

    let ctx = TenantContext {
        tenant_id,
        session_origin: "batch-rename".into(),
    };

    let storage = CqlStorage::connect(&batch_cql_config(config)).await?;
    tracing::info!("connected to CQL cluster");

    let entities = storage.entity_list_all(&ctx).await?;
    let fragment_count = entities
        .iter()
        .filter(|e| e.entity_name.split_whitespace().count() > 5)
        .count();
    tracing::info!(
        total = entities.len(),
        fragments = fragment_count,
        "loaded entities"
    );

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let ollama_url = &config.embeddings.ollama_base_url;
    let ner_model = &config.embeddings.ner_model;

    let mut renamed = 0;
    let mut skipped = 0;

    for entity in &entities {
        let word_count = entity.entity_name.split_whitespace().count();
        if word_count <= 5 {
            skipped += 1;
            continue;
        }

        let (new_name, new_type) = ferrosa_memory_core::ner::extract_entity_from_content(
            &http,
            ollama_url,
            ner_model,
            &entity.context_snippet,
            &entity.entity_type,
        )
        .await;

        if new_name == entity.entity_name && new_type == entity.entity_type {
            skipped += 1;
            continue;
        }

        let mut updated = entity.clone();
        updated.entity_name = new_name.clone();
        updated.entity_type = new_type.clone();
        storage.entity_put(&ctx, &updated).await?;

        tracing::info!(
            old_name = %entity.entity_name.chars().take(50).collect::<String>(),
            new_name = %new_name,
            old_type = %entity.entity_type,
            new_type = %new_type,
            "renamed entity"
        );
        renamed += 1;
    }

    tracing::info!(renamed, skipped, "rename complete");
    Ok(())
}

/// Run the nightly guideline refinement job.
async fn run_guidelines(config: &ferrosa_memory_core::config::Config) -> anyhow::Result<()> {
    tracing::info!("ferrosa-memory-batch starting");

    tracing::info!(
        guideline_version = %config.routing.guideline_version,
        "current guideline version"
    );

    let storage = CqlStorage::connect(&batch_cql_config(config)).await?;
    tracing::info!("connected to CQL cluster");

    let outcomes = storage.feedback_list_all().await?;

    if outcomes.is_empty() {
        tracing::info!("no feedback outcomes found — nothing to compute");
        tracing::info!("ferrosa-memory-batch complete");
        return Ok(());
    }

    tracing::info!(count = outcomes.len(), "loaded feedback outcomes");

    let stats = batch::compute_strategy_accuracy(&outcomes);

    for s in &stats {
        tracing::info!(
            program_type = %s.program_type,
            task_complexity = %s.task_complexity,
            accuracy = s.accuracy,
            total = s.total,
            succeeded = s.succeeded,
            avg_latency_ms = s.avg_latency_ms,
            "strategy stats"
        );
    }

    let next_version = next_guideline_version(&config.routing.guideline_version);
    let guidelines = batch::generate_guidelines(&stats, &next_version);

    tracing::info!(
        version = %next_version,
        strategies = stats.len(),
        "generated routing guidelines"
    );

    let ks = &config.ferrosa.keyspace;
    let query = format!(
        "INSERT INTO {ks}.routing_guidelines (version, rules, created_at) VALUES (?, ?, toTimestamp(now()))"
    );
    #[allow(deprecated)]
    storage
        .session()
        .query_unpaged(query, (next_version.clone(), guidelines.clone()))
        .await?;

    tracing::info!(version = %next_version, "routing guidelines written to CQL");
    tracing::info!("ferrosa-memory-batch complete");
    Ok(())
}

// --- Rich entity schema backfill -------------------------------------

/// Parse the boundary between an ENRICHED_PREFIX'd context_snippet and the
/// original content. Returns `(description, original_context)` when the
/// input is in the legacy format, `None` otherwise.
fn split_enriched_context(raw: &str) -> Option<(String, String)> {
    const PREFIX: &str = "[enriched] ";
    const SEPARATOR: &str = "\n---\n";
    let tail = raw.strip_prefix(PREFIX)?;
    match tail.split_once(SEPARATOR) {
        Some((desc, orig)) => Some((desc.to_string(), orig.to_string())),
        // Prefix present but no separator — treat everything after the
        // prefix as the description and leave original blank.
        None => Some((tail.to_string(), String::new())),
    }
}

/// Backfill the rich entity schema columns on existing rows.
///
/// Phases:
/// - 0: regenerate `entity_embedding`, `fold_embedding`, and
///   `memo_embedding`/`result_embedding` with the configured embedding model.
/// - 1: migrate legacy ENRICHED_PREFIX context_snippet into the dedicated
///   `description` field, restoring the original extraction text to
///   `context_snippet`.
/// - 2: generate `description_embedding` for any entity with a populated
///   `description` but no embedding.
/// - 4: compute `content_hash = sha256(name || description ||
///   properties_json)` for entities that have a description but no stored
///   hash.
///
/// Flags (all via env because the batch binary is minimal):
/// - `BACKFILL_PHASES=0,1,2,4` — comma-separated phase list (default 0,1,2,4)
/// - `BACKFILL_DRY_RUN=1` — don't write
/// - `BACKFILL_FORCE=1` — re-generate description_embedding even when present
async fn backfill_rich_entities(
    config: &ferrosa_memory_core::config::Config,
) -> anyhow::Result<()> {
    let tenant_id = config
        .server
        .tenant_id
        .as_ref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| anyhow::anyhow!("no tenant_id configured in [server]"))?;
    let ctx = TenantContext {
        tenant_id,
        session_origin: "batch-backfill".into(),
    };

    let phases: std::collections::HashSet<u8> = std::env::var("BACKFILL_PHASES")
        .unwrap_or_else(|_| "0,1,2,4".into())
        .split(',')
        .filter_map(|s| s.trim().parse::<u8>().ok())
        .collect();
    let dry_run = std::env::var("BACKFILL_DRY_RUN").is_ok();
    let force = std::env::var("BACKFILL_FORCE").is_ok();
    let concurrency = std::env::var("BACKFILL_CONCURRENCY")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(8);

    tracing::info!(
        tenant_id = %tenant_id,
        phases = ?phases,
        dry_run,
        force,
        concurrency,
        "backfill-rich-entities starting"
    );

    let storage = CqlStorage::connect(&batch_cql_config(config)).await?;
    let mut entities = storage.entity_list_all(&ctx).await?;
    let folds = storage.fold_list_all(&ctx).await?;
    let memos = storage.memo_list_all(&ctx).await?;
    tracing::info!(count = entities.len(), "loaded entities for backfill");
    tracing::info!(count = folds.len(), "loaded folds for backfill");
    tracing::info!(count = memos.len(), "loaded memos for backfill");

    let embed_client = ferrosa_memory_core::embedding::EmbeddingClient::new(&config.embeddings);

    let mut p0_entities_embedded = 0usize;
    let mut p0_folds_embedded = 0usize;
    let mut p0_memos_embedded = 0usize;
    let mut p0_failed = 0usize;
    let mut p1_migrated = 0usize;
    let mut p2_embedded = 0usize;
    let mut p2_failed = 0usize;
    let mut p4_hashed = 0usize;

    if phases.contains(&0) {
        let entity_batches = entities.len().div_ceil(concurrency);
        for (batch_index, chunk) in entities.chunks(concurrency).enumerate() {
            let results = join_all(
                chunk
                    .iter()
                    .map(|entity| embed_client.embed(&entity.entity_name)),
            )
            .await;
            for (entity, result) in chunk.iter().zip(results) {
                match result {
                    Ok(embedding) => {
                        if !dry_run {
                            let now = chrono::Utc::now();
                            if let Err(e) = storage
                                .entity_update_embedding(
                                    &ctx,
                                    entity.session_id,
                                    entity.entity_id,
                                    &embedding,
                                    now,
                                )
                                .await
                            {
                                tracing::warn!(
                                    entity = %entity.entity_name,
                                    error = %e,
                                    "Phase 0: entity embedding update failed"
                                );
                                p0_failed += 1;
                                continue;
                            }
                        }
                        p0_entities_embedded += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            entity = %entity.entity_name,
                            error = %e,
                            "Phase 0: entity embedding failed"
                        );
                        p0_failed += 1;
                    }
                }
            }
            if (batch_index + 1) % 25 == 0 || batch_index + 1 == entity_batches {
                tracing::info!(
                    phase = "0/entities",
                    processed = ((batch_index + 1) * concurrency).min(entities.len()),
                    total = entities.len(),
                    succeeded = p0_entities_embedded,
                    failed = p0_failed,
                    "phase progress"
                );
            }
        }

        let fold_batches = folds.len().max(1).div_ceil(concurrency);
        for (batch_index, chunk) in folds.chunks(concurrency).enumerate() {
            let results = join_all(chunk.iter().map(|fold| async {
                let Some(summary) = fold.fold_summary.as_deref() else {
                    return Ok::<Option<Vec<f32>>, ferrosa_memory_core::embedding::EmbeddingError>(
                        None,
                    );
                };
                if summary.trim().is_empty() {
                    return Ok(None);
                }
                embed_client.embed(summary).await.map(Some)
            }))
            .await;
            for (fold, result) in chunk.iter().zip(results) {
                match result {
                    Ok(Some(embedding)) => {
                        if !dry_run
                            && let Err(e) = storage
                                .fold_update_embedding(
                                    &ctx,
                                    fold.session_id,
                                    fold.fold_id,
                                    &embedding,
                                )
                                .await
                        {
                            tracing::warn!(
                                fold_id = %fold.fold_id,
                                error = %e,
                                "Phase 0: fold embedding update failed"
                            );
                            p0_failed += 1;
                            continue;
                        }
                        p0_folds_embedded += 1;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(
                            fold_id = %fold.fold_id,
                            error = %e,
                            "Phase 0: fold embedding failed"
                        );
                        p0_failed += 1;
                    }
                }
            }
            if !folds.is_empty() && ((batch_index + 1) % 25 == 0 || batch_index + 1 == fold_batches)
            {
                tracing::info!(
                    phase = "0/folds",
                    processed = ((batch_index + 1) * concurrency).min(folds.len()),
                    total = folds.len(),
                    succeeded = p0_folds_embedded,
                    failed = p0_failed,
                    "phase progress"
                );
            }
        }

        let memo_batches = memos.len().max(1).div_ceil(concurrency);
        for (batch_index, chunk) in memos.chunks(concurrency).enumerate() {
            let results = join_all(chunk.iter().map(|memo| embed_client.embed(&memo.result))).await;
            for (memo, result) in chunk.iter().zip(results) {
                match result {
                    Ok(embedding) => {
                        if !dry_run
                            && let Err(e) = storage
                                .memo_update_embedding(
                                    &ctx,
                                    &memo.content_hash,
                                    &memo.model_version,
                                    &embedding,
                                )
                                .await
                        {
                            tracing::warn!(
                                content_hash = %memo.content_hash,
                                model_version = %memo.model_version,
                                error = %e,
                                "Phase 0: memo embedding update failed"
                            );
                            p0_failed += 1;
                            continue;
                        }
                        p0_memos_embedded += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            content_hash = %memo.content_hash,
                            model_version = %memo.model_version,
                            error = %e,
                            "Phase 0: memo embedding failed"
                        );
                        p0_failed += 1;
                    }
                }
            }
            if !memos.is_empty() && ((batch_index + 1) % 25 == 0 || batch_index + 1 == memo_batches)
            {
                tracing::info!(
                    phase = "0/memos",
                    processed = ((batch_index + 1) * concurrency).min(memos.len()),
                    total = memos.len(),
                    succeeded = p0_memos_embedded,
                    failed = p0_failed,
                    "phase progress"
                );
            }
        }

        // Phase 0 writes entity embeddings back through entity_put. Refresh the
        // in-memory snapshot before later phases so phase 1/2/4 do not
        // overwrite newly written embeddings with stale pre-phase-0 rows.
        if !dry_run && (phases.contains(&1) || phases.contains(&2) || phases.contains(&4)) {
            entities = storage.entity_list_all(&ctx).await?;
            tracing::info!(
                count = entities.len(),
                "reloaded entities after phase 0 to preserve freshly written embeddings"
            );
        }
    }

    for entity in &entities {
        let mut working = entity.clone();
        let mut changed = false;

        // Phase 1: split ENRICHED_PREFIX into description + clean context.
        if phases.contains(&1)
            && working.description.is_none()
            && let Some((desc, orig)) = split_enriched_context(&working.context_snippet)
        {
            working.description = Some(desc);
            working.context_snippet = orig;
            working.updated_at = Some(chrono::Utc::now());
            changed = true;
            p1_migrated += 1;
        }

        // Phase 2: generate description_embedding when missing (or --force).
        if phases.contains(&2)
            && let Some(ref desc) = working.description
            && (working.description_embedding.is_none() || force)
        {
            match embed_client.embed(desc).await {
                Ok(v) => {
                    working.description_embedding = Some(v);
                    working.updated_at = Some(chrono::Utc::now());
                    changed = true;
                    p2_embedded += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        entity = %working.entity_name,
                        error = %e,
                        "Phase 2: description embedding failed; skipping entity"
                    );
                    p2_failed += 1;
                }
            }
        }

        // Phase 4: content_hash backfill.
        if phases.contains(&4) && working.description.is_some() && working.content_hash.is_none() {
            let props_json = serde_json::to_string(&working.properties).unwrap_or_default();
            let hash = sha256_hex(&format!(
                "{}|{}|{}",
                working.entity_name,
                working.description.as_deref().unwrap_or(""),
                props_json
            ));
            working.content_hash = Some(format!("sha256:{hash}"));
            changed = true;
            p4_hashed += 1;
        }

        if changed
            && !dry_run
            && let Err(e) = storage.entity_put(&ctx, &working).await
        {
            tracing::warn!(
                entity = %working.entity_name,
                error = %e,
                "entity_put failed during backfill"
            );
        }
    }

    tracing::info!(
        p0_entities_embedded,
        p0_folds_embedded,
        p0_memos_embedded,
        p0_failed,
        p1_migrated,
        p2_embedded,
        p2_failed,
        p4_hashed,
        dry_run,
        "backfill-rich-entities complete"
    );

    if (p0_failed > 0 || p2_failed > 0) && !dry_run {
        anyhow::bail!(
            "{p0_failed} Phase 0 embeddings and {p2_failed} Phase 2 embeddings failed — check embedding provider and re-run"
        );
    }

    Ok(())
}

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    let out = h.finalize();
    out.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Increment a version string like "v1" -> "v2", "v42" -> "v43".
fn next_guideline_version(current: &str) -> String {
    if let Some(num_str) = current.strip_prefix('v')
        && let Ok(n) = num_str.parse::<u64>()
    {
        return format!("v{}", n + 1);
    }
    "v1".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_increment() {
        assert_eq!(next_guideline_version("v1"), "v2");
        assert_eq!(next_guideline_version("v42"), "v43");
        assert_eq!(next_guideline_version("v0"), "v1");
    }

    #[test]
    fn version_fallback() {
        assert_eq!(next_guideline_version(""), "v1");
        assert_eq!(next_guideline_version("latest"), "v1");
        assert_eq!(next_guideline_version("abc"), "v1");
    }

    #[test]
    fn split_enriched_parses_legacy_format() {
        let raw = "[enriched] Foo manages bar state.\n---\nstruct `Foo` @ src/lib.rs:42";
        let (desc, orig) = split_enriched_context(raw).unwrap();
        assert_eq!(desc, "Foo manages bar state.");
        assert_eq!(orig, "struct `Foo` @ src/lib.rs:42");
    }

    #[test]
    fn split_enriched_returns_none_on_plain_context() {
        assert!(split_enriched_context("struct `Foo` @ src/lib.rs:42").is_none());
        assert!(split_enriched_context("").is_none());
    }

    #[test]
    fn split_enriched_handles_prefix_without_separator() {
        // Edge case: prefix present but no separator — treat the rest as the
        // description and leave original context blank. Not ideal but the
        // migration is still safe to run.
        let (desc, orig) = split_enriched_context("[enriched] just a description").unwrap();
        assert_eq!(desc, "just a description");
        assert!(orig.is_empty());
    }

    #[test]
    fn sha256_hex_is_deterministic() {
        let a = sha256_hex("hello");
        let b = sha256_hex("hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sha256_hex_differs_for_different_inputs() {
        assert_ne!(sha256_hex("a"), sha256_hex("b"));
    }
}
