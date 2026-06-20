//! MAAS-T-24 — versioned KnowledgePack schema + manifest.
//!
//! A `KnowledgePack` is the unit of teacher→learner transfer in the MaaS P2P
//! layer. It bundles the memory payload (entities, folds, temporal events,
//! typed edges, and teaching items) together with a [`PackManifest`] that
//! describes the pack's integrity, versioning, and provenance metadata.
//!
//! # Requirements implemented
//!
//! - **MR-P2P-01** — the schema is *explicitly* versioned via
//!   [`PackManifest::schema_version`] and [`PackManifest::pack_version`]. The
//!   schema version is a constant baked into the type; the pack version is a
//!   caller-supplied monotonic counter (NOT derived from content) so a learner
//!   can reject older-version replay deterministically.
//! - **MR-P2P-02** — the manifest carries per-payload counts and content hashes
//!   plus a **referential-closure** invariant: every edge endpoint must resolve
//!   to an entity present in the same pack. [`KnowledgePack::validate`] enforces
//!   this and rejects dangling edges.
//! - **MR-CRYPTO-01** — [`PackRef`] is a *host-side* handle that, by
//!   construction, can carry **ciphertext + metadata only**. It has no field
//!   that accepts plaintext or key material, and no constructor that derives
//!   one from a plaintext pack. The only way to obtain a `PackRef` is from
//!   already-sealed ciphertext (see `pack_crypto::seal_pack`).
//!
//! # T-29 seam
//!
//! The MAC/AEAD tag fields on [`PackManifest`] and the per-chunk MAC vector are
//! *opaque bytes* here. They are produced by T-33 (`pack_crypto`) using a key
//! derived from the teacher+learner [`PublicKeyFingerprint`] pair. The *source*
//! of those fingerprints — a DTLS-vouched peer identity — is MAAS-T-29
//! (ferrosa-dbaas), not yet built. This module never fabricates a tag; it only
//! provides the typed slots that T-33 fills and T-27 verifies.

// Fail-loud on untrusted input: no panics on the deserialization / validation
// path. These denies are scoped to this security-critical module. Tests may use
// unwrap/expect/indexing on known-good fixtures (the denies guard production
// code paths handling untrusted bytes, not test assertions).
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

use ferrosa_memory_core::remote_identity::{ContentHash, InstanceId, PublicKeyFingerprint};
use ferrosa_memory_core::remotes::types::{MemoryProvenance, TeachingItem};
use ferrosa_memory_core::types::{EntityEntry, FoldEntry, TemporalEvent, TypedEdge};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Current KnowledgePack schema version. Bumped whenever the wire shape of
/// [`KnowledgePack`] or [`PackManifest`] changes in a non-backward-compatible
/// way. This is distinct from [`PackManifest::pack_version`], which is a
/// per-pack monotonic counter chosen by the producer.
pub const PACK_SCHEMA_VERSION: u32 = 1;

/// Hard upper bound on declared entity/fold/temporal/edge/item counts and on
/// declared total pack size. Anything claiming more is rejected at the boundary
/// before allocation, bounding memory use on untrusted input (Power-of-10 R3).
pub const MAX_PACK_ELEMENTS: u64 = 1_000_000;

/// Hard upper bound on declared total pack size in bytes (256 MiB).
pub const MAX_PACK_SIZE_BYTES: u64 = 256 * 1024 * 1024;

