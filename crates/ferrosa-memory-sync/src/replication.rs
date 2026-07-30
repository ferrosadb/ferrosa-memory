//! MAAS-T-26 — teacher-side selective replication: pack **build + emit**.
//!
//! This is the teacher counterpart to [`crate::learner_ingest`] (T-27). It turns
//! a teacher-chosen memory subset into a versioned [`KnowledgePack`] (T-24) and,
//! optionally, seals it into a host-side [`PackRef`] via [`crate::pack_crypto`]
//! (T-33) — the "emit" step.
//!
//! # Requirements implemented
//!
//! - **MR-P2P-07** (MT-P2P-07) — *selective copy of the teacher-selected subset,
//!   no neighbor bleed.* The builder copies **exactly** the entities the teacher
//!   put in the [`TeacherSelection`] and performs **no** graph expansion. Edges
//!   and temporal events whose endpoints fall outside the selected entity set are
//!   **dropped** (never pull the missing neighbour in), and the drop is counted
//!   and logged — it is never a silent truncation.
//! - **MR-P2P-08** (MT-P2P-08) — *summary-first discloses summaries only.* When
//!   [`PackBuildParams::summary_first`] is set, every "full body" field is
//!   redacted before hashing: [`TeachingItem::body`] → `None` and
//!   [`FoldEntry::raw_trajectory`] → empty. Summary-sized fields
//!   (`TeachingItem::summary`, `FoldEntry::fold_summary`, entity context/desc)
//!   are preserved. A summary-first pack therefore carries zero full bodies.
//! - **MR-P2P-09** (MT-P2P-09) — *provenance recorded on build.* The
//!   [`PackProvenanceEnvelope`] (teacher + learner fingerprints, instance,
//!   namespace, request id) is stamped into the manifest, and any per-item
//!   [`MemoryProvenance`] rows the teacher supplies are carried into the payload
//!   for the learner to commit atomically (T-27 / MR-P2P-13).
//!
//! # Postcondition
//!
//! [`build_pack`] never returns a pack that fails [`KnowledgePack::validate`].
//! Closure filtering guarantees no dangling edges, counts/hashes are computed
//! from the *final* (filtered + redacted) payload, and the result is validated
//! before return. A validation failure surfaces as [`BuildError::Invalid`]
//! (fail-loud) rather than emitting a malformed pack.

// Fail-loud on the build path: no panics, no unwrap/expect, no indexing on
// caller-supplied data. Mirrors the denies in `pack.rs` / `pack_crypto.rs`.
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::panic)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use ferrosa_memory_core::remote_identity::{ContentHash, PublicKeyFingerprint};
use ferrosa_memory_core::remotes::types::{MemoryProvenance, TeachingItem};
use ferrosa_memory_core::types::{EntityEntry, FoldEntry, TemporalEvent, TypedEdge};
use uuid::Uuid;

use crate::pack::PackRef;
use crate::pack::{
    CipherSuite, KnowledgePack, PACK_SCHEMA_VERSION, PackError, PackManifest, PackPayload,
    PackProvenanceEnvelope,
};
use crate::pack_crypto::{CipherFloor, PackCryptoError, Secret, seal_pack};

/// The exact memory subset a teacher chose to share.
///
/// The builder copies these and **only** these — it never expands the graph to
/// pull in neighbours (MR-P2P-07). Edges and temporal events that reference an
/// entity outside [`TeacherSelection::entities`] are dropped during the build.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TeacherSelection {
    /// Exactly the entities the teacher selected. Defines the closure set.
    pub entities: Vec<EntityEntry>,
    /// Folds to include. In summary-first mode their raw trajectories are stripped.
    pub folds: Vec<FoldEntry>,
    /// Temporal events; only those whose `entity_id` is in the selection survive.
    pub temporal: Vec<TemporalEvent>,
    /// Typed edges; only those with **both** endpoints in the selection survive.
    pub edges: Vec<TypedEdge>,
    /// Teaching items. In summary-first mode their bodies are stripped.
    pub items: Vec<TeachingItem>,
    /// Per-item provenance rows the teacher already tracks. Carried into the
    /// payload verbatim; the learner stamps the channel-attested `remote_id`.
    pub provenance_rows: Vec<MemoryProvenance>,
}

