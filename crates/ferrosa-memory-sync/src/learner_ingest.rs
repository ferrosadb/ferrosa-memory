//! MAAS-T-27 — learner-side pack ingest: verify → atomic+idempotent apply →
//! provenance + TTL.
//!
//! This module takes a host-side [`PackRef`] plus the shared keying material and
//! peer fingerprints, then:
//!
//! 1. **Verifies the whole pack via AEAD before any plaintext parse**
//!    (MR-P2P-10). [`crate::pack_crypto::open_pack`] decrypts and tag-checks
//!    every chunk first; only then is the plaintext deserialized.
//! 2. **Validates every field at the boundary** (MR-P2P-14): size/counts/edge
//!    closure/embedding-dim/finiteness/TTL — all via [`KnowledgePack::validate`]
//!    plus the replay/version checks here. No `unwrap`/`expect`/indexing on
//!    untrusted bytes.
//! 3. **Applies as a single atomic unit** (MR-P2P-11). Ferrosa's `Storage`
//!    trait exposes only per-row writers (no multi-statement CQL BATCH), so we
//!    use a **stage-then-flip** model: rows are written to a staging area keyed
//!    by `(pack_id, pack_version)` and only a final *flip* makes them visible.
//!    A failure before the flip leaves the graph with nothing applied. See
//!    [`PackApplyStore`] for the seam and the "atomicity gap" note below.
//! 4. **Idempotent upsert-by-PK** (MR-P2P-12): re-applying the same pack equals
//!    applying once (CQL INSERT is upsert-by-PK). **Older-version replay is
//!    rejected** by comparing `pack_version` against the last-applied version.
//! 5. **Commits provenance + UTC-anchored TTL in the same atomic unit**
//!    (MR-P2P-13). The sender identity is **channel-attested** — taken from the
//!    `attested_*` fingerprints the caller supplies (from the secure channel /
//!    T-29 DTLS vouch), **never** from the pack's self-claimed provenance.
//!
//! # Atomicity gap (reported, not faked)
//!
//! The `Storage` trait has no atomic multi-row CQL BATCH. True single-statement
//! atomicity therefore cannot be achieved through the current trait. The
//! safest available approach is **stage-then-flip**: all rows land in a staging
//! buffer first; the flip is the single observable transition. If staging
//! fails, nothing is flipped. The flip itself iterates per-row writes — if the
//! flip is interrupted, a resumable re-apply (idempotent by PK) completes it.
//! This is fail-closed (a half-staged pack is never made visible) but it is not
//! a database transaction. Closing the gap requires an atomic BATCH/apply
//! method on `Storage` (cross-repo follow-up).

// Fail-loud on untrusted input. Tests may use unwrap/expect on known-good
// fixtures (the denies guard production paths handling untrusted bytes).
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

use ferrosa_memory_core::remote_identity::PublicKeyFingerprint;
use ferrosa_memory_core::remotes::types::MemoryProvenance;
use ferrosa_memory_core::types::{EntityEntry, FoldEntry, TemporalEvent, TypedEdge};
use uuid::Uuid;

use crate::pack::{KnowledgePack, PackError, PackRef};
use crate::pack_crypto::{CipherFloor, PackCryptoError, Secret, open_pack};

/// Channel-attested sender identity (MR-P2P-13).
///
/// These fingerprints come from the secure transport / DTLS vouch (T-29) —
/// **not** from the pack's self-claimed provenance. The learner trusts these,
/// not `pack.manifest.provenance.*`. The ingest path cross-checks that the
/// pack was *sealed for* this learner and *by* this teacher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelAttestation {
    /// Teacher fingerprint as attested by the channel.
    pub attested_teacher: PublicKeyFingerprint,
    /// This learner's own fingerprint as attested by the channel.
    pub attested_learner: PublicKeyFingerprint,
    /// Remote id this channel maps to (for provenance rows).
    pub remote_id: Uuid,
}

/// The last-applied version record for a pack, used for replay defense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedVersion {
    pub pack_id: Uuid,
    pub pack_version: u64,
}