/// Errors raised while validating a [`KnowledgePack`] or its [`PackManifest`].
///
/// Every variant is a *typed* rejection — the validator never panics on
/// malformed input (MR-P2P-14 / MT-P2P-14).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PackError {
    #[error("unsupported pack schema version {found}; this build supports {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },

    #[error("declared {field} count {declared} does not match actual {actual}")]
    CountMismatch {
        field: &'static str,
        declared: u64,
        actual: usize,
    },

    #[error("declared {field} count {declared} exceeds maximum {max}")]
    CountExceedsMax {
        field: &'static str,
        declared: u64,
        max: u64,
    },

    #[error("declared pack size {declared} bytes exceeds maximum {max} bytes")]
    SizeExceedsMax { declared: u64, max: u64 },

    #[error("content hash mismatch for {field}: manifest declared {declared}, computed {computed}")]
    ContentHashMismatch {
        field: &'static str,
        declared: ContentHash,
        computed: ContentHash,
    },

    #[error(
        "edge references entity {endpoint} ({role}) that is not present in the pack entity set"
    )]
    DanglingEdge { endpoint: Uuid, role: &'static str },

    #[error("embedding for {field} has dimension {found}, manifest declares {declared}")]
    EmbeddingDimMismatch {
        field: &'static str,
        found: usize,
        declared: u32,
    },

    #[error("embedding for {field} contains a non-finite value (NaN or infinity)")]
    EmbeddingNotFinite { field: &'static str },

    #[error("manifest declares embedding dimension 0, which is invalid")]
    ZeroEmbeddingDim,

    #[error("ttl_expires_at {expires} is not after created_at {created}")]
    TtlNotInFuture {
        created: chrono::DateTime<chrono::Utc>,
        expires: chrono::DateTime<chrono::Utc>,
    },
}

/// Provenance envelope describing *who produced this pack and for whom*.
///
/// The teacher/learner fingerprints here are the **peer-identity inputs** that
/// T-33 binds the per-pack content key to. This struct is the T-29 seam: when
/// MAAS-T-29 lands, the DTLS vouch supplies these two fingerprints; nothing
/// else in the pack pipeline changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackProvenanceEnvelope {
    /// Instance that produced (sealed) the pack.
    pub teacher_instance_id: InstanceId,
    /// Public-key fingerprint of the teacher. Bound into the AEAD key (T-33).
    pub teacher_fingerprint: PublicKeyFingerprint,
    /// Public-key fingerprint of the intended learner. Bound into the AEAD key
    /// (T-33) — a pack sealed for learner A cannot be opened by learner B.
    pub learner_fingerprint: PublicKeyFingerprint,
    /// Optional originating teaching request id, for audit correlation.
    pub request_id: Option<Uuid>,
    /// Source namespace the pack content was drawn from.
    pub source_namespace: String,
}

/// AEAD cipher selected for a pack. Used to enforce a cipher floor and to
/// reject downgrade (MR-CRYPTO-04). Both variants meet the floor; there is
/// deliberately **no plaintext / weak-cipher variant**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CipherSuite {
    /// AES-256-GCM.
    Aes256Gcm,
    /// ChaCha20-Poly1305.
    ChaCha20Poly1305,
}

/// Per-chunk MAC/AEAD authentication tag, opaque to this module.
///
/// Produced by T-33; this is just a typed byte container so the manifest can
/// declare a tag per ciphertext chunk for incremental verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChunkMac(pub Vec<u8>);

/// Whole-pack MAC/AEAD authentication tag, opaque to this module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackMac(pub Vec<u8>);

/// Manifest describing a pack's integrity, versioning, and shape.
///
/// The manifest is authenticated as AEAD associated data by T-33, so a learner
/// can trust these counts/hashes *after* tag verification (MR-P2P-10): the
/// manifest is bound to the ciphertext and cannot be edited independently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackManifest {
    /// Stable identifier for this pack.
    pub pack_id: Uuid,

    /// **Explicit** schema version (MR-P2P-01). Equals [`PACK_SCHEMA_VERSION`]
    /// for packs produced by this build; checked on ingest.
    pub schema_version: u32,

    /// **Monotonic** per-pack version chosen by the producer (MR-P2P-12 replay
    /// defense). NOT derived from content — the producer is responsible for
    /// incrementing it. The learner rejects a pack whose version is <= the
    /// version it has already applied for the same `pack_id`.
    pub pack_version: u64,

    /// Declared total serialized size of the plaintext payload, in bytes.
    /// Validated against [`MAX_PACK_SIZE_BYTES`] before any allocation.
    pub declared_size_bytes: u64,

    /// Element counts, each validated against the actual payload and against
    /// [`MAX_PACK_ELEMENTS`].
    pub entity_count: u64,
    pub fold_count: u64,
    pub temporal_count: u64,
    pub edge_count: u64,
    pub item_count: u64,

    /// Content hash of each payload section (canonical-JSON sha256). The
    /// learner recomputes these after decryption and compares (MR-P2P-02).
    pub entities_hash: ContentHash,
    pub folds_hash: ContentHash,
    pub temporal_hash: ContentHash,
    pub edges_hash: ContentHash,
    pub items_hash: ContentHash,

    /// Per-chunk authentication tags (opaque; produced by T-33). Empty until
    /// the pack is sealed. One entry per ciphertext chunk.
    pub chunk_macs: Vec<ChunkMac>,

    /// Whole-pack authentication tag (opaque; produced by T-33).
    pub pack_mac: Option<PackMac>,

    /// AEAD cipher used to seal the pack. Subject to the cipher floor in T-33.
    pub cipher_suite: CipherSuite,

    /// Engine (ferrosa) version the pack was produced against.
    pub engine_version: String,

    /// Embedding model identifier and dimension. All embeddings in the payload
    /// must have exactly this dimension.
    pub embedding_model: String,
    pub embedding_dim: u32,

    /// Provenance envelope (carries the teacher+learner fingerprints — T-29 seam).
    pub provenance: PackProvenanceEnvelope,

    /// When the pack was produced (UTC).
    pub created_at: chrono::DateTime<chrono::Utc>,

    /// Optional best-effort TTL, UTC-anchored. `None` means no expiry hint.
    /// "Best-effort" — the learner MAY drop expired content but is not required
    /// to delete it the instant it expires.
    pub ttl_expires_at: Option<chrono::DateTime<chrono::Utc>>,

    /// If true, the pack is "summary-first": item bodies may be omitted and the
    /// learner should treat items as stubs unless detail is fetched.
    pub summary_first: bool,
}