/// Crypto-independent parameters for building a pack.
#[derive(Debug, Clone)]
pub struct PackBuildParams {
    /// Stable id for the produced pack.
    pub pack_id: Uuid,
    /// Monotonic per-pack version (producer-chosen; replay defense on the learner).
    pub pack_version: u64,
    /// AEAD suite the pack will be sealed with (recorded in the manifest).
    pub cipher_suite: CipherSuite,
    /// Engine (ferrosa) version the pack was produced against.
    pub engine_version: String,
    /// Embedding model identifier; every embedding must match `embedding_dim`.
    pub embedding_model: String,
    /// Embedding dimension. `0` is rejected by [`KnowledgePack::validate`].
    pub embedding_dim: u32,
    /// When set, redact every full body (summaries only) — MR-P2P-08.
    pub summary_first: bool,
    /// Pack creation timestamp (UTC).
    pub created_at: DateTime<Utc>,
    /// Optional best-effort TTL; must be after `created_at` if present.
    pub ttl_expires_at: Option<DateTime<Utc>>,
    /// Provenance envelope (teacher + learner identity) — MR-P2P-09.
    pub provenance: PackProvenanceEnvelope,
}

/// What a build dropped to maintain referential closure, surfaced so the caller
/// can observe selection trimming rather than discovering a silent truncation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BuildReport {
    /// Edges dropped because an endpoint was outside the selected entity set.
    pub dropped_edges: usize,
    /// Temporal events dropped because their entity was outside the selection.
    pub dropped_temporal: usize,
    /// Teaching-item bodies redacted by summary-first mode.
    pub redacted_item_bodies: usize,
    /// Fold raw trajectories redacted by summary-first mode.
    pub redacted_fold_bodies: usize,
}

/// A built, validated pack together with its [`BuildReport`].
#[derive(Debug, Clone)]
pub struct BuiltPack {
    /// The validated, ready-to-seal pack.
    pub pack: KnowledgePack,
    /// What the build dropped/redacted.
    pub report: BuildReport,
}

/// Errors raised while building a pack from a teacher selection.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// A payload section could not be canonicalized for hashing.
    #[error("failed to hash pack section {section}: {source}")]
    Hash {
        section: &'static str,
        #[source]
        source: ferrosa_memory_core::remote_identity::RemoteIdentityError,
    },
    /// The payload could not be serialized to measure its declared size.
    #[error("failed to serialize payload to measure size: {0}")]
    Serialize(String),
    /// The assembled pack failed structural validation. This is a builder bug or
    /// bad parameters (e.g. wrong embedding dim) — never emit such a pack.
    #[error("built pack failed validation (this is a bug): {0}")]
    Invalid(#[from] PackError),
}