/// Errors from the learner ingest path. All fail-closed.
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("crypto verification failed: {0}")]
    Crypto(#[from] PackCryptoError),

    #[error("pack validation failed: {0}")]
    Pack(#[from] PackError),

    #[error(
        "channel attestation mismatch: pack sealed for ({pack_teacher}, {pack_learner}) but \
         channel attests ({chan_teacher}, {chan_learner})"
    )]
    AttestationMismatch {
        pack_teacher: String,
        pack_learner: String,
        chan_teacher: String,
        chan_learner: String,
    },

    #[error(
        "replay rejected: pack {pack_id} version {incoming} is not newer than applied {applied}"
    )]
    ReplayRejected {
        pack_id: Uuid,
        incoming: u64,
        applied: u64,
    },

    #[error("staging failed (nothing applied): {0}")]
    StagingFailed(String),

    #[error("flip failed mid-apply (resumable re-apply required): {0}")]
    FlipFailed(String),
}

/// Outcome of a successful ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOutcome {
    pub pack_id: Uuid,
    pub pack_version: u64,
    pub entities_applied: usize,
    pub folds_applied: usize,
    pub temporal_applied: usize,
    pub edges_applied: usize,
    pub provenance_rows: usize,
    /// True if this pack version was already applied and the call was a no-op
    /// (idempotent re-apply).
    pub idempotent_noop: bool,
}

/// The apply seam (MR-P2P-11). A learner store that supports stage-then-flip.
///
/// Implementors stage all rows for `(pack_id, pack_version)`, then make them
/// visible in a single `flip`. A real `Storage`-backed adapter performs the
/// per-row writes during `flip` (idempotent by PK); a test mock can fail at a
/// chosen step to prove "nothing applied on interrupt".
///
/// All writes are idempotent upsert-by-PK (MR-P2P-12).
pub trait PackApplyStore {
    /// Return the last-applied version for `pack_id`, if any. Used for replay
    /// defense.
    fn last_applied_version(
        &self,
        pack_id: Uuid,
    ) -> impl std::future::Future<Output = anyhow::Result<Option<u64>>> + Send;