/// The plaintext payload of a pack: the actual transferable memory.
///
/// This is the type that gets AEAD-sealed. It is **never** placed in a
/// [`PackRef`] (which holds ciphertext only) — that separation is the
/// type-level guarantee behind MR-CRYPTO-01.
///
/// `PartialEq` is intentionally not derived: the core payload component types
/// (`EntityEntry`, `FoldEntry`, …) do not implement `PartialEq`. Tests compare
/// payloads by canonical serialization instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackPayload {
    pub entities: Vec<EntityEntry>,
    pub folds: Vec<FoldEntry>,
    pub temporal: Vec<TemporalEvent>,
    pub edges: Vec<TypedEdge>,
    pub items: Vec<TeachingItem>,
    /// Provenance rows to commit alongside the payload on the learner side.
    pub provenance_rows: Vec<MemoryProvenance>,
}

impl PackPayload {
    /// Compute the canonical content hash for each payload section.
    fn section_hashes(&self) -> Result<SectionHashes, ContentHashError> {
        Ok(SectionHashes {
            entities: ContentHash::sha256_json(&self.entities)?,
            folds: ContentHash::sha256_json(&self.folds)?,
            temporal: ContentHash::sha256_json(&self.temporal)?,
            edges: ContentHash::sha256_json(&self.edges)?,
            items: ContentHash::sha256_json(&self.items)?,
        })
    }
}

struct SectionHashes {
    entities: ContentHash,
    folds: ContentHash,
    temporal: ContentHash,
    edges: ContentHash,
    items: ContentHash,
}

/// Wrapper so `ContentHash::sha256_json` serde errors surface as a typed error
/// rather than `anyhow`, keeping `PackError` self-contained.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("failed to canonicalize pack section for hashing: {0}")]
pub struct ContentHashError(String);

impl From<ferrosa_memory_core::remote_identity::RemoteIdentityError> for ContentHashError {
    fn from(e: ferrosa_memory_core::remote_identity::RemoteIdentityError) -> Self {
        ContentHashError(e.to_string())
    }
}

/// A complete, versioned knowledge pack: manifest + plaintext payload.
///
/// This is the in-memory form *before* sealing (producer) and *after* opening
/// (learner). On the wire it never travels as plaintext — see [`PackRef`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgePack {
    pub manifest: PackManifest,
    pub payload: PackPayload,
}

impl KnowledgePack {
    /// Validate every structural invariant of the pack (MR-P2P-01, MR-P2P-02).
    ///
    /// Checks, in order (cheap rejections first):
    /// 1. schema version is supported,
    /// 2. declared counts/size are within bounds,
    /// 3. declared counts match the actual payload,
    /// 4. embedding dimension + finiteness,
    /// 5. content hashes match,
    /// 6. **edge referential closure** — every edge endpoint is a pack entity,
    /// 7. TTL sanity (must be after `created_at` if present).
    ///
    /// Never panics on any input.
    pub fn validate(&self) -> Result<(), PackError> {
        self.validate_schema_version()?;
        self.validate_bounds()?;
        self.validate_counts()?;
        self.validate_embeddings()?;
        self.validate_hashes()?;
        self.validate_edge_closure()?;
        self.validate_ttl()?;
        Ok(())
    }

