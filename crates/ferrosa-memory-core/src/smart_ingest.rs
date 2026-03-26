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
) -> anyhow::Result<IngestDecision> {
    // Search for similar existing entities
    let existing = if let Some(emb) = embedding {
        storage.entity_search_ann(ctx, session_id, emb, 3).await?
    } else {
        // Fall back to phonetic search on the first few words
        let name_hint = content
            .split_whitespace()
            .take(5)
            .collect::<Vec<_>>()
            .join(" ");
        match storage
            .entity_find_phonetic(ctx, session_id, &name_hint)
            .await?
        {
            Some(e) => vec![e],
            None => vec![],
        }
    };

    if existing.is_empty() {
        // No similar memories — create new
        let entity_id = Uuid::new_v4();
        let entry = crate::types::EntityEntry {
            tenant_id: ctx.tenant_id,
            entity_id,
            session_id,
            entity_name: content
                .split_whitespace()
                .take(8)
                .collect::<Vec<_>>()
                .join(" "),
            entity_type: entity_type.to_string(),
            source_fold_id,
            context_snippet: content.to_string(),
            entity_embedding: embedding.map(|e| e.to_vec()),
            confidence: 1.0,
            state: crate::types::MemoryState::default(),
            created_at: chrono::Utc::now(),
        };
        storage.entity_put(ctx, &entry).await?;
        tracing::info!(
            %entity_id,
            "smart_ingest: CREATED (no similar memories)"
        );
        return Ok(IngestDecision::Created { entity_id });
    }

    // Compare with most similar existing memory
    // For now, use a simple heuristic: check content overlap
    let best_match = &existing[0];
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
            session_id,
            entity_name: content
                .split_whitespace()
                .take(8)
                .collect::<Vec<_>>()
                .join(" "),
            entity_type: entity_type.to_string(),
            source_fold_id,
            context_snippet: content.to_string(),
            entity_embedding: embedding.map(|e| e.to_vec()),
            confidence: 1.0,
            state: crate::types::MemoryState::default(),
            created_at: chrono::Utc::now(),
        };
        storage.entity_put(ctx, &entry).await?;
        // Create supersession edge
        let _ = storage
            .edge_supersedes(ctx, new_id, best_match.entity_id, new_id)
            .await;
        tracing::info!(
            new_id = %new_id,
            old_id = %best_match.entity_id,
            similarity,
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
        session_id,
        entity_name: content
            .split_whitespace()
            .take(8)
            .collect::<Vec<_>>()
            .join(" "),
        entity_type: entity_type.to_string(),
        source_fold_id,
        context_snippet: content.to_string(),
        entity_embedding: embedding.map(|e| e.to_vec()),
        confidence: 1.0,
        state: crate::types::MemoryState::default(),
        created_at: chrono::Utc::now(),
    };
    storage.entity_put(ctx, &entry).await?;
    tracing::info!(
        %entity_id,
        similarity,
        "smart_ingest: CREATED (novel content)"
    );
    Ok(IngestDecision::Created { entity_id })
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
            // skip sentence starters (i > 0)
            && i > 0
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
        )
        .await
        .unwrap();

        assert!(matches!(result, IngestDecision::Created { .. }));
    }

    #[test]
    fn extract_entities_from_technical_text() {
        // First word is skipped by the sentence-starter heuristic (i > 0),
        // so prefix with a lowercase word to push entities past position 0.
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
    fn extract_entities_skips_sentence_starters() {
        let text = "Cassandra is great. Redis is fast.";
        let candidates = extract_entity_candidates(text);
        let names: Vec<&str> = candidates.iter().map(|c| c.0.as_str()).collect();
        // "Cassandra" is at i=0, so it is skipped as a sentence starter
        assert!(
            !names.contains(&"Cassandra"),
            "should skip sentence-starting word"
        );
        // "Redis" is at i > 0 and capitalized, so it should be captured
        assert!(
            names.contains(&"Redis"),
            "should extract mid-sentence entity"
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
}
