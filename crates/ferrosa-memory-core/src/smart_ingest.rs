//! Smart ingestion with prediction error gating.
//!
//! Inspired by vestige's prediction_error module and neuroscience research
//! (Sinclair & Bhavnani 2020, Lee et al. 2017). When new content arrives,
//! compare against existing memories to decide: CREATE new, UPDATE existing,
//! or SUPERSEDE outdated.
//!
//! The key insight: only store what's SURPRISING. If the new content is
//! similar to an existing memory, update it. If it contradicts, supersede.
//! If it's genuinely new, create.

use uuid::Uuid;

use crate::storage::Storage;
use crate::types::TenantContext;

/// Decision made by the prediction error gate.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "action")]
pub enum IngestDecision {
    /// Content is new — no similar memories found.
    Created { entity_id: Uuid },
    /// Content is similar to existing memory — updated in place.
    Updated { entity_id: Uuid, similarity: f64 },
    /// Content contradicts existing memory — old one superseded.
    Superseded {
        new_entity_id: Uuid,
        old_entity_id: Uuid,
        similarity: f64,
    },
    /// Content is too similar to existing — skipped (not novel enough).
    Skipped {
        existing_entity_id: Uuid,
        similarity: f64,
        reason: String,
    },
}

/// Thresholds for prediction error gating.
pub struct IngestConfig {
    /// Below this similarity, create new memory (content is novel).
    pub create_threshold: f64,
    /// Above this similarity, skip (content is redundant).
    pub skip_threshold: f64,
    /// Between create and skip: update if consistent, supersede if contradictory.
    pub update_threshold: f64,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            create_threshold: 0.3,
            skip_threshold: 0.9,
            update_threshold: 0.6,
        }
    }
}

/// NER configuration for LLM-based entity extraction.
pub struct NerConfig {
    pub http: reqwest::Client,
    pub ollama_base_url: String,
    pub model: String,
}

/// Three-tier entity name resolution.
/// Tier 1: explicit name -> Tier 2: LLM extraction -> Tier 3: heuristic
async fn resolve_entity_name(
    explicit_name: Option<&str>,
    content: &str,
    caller_type: &str,
    ner_config: Option<&NerConfig>,
) -> (String, String) {
    // Tier 1: explicit name provided
    if let Some(name) = explicit_name
        && !name.trim().is_empty()
    {
        return (name.trim().to_string(), caller_type.to_string());
    }

    // Tier 2+3: LLM extraction with heuristic fallback
    if let Some(ner) = ner_config {
        return crate::ner::extract_entity_from_content(
            &ner.http,
            &ner.ollama_base_url,
            &ner.model,
            content,
            caller_type,
        )
        .await;
    }

    // No NER config — heuristic only
    crate::ner::heuristic_extract_entity(content)
}