/// Errors raised while building **and sealing** (emitting) a pack.
#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    /// The build step failed.
    #[error(transparent)]
    Build(#[from] BuildError),
    /// The seal (AEAD) step failed — fail-closed, no ciphertext produced.
    #[error(transparent)]
    Seal(#[from] PackCryptoError),
}

/// Build a versioned [`KnowledgePack`] from a teacher's selection.
///
/// See the module docs for the MR-P2P-07/08/09 contract. The returned pack is
/// guaranteed to pass [`KnowledgePack::validate`].
pub fn build_pack(
    selection: &TeacherSelection,
    params: &PackBuildParams,
) -> Result<BuiltPack, BuildError> {
    // Closure set: exactly the selected entity ids. No expansion (MR-P2P-07).
    let selected: HashSet<Uuid> = selection.entities.iter().map(|e| e.entity_id).collect();

    // Keep only edges fully inside the selection; dropping the rest avoids
    // pulling unselected neighbours in (no bleed) and guarantees edge closure.
    let mut dropped_edges = 0usize;
    let edges: Vec<TypedEdge> = selection
        .edges
        .iter()
        .filter(|e| {
            let inside = selected.contains(&e.src_id) && selected.contains(&e.dst_id);
            if !inside {
                dropped_edges += 1;
            }
            inside
        })
        .cloned()
        .collect();

    // Keep only temporal events for selected entities (no bleed).
    let mut dropped_temporal = 0usize;
    let temporal: Vec<TemporalEvent> = selection
        .temporal
        .iter()
        .filter(|t| {
            let inside = selected.contains(&t.entity_id);
            if !inside {
                dropped_temporal += 1;
            }
            inside
        })
        .cloned()
        .collect();

    // Summary-first redaction of full bodies (MR-P2P-08).
    let mut redacted_item_bodies = 0usize;
    let items: Vec<TeachingItem> = selection
        .items
        .iter()
        .map(|item| {
            let mut item = item.clone();
            if params.summary_first && item.body.is_some() {
                item.body = None;
                redacted_item_bodies += 1;
            }
            item
        })
        .collect();

    let mut redacted_fold_bodies = 0usize;
    let folds: Vec<FoldEntry> = selection
        .folds
        .iter()
        .map(|fold| {
            let mut fold = fold.clone();
            if params.summary_first && !fold.raw_trajectory.is_empty() {
                fold.raw_trajectory = String::new();
                redacted_fold_bodies += 1;
            }
            fold
        })
        .collect();

    if dropped_edges > 0 || dropped_temporal > 0 {
        tracing::warn!(
            dropped_edges,
            dropped_temporal,
            pack_id = %params.pack_id,
            "selective replication dropped out-of-selection references to maintain closure"
        );
    }

    let payload = PackPayload {
        entities: selection.entities.clone(),
        folds,
        temporal,
        edges,
        items,
        provenance_rows: selection.provenance_rows.clone(),
    };

    let manifest = build_manifest(&payload, params)?;
    let pack = KnowledgePack { manifest, payload };

    // Postcondition: never emit a pack that would be rejected on ingest.
    pack.validate()?;

    Ok(BuiltPack {
        pack,
        report: BuildReport {
            dropped_edges,
            dropped_temporal,
            redacted_item_bodies,
            redacted_fold_bodies,
        },
    })
}

/// Build a pack and seal it into a host-side [`PackRef`] (the "emit" step).
///
/// The teacher/learner fingerprints the key is bound to come from the build
/// provenance envelope, so the crypto identity and the recorded provenance can
/// never disagree. Fail-closed: any crypto failure yields no `PackRef`.
pub fn build_and_seal(
    selection: &TeacherSelection,
    params: &PackBuildParams,
    ikm: &Secret,
    chunk_size: usize,
    floor: CipherFloor,
) -> Result<(PackRef, BuildReport), EmitError> {
    let built = build_pack(selection, params)?;
    let teacher = params.provenance.teacher_fingerprint.clone();
    let learner = params.provenance.learner_fingerprint.clone();
    let pack_ref = seal_pack(&built.pack, ikm, &teacher, &learner, chunk_size, floor)?;
    Ok((pack_ref, built.report))
}

/// Assemble a [`PackManifest`] whose counts and hashes describe `payload`
/// exactly. The MAC/tag slots are left empty for [`seal_pack`] to fill.
fn build_manifest(
    payload: &PackPayload,
    params: &PackBuildParams,
) -> Result<PackManifest, BuildError> {
    let entities_hash = hash_section("entities", &payload.entities)?;
    let folds_hash = hash_section("folds", &payload.folds)?;
    let temporal_hash = hash_section("temporal", &payload.temporal)?;
    let edges_hash = hash_section("edges", &payload.edges)?;
    let items_hash = hash_section("items", &payload.items)?;

    let declared_size_bytes = serde_json::to_vec(payload)
        .map_err(|e| BuildError::Serialize(e.to_string()))?
        .len() as u64;

    Ok(PackManifest {
        pack_id: params.pack_id,
        schema_version: PACK_SCHEMA_VERSION,
        pack_version: params.pack_version,
        declared_size_bytes,
        entity_count: payload.entities.len() as u64,
        fold_count: payload.folds.len() as u64,
        temporal_count: payload.temporal.len() as u64,
        edge_count: payload.edges.len() as u64,
        item_count: payload.items.len() as u64,
        entities_hash,
        folds_hash,
        temporal_hash,
        edges_hash,
        items_hash,
        chunk_macs: vec![],
        pack_mac: None,
        cipher_suite: params.cipher_suite,
        engine_version: params.engine_version.clone(),
        embedding_model: params.embedding_model.clone(),
        embedding_dim: params.embedding_dim,
        provenance: params.provenance.clone(),
        created_at: params.created_at,
        ttl_expires_at: params.ttl_expires_at,
        summary_first: params.summary_first,
    })
}

fn hash_section<T: serde::Serialize>(
    section: &'static str,
    value: &T,
) -> Result<ContentHash, BuildError> {
    ContentHash::sha256_json(value).map_err(|source| BuildError::Hash { section, source })
}

/// Convenience: are the two fingerprints the pack binds to consistent? Used by
/// callers wiring T-29; kept here so the seam has a single reference point.
pub fn binds_fingerprints(
    params: &PackBuildParams,
    teacher: &PublicKeyFingerprint,
    learner: &PublicKeyFingerprint,
) -> bool {
    &params.provenance.teacher_fingerprint == teacher
        && &params.provenance.learner_fingerprint == learner
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrosa_memory_core::remote_identity::{InstanceId, InstanceSigningIdentity};
    use ferrosa_memory_core::remotes::types::{
        ApplicabilityFrame, SafetyClassification, SafetyRisk, TeachingKind,
    };
    use ferrosa_memory_core::types::{FoldStatus, MemoryState};

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn fingerprint(seed: u128) -> PublicKeyFingerprint {
        InstanceSigningIdentity::generate(InstanceId(id(seed)))
            .public_identity()
            .public_key_fingerprint
    }

    fn entity(entity_id: Uuid, embedding: Option<Vec<f32>>) -> EntityEntry {
        EntityEntry {
            tenant_id: id(99),
            entity_id,
            session_id: id(1),
            entity_name: format!("e{entity_id}"),
            entity_type: "concept".into(),
            context_snippet: "ctx".into(),
            entity_embedding: embedding,
            confidence: 0.9,
            state: MemoryState::Active,
            created_at: Utc::now(),
            ..Default::default()
        }
    }

    fn edge(src: Uuid, dst: Uuid) -> TypedEdge {
        TypedEdge {
            tenant_id: id(99),
            session_id: id(1),
            src_id: src,
            edge_type: "related_to".into(),
            dst_id: dst,
            weight: 1.0,
            metadata: None,
            created_at: Utc::now(),
        }
    }

    fn temporal(entity_id: Uuid) -> TemporalEvent {
        TemporalEvent {
            tenant_id: id(99),
            entity_id,
            event_time: Utc::now(),
            event_id: id(entity_id.as_u128() + 7000),
            fact_text: "fact".into(),
            supersedes_id: None,
            valid_until: None,
            source_session: id(1),
            confidence: 0.8,
        }
    }

    fn fold(fold_id: Uuid, raw: &str) -> FoldEntry {
        FoldEntry {
            session_id: id(1),
            fold_id,
            tenant_id: id(99),
            depth: 0,
            parent_fold_id: None,
            raw_trajectory: raw.into(),
            fold_summary: Some("summary".into()),
            fold_embedding: None,
            token_count: 10,
            compression_ratio: None,
            status: FoldStatus::Active,
            created_at: Utc::now(),
            folded_at: None,
        }
    }

    fn teaching_item(item_id: Uuid, body: Option<&str>) -> TeachingItem {
        TeachingItem {
            item_id,
            packet_id: id(500),
            kind: TeachingKind::Fact,
            title: "t".into(),
            summary: "the summary".into(),
            body: body.map(|b| b.to_string()),
            content_hash: ContentHash("h".into()),
            applicability: ApplicabilityFrame {
                namespaces: vec!["ns".into()],
                host_os: None,
                container_runtime: None,
                hardware: vec![],
                required_tags: vec![],
                excluded_tags: vec![],
                confidence: 0.9,
            },
            safety: SafetyClassification {
                risk: SafetyRisk::Low,
                reasons: vec![],
                redacted: false,
                requires_human: false,
            },
            detail_ref: None,
            metadata: serde_json::Value::Null,
            created_at: Utc::now(),
        }
    }

    fn provenance_row(item_id: Uuid, local_entity_id: Uuid) -> MemoryProvenance {
        MemoryProvenance {
            provenance_id: id(item_id.as_u128() + 9000),
            local_entity_id,
            remote_id: id(0),
            packet_id: id(500),
            item_id,
            content_hash: ContentHash("ch".into()),
            signature_hash: ContentHash("sh".into()),
            imported_at: Utc::now(),
        }
    }

    fn envelope() -> PackProvenanceEnvelope {
        PackProvenanceEnvelope {
            teacher_instance_id: InstanceId(id(2)),
            teacher_fingerprint: fingerprint(2),
            learner_fingerprint: fingerprint(3),
            request_id: Some(id(42)),
            source_namespace: "teacher-ns".into(),
        }
    }

    fn params(summary_first: bool, embedding_dim: u32) -> PackBuildParams {
        let created = Utc::now();
        PackBuildParams {
            pack_id: id(1000),
            pack_version: 7,
            cipher_suite: CipherSuite::Aes256Gcm,
            engine_version: "ferrosa-test".into(),
            embedding_model: "test-embed".into(),
            embedding_dim,
            summary_first,
            created_at: created,
            ttl_expires_at: Some(created + chrono::Duration::hours(1)),
            provenance: envelope(),
        }
    }

    // ── MT-P2P-07 — exactly the selected set, no neighbour bleed ──────────────

    #[test]
    fn mt_p2p_07_pack_contains_exactly_selected_entities() {
        let a = id(1);
        let b = id(2);
        // Entity c is NOT selected; an edge a->c and a temporal event for c must
        // be dropped without dragging c into the pack.
        let c = id(3);
        let selection = TeacherSelection {
            entities: vec![entity(a, None), entity(b, None)],
            edges: vec![edge(a, b), edge(a, c)],
            temporal: vec![temporal(a), temporal(c)],
            ..Default::default()
        };

        let built = build_pack(&selection, &params(false, 4)).expect("build");

        // Exactly the two selected entities, no third.
        let ids: HashSet<Uuid> = built
            .pack
            .payload
            .entities
            .iter()
            .map(|e| e.entity_id)
            .collect();
        assert_eq!(ids, HashSet::from([a, b]));

        // The a->c edge and c's temporal event were dropped (no bleed).
        assert_eq!(built.pack.payload.edges.len(), 1);
        assert_eq!(built.report.dropped_edges, 1);
        assert_eq!(built.pack.payload.temporal.len(), 1);
        assert_eq!(built.report.dropped_temporal, 1);

        // No edge or temporal references the unselected entity.
        for e in &built.pack.payload.edges {
            assert!(ids.contains(&e.src_id) && ids.contains(&e.dst_id));
        }
        for t in &built.pack.payload.temporal {
            assert!(ids.contains(&t.entity_id));
        }
    }

    #[test]
    fn mt_p2p_07_built_pack_always_validates() {
        let a = id(1);
        let b = id(2);
        let selection = TeacherSelection {
            entities: vec![entity(a, None), entity(b, None)],
            edges: vec![edge(a, b), edge(a, id(999))],
            ..Default::default()
        };
        let built = build_pack(&selection, &params(false, 4)).expect("build");
        // Closure filtering means validate() (which rejects dangling edges) passes.
        built.pack.validate().expect("built pack must validate");
    }

    // ── MT-P2P-08 — summary-first discloses summaries only, zero full bodies ──

    #[test]
    fn mt_p2p_08_summary_first_strips_all_full_bodies() {
        let a = id(1);
        let selection = TeacherSelection {
            entities: vec![entity(a, None)],
            folds: vec![fold(id(10), "RAW TRAJECTORY SECRET")],
            items: vec![
                teaching_item(id(20), Some("FULL BODY SECRET")),
                teaching_item(id(21), Some("ANOTHER BODY")),
            ],
            ..Default::default()
        };

        let built = build_pack(&selection, &params(true, 4)).expect("build");

        // Zero full item bodies.
        for item in &built.pack.payload.items {
            assert!(item.body.is_none(), "summary-first item must have no body");
            assert_eq!(item.summary, "the summary", "summary preserved");
        }
        // Zero fold raw trajectories.
        for fold in &built.pack.payload.folds {
            assert!(
                fold.raw_trajectory.is_empty(),
                "summary-first fold must have empty raw_trajectory"
            );
            assert_eq!(fold.fold_summary.as_deref(), Some("summary"));
        }
        assert_eq!(built.report.redacted_item_bodies, 2);
        assert_eq!(built.report.redacted_fold_bodies, 1);

        // Belt-and-suspenders: the serialized pack contains none of the secrets.
        let json = serde_json::to_string(&built.pack).expect("serialize");
        assert!(!json.contains("FULL BODY SECRET"));
        assert!(!json.contains("ANOTHER BODY"));
        assert!(!json.contains("RAW TRAJECTORY SECRET"));
        assert!(built.pack.manifest.summary_first);
    }

    #[test]
    fn mt_p2p_08_non_summary_first_keeps_bodies() {
        let a = id(1);
        let selection = TeacherSelection {
            entities: vec![entity(a, None)],
            folds: vec![fold(id(10), "RAW")],
            items: vec![teaching_item(id(20), Some("BODY"))],
            ..Default::default()
        };
        let built = build_pack(&selection, &params(false, 4)).expect("build");
        assert_eq!(built.pack.payload.items[0].body.as_deref(), Some("BODY"));
        assert_eq!(built.pack.payload.folds[0].raw_trajectory, "RAW");
        assert_eq!(built.report.redacted_item_bodies, 0);
        assert_eq!(built.report.redacted_fold_bodies, 0);
        assert!(!built.pack.manifest.summary_first);
    }

    // ── MT-P2P-09 — provenance recorded on build ──────────────────────────────

    #[test]
    fn mt_p2p_09_provenance_envelope_and_rows_recorded() {
        let a = id(1);
        let selection = TeacherSelection {
            entities: vec![entity(a, None)],
            items: vec![teaching_item(id(20), None)],
            provenance_rows: vec![provenance_row(id(20), a)],
            ..Default::default()
        };
        let p = params(false, 4);
        let built = build_pack(&selection, &p).expect("build");

        // Envelope stamped into the manifest verbatim.
        let env = &built.pack.manifest.provenance;
        assert_eq!(env.teacher_fingerprint, p.provenance.teacher_fingerprint);
        assert_eq!(env.learner_fingerprint, p.provenance.learner_fingerprint);
        assert_eq!(env.source_namespace, "teacher-ns");
        assert_eq!(env.request_id, Some(id(42)));

        // Per-item provenance rows carried into the payload for the learner.
        assert_eq!(built.pack.payload.provenance_rows.len(), 1);
        assert_eq!(built.pack.payload.provenance_rows[0].item_id, id(20));

        // The build binds the crypto identity the same provenance declares.
        assert!(binds_fingerprints(
            &p,
            &p.provenance.teacher_fingerprint,
            &p.provenance.learner_fingerprint
        ));
    }

    // ── Manifest fidelity + emit (build → seal → open round-trip) ─────────────

    #[test]
    fn manifest_counts_and_hashes_describe_final_payload() {
        let a = id(1);
        let b = id(2);
        let selection = TeacherSelection {
            entities: vec![entity(a, None), entity(b, None)],
            edges: vec![edge(a, b)],
            temporal: vec![temporal(a)],
            items: vec![teaching_item(id(20), Some("body"))],
            ..Default::default()
        };
        let built = build_pack(&selection, &params(true, 4)).expect("build");
        let m = &built.pack.manifest;
        assert_eq!(m.entity_count, 2);
        assert_eq!(m.edge_count, 1);
        assert_eq!(m.temporal_count, 1);
        assert_eq!(m.item_count, 1);
        assert_eq!(m.pack_version, 7);
        assert!(m.declared_size_bytes > 0);
        // Hashes match a fresh recompute of the redacted payload.
        assert_eq!(
            m.items_hash,
            ContentHash::sha256_json(&built.pack.payload.items).unwrap()
        );
    }

    #[test]
    fn mt_p2p_07_build_and_seal_round_trips_through_open() {
        let a = id(1);
        let b = id(2);
        let selection = TeacherSelection {
            entities: vec![entity(a, None), entity(b, None)],
            edges: vec![edge(a, b), edge(a, id(404))], // dangling edge dropped
            items: vec![teaching_item(id(20), Some("body"))],
            ..Default::default()
        };
        let p = params(false, 4);
        let ikm = Secret::random(32);

        let (pack_ref, report) =
            build_and_seal(&selection, &p, &ikm, 64, CipherFloor::default()).expect("emit");
        assert_eq!(report.dropped_edges, 1);
        assert_eq!(pack_ref.manifest.entity_count, 2);
        assert!(pack_ref.ciphertext_len() > 0);

        // The learner side (T-33 open) recovers exactly what we sealed.
        let opened = crate::pack_crypto::open_pack(
            &pack_ref,
            &ikm,
            &p.provenance.teacher_fingerprint,
            &p.provenance.learner_fingerprint,
            CipherFloor::default(),
        )
        .expect("open");
        opened.validate().expect("opened pack valid");
        assert_eq!(opened.payload.entities.len(), 2);
        assert_eq!(opened.payload.edges.len(), 1);
        assert_eq!(opened.payload.items[0].body.as_deref(), Some("body"));
    }

    #[test]
    fn wrong_embedding_dim_fails_loud_not_silent() {
        let a = id(1);
        // Entity carries a 3-dim embedding but params declare 4.
        let selection = TeacherSelection {
            entities: vec![entity(a, Some(vec![0.1, 0.2, 0.3]))],
            ..Default::default()
        };
        let err = build_pack(&selection, &params(false, 4)).expect_err("must reject");
        assert!(matches!(
            err,
            BuildError::Invalid(PackError::EmbeddingDimMismatch { found: 3, .. })
        ));
    }
}