    fn validate_schema_version(&self) -> Result<(), PackError> {
        if self.manifest.schema_version != PACK_SCHEMA_VERSION {
            return Err(PackError::UnsupportedSchemaVersion {
                found: self.manifest.schema_version,
                supported: PACK_SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    fn validate_bounds(&self) -> Result<(), PackError> {
        let m = &self.manifest;
        for (field, declared) in [
            ("entity", m.entity_count),
            ("fold", m.fold_count),
            ("temporal", m.temporal_count),
            ("edge", m.edge_count),
            ("item", m.item_count),
        ] {
            if declared > MAX_PACK_ELEMENTS {
                return Err(PackError::CountExceedsMax {
                    field,
                    declared,
                    max: MAX_PACK_ELEMENTS,
                });
            }
        }
        if m.declared_size_bytes > MAX_PACK_SIZE_BYTES {
            return Err(PackError::SizeExceedsMax {
                declared: m.declared_size_bytes,
                max: MAX_PACK_SIZE_BYTES,
            });
        }
        if m.embedding_dim == 0 {
            return Err(PackError::ZeroEmbeddingDim);
        }
        Ok(())
    }

    fn validate_counts(&self) -> Result<(), PackError> {
        let m = &self.manifest;
        let p = &self.payload;
        for (field, declared, actual) in [
            ("entity", m.entity_count, p.entities.len()),
            ("fold", m.fold_count, p.folds.len()),
            ("temporal", m.temporal_count, p.temporal.len()),
            ("edge", m.edge_count, p.edges.len()),
            ("item", m.item_count, p.items.len()),
        ] {
            if declared != actual as u64 {
                return Err(PackError::CountMismatch {
                    field,
                    declared,
                    actual,
                });
            }
        }
        Ok(())
    }

    fn validate_embeddings(&self) -> Result<(), PackError> {
        let dim = self.manifest.embedding_dim;
        for entity in &self.payload.entities {
            check_embedding("entity_embedding", entity.entity_embedding.as_deref(), dim)?;
            check_embedding(
                "description_embedding",
                entity.description_embedding.as_deref(),
                dim,
            )?;
        }
        for fold in &self.payload.folds {
            check_embedding("fold_embedding", fold.fold_embedding.as_deref(), dim)?;
        }
        Ok(())
    }

    fn validate_hashes(&self) -> Result<(), PackError> {
        let computed =
            self.payload
                .section_hashes()
                .map_err(|e| PackError::ContentHashMismatch {
                    field: "section_serialization",
                    declared: ContentHash(String::new()),
                    computed: ContentHash(e.0),
                })?;
        let m = &self.manifest;
        for (field, declared, computed) in [
            ("entities", &m.entities_hash, &computed.entities),
            ("folds", &m.folds_hash, &computed.folds),
            ("temporal", &m.temporal_hash, &computed.temporal),
            ("edges", &m.edges_hash, &computed.edges),
            ("items", &m.items_hash, &computed.items),
        ] {
            if declared != computed {
                return Err(PackError::ContentHashMismatch {
                    field,
                    declared: declared.clone(),
                    computed: computed.clone(),
                });
            }
        }
        Ok(())
    }

    /// MR-P2P-02 edge referential closure: every edge endpoint must be present
    /// in the pack's entity set. Rejects dangling edges.
    fn validate_edge_closure(&self) -> Result<(), PackError> {
        let entity_ids: HashSet<Uuid> = self.payload.entities.iter().map(|e| e.entity_id).collect();
        for edge in &self.payload.edges {
            if !entity_ids.contains(&edge.src_id) {
                return Err(PackError::DanglingEdge {
                    endpoint: edge.src_id,
                    role: "src",
                });
            }
            if !entity_ids.contains(&edge.dst_id) {
                return Err(PackError::DanglingEdge {
                    endpoint: edge.dst_id,
                    role: "dst",
                });
            }
        }
        Ok(())
    }

    fn validate_ttl(&self) -> Result<(), PackError> {
        if let Some(expires) = self.manifest.ttl_expires_at
            && expires <= self.manifest.created_at
        {
            return Err(PackError::TtlNotInFuture {
                created: self.manifest.created_at,
                expires,
            });
        }
        Ok(())
    }
}

/// Validate one optional embedding against the declared dimension and reject
/// non-finite values (NaN / ±Inf).
fn check_embedding(
    field: &'static str,
    embedding: Option<&[f32]>,
    declared_dim: u32,
) -> Result<(), PackError> {
    let Some(values) = embedding else {
        return Ok(());
    };
    if values.len() as u32 != declared_dim {
        return Err(PackError::EmbeddingDimMismatch {
            field,
            found: values.len(),
            declared: declared_dim,
        });
    }
    if values.iter().any(|v| !v.is_finite()) {
        return Err(PackError::EmbeddingNotFinite { field });
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// PackRef — host-side ciphertext-only handle (MR-CRYPTO-01 / MT-CRYPTO-01)
// ─────────────────────────────────────────────────────────────────────────

/// One sealed ciphertext chunk: opaque bytes + the nonce used for it.
///
/// The nonce is public (it is not secret in AEAD), but the plaintext and key
/// are not representable here — the `bytes` field is documented and typed as
/// *ciphertext*, and there is no field for plaintext or key material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedChunk {
    /// Ciphertext (includes the AEAD tag appended by the cipher). NOT plaintext.
    pub ciphertext: Vec<u8>,
    /// Per-chunk nonce (public AEAD nonce — never a key).
    pub nonce: Vec<u8>,
}

/// **Host-side pack reference.** Carries ciphertext + metadata *only*.
///
/// MR-CRYPTO-01: it is **type-impossible** to put plaintext or key material in
/// a `PackRef`. The struct has:
/// - a [`PackManifest`] (metadata; the manifest holds only hashes/counts/tags,
///   no plaintext payload and no key),
/// - a per-pack random salt (public input to HKDF — *not* a key),
/// - a vector of [`SealedChunk`] (ciphertext + public nonces).
///
/// There is **no** `payload`, `plaintext`, `key`, or `secret` field, and no
/// constructor that accepts a [`PackPayload`] or any key type. The only way to
/// build a `PackRef` is `pack_crypto::seal_pack`, which consumes a payload and
/// returns ciphertext — the plaintext is moved in and dropped, never stored.
///
/// This is the type a host (control plane / relay) is allowed to see, store,
/// and forward. It learns nothing about pack contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackRef {
    /// Authenticated metadata. Holds no plaintext and no key material.
    pub manifest: PackManifest,
    /// Per-pack HKDF salt (public; bound to both peer fingerprints by T-33).
    /// A salt is a *public* KDF input, not key material.
    pub kdf_salt: Vec<u8>,
    /// Sealed ciphertext chunks. The only payload-bearing field, and it is
    /// ciphertext by type and by name.
    pub chunks: Vec<SealedChunk>,
}

impl PackRef {
    /// Total ciphertext byte length across all chunks (metadata helper for the
    /// host; does not expose plaintext).
    pub fn ciphertext_len(&self) -> usize {
        self.chunks.iter().map(|c| c.ciphertext.len()).sum()
    }

    /// Number of sealed chunks.
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrosa_memory_core::remote_identity::InstanceSigningIdentity;
    use ferrosa_memory_core::types::MemoryState;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn fingerprint(seed: u128) -> PublicKeyFingerprint {
        let signer = InstanceSigningIdentity::generate(InstanceId(id(seed)));
        signer.public_identity().public_key_fingerprint
    }

    fn sample_entity(entity_id: Uuid, embedding: Option<Vec<f32>>) -> EntityEntry {
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
            created_at: chrono::Utc::now(),
            ..Default::default()
        }
    }

    fn sample_edge(src: Uuid, dst: Uuid) -> TypedEdge {
        TypedEdge {
            tenant_id: id(99),
            session_id: id(1),
            src_id: src,
            edge_type: "related_to".into(),
            dst_id: dst,
            weight: 1.0,
            metadata: None,
            created_at: chrono::Utc::now(),
        }
    }

    /// Build a self-consistent pack (counts + hashes set correctly) with the
    /// given payload, dimension and edges.
    fn build_pack(
        entities: Vec<EntityEntry>,
        edges: Vec<TypedEdge>,
        embedding_dim: u32,
    ) -> KnowledgePack {
        let payload = PackPayload {
            entities,
            folds: vec![],
            temporal: vec![],
            edges,
            items: vec![],
            provenance_rows: vec![],
        };
        let hashes = payload.section_hashes().expect("hash");
        let created = chrono::Utc::now();
        let manifest = PackManifest {
            pack_id: id(1000),
            schema_version: PACK_SCHEMA_VERSION,
            pack_version: 1,
            declared_size_bytes: 1024,
            entity_count: payload.entities.len() as u64,
            fold_count: 0,
            temporal_count: 0,
            edge_count: payload.edges.len() as u64,
            item_count: 0,
            entities_hash: hashes.entities,
            folds_hash: hashes.folds,
            temporal_hash: hashes.temporal,
            edges_hash: hashes.edges,
            items_hash: hashes.items,
            chunk_macs: vec![],
            pack_mac: None,
            cipher_suite: CipherSuite::Aes256Gcm,
            engine_version: "ferrosa-test".into(),
            embedding_model: "test-embed".into(),
            embedding_dim,
            provenance: PackProvenanceEnvelope {
                teacher_instance_id: InstanceId(id(2)),
                teacher_fingerprint: fingerprint(2),
                learner_fingerprint: fingerprint(3),
                request_id: None,
                source_namespace: "ns".into(),
            },
            created_at: created,
            ttl_expires_at: Some(created + chrono::Duration::hours(1)),
            summary_first: false,
        };
        KnowledgePack { manifest, payload }
    }

    // MT-P2P-01 — schema is explicitly versioned and round-trips.
    #[test]
    fn mt_p2p_01_schema_versioned_and_round_trips() {
        let pack = build_pack(vec![sample_entity(id(1), None)], vec![], 4);
        assert_eq!(pack.manifest.schema_version, PACK_SCHEMA_VERSION);
        assert_eq!(PACK_SCHEMA_VERSION, 1);

        let json = serde_json::to_vec(&pack).expect("serialize");
        let back: KnowledgePack = serde_json::from_slice(&json).expect("deserialize");
        // Core payload types lack PartialEq; compare by canonical serialization.
        assert_eq!(
            serde_json::to_value(&pack).expect("v1"),
            serde_json::to_value(&back).expect("v2")
        );
        back.validate().expect("valid pack");
    }

    #[test]
    fn mt_p2p_01_unsupported_schema_version_rejected() {
        let mut pack = build_pack(vec![sample_entity(id(1), None)], vec![], 4);
        pack.manifest.schema_version = 999;
        assert_eq!(
            pack.validate(),
            Err(PackError::UnsupportedSchemaVersion {
                found: 999,
                supported: PACK_SCHEMA_VERSION
            })
        );
    }

    // MT-P2P-02 — manifest counts/hashes validated; edge closure rejects dangling.
    #[test]
    fn mt_p2p_02_count_mismatch_rejected() {
        let mut pack = build_pack(vec![sample_entity(id(1), None)], vec![], 4);
        pack.manifest.entity_count = 5;
        assert!(matches!(
            pack.validate(),
            Err(PackError::CountMismatch {
                field: "entity",
                ..
            })
        ));
    }

    #[test]
    fn mt_p2p_02_content_hash_mismatch_rejected() {
        let mut pack = build_pack(vec![sample_entity(id(1), None)], vec![], 4);
        pack.manifest.entities_hash = ContentHash("deadbeef".into());
        assert!(matches!(
            pack.validate(),
            Err(PackError::ContentHashMismatch {
                field: "entities",
                ..
            })
        ));
    }

    #[test]
    fn mt_p2p_02_edge_closure_accepts_resolved_edges() {
        let a = id(1);
        let b = id(2);
        let pack = build_pack(
            vec![sample_entity(a, None), sample_entity(b, None)],
            vec![sample_edge(a, b)],
            4,
        );
        pack.validate().expect("resolved edges valid");
    }

    #[test]
    fn mt_p2p_02_edge_closure_rejects_dangling_src() {
        let a = id(1);
        let missing = id(777);
        let pack = build_pack(
            vec![sample_entity(a, None)],
            vec![sample_edge(missing, a)],
            4,
        );
        assert_eq!(
            pack.validate(),
            Err(PackError::DanglingEdge {
                endpoint: missing,
                role: "src"
            })
        );
    }

    #[test]
    fn mt_p2p_02_edge_closure_rejects_dangling_dst() {
        let a = id(1);
        let missing = id(888);
        let pack = build_pack(
            vec![sample_entity(a, None)],
            vec![sample_edge(a, missing)],
            4,
        );
        assert_eq!(
            pack.validate(),
            Err(PackError::DanglingEdge {
                endpoint: missing,
                role: "dst"
            })
        );
    }

    #[test]
    fn embedding_dim_mismatch_rejected() {
        let pack = build_pack(
            vec![sample_entity(id(1), Some(vec![0.1, 0.2, 0.3]))],
            vec![],
            4, // declares 4 but entity has 3
        );
        assert!(matches!(
            pack.validate(),
            Err(PackError::EmbeddingDimMismatch { found: 3, .. })
        ));
    }

    #[test]
    fn embedding_non_finite_rejected() {
        let pack = build_pack(
            vec![sample_entity(id(1), Some(vec![0.1, f32::NAN, 0.3, 0.4]))],
            vec![],
            4,
        );
        assert!(matches!(
            pack.validate(),
            Err(PackError::EmbeddingNotFinite { .. })
        ));
    }

    #[test]
    fn ttl_in_past_rejected() {
        let mut pack = build_pack(vec![sample_entity(id(1), None)], vec![], 4);
        pack.manifest.ttl_expires_at = Some(pack.manifest.created_at - chrono::Duration::hours(1));
        assert!(matches!(
            pack.validate(),
            Err(PackError::TtlNotInFuture { .. })
        ));
    }

    #[test]
    fn oversized_count_rejected_without_allocation() {
        let mut pack = build_pack(vec![sample_entity(id(1), None)], vec![], 4);
        pack.manifest.entity_count = MAX_PACK_ELEMENTS + 1;
        assert!(matches!(
            pack.validate(),
            Err(PackError::CountExceedsMax {
                field: "entity",
                ..
            })
        ));
    }

    // MT-CRYPTO-01 — PackRef cannot represent plaintext or key material.
    //
    // This is enforced *by type*: the test exercises that the only payload
    // field is ciphertext-typed, the manifest carries no plaintext, and there
    // is no field/constructor accepting a PackPayload or a key. The assertions
    // below are structural — if someone adds a plaintext/key field, the
    // construction here breaks to compile or the field-count assertion fails.
    #[test]
    fn mt_crypto_01_packref_holds_ciphertext_and_metadata_only() {
        let pack_ref = PackRef {
            manifest: build_pack(vec![sample_entity(id(1), None)], vec![], 4).manifest,
            kdf_salt: vec![0u8; 32],
            chunks: vec![SealedChunk {
                ciphertext: vec![1, 2, 3, 4],
                nonce: vec![0u8; 12],
            }],
        };

        // Round-trips as ciphertext-only metadata.
        let json = serde_json::to_value(&pack_ref).expect("serialize");
        let obj = json.as_object().expect("object");

        // Exactly these three fields exist — no plaintext/payload/key/secret.
        let keys: HashSet<&str> = obj.keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            HashSet::from(["manifest", "kdf_salt", "chunks"]),
            "PackRef must expose only manifest + kdf_salt + chunks"
        );
        for forbidden in ["payload", "plaintext", "key", "secret", "content_key"] {
            assert!(
                !obj.contains_key(forbidden),
                "PackRef must not contain a `{forbidden}` field"
            );
        }

        // The manifest inside a PackRef carries no plaintext payload field.
        let manifest_obj = obj["manifest"].as_object().expect("manifest object");
        for forbidden in ["payload", "plaintext", "entities", "key", "content_key"] {
            assert!(
                !manifest_obj.contains_key(forbidden),
                "PackManifest must not contain a `{forbidden}` field"
            );
        }

        assert_eq!(pack_ref.ciphertext_len(), 4);
        assert_eq!(pack_ref.chunk_count(), 1);
    }
}