/// Smart ingest: decide whether to create, update, supersede, or skip.
///
/// Uses entity search to find similar existing memories, then applies
/// prediction error gating based on similarity thresholds.
#[allow(clippy::too_many_arguments)] // source_fold_id is per-call provenance, not config
pub async fn smart_ingest(
    storage: &(impl Storage + ?Sized),
    ctx: &TenantContext,
    session_id: Uuid,
    content: &str,
    entity_type: &str,
    embedding: Option<&[f32]>,
    source_fold_id: Option<Uuid>,
    config: &IngestConfig,
    entity_name: Option<&str>,
    ner_config: Option<&NerConfig>,
) -> anyhow::Result<IngestDecision> {
    // Resolve entity name once for all code paths
    let (resolved_name, resolved_type) =
        resolve_entity_name(entity_name, content, entity_type, ner_config).await;

    // Issue #148: derive the scope from the type and resolve the physical
    // storage partition ONCE. A global-scope entity must live under the tenant
    // global-sentinel partition (not the caller's session) so a later session
    // can read it — setting the `scope` label alone leaves the row session-keyed
    // and invisible cross-session. Every dedup lookup and write below uses
    // `storage_session`; for session-scoped types it resolves back to the
    // caller's own session, so session-scoped behavior is unchanged.
    let scope = crate::scope::default_scope_for(&resolved_type);
    let (storage_session, ingested_by) =
        crate::scope::resolve_storage_session(session_id, scope, ctx.tenant_id);

    // Always check for an exact-name match first. This path preserves updates for
    // repeated entity names, but it is still a dedup optimization: a read-side
    // timeout here must not prevent the write path from creating a new memory.
    let mut existing = if !resolved_name.is_empty() {
        match storage
            .entity_find_by_exact_name(ctx, storage_session, &resolved_name, &resolved_type)
            .await
        {
            Ok(Some(entry)) => vec![entry],
            Ok(None) => Vec::new(),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    entity_name = %resolved_name,
                    entity_type = %resolved_type,
                    "smart_ingest: exact dedup lookup failed; continuing with create path"
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // Suppress obvious cross-session duplicates. Keep this deliberately exact
    // for now: cross-session fuzzy/ANN matches can create surprising updates
    // when two sessions discuss similarly named but unrelated things.
    if existing.is_empty() && !resolved_name.is_empty() {
        match storage
            .entity_find_by_exact_name_any_session(ctx, &resolved_name, &resolved_type)
            .await
        {
            Ok(Some(entry)) => existing.push(entry),
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    entity_name = %resolved_name,
                    entity_type = %resolved_type,
                    "smart_ingest: cross-session exact dedup lookup failed; continuing with create path"
                );
            }
        }
    }

    // If no exact name match, fall back to semantic/phonetic search. These reads
    // are best-effort duplicate suppression only; availability of memory writes
    // is more important than fuzzy dedup when a quorum read times out.
    if existing.is_empty() {
        existing = if let Some(emb) = embedding {
            match storage
                .entity_search_ann(ctx, storage_session, emb, 3)
                .await
            {
                Ok(matches) => matches,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "smart_ingest: ANN dedup lookup failed; continuing with create path"
                    );
                    Vec::new()
                }
            }
        } else {
            let name_hint = if !resolved_name.is_empty() {
                resolved_name.clone()
            } else {
                content
                    .split_whitespace()
                    .take(5)
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            match storage
                .entity_find_phonetic(ctx, storage_session, &name_hint)
                .await
            {
                Ok(mut matches) => {
                    matches.truncate(3);
                    matches
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        name_hint = %name_hint,
                        "smart_ingest: fuzzy dedup lookup failed; continuing with create path"
                    );
                    Vec::new()
                }
            }
        };
    }

    if existing.is_empty() {
        // No similar memories — create new
        let entity_id = Uuid::new_v4();
        let entry = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id,
            session_id: storage_session,
            entity_name: resolved_name.clone(),
            entity_type: resolved_type.clone(),
            // Issue #148: store under the resolved partition (global sentinel for
            // global types, caller's session otherwise) and record the caller as
            // ingested_by for global entities so the audit/re-rank signal survives.
            scope,
            ingested_by_session: ingested_by,
            source_fold_id,
            context_snippet: content.to_string(),
            entity_embedding: embedding.map(|e| e.to_vec()),
            confidence: 1.0,
            state: crate::types::MemoryState::default(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        };
        storage.entity_put(ctx, &entry).await?;
        tracing::info!(
            %entity_id,
            "smart_ingest: CREATED (no similar memories)"
        );
        return Ok(IngestDecision::Created { entity_id });
    }

    // Compare with most similar existing memory.
    // Phonetic matches are lightweight (no context), so fetch full entity for comparison.
    let best_match = if existing[0].context_snippet.is_empty() {
        // Lightweight match — fetch full entity for similarity comparison
        storage
            .entity_get_by_id(ctx, existing[0].session_id, existing[0].entity_id)
            .await?
            .unwrap_or_else(|| existing[0].clone())
    } else {
        existing[0].clone()
    };

    // Exact name match → always update, regardless of content similarity.
    // This handles the case where the same entity is being updated with new info
    // (e.g., marking a bug as resolved).
    if best_match.entity_name == resolved_name {
        let updated = crate::types::EntityEntry {
            context_snippet: content.to_string(),
            entity_embedding: embedding.map(|e| e.to_vec()),
            created_at: chrono::Utc::now(),
            // Preserve the physical session partition, but retain the caller
            // that most recently refreshed this entity as provenance.
            ingested_by_session: Some(session_id),
            ..best_match.clone()
        };
        storage.entity_put(ctx, &updated).await?;
        tracing::info!(
            entity_id = %best_match.entity_id,
            "smart_ingest: UPDATED (exact name match)"
        );
        return Ok(IngestDecision::Updated {
            entity_id: best_match.entity_id,
            similarity: 1.0,
        });
    }

    let similarity = compute_text_similarity(content, &best_match.context_snippet);

    if similarity > config.skip_threshold {
        tracing::debug!(
            entity_id = %best_match.entity_id,
            similarity,
            "smart_ingest: SKIPPED (too similar)"
        );
        return Ok(IngestDecision::Skipped {
            existing_entity_id: best_match.entity_id,
            similarity,
            reason: "content too similar to existing memory".into(),
        });
    }

    if similarity > config.update_threshold {
        // Similar enough to be about the same topic — update
        tracing::info!(
            entity_id = %best_match.entity_id,
            similarity,
            "smart_ingest: UPDATED"
        );
        return Ok(IngestDecision::Updated {
            entity_id: best_match.entity_id,
            similarity,
        });
    }

    if similarity > config.create_threshold {
        // Moderately similar but different — supersede
        let new_id = Uuid::new_v4();
        let entry = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id: new_id,
            session_id: storage_session,
            entity_name: resolved_name.clone(),
            entity_type: resolved_type.clone(),
            // Issue #148: store under the resolved partition (global sentinel for
            // global types, caller's session otherwise) and record the caller as
            // ingested_by for global entities so the audit/re-rank signal survives.
            scope,
            ingested_by_session: ingested_by,
            source_fold_id,
            context_snippet: content.to_string(),
            entity_embedding: embedding.map(|e| e.to_vec()),
            confidence: 1.0,
            state: crate::types::MemoryState::default(),
            created_at: chrono::Utc::now(),
            ..Default::default()
        };
        storage.entity_put(ctx, &entry).await?;
        let removed_edges = delete_typed_edges_referencing_entity(
            storage,
            ctx,
            storage_session,
            best_match.entity_id,
        )
        .await?;
        // Create supersession edge
        let _ = crate::graph_write::create_supersedes_edge(
            storage,
            ctx,
            new_id,
            best_match.entity_id,
            new_id,
        )
        .await;
        tracing::info!(
            new_id = %new_id,
            old_id = %best_match.entity_id,
            similarity,
            removed_edges,
            "smart_ingest: SUPERSEDED"
        );
        return Ok(IngestDecision::Superseded {
            new_entity_id: new_id,
            old_entity_id: best_match.entity_id,
            similarity,
        });
    }

    // Very different — create new
    let entity_id = Uuid::new_v4();
    let entry = crate::types::EntityEntry {
        tenant_id: ctx.tenant_id,
        entity_id,
        session_id: storage_session,
        entity_name: resolved_name.clone(),
        entity_type: resolved_type.clone(),
        // Issue #148: store under the resolved partition (global sentinel for
        // global types, caller's session otherwise) and record the caller as
        // ingested_by for global entities so the audit/re-rank signal survives.
        scope,
        ingested_by_session: ingested_by,
        source_fold_id,
        context_snippet: content.to_string(),
        entity_embedding: embedding.map(|e| e.to_vec()),
        confidence: 1.0,
        state: crate::types::MemoryState::default(),
        created_at: chrono::Utc::now(),
        ..Default::default()
    };
    storage.entity_put(ctx, &entry).await?;
    tracing::info!(
        %entity_id,
        similarity,
        "smart_ingest: CREATED (novel content)"
    );
    Ok(IngestDecision::Created { entity_id })
}