    /// Stage all rows for one pack version. MUST NOT make anything visible.
    /// Returning an error here means **nothing applied**.
    fn stage(
        &self,
        staged: &StagedPack,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Atomically (as atomically as the backend allows) make the staged pack
    /// visible AND record provenance + TTL + applied-version in the same unit.
    fn flip(
        &self,
        staged: &StagedPack,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

/// The set of rows to apply for one pack, plus provenance + TTL + version.
///
/// Provenance and TTL travel *with* the staged rows so the flip commits them in
/// the same unit (MR-P2P-13).
///
/// No `PartialEq`: core payload component types do not implement it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StagedPack {
    pub pack_id: Uuid,
    pub pack_version: u64,
    pub entities: Vec<EntityEntry>,
    pub folds: Vec<FoldEntry>,
    pub temporal: Vec<TemporalEvent>,
    pub edges: Vec<TypedEdge>,
    pub provenance: Vec<MemoryProvenance>,
    /// UTC-anchored TTL committed alongside the rows.
    pub ttl_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Channel-attested remote id (sender identity is channel-attested).
    pub remote_id: Uuid,
}

/// Ingest a pack end-to-end. The single entry point for T-27.
///
/// Order is security-critical:
/// 1. AEAD-open (verify-before-parse, MR-P2P-10),
/// 2. structural validation (already done inside `open_pack`; re-checked),
/// 3. channel-attestation cross-check (MR-P2P-13),
/// 4. replay/version check (MR-P2P-12),
/// 5. stage,
/// 6. flip (atomic unit: rows + provenance + TTL, MR-P2P-11/13).
pub async fn ingest_pack<S: PackApplyStore>(
    store: &S,
    pack_ref: &PackRef,
    ikm: &Secret,
    attestation: &ChannelAttestation,
    floor: CipherFloor,
) -> Result<IngestOutcome, IngestError> {
    // (1)+(2) AEAD-verify the whole pack BEFORE any plaintext parse (and reject
    // any sub-floor cipher — downgrade — inside open_pack), then re-validate the
    // recovered plaintext's structural invariants.
    let pack = open_pack(
        pack_ref,
        ikm,
        &attestation.attested_teacher,
        &attestation.attested_learner,
        floor,
    )?;
    pack.validate()?;

    // (3) Sender identity is channel-attested, NOT pack-claimed. Cross-check
    // that the pack was sealed by/for the attested peers. (open_pack already
    // guarantees the *key* matched the attested fingerprints — every tag
    // verified — so this is a defense-in-depth equality check on the claimed
    // provenance to catch a pack whose self-claim disagrees with the channel.)
    let prov = &pack.manifest.provenance;
    if prov.teacher_fingerprint != attestation.attested_teacher
        || prov.learner_fingerprint != attestation.attested_learner
    {
        return Err(IngestError::AttestationMismatch {
            pack_teacher: prov.teacher_fingerprint.0.clone(),
            pack_learner: prov.learner_fingerprint.0.clone(),
            chan_teacher: attestation.attested_teacher.0.clone(),
            chan_learner: attestation.attested_learner.0.clone(),
        });
    }

    let pack_id = pack.manifest.pack_id;
    let incoming_version = pack.manifest.pack_version;

    // (4) Replay defense (MR-P2P-12): older-or-equal version is rejected unless
    // it is exactly the already-applied version, which is an idempotent no-op.
    let applied = store
        .last_applied_version(pack_id)
        .await
        .map_err(|e| IngestError::StagingFailed(format!("version lookup: {e}")))?;
    if let Some(applied_version) = applied {
        if incoming_version == applied_version {
            // Same version already applied — idempotent no-op (MR-P2P-12).
            return Ok(IngestOutcome {
                pack_id,
                pack_version: incoming_version,
                entities_applied: pack.payload.entities.len(),
                folds_applied: pack.payload.folds.len(),
                temporal_applied: pack.payload.temporal.len(),
                edges_applied: pack.payload.edges.len(),
                provenance_rows: pack.payload.provenance_rows.len(),
                idempotent_noop: true,
            });
        }
        if incoming_version < applied_version {
            return Err(IngestError::ReplayRejected {
                pack_id,
                incoming: incoming_version,
                applied: applied_version,
            });
        }
    }

    // Build the staged unit. Provenance rows carry the channel-attested
    // remote_id (sender identity is channel-attested, MR-P2P-13).
    let staged = build_staged(&pack, attestation);

    // (5) Stage. Failure here ⇒ nothing applied (MR-P2P-11).
    store
        .stage(&staged)
        .await
        .map_err(|e| IngestError::StagingFailed(e.to_string()))?;

    // (6) Flip: rows + provenance + TTL committed as one unit.
    store
        .flip(&staged)
        .await
        .map_err(|e| IngestError::FlipFailed(e.to_string()))?;

    Ok(IngestOutcome {
        pack_id,
        pack_version: incoming_version,
        entities_applied: staged.entities.len(),
        folds_applied: staged.folds.len(),
        temporal_applied: staged.temporal.len(),
        edges_applied: staged.edges.len(),
        provenance_rows: staged.provenance.len(),
        idempotent_noop: false,
    })
}

/// Build the staged unit from a verified pack + channel attestation.
fn build_staged(pack: &KnowledgePack, attestation: &ChannelAttestation) -> StagedPack {
    // Provenance rows: prefer the pack's declared provenance rows but stamp the
    // channel-attested remote_id onto each (sender identity is channel-attested).
    let provenance: Vec<MemoryProvenance> = pack
        .payload
        .provenance_rows
        .iter()
        .map(|p| MemoryProvenance {
            remote_id: attestation.remote_id,
            ..p.clone()
        })
        .collect();

    StagedPack {
        pack_id: pack.manifest.pack_id,
        pack_version: pack.manifest.pack_version,
        entities: pack.payload.entities.clone(),
        folds: pack.payload.folds.clone(),
        temporal: pack.payload.temporal.clone(),
        edges: pack.payload.edges.clone(),
        provenance,
        ttl_expires_at: pack.manifest.ttl_expires_at,
        remote_id: attestation.remote_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::{
        CipherSuite, PACK_SCHEMA_VERSION, PackManifest, PackPayload, PackProvenanceEnvelope,
    };
    use crate::pack_crypto::seal_pack;
    use ferrosa_memory_core::remote_identity::{ContentHash, InstanceId, InstanceSigningIdentity};
    use ferrosa_memory_core::types::MemoryState;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn fp(seed: u128) -> PublicKeyFingerprint {
        InstanceSigningIdentity::generate(InstanceId(id(seed)))
            .public_identity()
            .public_key_fingerprint
    }

    fn entity(n: u128) -> EntityEntry {
        EntityEntry {
            tenant_id: id(99),
            entity_id: id(n),
            session_id: id(1),
            entity_name: format!("e{n}"),
            entity_type: "concept".into(),
            context_snippet: "ctx".into(),
            confidence: 0.9,
            state: MemoryState::Active,
            created_at: chrono::Utc::now(),
            ..Default::default()
        }
    }

    fn provenance_row(n: u128) -> MemoryProvenance {
        MemoryProvenance {
            provenance_id: id(n + 5000),
            local_entity_id: id(n),
            remote_id: id(0), // will be overwritten by channel attestation
            packet_id: id(1000),
            item_id: id(n + 6000),
            content_hash: ContentHash("ch".into()),
            signature_hash: ContentHash("sh".into()),
            imported_at: chrono::Utc::now(),
        }
    }

    /// Build a sealed pack for (teacher, learner) with a given version.
    fn sealed_pack(
        teacher: &PublicKeyFingerprint,
        learner: &PublicKeyFingerprint,
        ikm: &Secret,
        version: u64,
    ) -> PackRef {
        let payload = PackPayload {
            entities: vec![entity(1), entity(2)],
            folds: vec![],
            temporal: vec![],
            edges: vec![],
            items: vec![],
            provenance_rows: vec![provenance_row(1), provenance_row(2)],
        };
        let created = chrono::Utc::now();
        let manifest = PackManifest {
            pack_id: id(1000),
            schema_version: PACK_SCHEMA_VERSION,
            pack_version: version,
            declared_size_bytes: 4096,
            entity_count: payload.entities.len() as u64,
            fold_count: 0,
            temporal_count: 0,
            edge_count: 0,
            item_count: 0,
            entities_hash: ContentHash::sha256_json(&payload.entities)
                .unwrap_or(ContentHash(String::new())),
            folds_hash: ContentHash::sha256_json(&payload.folds)
                .unwrap_or(ContentHash(String::new())),
            temporal_hash: ContentHash::sha256_json(&payload.temporal)
                .unwrap_or(ContentHash(String::new())),
            edges_hash: ContentHash::sha256_json(&payload.edges)
                .unwrap_or(ContentHash(String::new())),
            items_hash: ContentHash::sha256_json(&payload.items)
                .unwrap_or(ContentHash(String::new())),
            chunk_macs: vec![],
            pack_mac: None,
            cipher_suite: CipherSuite::Aes256Gcm,
            engine_version: "ferrosa-test".into(),
            embedding_model: "test".into(),
            embedding_dim: 4,
            provenance: PackProvenanceEnvelope {
                teacher_instance_id: InstanceId(id(2)),
                teacher_fingerprint: teacher.clone(),
                learner_fingerprint: learner.clone(),
                request_id: None,
                source_namespace: "ns".into(),
            },
            created_at: created,
            ttl_expires_at: Some(created + chrono::Duration::hours(1)),
            summary_first: false,
        };
        let pack = KnowledgePack { manifest, payload };
        seal_pack(&pack, ikm, teacher, learner, 32, CipherFloor::default()).expect("seal")
    }

    /// In-memory apply store with controllable failure points.
    #[derive(Default)]
    struct MockStore {
        applied_versions: Mutex<HashMap<Uuid, u64>>,
        committed_entities: Mutex<Vec<EntityEntry>>,
        committed_provenance: Mutex<Vec<MemoryProvenance>>,
        committed_ttl: Mutex<Option<chrono::DateTime<chrono::Utc>>>,
        fail_stage: bool,
        /// Fail the flip after this many entity writes (simulate interrupt).
        fail_flip_after: Option<usize>,
        flip_calls: AtomicUsize,
    }

    impl PackApplyStore for MockStore {
        async fn last_applied_version(&self, pack_id: Uuid) -> anyhow::Result<Option<u64>> {
            Ok(self
                .applied_versions
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?
                .get(&pack_id)
                .copied())
        }

        async fn stage(&self, _staged: &StagedPack) -> anyhow::Result<()> {
            if self.fail_stage {
                anyhow::bail!("staging blew up");
            }
            Ok(())
        }

        async fn flip(&self, staged: &StagedPack) -> anyhow::Result<()> {
            self.flip_calls.fetch_add(1, Ordering::SeqCst);
            // Apply entities one-by-one; optionally fail mid-way to simulate an
            // interrupt. Because we only record applied_version at the END, a
            // mid-flip failure leaves nothing "committed" from the caller's POV.
            let mut staged_entities = Vec::new();
            for (i, e) in staged.entities.iter().enumerate() {
                if let Some(limit) = self.fail_flip_after
                    && i >= limit
                {
                    anyhow::bail!("flip interrupted after {limit} entities");
                }
                staged_entities.push(e.clone());
            }
            // Commit only on full success (the "flip" is the single visible
            // transition). Idempotent upsert-by-PK: replace existing rows.
            {
                let mut ents = self
                    .committed_entities
                    .lock()
                    .map_err(|_| anyhow::anyhow!("poisoned"))?;
                for e in staged_entities {
                    if let Some(pos) = ents
                        .iter()
                        .position(|x| x.entity_id == e.entity_id && x.session_id == e.session_id)
                    {
                        if let Some(slot) = ents.get_mut(pos) {
                            *slot = e;
                        }
                    } else {
                        ents.push(e);
                    }
                }
            }
            *self
                .committed_provenance
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))? = staged.provenance.clone();
            *self
                .committed_ttl
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))? = staged.ttl_expires_at;
            self.applied_versions
                .lock()
                .map_err(|_| anyhow::anyhow!("poisoned"))?
                .insert(staged.pack_id, staged.pack_version);
            Ok(())
        }
    }

    fn attestation(
        teacher: &PublicKeyFingerprint,
        learner: &PublicKeyFingerprint,
    ) -> ChannelAttestation {
        ChannelAttestation {
            attested_teacher: teacher.clone(),
            attested_learner: learner.clone(),
            remote_id: id(42),
        }
    }

    // MT-P2P-13 — provenance + TTL committed atomically; sender channel-attested.
    #[tokio::test]
    async fn mt_p2p_13_provenance_and_ttl_committed_atomically_with_attested_sender() {
        let t = fp(10);
        let l = fp(11);
        let ikm = Secret::random(32);
        let pack_ref = sealed_pack(&t, &l, &ikm, 1);
        let store = MockStore::default();
        let att = attestation(&t, &l);

        let outcome = ingest_pack(&store, &pack_ref, &ikm, &att, CipherFloor::default())
            .await
            .expect("ingest");
        assert_eq!(outcome.entities_applied, 2);
        assert_eq!(outcome.provenance_rows, 2);

        // TTL committed.
        assert!(store.committed_ttl.lock().unwrap().is_some());
        // Provenance rows carry the channel-attested remote_id, NOT pack-claimed.
        let prov = store.committed_provenance.lock().unwrap();
        assert_eq!(prov.len(), 2);
        for p in prov.iter() {
            assert_eq!(p.remote_id, id(42), "remote_id must be channel-attested");
        }
    }

    // MT-P2P-10 — flip ciphertext/tag bits ⇒ reject, no partial apply.
    #[tokio::test]
    async fn mt_p2p_10_tampered_ciphertext_rejected_no_partial_apply() {
        let t = fp(10);
        let l = fp(11);
        let ikm = Secret::random(32);
        let mut pack_ref = sealed_pack(&t, &l, &ikm, 1);
        // Flip a bit in the first chunk's ciphertext.
        if let Some(chunk) = pack_ref.chunks.get_mut(0)
            && let Some(byte) = chunk.ciphertext.get_mut(0)
        {
            *byte ^= 0xff;
        }
        let store = MockStore::default();
        let att = attestation(&t, &l);

        let err = ingest_pack(&store, &pack_ref, &ikm, &att, CipherFloor::default())
            .await
            .unwrap_err();
        assert!(matches!(err, IngestError::Crypto(PackCryptoError::Decrypt)));
        // Nothing applied.
        assert!(store.committed_entities.lock().unwrap().is_empty());
        assert!(store.committed_provenance.lock().unwrap().is_empty());
        assert_eq!(store.flip_calls.load(Ordering::SeqCst), 0);
    }

    // MT-P2P-11 — interrupt mid-apply ⇒ nothing applied.
    #[tokio::test]
    async fn mt_p2p_11_interrupt_mid_flip_applies_nothing() {
        let t = fp(10);
        let l = fp(11);
        let ikm = Secret::random(32);
        let pack_ref = sealed_pack(&t, &l, &ikm, 1);
        let store = MockStore {
            fail_flip_after: Some(1), // fail after first entity
            ..Default::default()
        };
        let att = attestation(&t, &l);

        let err = ingest_pack(&store, &pack_ref, &ikm, &att, CipherFloor::default())
            .await
            .unwrap_err();
        assert!(matches!(err, IngestError::FlipFailed(_)));
        // The flip aborted before committing — nothing visible, no version set.
        assert!(store.committed_entities.lock().unwrap().is_empty());
        assert!(
            store
                .last_applied_version(id(1000))
                .await
                .unwrap()
                .is_none()
        );
    }

    // MT-P2P-12 — apply twice == once; older-version replay rejected.
    #[tokio::test]
    async fn mt_p2p_12_idempotent_and_replay_rejected() {
        let t = fp(10);
        let l = fp(11);
        let ikm = Secret::random(32);
        let store = MockStore::default();
        let att = attestation(&t, &l);

        // Apply version 2.
        let v2 = sealed_pack(&t, &l, &ikm, 2);
        let o1 = ingest_pack(&store, &v2, &ikm, &att, CipherFloor::default())
            .await
            .expect("apply v2");
        assert!(!o1.idempotent_noop);
        assert_eq!(store.committed_entities.lock().unwrap().len(), 2);

        // Apply version 2 again ⇒ idempotent no-op, still 2 entities.
        let o2 = ingest_pack(&store, &v2, &ikm, &att, CipherFloor::default())
            .await
            .expect("reapply v2");
        assert!(o2.idempotent_noop);
        assert_eq!(store.committed_entities.lock().unwrap().len(), 2);

        // Apply older version 1 ⇒ replay rejected.
        let v1 = sealed_pack(&t, &l, &ikm, 1);
        let err = ingest_pack(&store, &v1, &ikm, &att, CipherFloor::default())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            IngestError::ReplayRejected {
                incoming: 1,
                applied: 2,
                ..
            }
        ));
    }

    // MT-P2P-14 — malformed/truncated/oversized pack rejected without panic.
    #[tokio::test]
    async fn mt_p2p_14_truncated_pack_rejected_without_panic() {
        let t = fp(10);
        let l = fp(11);
        let ikm = Secret::random(32);
        let mut pack_ref = sealed_pack(&t, &l, &ikm, 1);
        // Truncate the first chunk's ciphertext (shorter than the tag).
        if let Some(chunk) = pack_ref.chunks.get_mut(0) {
            chunk.ciphertext.truncate(3);
        }
        let store = MockStore::default();
        let att = attestation(&t, &l);
        let err = ingest_pack(&store, &pack_ref, &ikm, &att, CipherFloor::default())
            .await
            .unwrap_err();
        assert!(matches!(err, IngestError::Crypto(PackCryptoError::Decrypt)));
        assert!(store.committed_entities.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn attestation_mismatch_rejected() {
        let t = fp(10);
        let l = fp(11);
        let ikm = Secret::random(32);
        let pack_ref = sealed_pack(&t, &l, &ikm, 1);
        let store = MockStore::default();
        // Attest with a DIFFERENT teacher: open_pack fails first (wrong key).
        let wrong = fp(777);
        let att = attestation(&wrong, &l);
        let err = ingest_pack(&store, &pack_ref, &ikm, &att, CipherFloor::default())
            .await
            .unwrap_err();
        // Wrong key ⇒ tag failure surfaces as Crypto(Decrypt) before attestation
        // equality is even checked — fail-closed at the earliest gate.
        assert!(matches!(err, IngestError::Crypto(PackCryptoError::Decrypt)));
    }
}