async fn delete_typed_edges_referencing_entity<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    entity_id: Uuid,
) -> anyhow::Result<usize> {
    let edges = storage.typed_edge_list_session(ctx, session_id).await?;
    let mut deleted = 0;
    for edge in edges
        .into_iter()
        .filter(|edge| edge.src_id == entity_id || edge.dst_id == entity_id)
    {
        if storage
            .typed_edge_delete(ctx, session_id, edge.src_id, &edge.edge_type, edge.dst_id)
            .await?
        {
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Delete every typed edge that references `entity_id` (as src or dst) across
/// the WHOLE tenant, not just one session. Session-scoped cleanup misses edges
/// whose `session_id` drifted (see `fix_edge_sessions`), which is how deleted
/// entities leave dangling `CO_OCCURS_WITH` edges behind.
///
/// Call this BEFORE deleting the entity: the graph-backed `typed_edge_delete`
/// anchors its Cypher on the endpoint `:Entity` nodes, so it only works while
/// at least the surviving endpoint still exists. Returns edges removed.
pub(crate) async fn delete_typed_edges_referencing_entity_tenant_wide<S: Storage + ?Sized>(
    storage: &S,
    ctx: &TenantContext,
    entity_id: Uuid,
) -> anyhow::Result<usize> {
    let edges = storage.typed_edge_list_all(ctx).await?;
    let mut deleted = 0;
    for edge in edges
        .into_iter()
        .filter(|edge| edge.src_id == entity_id || edge.dst_id == entity_id)
    {
        if storage
            .typed_edge_delete(
                ctx,
                edge.session_id,
                edge.src_id,
                &edge.edge_type,
                edge.dst_id,
            )
            .await?
        {
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Extract candidate entities from text using simple heuristics.
/// Returns (name, entity_type) pairs.
///
/// Finds capitalized multi-word phrases (2+ capital-letter words) which are
/// likely named entities (people, places, orgs, technical concepts). Also
/// captures standalone capitalized words that are not common English words.
/// Uses [`infer_entity_type`] to classify each candidate.
pub fn extract_entity_candidates(text: &str) -> Vec<(String, String)> {
    let mut candidates = Vec::new();

    // Find capitalized multi-word phrases (words starting with uppercase)
    // These are likely named entities (people, places, orgs, concepts)
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        // Skip common sentence starters and short words
        if word.len() > 1
            && word.chars().next().is_some_and(|c| c.is_uppercase())
            && !is_common_word(word)
        {
            // Collect consecutive capitalized words
            let start = i;
            while i < words.len()
                && words[i].chars().next().is_some_and(|c| c.is_uppercase())
                && words[i].len() > 1
            {
                i += 1;
            }
            if i > start {
                let phrase: String = words[start..i].join(" ");
                // Clean trailing punctuation
                let clean = phrase.trim_end_matches(|c: char| c.is_ascii_punctuation());
                if clean.len() > 1 {
                    let entity_type = infer_entity_type(clean);
                    candidates.push((clean.to_string(), entity_type.to_string()));
                }
            }
        } else {
            i += 1;
        }
    }

    candidates.dedup_by(|a, b| a.0 == b.0);
    candidates
}

/// Heuristic entity type inference from a capitalized phrase.
///
/// Classification priority (first match wins):
/// 1. Organization suffixes (Inc, Corp, Ltd, Foundation, Labs, etc.)
/// 2. All-caps acronyms (2–6 chars) → organization
/// 3. Known tech/tool names → tool
/// 4. Two-word phrase where first word is a common given name → person
/// 5. Everything else → concept
pub fn infer_entity_type(name: &str) -> &'static str {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "concept";
    }

    // 1. Organization suffixes
    let lower = trimmed.to_lowercase();
    if has_org_suffix(&lower) {
        return "organization";
    }

    // 2. All-caps acronym (e.g. AWS, IBM, NIST, NASA)
    let alpha_only: String = trimmed.chars().filter(|c| c.is_alphabetic()).collect();
    if (2..=6).contains(&alpha_only.len()) && alpha_only.chars().all(|c| c.is_uppercase()) {
        return "organization";
    }

    // 3. Known tool / technology names
    if is_known_tool(trimmed) {
        return "tool";
    }

    // 4. Person heuristic: exactly 2 words, first is a common given name
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() == 2 && is_common_given_name(parts[0]) {
        return "person";
    }

    "concept"
}

fn has_org_suffix(lower: &str) -> bool {
    let last = lower.split_whitespace().next_back().unwrap_or("");
    matches!(
        last,
        "inc"
            | "inc."
            | "corp"
            | "corp."
            | "corporation"
            | "ltd"
            | "ltd."
            | "llc"
            | "llp"
            | "foundation"
            | "labs"
            | "laboratory"
            | "institute"
            | "university"
            | "group"
            | "systems"
            | "technologies"
            | "partners"
            | "association"
            | "consortium"
    )
}

/// Small curated list of well-known tech tools and platforms.
fn is_known_tool(name: &str) -> bool {
    matches!(
        name,
        "Docker"
            | "Kubernetes"
            | "Terraform"
            | "Ansible"
            | "Jenkins"
            | "Grafana"
            | "Prometheus"
            | "Kafka"
            | "Redis"
            | "PostgreSQL"
            | "Postgres"
            | "MongoDB"
            | "Cassandra"
            | "Elasticsearch"
            | "Nginx"
            | "Git"
            | "GitHub"
            | "GitLab"
            | "Cargo"
            | "Rustup"
            | "Clippy"
            | "Neo4j"
            | "ScyllaDB"
            | "Ferrosa"
            | "Linux"
            | "Vim"
            | "Neovim"
            | "Emacs"
            | "Webpack"
            | "Vite"
            | "Node"
            | "Deno"
            | "Bun"
            | "Ollama"
            | "LangChain"
            | "Tokio"
            | "Axum"
            | "Actix"
            | "Flask"
            | "FastAPI"
            | "Django"
            | "Phoenix"
            | "React"
            | "Vue"
            | "Svelte"
            | "Playwright"
    )
}

/// Common English given names (~120). A two-word phrase starting with one
/// of these is likely a person name.
fn is_common_given_name(word: &str) -> bool {
    matches!(
        word,
        "Aaron"
            | "Adam"
            | "Alex"
            | "Alice"
            | "Amanda"
            | "Amy"
            | "Andrea"
            | "Andrew"
            | "Angela"
            | "Anna"
            | "Anthony"
            | "Ashley"
            | "Barbara"
            | "Ben"
            | "Benjamin"
            | "Beth"
            | "Brandon"
            | "Brian"
            | "Carol"
            | "Charles"
            | "Charlotte"
            | "Chris"
            | "Christina"
            | "Christopher"
            | "Claire"
            | "Craig"
            | "Dan"
            | "Daniel"
            | "Dave"
            | "David"
            | "Deborah"
            | "Diana"
            | "Donald"
            | "Donna"
            | "Dorothy"
            | "Edward"
            | "Elizabeth"
            | "Emily"
            | "Eric"
            | "Frank"
            | "Gary"
            | "George"
            | "Grace"
            | "Hannah"
            | "Helen"
            | "Henry"
            | "Jack"
            | "Jacob"
            | "James"
            | "Jane"
            | "Jason"
            | "Jeff"
            | "Jennifer"
            | "Jessica"
            | "Jim"
            | "Joe"
            | "John"
            | "Jonathan"
            | "Joseph"
            | "Josh"
            | "Joshua"
            | "Julie"
            | "Justin"
            | "Karen"
            | "Kate"
            | "Katherine"
            | "Kelly"
            | "Ken"
            | "Kevin"
            | "Kim"
            | "Laura"
            | "Lauren"
            | "Linda"
            | "Lisa"
            | "Luke"
            | "Margaret"
            | "Maria"
            | "Mark"
            | "Martin"
            | "Mary"
            | "Matt"
            | "Matthew"
            | "Megan"
            | "Michael"
            | "Michelle"
            | "Mike"
            | "Nancy"
            | "Nathan"
            | "Nicholas"
            | "Nick"
            | "Nicole"
            | "Noah"
            | "Olivia"
            | "Patrick"
            | "Paul"
            | "Peter"
            | "Rachel"
            | "Rebecca"
            | "Richard"
            | "Robert"
            | "Ronald"
            | "Ryan"
            | "Sam"
            | "Samuel"
            | "Sandra"
            | "Sarah"
            | "Scott"
            | "Sean"
            | "Sharon"
            | "Sophia"
            | "Stephen"
            | "Steve"
            | "Steven"
            | "Susan"
            | "Thomas"
            | "Tim"
            | "Timothy"
            | "Tom"
            | "Tony"
            | "Tyler"
            | "Victoria"
            | "William"
    )
}

fn is_common_word(word: &str) -> bool {
    matches!(
        word,
        "The"
            | "This"
            | "That"
            | "These"
            | "Those"
            | "When"
            | "Where"
            | "Which"
            | "What"
            | "How"
            | "For"
            | "With"
            | "From"
            | "Into"
            | "After"
            | "Before"
            | "During"
            | "Between"
            | "Through"
            | "About"
            | "Each"
            | "Every"
            | "Also"
            | "Both"
            | "Either"
            | "Neither"
            | "Some"
            | "Any"
            | "All"
            | "Most"
            | "Other"
            | "Another"
            | "Such"
            | "Only"
            | "Very"
            | "Just"
            | "But"
            | "And"
            | "Not"
            | "Yet"
            | "Still"
            | "Already"
            | "However"
            | "Therefore"
            | "Thus"
            | "Hence"
            | "Since"
            | "Because"
            | "Although"
            | "While"
            | "Until"
            | "Unless"
            | "Once"
            | "Here"
            | "There"
            | "Then"
            | "Now"
            | "If"
            | "Or"
            | "So"
    )
}

/// Simple text similarity using word overlap (Jaccard coefficient).
/// For production, this should use embedding cosine similarity.
pub fn compute_text_similarity(a: &str, b: &str) -> f64 {
    let words_a: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let words_b: std::collections::HashSet<&str> = b.split_whitespace().collect();
    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tenant_wide_edge_cleanup_removes_cross_session_edges() {
        use crate::storage::Storage;
        use crate::storage::mock::MockStorage;
        use crate::types::TypedEdge;

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let target = Uuid::new_v4();
        let other = Uuid::new_v4();
        let drift_session = Uuid::new_v4(); // edges stored under a mismatched session

        let mk = |src, etype: &str, dst| TypedEdge {
            tenant_id: ctx.tenant_id,
            session_id: drift_session,
            src_id: src,
            edge_type: etype.to_string(),
            dst_id: dst,
            weight: 1.0,
            metadata: None,
            created_at: chrono::Utc::now(),
        };
        // Two edges touch `target` (as src, then as dst), under a drifted session.
        store
            .typed_edge_put(&ctx, &mk(target, "CO_OCCURS_WITH", other))
            .await
            .unwrap();
        store
            .typed_edge_put(&ctx, &mk(other, "references", target))
            .await
            .unwrap();
        // One unrelated edge must survive.
        let keep_a = Uuid::new_v4();
        let keep_b = Uuid::new_v4();
        store
            .typed_edge_put(&ctx, &mk(keep_a, "references", keep_b))
            .await
            .unwrap();

        let deleted = delete_typed_edges_referencing_entity_tenant_wide(&store, &ctx, target)
            .await
            .unwrap();
        assert_eq!(deleted, 2, "both edges touching the target must be removed");

        let remaining = store.typed_edge_list_all(&ctx).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].src_id, keep_a);
    }

    #[test]
    fn text_similarity_identical() {
        assert!((compute_text_similarity("hello world", "hello world") - 1.0).abs() < 0.01);
    }

    #[test]
    fn text_similarity_different() {
        assert!(compute_text_similarity("hello world", "foo bar baz") < 0.1);
    }

    #[test]
    fn text_similarity_partial() {
        let sim = compute_text_similarity("the quick brown fox", "the quick red fox jumps");
        assert!(sim > 0.3 && sim < 0.8);
    }

    #[tokio::test]
    async fn smart_ingest_creates_on_empty_store() {
        use crate::storage::mock::MockStorage;

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };

        let result = smart_ingest(
            &store,
            &ctx,
            Uuid::new_v4(),
            "Ferrosa is a Rust-native Cassandra-compatible database",
            "concept",
            None,
            None,
            &IngestConfig::default(),
            None,
            None,
        )
        .await
        .unwrap();

        assert!(matches!(result, IngestDecision::Created { .. }));
    }

    #[tokio::test]
    async fn smart_ingest_new_memory_is_not_blocked_by_fuzzy_lookup_failure() {
        use crate::storage::mock::MockStorage;

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        *store.force_phonetic_error.lock().await =
            Some("simulated LOCAL_QUORUM fuzzy lookup timeout".into());

        let result = smart_ingest(
            &store,
            &ctx,
            Uuid::new_v4(),
            "Memory writes must remain available when best-effort dedup lookup times out",
            "pattern",
            None,
            None,
            &IngestConfig::default(),
            Some("fmem save availability"),
            None,
        )
        .await
        .expect("a fuzzy dedup read timeout must not block creating a new memory");

        assert!(matches!(result, IngestDecision::Created { .. }));
    }

    #[tokio::test]
    async fn smart_ingest_scopes_entities_by_type_not_session() {
        // Issue 13 regression: smart_ingest must derive scope from the entity
        // type via default_scope_for, not leave every write at the Session
        // default. A global type (skill/concept/...) lands Global so the memory
        // is durable across sessions; a session type (bug/...) stays Session.
        // Passing a name keeps resolve_entity_name from re-classifying the type.
        use crate::storage::mock::MockStorage;
        use crate::types::EntityScope;

        for (entity_type, expected) in [
            ("skill", EntityScope::Global),
            ("bug", EntityScope::Session),
        ] {
            let store = MockStorage::new();
            let ctx = TenantContext {
                tenant_id: Uuid::new_v4(),
                session_origin: "test".into(),
            };
            let result = smart_ingest(
                &store,
                &ctx,
                Uuid::new_v4(),
                "novel content exercising the scope-wiring regression",
                entity_type,
                None,
                None,
                &IngestConfig::default(),
                Some("scope wiring regression entity"),
                None,
            )
            .await
            .expect("ingest should create a new entity");
            assert!(matches!(result, IngestDecision::Created { .. }));

            let entities = store.entities.lock().await;
            assert_eq!(entities.len(), 1);
            assert_eq!(
                entities[0].scope, expected,
                "{entity_type} must ingest as {expected:?} via default_scope_for, not the Session default"
            );
        }
    }

    #[tokio::test]
    async fn smart_ingest_global_entity_is_stored_in_the_global_partition() {
        // Issue #148: setting scope=Global on the record is not enough. The CQL
        // write must be rerouted to the tenant global-sentinel partition, or the
        // entity lives in a session-keyed row no other session can read. Before
        // the fix, `session_id` on the stored row is the caller's session.
        use crate::scope::tenant_global_session_uuid;
        use crate::storage::mock::MockStorage;
        use crate::types::EntityScope;

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let caller = Uuid::new_v4();
        let sentinel = tenant_global_session_uuid(ctx.tenant_id);

        // "decision" is a global type per default_scope_for.
        smart_ingest(
            &store,
            &ctx,
            caller,
            "route global entities to the sentinel partition",
            "decision",
            None,
            None,
            &IngestConfig::default(),
            Some("global-partition routing decision"),
            None,
        )
        .await
        .expect("ingest should create a new entity");

        let entities = store.entities.lock().await;
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].scope, EntityScope::Global);
        assert_eq!(
            entities[0].session_id, sentinel,
            "a global entity must be stored under the tenant global-sentinel partition, not the caller's session (Issue #148)"
        );
        assert_ne!(
            entities[0].session_id, caller,
            "global entity must NOT live in the caller's session partition"
        );
        assert_eq!(
            entities[0].ingested_by_session,
            Some(caller),
            "global entity must record the originating session for audit"
        );
    }

    #[tokio::test]
    async fn global_entity_ingested_in_one_session_is_visible_from_another() {
        // Issue #148 headline ("context that survives sessions"): a global entity
        // ingested in session A must be readable from the global partition by a
        // brand-new session B. Asserted via a direct partition read (Storage),
        // NOT hybrid_search — ANN scope leakage (#147) would give a false positive.
        use crate::scope::tenant_global_session_uuid;
        use crate::storage::Storage;
        use crate::storage::mock::MockStorage;

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let session_a = Uuid::new_v4();
        let session_b = Uuid::new_v4();
        let canary = "xyzzy-canary-8842 global routing works";

        smart_ingest(
            &store,
            &ctx,
            session_a,
            canary,
            "decision",
            None,
            None,
            &IngestConfig::default(),
            Some("cross session canary"),
            None,
        )
        .await
        .expect("ingest should create a new entity");

        // Session B reads the GLOBAL partition directly.
        let global = store
            .entity_list_session(&ctx, tenant_global_session_uuid(ctx.tenant_id))
            .await
            .unwrap();
        assert!(
            global.iter().any(|e| e.context_snippet.contains(canary)),
            "a global entity ingested in session A must be readable from the global partition in session B (Issue #148)"
        );

        // ...and must not be stranded in the caller's session partition.
        let in_a = store.entity_list_session(&ctx, session_a).await.unwrap();
        assert!(
            !in_a.iter().any(|e| e.context_snippet.contains(canary)),
            "a global entity must not be stranded in the caller's session partition"
        );
        let _ = session_b;
    }

    #[tokio::test]
    async fn smart_ingest_updates_exact_name_match_from_another_session() {
        use crate::storage::Storage;
        use crate::storage::mock::MockStorage;
        use crate::types::EntityEntry;

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let original_session_id = Uuid::new_v4();
        let ingest_session_id = Uuid::new_v4();
        let entity_id = Uuid::new_v4();

        store
            .entity_put(
                &ctx,
                &EntityEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id,
                    session_id: original_session_id,
                    entity_name: "BRIGHT-Pro".into(),
                    entity_type: "concept".into(),
                    context_snippet: "BRIGHT-Pro evaluates reasoning-intensive retrieval".into(),
                    created_at: chrono::Utc::now(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let result = smart_ingest(
            &store,
            &ctx,
            ingest_session_id,
            "BRIGHT-Pro adds aspect-aware retrieval metrics for agentic search",
            "concept",
            None,
            None,
            &IngestConfig::default(),
            Some("BRIGHT-Pro"),
            None,
        )
        .await
        .unwrap();

        match result {
            IngestDecision::Updated {
                entity_id: updated_id,
                similarity,
            } => {
                assert_eq!(updated_id, entity_id);
                assert_eq!(similarity, 1.0);
            }
            other => panic!("expected cross-session exact duplicate to update, got {other:?}"),
        }

        let entities = store.entities.lock().await;
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].entity_id, entity_id);
        assert_eq!(entities[0].session_id, original_session_id);
        assert_eq!(entities[0].ingested_by_session, Some(ingest_session_id));
        assert_eq!(
            entities[0].context_snippet,
            "BRIGHT-Pro adds aspect-aware retrieval metrics for agentic search"
        );
    }

    #[tokio::test]
    async fn smart_ingest_supersede_removes_typed_edges_referencing_old_entity() {
        use crate::storage::Storage;
        use crate::storage::mock::MockStorage;
        use crate::types::{EntityEntry, TypedEdge};

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };
        let session_id = Uuid::new_v4();
        // "concept" is a global type: global entities and their typed edges live
        // in the tenant global-sentinel partition (Issue #148). Set the fixture up
        // there and assert edge cleanup there, while the ingest is still driven
        // from the caller's `session_id`.
        let storage_session = crate::scope::tenant_global_session_uuid(ctx.tenant_id);
        let old_entity_id = Uuid::new_v4();
        let other_entity_id = Uuid::new_v4();
        let unrelated_src_id = Uuid::new_v4();
        let unrelated_dst_id = Uuid::new_v4();

        store
            .entity_put(
                &ctx,
                &EntityEntry {
                    tenant_id: ctx.tenant_id,
                    entity_id: old_entity_id,
                    session_id: storage_session,
                    entity_name: "Old Topic".into(),
                    entity_type: "concept".into(),
                    context_snippet: "alpha beta gamma".into(),
                    created_at: chrono::Utc::now(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        for (src_id, edge_type, dst_id) in [
            (old_entity_id, "related_to", other_entity_id),
            (other_entity_id, "references", old_entity_id),
            (unrelated_src_id, "keeps", unrelated_dst_id),
        ] {
            store
                .typed_edge_put(
                    &ctx,
                    &TypedEdge {
                        tenant_id: ctx.tenant_id,
                        session_id: storage_session,
                        src_id,
                        edge_type: edge_type.into(),
                        dst_id,
                        weight: 1.0,
                        metadata: None,
                        created_at: chrono::Utc::now(),
                    },
                )
                .await
                .unwrap();
        }

        let result = smart_ingest(
            &store,
            &ctx,
            session_id,
            "alpha beta delta",
            "concept",
            Some(&[0.1, 0.2]),
            None,
            &IngestConfig::default(),
            Some("New Topic"),
            None,
        )
        .await
        .unwrap();

        assert!(matches!(result, IngestDecision::Superseded { .. }));
        let typed_edges = store
            .typed_edge_list_session(&ctx, storage_session)
            .await
            .unwrap();
        assert!(
            typed_edges
                .iter()
                .all(|edge| edge.src_id != old_entity_id && edge.dst_id != old_entity_id),
            "supersede must not leave typed_edges pointing at the old entity: {typed_edges:?}"
        );
        assert!(
            typed_edges
                .iter()
                .any(|edge| edge.src_id == unrelated_src_id && edge.dst_id == unrelated_dst_id),
            "supersede cleanup should preserve unrelated typed_edges"
        );
    }

    #[test]
    fn extract_entities_from_technical_text() {
        let text = "uses Ferrosa with LSM-tree storage and S3 tiering";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        assert!(names.contains(&"Ferrosa"), "should extract Ferrosa");
        assert!(names.contains(&"LSM-tree"), "should extract LSM-tree");
        assert!(names.contains(&"S3"), "should extract S3");
    }

    #[test]
    fn extract_entities_filters_common_words() {
        let text = "However the system Also provides Some features";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        assert!(!names.contains(&"However"), "should filter However");
        assert!(!names.contains(&"Also"), "should filter Also");
        assert!(!names.contains(&"Some"), "should filter Some");
    }

    #[test]
    fn extract_entities_at_sentence_start() {
        let text = "Cassandra is great. Redis is fast.";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        assert!(
            names.contains(&"Cassandra"),
            "should extract entity at sentence start, got: {names:?}"
        );
        assert!(
            names.contains(&"Redis"),
            "should extract mid-sentence entity, got: {names:?}"
        );
    }

    #[test]
    fn extract_entities_at_position_zero() {
        let text = "Ben Kearns built Ferrosa from scratch";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        assert!(
            names.contains(&"Ben Kearns"),
            "should extract entity at position 0, got: {names:?}"
        );
    }

    #[test]
    fn extract_entities_multi_word_phrase() {
        let text = "uses Apache Kafka for streaming";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        assert!(
            names.contains(&"Apache Kafka"),
            "should extract multi-word phrase, got: {names:?}"
        );
    }

    #[test]
    fn extract_entities_empty_text() {
        let candidates = extract_entity_candidates("");
        assert!(candidates.is_empty());
    }

    #[test]
    fn extract_entities_all_lowercase() {
        let candidates = extract_entity_candidates("everything is lowercase here");
        assert!(candidates.is_empty());
    }

    // --- infer_entity_type tests ---

    #[test]
    fn infer_type_person() {
        assert_eq!(infer_entity_type("Ben Kearns"), "person");
        assert_eq!(infer_entity_type("Alice Smith"), "person");
        assert_eq!(infer_entity_type("David Chen"), "person");
    }

    #[test]
    fn infer_type_organization_suffix() {
        assert_eq!(infer_entity_type("Acme Corp"), "organization");
        assert_eq!(infer_entity_type("Mozilla Foundation"), "organization");
        assert_eq!(infer_entity_type("HashiCorp Labs"), "organization");
    }

    #[test]
    fn infer_type_organization_acronym() {
        assert_eq!(infer_entity_type("AWS"), "organization");
        assert_eq!(infer_entity_type("IBM"), "organization");
        assert_eq!(infer_entity_type("NASA"), "organization");
        assert_eq!(infer_entity_type("NIST"), "organization");
    }

    #[test]
    fn infer_type_tool() {
        assert_eq!(infer_entity_type("Docker"), "tool");
        assert_eq!(infer_entity_type("Ferrosa"), "tool");
        assert_eq!(infer_entity_type("Neo4j"), "tool");
        assert_eq!(infer_entity_type("Tokio"), "tool");
    }

    #[test]
    fn infer_type_concept_fallback() {
        assert_eq!(infer_entity_type("Memory Lifecycle"), "concept");
        assert_eq!(infer_entity_type("Dream Consolidation"), "concept");
    }

    #[test]
    fn infer_type_empty() {
        assert_eq!(infer_entity_type(""), "concept");
        assert_eq!(infer_entity_type("  "), "concept");
    }

    #[test]
    fn extract_entities_uses_ner() {
        let text = "uses Docker and Ben Kearns built Ferrosa at Acme Labs";
        let candidates = extract_entity_candidates(text);
        let by_name: std::collections::HashMap<&str, &str> = candidates
            .iter()
            .map(|(n, t)| (n.as_str(), t.as_str()))
            .collect();
        assert_eq!(by_name.get("Docker"), Some(&"tool"));
        assert_eq!(by_name.get("Ben Kearns"), Some(&"person"));
        assert_eq!(by_name.get("Ferrosa"), Some(&"tool"));
        assert_eq!(by_name.get("Acme Labs"), Some(&"organization"));
    }

    // --- has_org_suffix tests ---

    #[test]
    fn has_org_suffix_inc() {
        assert!(has_org_suffix("acme inc"));
        assert!(has_org_suffix("acme inc."));
    }

    #[test]
    fn has_org_suffix_corp() {
        assert!(has_org_suffix("acme corp"));
        assert!(has_org_suffix("acme corp."));
        assert!(has_org_suffix("acme corporation"));
    }

    #[test]
    fn has_org_suffix_ltd() {
        assert!(has_org_suffix("acme ltd"));
        assert!(has_org_suffix("acme ltd."));
    }

    #[test]
    fn has_org_suffix_llc_llp() {
        assert!(has_org_suffix("acme llc"));
        assert!(has_org_suffix("acme llp"));
    }

    #[test]
    fn has_org_suffix_foundation_labs() {
        assert!(has_org_suffix("mozilla foundation"));
        assert!(has_org_suffix("hashicorp labs"));
    }

    #[test]
    fn has_org_suffix_institute_university() {
        assert!(has_org_suffix("mit institute"));
        assert!(has_org_suffix("stanford university"));
    }

    #[test]
    fn has_org_suffix_group_systems() {
        assert!(has_org_suffix("carlyle group"));
        assert!(has_org_suffix("bae systems"));
    }

    #[test]
    fn has_org_suffix_technologies_partners() {
        assert!(has_org_suffix("palantir technologies"));
        assert!(has_org_suffix("andreessen partners"));
    }

    #[test]
    fn has_org_suffix_association_consortium() {
        assert!(has_org_suffix("national association"));
        assert!(has_org_suffix("w3c consortium"));
    }

    #[test]
    fn has_org_suffix_laboratory() {
        assert!(has_org_suffix("bell laboratory"));
    }

    #[test]
    fn has_org_suffix_false_for_non_org() {
        assert!(!has_org_suffix("hello world"));
        assert!(!has_org_suffix("docker"));
        assert!(!has_org_suffix("rust programming"));
        assert!(!has_org_suffix(""));
    }

    #[test]
    fn has_org_suffix_single_word_suffix() {
        // A single word that is itself a suffix
        assert!(has_org_suffix("foundation"));
        assert!(has_org_suffix("university"));
    }

    // --- is_known_tool tests ---

    #[test]
    fn is_known_tool_matches_various_tools() {
        assert!(is_known_tool("Docker"));
        assert!(is_known_tool("Kubernetes"));
        assert!(is_known_tool("Terraform"));
        assert!(is_known_tool("Redis"));
        assert!(is_known_tool("PostgreSQL"));
        assert!(is_known_tool("Git"));
        assert!(is_known_tool("GitHub"));
        assert!(is_known_tool("Cargo"));
        assert!(is_known_tool("Neo4j"));
        assert!(is_known_tool("Tokio"));
        assert!(is_known_tool("Ollama"));
        assert!(is_known_tool("React"));
        assert!(is_known_tool("Svelte"));
        assert!(is_known_tool("Playwright"));
    }

    #[test]
    fn is_known_tool_case_sensitive() {
        // is_known_tool is case-sensitive
        assert!(!is_known_tool("docker"));
        assert!(!is_known_tool("DOCKER"));
        assert!(!is_known_tool("kubernetes"));
    }

    #[test]
    fn is_known_tool_false_for_non_tools() {
        assert!(!is_known_tool("FooBar"));
        assert!(!is_known_tool("Hello"));
        assert!(!is_known_tool(""));
        assert!(!is_known_tool("Memory"));
        assert!(!is_known_tool("Consolidation"));
    }

    #[test]
    fn is_known_tool_rust_ecosystem() {
        assert!(is_known_tool("Rustup"));
        assert!(is_known_tool("Clippy"));
        assert!(is_known_tool("Axum"));
        assert!(is_known_tool("Actix"));
    }

    #[test]
    fn is_known_tool_js_ecosystem() {
        assert!(is_known_tool("Webpack"));
        assert!(is_known_tool("Vite"));
        assert!(is_known_tool("Node"));
        assert!(is_known_tool("Deno"));
        assert!(is_known_tool("Bun"));
        assert!(is_known_tool("Vue"));
    }

    #[test]
    fn is_known_tool_python_ecosystem() {
        assert!(is_known_tool("Flask"));
        assert!(is_known_tool("FastAPI"));
        assert!(is_known_tool("Django"));
    }

    #[test]
    fn is_known_tool_databases() {
        assert!(is_known_tool("MongoDB"));
        assert!(is_known_tool("Cassandra"));
        assert!(is_known_tool("Elasticsearch"));
        assert!(is_known_tool("ScyllaDB"));
        assert!(is_known_tool("Postgres"));
    }

    // --- is_common_given_name tests ---

    #[test]
    fn is_common_given_name_matches() {
        assert!(is_common_given_name("Alice"));
        assert!(is_common_given_name("Ben"));
        assert!(is_common_given_name("David"));
        assert!(is_common_given_name("Emily"));
        assert!(is_common_given_name("John"));
        assert!(is_common_given_name("Sarah"));
        assert!(is_common_given_name("William"));
    }

    #[test]
    fn is_common_given_name_case_sensitive() {
        assert!(!is_common_given_name("alice"));
        assert!(!is_common_given_name("ALICE"));
        assert!(!is_common_given_name("ben"));
    }

    #[test]
    fn is_common_given_name_non_matches() {
        assert!(!is_common_given_name("Docker"));
        assert!(!is_common_given_name("Zzyzx"));
        assert!(!is_common_given_name(""));
        assert!(!is_common_given_name("Gandalf"));
        assert!(!is_common_given_name("Xander"));
    }

    #[test]
    fn is_common_given_name_edge_names() {
        // Short names
        assert!(is_common_given_name("Kim"));
        assert!(is_common_given_name("Sam"));
        assert!(is_common_given_name("Tom"));
        assert!(is_common_given_name("Joe"));
        assert!(is_common_given_name("Dan"));
        assert!(is_common_given_name("Jim"));
        assert!(is_common_given_name("Ken"));
        assert!(is_common_given_name("Ben"));
        assert!(is_common_given_name("Amy"));
    }

    // --- infer_entity_type edge cases ---

    #[test]
    fn infer_type_mixed_case_acronym() {
        // Not all-caps, so not treated as org acronym
        assert_ne!(infer_entity_type("AwS"), "organization");
    }

    #[test]
    fn infer_type_single_word_not_tool() {
        // Single word, not a known tool, not an acronym
        assert_eq!(infer_entity_type("Consolidation"), "concept");
    }

    #[test]
    fn infer_type_three_word_phrase() {
        // Three words: not exactly 2 words, so person heuristic doesn't apply
        assert_eq!(infer_entity_type("Alice Bob Charlie"), "concept");
    }

    #[test]
    fn infer_type_trailing_whitespace() {
        assert_eq!(infer_entity_type("  Docker  "), "tool");
        assert_eq!(infer_entity_type("  AWS  "), "organization");
    }

    #[test]
    fn infer_type_org_suffix_takes_priority() {
        // "Jenkins Labs" — "Jenkins" is a known tool, but org suffix takes priority
        assert_eq!(infer_entity_type("Jenkins Labs"), "organization");
    }

    #[test]
    fn infer_type_two_char_acronym() {
        // Two-char all-caps is still treated as org
        assert_eq!(infer_entity_type("AI"), "organization");
    }

    #[test]
    fn infer_type_six_char_acronym() {
        // Six-char all-caps is the max for acronym
        assert_eq!(infer_entity_type("ABCDEF"), "organization");
    }

    #[test]
    fn infer_type_seven_char_acronym_not_org() {
        // Seven-char all-caps exceeds the acronym range
        assert_ne!(infer_entity_type("ABCDEFG"), "organization");
    }

    #[test]
    fn infer_type_person_with_known_first_name() {
        assert_eq!(infer_entity_type("Chris Evans"), "person");
        assert_eq!(infer_entity_type("Grace Hopper"), "person");
        assert_eq!(infer_entity_type("Noah Smith"), "person");
    }

    #[test]
    fn infer_type_single_known_name_not_person() {
        // Single word that is a common name but not 2 words — not "person"
        // "Alice" alone — not a known tool, not an org suffix, not an acronym
        assert_eq!(infer_entity_type("Alice"), "concept");
    }

    // --- text similarity edge cases ---

    #[test]
    fn text_similarity_empty_strings() {
        assert_eq!(compute_text_similarity("", ""), 0.0);
    }

    #[test]
    fn text_similarity_one_empty() {
        assert_eq!(compute_text_similarity("hello world", ""), 0.0);
        assert_eq!(compute_text_similarity("", "hello world"), 0.0);
    }

    #[test]
    fn text_similarity_single_word_match() {
        assert!((compute_text_similarity("hello", "hello") - 1.0).abs() < 0.01);
    }

    #[test]
    fn text_similarity_subset() {
        let sim = compute_text_similarity("a b", "a b c d");
        // intersection = 2, union = 4, expected = 0.5
        assert!((sim - 0.5).abs() < 0.01);
    }

    // --- is_common_word tests ---

    #[test]
    fn is_common_word_filters_expected() {
        assert!(is_common_word("The"));
        assert!(is_common_word("This"));
        assert!(is_common_word("However"));
        assert!(is_common_word("Because"));
        assert!(is_common_word("If"));
        assert!(is_common_word("Or"));
    }

    #[test]
    fn is_common_word_does_not_filter_entities() {
        assert!(!is_common_word("Docker"));
        assert!(!is_common_word("Alice"));
        assert!(!is_common_word("NASA"));
        assert!(!is_common_word("Ferrosa"));
    }

    // --- IngestConfig default tests ---

    #[test]
    fn ingest_config_default_thresholds() {
        let cfg = IngestConfig::default();
        assert!((cfg.create_threshold - 0.3).abs() < f64::EPSILON);
        assert!((cfg.skip_threshold - 0.9).abs() < f64::EPSILON);
        assert!((cfg.update_threshold - 0.6).abs() < f64::EPSILON);
    }

    // --- extract_entity_candidates edge cases ---

    #[test]
    fn extract_entities_deduplicates() {
        let text = "uses Docker and Docker again";
        let candidates = extract_entity_candidates(text);
        let docker_count = candidates.iter().filter(|(n, _)| n == "Docker").count();
        // dedup_by removes consecutive duplicates
        assert!(docker_count <= 1);
    }

    #[test]
    fn extract_entities_strips_trailing_punctuation() {
        let text = "uses Docker, Redis. PostgreSQL!";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        // Names should not have trailing punctuation
        for name in &names {
            assert!(
                !name.ends_with(','),
                "name should not end with comma: {name}"
            );
            assert!(
                !name.ends_with('.'),
                "name should not end with period: {name}"
            );
            assert!(
                !name.ends_with('!'),
                "name should not end with bang: {name}"
            );
        }
    }

    #[test]
    fn extract_entities_single_word_entity() {
        // Single capitalized word that is not common should be extracted
        let text = "uses Ferrosa for storage";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        assert!(names.contains(&"Ferrosa"));
    }

    #[tokio::test]
    async fn smart_ingest_uses_explicit_entity_name() {
        use crate::storage::mock::MockStorage;

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };

        let result = smart_ingest(
            &store,
            &ctx,
            Uuid::new_v4(),
            "Ben Kearns is the developer of ferrosa-memory-mcp and has ops background",
            "person",
            None,
            None,
            &IngestConfig::default(),
            Some("Ben Kearns"),
            None,
        )
        .await
        .unwrap();

        assert!(matches!(result, IngestDecision::Created { .. }));

        let entities = store.entities.lock().await;
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].entity_name, "Ben Kearns");
    }

    #[tokio::test]
    async fn smart_ingest_without_name_falls_back_to_heuristic() {
        use crate::storage::mock::MockStorage;

        let store = MockStorage::new();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".into(),
        };

        let result = smart_ingest(
            &store,
            &ctx,
            Uuid::new_v4(),
            "The project called Docker is widely used in production",
            "concept",
            None,
            None,
            &IngestConfig::default(),
            None,
            None,
        )
        .await
        .unwrap();

        assert!(matches!(result, IngestDecision::Created { .. }));

        let entities = store.entities.lock().await;
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].entity_name, "Docker");
        assert_eq!(entities[0].entity_type, "tool");
    }
}
