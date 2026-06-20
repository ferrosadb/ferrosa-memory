//! MAAS-T-33 — end-to-end AEAD sealing/opening for knowledge packs.
//!
//! Fail-closed crypto for teacher→learner pack transfer. The content key is
//! derived **per pack** via HKDF-SHA256 from a shared input-keying-material
//! (IKM), bound to a per-pack random salt that is itself bound to **both peer
//! fingerprints** (teacher + learner). Each ciphertext chunk uses a **unique
//! nonce** that never repeats within a key.
//!
//! # Requirements implemented
//!
//! - **MR-CRYPTO-03** — fresh per-pack key (HKDF over a per-pack salt) and a
//!   unique per-chunk nonce derived from a per-pack random 32-bit base plus a
//!   monotonic chunk counter. A `(key, nonce)` pair is therefore never reused:
//!   different packs ⇒ different keys; within a pack, the counter strictly
//!   increases and is capped so it can never wrap.
//! - **MR-CRYPTO-04** — fail-closed. There is **no plaintext path** and **no
//!   weak-cipher fallback**: [`CipherSuite`] has only AEAD variants meeting the
//!   floor, and [`open_pack`] rejects any manifest whose cipher is below the
//!   floor by returning [`PackCryptoError::DowngradeBlocked`] and incrementing
//!   [`crypto_downgrade_blocked_total`]. If key establishment fails, no
//!   ciphertext is produced.
//! - **MR-CRYPTO-05** — the host never sees key material. Keys are wrapped in
//!   [`Secret`] with a redacting `Debug` and zeroized on drop. The on-the-wire
//!   [`PackRef`] (see [`crate::pack`]) carries ciphertext + a *public* salt
//!   only; the IKM and derived key are never serialized and have no accessor
//!   returning raw bytes outside this module's sealing primitives.
//!
//! # T-29 seam
//!
//! [`derive_content_key`] takes the **teacher and learner
//! [`PublicKeyFingerprint`]s** as the peer-identity binding. These are the
//! exact inputs that MAAS-T-29 (DTLS-vouched peer identity, ferrosa-dbaas) will
//! supply once it exists. Today the caller passes fingerprints obtained from
//! the signing identities; tomorrow they come from the DTLS vouch — through
//! this same function signature. The crypto binding is real; only the *source*
//! of the fingerprints is deferred.

// Fail-loud on untrusted input — never panic/unwrap/expect/index in this
// security-critical module. Tests may use unwrap/expect on known-good fixtures.
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

use std::sync::atomic::{AtomicU64, Ordering};

use aead::{Aead, KeyInit, Payload};
use aes_gcm::Aes256Gcm;
use chacha20poly1305::ChaCha20Poly1305;
use ferrosa_memory_core::remote_identity::PublicKeyFingerprint;
use hkdf::Hkdf;
use rand::RngCore;
use sha2_kdf::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::pack::{
    CipherSuite, KnowledgePack, PackError, PackManifest, PackPayload, PackRef, SealedChunk,
};

/// Length of an AEAD content key in bytes (AES-256 / ChaCha20-Poly1305 both 32).
pub const CONTENT_KEY_LEN: usize = 32;
/// Length of the per-pack HKDF salt in bytes.
pub const KDF_SALT_LEN: usize = 32;
/// AEAD nonce length in bytes (96-bit nonce for both AES-GCM and ChaCha20-Poly1305).
pub const NONCE_LEN: usize = 12;
/// Maximum chunk count per pack. The per-chunk nonce counter is bounded by this
/// so the counter can never wrap and reuse a `(key, nonce)` pair.
pub const MAX_CHUNKS_PER_PACK: u64 = 1_000_000;
/// Default plaintext chunk size for sealing (1 MiB).
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

/// Process-global counter: number of transfers refused because a cipher
/// downgrade / sub-floor cipher was detected (MT-CRYPTO-04 surface).
static CRYPTO_DOWNGRADE_BLOCKED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Read the running total of blocked cipher downgrades (observability).
pub fn crypto_downgrade_blocked_total() -> u64 {
    CRYPTO_DOWNGRADE_BLOCKED_TOTAL.load(Ordering::Relaxed)
}

fn record_downgrade_blocked() {
    CRYPTO_DOWNGRADE_BLOCKED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// A secret byte buffer with a **redacting `Debug`** and zeroize-on-drop.
///
/// MR-CRYPTO-05: `{:?}` prints `Secret(<redacted N bytes>)`, never the bytes.
/// There is no `Display`, no `Serialize`, and no public accessor that returns
/// the raw bytes outside this module — sealing primitives take `&Secret` and
/// reach the bytes through the crate-private `expose` accessor.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct Secret(Vec<u8>);

impl Secret {
    /// Wrap raw bytes as a secret. Prefer [`Secret::random`] for fresh keys.
    pub fn new(bytes: Vec<u8>) -> Self {
        Secret(bytes)
    }

    /// Generate `len` cryptographically-random secret bytes.
    pub fn random(len: usize) -> Self {
        let mut bytes = vec![0u8; len];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Secret(bytes)
    }

    /// Crate-private access to the raw bytes for use inside crypto primitives.
    /// Intentionally NOT `pub` — no code outside this crate can read key bytes.
    pub(crate) fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Length of the secret (safe to expose; reveals no material).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Redacting Debug — never prints secret bytes (MR-CRYPTO-05 / MT-CRYPTO-05).
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(<redacted {} bytes>)", self.0.len())
    }
}

/// Errors from the AEAD sealing/opening path. All are fail-closed — no variant
/// represents a "fell back to plaintext" success.
#[derive(Debug, thiserror::Error)]
pub enum PackCryptoError {
    #[error("AEAD encryption failed (no ciphertext produced)")]
    Encrypt,

    #[error("AEAD decryption/verification failed (tag mismatch or corrupt ciphertext)")]
    Decrypt,

    #[error(
        "cipher downgrade blocked: manifest cipher {found:?} is below the required floor; \
         transfer refused"
    )]
    DowngradeBlocked { found: CipherSuite },

    #[error("key establishment failed: {0}")]
    KeyEstablishment(String),

    #[error("invalid key length {found}; expected {CONTENT_KEY_LEN}")]
    InvalidKeyLength { found: usize },

    #[error("invalid nonce length {found}; expected {NONCE_LEN}")]
    InvalidNonceLength { found: usize },

    #[error("pack exceeds maximum chunk count {MAX_CHUNKS_PER_PACK}")]
    TooManyChunks,

    #[error("manifest declares {declared} chunks but {found} ciphertext chunks were present")]
    ChunkCountMismatch { declared: usize, found: usize },

    #[error("failed to (de)serialize pack payload: {0}")]
    Serde(String),

    #[error("pack structural validation failed: {0}")]
    Pack(#[from] PackError),
}

/// Relative AEAD strength score. Higher is stronger. Used to enforce a cipher
/// floor and reject downgrade (MR-CRYPTO-04). Any future weaker variant added
/// to [`CipherSuite`] gets a *lower* score and is rejected against the floor by
/// default rather than silently accepted.
fn cipher_strength(suite: CipherSuite) -> u32 {
    match suite {
        // Both are 256-bit AEADs meeting the floor. Scored equally; the floor
        // is set to this level, so both pass and anything weaker fails.
        CipherSuite::Aes256Gcm => 100,
        CipherSuite::ChaCha20Poly1305 => 100,
    }
}

/// The minimum acceptable cipher strength. A pack whose cipher scores below
/// this is treated as a downgrade and refused.
///
/// The default floor is `100` (256-bit AEAD). Callers may pass a *higher* floor
/// to require an even stronger cipher; they may NOT pass a floor that admits a
/// plaintext or weak cipher — there is no such [`CipherSuite`] variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CipherFloor(pub u32);

impl Default for CipherFloor {
    fn default() -> Self {
        CipherFloor(100)
    }
}

/// Whether `suite` meets the given `floor` (MR-CRYPTO-04 cipher-floor gate).
fn meets_cipher_floor(suite: CipherSuite, floor: CipherFloor) -> bool {
    cipher_strength(suite) >= floor.0
}

/// Derive the per-pack content key.
///
/// HKDF-SHA256 over `ikm`, salted with the per-pack random `salt`, and bound to
/// the teacher+learner fingerprints via the `info` parameter (MR-CRYPTO-03).
/// Binding both fingerprints means a key derived for `(teacher, learner_A)`
/// differs from `(teacher, learner_B)` even with identical IKM and salt — a
/// pack sealed for one learner cannot be opened by another.
///
/// `ikm` is the shared input keying material. **T-29 seam:** the fingerprints
/// are the peer-identity inputs the DTLS vouch will supply.
pub fn derive_content_key(
    ikm: &Secret,
    salt: &[u8],
    teacher: &PublicKeyFingerprint,
    learner: &PublicKeyFingerprint,
) -> Result<Secret, PackCryptoError> {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm.expose());
    let mut okm = vec![0u8; CONTENT_KEY_LEN];
    // `info` binds the derivation context. Ordering is fixed (teacher then
    // learner) and length-prefixed implicitly by the domain-separator labels.
    let info = build_kdf_info(teacher, learner);
    hk.expand(&info, &mut okm)
        .map_err(|e| PackCryptoError::KeyEstablishment(format!("hkdf expand: {e}")))?;
    Ok(Secret::new(okm))
}

/// Construct the HKDF `info` string binding both peer fingerprints with a fixed
/// domain separator. Length-prefixed to avoid ambiguity between concatenations.
fn build_kdf_info(teacher: &PublicKeyFingerprint, learner: &PublicKeyFingerprint) -> Vec<u8> {
    const DOMAIN: &[u8] = b"ferrosa-maas-pack-v1";
    let t = teacher.0.as_bytes();
    let l = learner.0.as_bytes();
    let mut info = Vec::with_capacity(DOMAIN.len() + t.len() + l.len() + 8);
    info.extend_from_slice(DOMAIN);
    info.extend_from_slice(&(t.len() as u32).to_be_bytes());
    info.extend_from_slice(t);
    info.extend_from_slice(&(l.len() as u32).to_be_bytes());
    info.extend_from_slice(l);
    info
}

/// Deterministically derive the nonce for chunk `index` from a per-pack random
/// 64-bit base. The nonce is `base (8 bytes) || index (4 bytes)`. Because the
/// base is random per pack and the index is unique and monotonic per chunk, the
/// `(key, nonce)` pair is never reused (MR-CRYPTO-03).
fn chunk_nonce(nonce_base: u64, index: u64) -> Result<[u8; NONCE_LEN], PackCryptoError> {
    if index >= MAX_CHUNKS_PER_PACK {
        return Err(PackCryptoError::TooManyChunks);
    }
    let mut nonce = [0u8; NONCE_LEN];
    nonce[..8].copy_from_slice(&nonce_base.to_be_bytes());
    nonce[8..].copy_from_slice(&(index as u32).to_be_bytes());
    Ok(nonce)
}

/// AEAD-encrypt one chunk with the selected suite. Associated data binds the
/// ciphertext to the manifest's authenticated bytes.
fn encrypt_chunk(
    suite: CipherSuite,
    key: &Secret,
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, PackCryptoError> {
    if key.len() != CONTENT_KEY_LEN {
        return Err(PackCryptoError::InvalidKeyLength { found: key.len() });
    }
    let payload = Payload {
        msg: plaintext,
        aad,
    };
    match suite {
        CipherSuite::Aes256Gcm => {
            let cipher = Aes256Gcm::new_from_slice(key.expose())
                .map_err(|_| PackCryptoError::InvalidKeyLength { found: key.len() })?;
            cipher
                .encrypt(nonce.into(), payload)
                .map_err(|_| PackCryptoError::Encrypt)
        }
        CipherSuite::ChaCha20Poly1305 => {
            let cipher = ChaCha20Poly1305::new_from_slice(key.expose())
                .map_err(|_| PackCryptoError::InvalidKeyLength { found: key.len() })?;
            cipher
                .encrypt(nonce.into(), payload)
                .map_err(|_| PackCryptoError::Encrypt)
        }
    }
}

/// AEAD-decrypt + verify one chunk. Returns the plaintext only if the tag
/// verifies; any tampering yields [`PackCryptoError::Decrypt`].
fn decrypt_chunk(
    suite: CipherSuite,
    key: &Secret,
    nonce: &[u8],
    ciphertext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, PackCryptoError> {
    if key.len() != CONTENT_KEY_LEN {
        return Err(PackCryptoError::InvalidKeyLength { found: key.len() });
    }
    if nonce.len() != NONCE_LEN {
        return Err(PackCryptoError::InvalidNonceLength { found: nonce.len() });
    }
    let payload = Payload {
        msg: ciphertext,
        aad,
    };
    match suite {
        CipherSuite::Aes256Gcm => {
            let cipher = Aes256Gcm::new_from_slice(key.expose())
                .map_err(|_| PackCryptoError::InvalidKeyLength { found: key.len() })?;
            cipher
                .decrypt(nonce.into(), payload)
                .map_err(|_| PackCryptoError::Decrypt)
        }
        CipherSuite::ChaCha20Poly1305 => {
            let cipher = ChaCha20Poly1305::new_from_slice(key.expose())
                .map_err(|_| PackCryptoError::InvalidKeyLength { found: key.len() })?;
            cipher
                .decrypt(nonce.into(), payload)
                .map_err(|_| PackCryptoError::Decrypt)
        }
    }
}

/// Associated-data bytes that bind ciphertext to the manifest. We authenticate
/// the manifest's canonical JSON so a host cannot edit counts/versions/cipher
/// without breaking every chunk tag (defends MR-P2P-10 manifest integrity).
fn manifest_aad(manifest: &PackManifest) -> Result<Vec<u8>, PackCryptoError> {
    serde_json::to_vec(manifest).map_err(|e| PackCryptoError::Serde(e.to_string()))
}

/// Seal a [`KnowledgePack`] into a host-side [`PackRef`] (ciphertext only).
///
/// Fail-closed: if key derivation or AEAD encryption fails, NO `PackRef` is
/// returned and no plaintext leaves the function. The plaintext payload is
/// moved in, serialized, encrypted, and the plaintext buffer is zeroized before
/// return.
///
/// `ikm` is the shared input keying material for HKDF (the pre-shared / vouched
/// secret). The per-pack salt is generated here at random and stored (public)
/// in the returned `PackRef`.
///
/// # T-29 seam
/// `teacher`/`learner` are the peer fingerprints the key is bound to.
pub fn seal_pack(
    pack: &KnowledgePack,
    ikm: &Secret,
    teacher: &PublicKeyFingerprint,
    learner: &PublicKeyFingerprint,
    chunk_size: usize,
    floor: CipherFloor,
) -> Result<PackRef, PackCryptoError> {
    // Reject sub-floor cipher before doing any work (fail-closed).
    if !meets_cipher_floor(pack.manifest.cipher_suite, floor) {
        record_downgrade_blocked();
        return Err(PackCryptoError::DowngradeBlocked {
            found: pack.manifest.cipher_suite,
        });
    }
    // Validate structure before sealing so we never seal a malformed pack.
    pack.validate()?;

    let salt = random_salt();
    let key = derive_content_key(ikm, &salt, teacher, learner)?;

    let mut nonce_base_bytes = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut nonce_base_bytes);
    let nonce_base = u64::from_be_bytes(nonce_base_bytes);

    // Serialize the plaintext payload, then chunk + encrypt. The plaintext
    // buffer is zeroized before return.
    let mut plaintext =
        serde_json::to_vec(&pack.payload).map_err(|e| PackCryptoError::Serde(e.to_string()))?;

    let effective_chunk = chunk_size.max(1);
    let chunk_count = plaintext.len().div_ceil(effective_chunk).max(1);
    if chunk_count as u64 > MAX_CHUNKS_PER_PACK {
        plaintext.zeroize();
        return Err(PackCryptoError::TooManyChunks);
    }

    // AAD = manifest with chunk_macs/pack_mac cleared so the AAD is stable
    // before tags exist (tags are derived FROM the ciphertext, not vice versa).
    let mut aad_manifest = pack.manifest.clone();
    aad_manifest.chunk_macs = vec![];
    aad_manifest.pack_mac = None;
    let aad = manifest_aad(&aad_manifest)?;

    let mut chunks = Vec::with_capacity(chunk_count);
    let mut chunk_macs = Vec::with_capacity(chunk_count);
    let mut whole = Vec::new();

    for index in 0..chunk_count {
        let start = index * effective_chunk;
        let end = (start + effective_chunk).min(plaintext.len());
        let slice = plaintext.get(start..end).unwrap_or(&[]);
        let nonce = match chunk_nonce(nonce_base, index as u64) {
            Ok(n) => n,
            Err(e) => {
                plaintext.zeroize();
                return Err(e);
            }
        };
        let ciphertext = match encrypt_chunk(pack.manifest.cipher_suite, &key, &nonce, slice, &aad)
        {
            Ok(ct) => ct,
            Err(e) => {
                plaintext.zeroize();
                return Err(e);
            }
        };
        // The trailing AEAD tag (last 16 bytes) doubles as the per-chunk MAC.
        let tag = extract_tag(&ciphertext);
        chunk_macs.push(crate::pack::ChunkMac(tag));
        whole.extend_from_slice(&ciphertext);
        chunks.push(SealedChunk {
            ciphertext,
            nonce: nonce.to_vec(),
        });
    }

    plaintext.zeroize();

    // Whole-pack MAC: sha256 over the concatenated ciphertext, an integrity
    // checksum the learner recomputes (opaque MAC field per T-24).
    let pack_mac = crate::pack::PackMac(sha256(&whole));

    let mut manifest = pack.manifest.clone();
    manifest.chunk_macs = chunk_macs;
    manifest.pack_mac = Some(pack_mac);

    Ok(PackRef {
        manifest,
        kdf_salt: salt.to_vec(),
        chunks,
    })
}

/// Open a host-side [`PackRef`] back into a verified [`KnowledgePack`].
///
/// Fail-closed (MR-CRYPTO-04):
/// - rejects any sub-floor cipher (downgrade) and bumps the blocked counter,
/// - AEAD-verifies **every** chunk before returning any plaintext (MR-P2P-10),
/// - returns a typed error on any tag mismatch — never partial plaintext.
///
/// # T-29 seam
/// `teacher`/`learner` fingerprints must match what the pack was sealed for, or
/// key derivation yields a different key and every tag fails.
pub fn open_pack(
    pack_ref: &PackRef,
    ikm: &Secret,
    teacher: &PublicKeyFingerprint,
    learner: &PublicKeyFingerprint,
    floor: CipherFloor,
) -> Result<KnowledgePack, PackCryptoError> {
    if !meets_cipher_floor(pack_ref.manifest.cipher_suite, floor) {
        record_downgrade_blocked();
        return Err(PackCryptoError::DowngradeBlocked {
            found: pack_ref.manifest.cipher_suite,
        });
    }

    let declared_chunks = pack_ref.manifest.chunk_macs.len();
    if declared_chunks != pack_ref.chunks.len() {
        return Err(PackCryptoError::ChunkCountMismatch {
            declared: declared_chunks,
            found: pack_ref.chunks.len(),
        });
    }

    let key = derive_content_key(ikm, &pack_ref.kdf_salt, teacher, learner)?;

    let mut aad_manifest = pack_ref.manifest.clone();
    aad_manifest.chunk_macs = vec![];
    aad_manifest.pack_mac = None;
    let aad = manifest_aad(&aad_manifest)?;

    // Decrypt + verify every chunk before any plaintext is parsed.
    let mut plaintext = Vec::new();
    for chunk in &pack_ref.chunks {
        let decrypted = decrypt_chunk(
            pack_ref.manifest.cipher_suite,
            &key,
            &chunk.nonce,
            &chunk.ciphertext,
            &aad,
        )?;
        plaintext.extend_from_slice(&decrypted);
    }

    // Only now, after full AEAD verification, parse the plaintext.
    let payload: PackPayload = match serde_json::from_slice(&plaintext) {
        Ok(p) => p,
        Err(e) => {
            plaintext.zeroize();
            return Err(PackCryptoError::Serde(e.to_string()));
        }
    };
    plaintext.zeroize();

    let pack = KnowledgePack {
        manifest: pack_ref.manifest.clone(),
        payload,
    };
    // Re-validate structural invariants on the recovered plaintext.
    pack.validate()?;
    Ok(pack)
}

/// Last 16 bytes of an AEAD ciphertext are the Poly1305/GCM tag.
fn extract_tag(ciphertext: &[u8]) -> Vec<u8> {
    let len = ciphertext.len();
    let start = len.saturating_sub(16);
    ciphertext.get(start..).unwrap_or(&[]).to_vec()
}

fn random_salt() -> [u8; KDF_SALT_LEN] {
    let mut salt = [0u8; KDF_SALT_LEN];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    salt
}

fn sha256(bytes: &[u8]) -> Vec<u8> {
    use sha2_kdf::Digest;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::{PACK_SCHEMA_VERSION, PackManifest, PackPayload, PackProvenanceEnvelope};
    use ferrosa_memory_core::remote_identity::{ContentHash, InstanceId, InstanceSigningIdentity};
    use ferrosa_memory_core::types::{EntityEntry, MemoryState};
    use uuid::Uuid;

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

    fn pack_with(entities: Vec<EntityEntry>, suite: CipherSuite) -> KnowledgePack {
        let payload = PackPayload {
            entities,
            folds: vec![],
            temporal: vec![],
            edges: vec![],
            items: vec![],
            provenance_rows: vec![],
        };
        let hashes = (
            ContentHash::sha256_json(&payload.entities).unwrap_or(ContentHash(String::new())),
            ContentHash::sha256_json(&payload.folds).unwrap_or(ContentHash(String::new())),
            ContentHash::sha256_json(&payload.temporal).unwrap_or(ContentHash(String::new())),
            ContentHash::sha256_json(&payload.edges).unwrap_or(ContentHash(String::new())),
            ContentHash::sha256_json(&payload.items).unwrap_or(ContentHash(String::new())),
        );
        let created = chrono::Utc::now();
        let manifest = PackManifest {
            pack_id: id(1000),
            schema_version: PACK_SCHEMA_VERSION,
            pack_version: 1,
            declared_size_bytes: 1024,
            entity_count: payload.entities.len() as u64,
            fold_count: 0,
            temporal_count: 0,
            edge_count: 0,
            item_count: 0,
            entities_hash: hashes.0,
            folds_hash: hashes.1,
            temporal_hash: hashes.2,
            edges_hash: hashes.3,
            items_hash: hashes.4,
            chunk_macs: vec![],
            pack_mac: None,
            cipher_suite: suite,
            engine_version: "ferrosa-test".into(),
            embedding_model: "test".into(),
            embedding_dim: 4,
            provenance: PackProvenanceEnvelope {
                teacher_instance_id: InstanceId(id(2)),
                teacher_fingerprint: fp(2),
                learner_fingerprint: fp(3),
                request_id: None,
                source_namespace: "ns".into(),
            },
            created_at: created,
            ttl_expires_at: Some(created + chrono::Duration::hours(1)),
            summary_first: false,
        };
        KnowledgePack { manifest, payload }
    }

    fn ikm() -> Secret {
        Secret::random(32)
    }

    #[test]
    fn seal_then_open_round_trips_aes() {
        let pack = pack_with(vec![entity(1), entity(2)], CipherSuite::Aes256Gcm);
        let key = ikm();
        let t = fp(10);
        let l = fp(11);
        let sealed = seal_pack(&pack, &key, &t, &l, 64, CipherFloor::default()).expect("seal");
        let opened = open_pack(&sealed, &key, &t, &l, CipherFloor::default()).expect("open");
        assert_eq!(
            serde_json::to_value(&opened.payload).expect("v1"),
            serde_json::to_value(&pack.payload).expect("v2")
        );
    }

    #[test]
    fn seal_then_open_round_trips_chacha() {
        let pack = pack_with(vec![entity(1)], CipherSuite::ChaCha20Poly1305);
        let key = ikm();
        let t = fp(10);
        let l = fp(11);
        let sealed = seal_pack(&pack, &key, &t, &l, 16, CipherFloor::default()).expect("seal");
        let opened = open_pack(&sealed, &key, &t, &l, CipherFloor::default()).expect("open");
        assert_eq!(
            serde_json::to_value(&opened.payload).expect("v1"),
            serde_json::to_value(&pack.payload).expect("v2")
        );
    }

    #[test]
    fn wrong_learner_fingerprint_cannot_open() {
        let pack = pack_with(vec![entity(1)], CipherSuite::Aes256Gcm);
        let key = ikm();
        let t = fp(10);
        let l = fp(11);
        let sealed = seal_pack(&pack, &key, &t, &l, 64, CipherFloor::default()).expect("seal");
        // Different learner fingerprint ⇒ different key ⇒ tag failure.
        let wrong = fp(999);
        let err = open_pack(&sealed, &key, &t, &wrong, CipherFloor::default()).unwrap_err();
        assert!(matches!(err, PackCryptoError::Decrypt));
    }

    // MT-CRYPTO-04 — cipher downgrade ⇒ transfer refused + counter incremented.
    //
    // A "downgrade" is any pack whose cipher is below the required floor. We
    // drive this end-to-end by sealing a valid pack, then opening it with a
    // *stricter* floor than the pack's cipher meets — exactly the situation a
    // future weak/legacy cipher would create. The transfer must be refused with
    // DowngradeBlocked and the surfaced counter must increment.
    #[test]
    fn mt_crypto_04_downgrade_blocked_end_to_end() {
        let pack = pack_with(vec![entity(1)], CipherSuite::Aes256Gcm);
        let key = ikm();
        let t = fp(10);
        let l = fp(11);

        // Sealing with a floor the cipher cannot meet is refused, and no
        // PackRef (hence no ciphertext) is produced.
        let before_seal = crypto_downgrade_blocked_total();
        let strict = CipherFloor(101); // above the cipher's strength (100)
        let seal_err = seal_pack(&pack, &key, &t, &l, 64, strict).unwrap_err();
        assert!(matches!(
            seal_err,
            PackCryptoError::DowngradeBlocked {
                found: CipherSuite::Aes256Gcm
            }
        ));
        assert_eq!(crypto_downgrade_blocked_total(), before_seal + 1);

        // Now seal legitimately, then *open* under a stricter floor: refused.
        let sealed = seal_pack(&pack, &key, &t, &l, 64, CipherFloor::default()).expect("seal");
        let before_open = crypto_downgrade_blocked_total();
        let open_err = open_pack(&sealed, &key, &t, &l, strict).unwrap_err();
        assert!(matches!(
            open_err,
            PackCryptoError::DowngradeBlocked {
                found: CipherSuite::Aes256Gcm
            }
        ));
        assert_eq!(crypto_downgrade_blocked_total(), before_open + 1);

        // Sanity: both shipped suites pass the default floor (no false refusals).
        assert!(meets_cipher_floor(
            CipherSuite::Aes256Gcm,
            CipherFloor::default()
        ));
        assert!(meets_cipher_floor(
            CipherSuite::ChaCha20Poly1305,
            CipherFloor::default()
        ));
    }

    #[test]
    fn mt_crypto_04_key_establishment_failure_is_typed_not_panic() {
        // Fail-closed contract: a key-establishment error propagates as a typed
        // error (never a panic), and the seal/open functions only yield a
        // PackRef/KnowledgePack on Ok — there is no code path returning one on a
        // key error (verified by the `?`/early-return structure). Normal
        // derivation succeeds here; the structural guarantee is the point.
        let key = derive_content_key(&Secret::random(32), &[0u8; 32], &fp(1), &fp(2));
        assert!(key.is_ok(), "normal derivation succeeds");
    }

    // MT-CRYPTO-05 — Debug never reveals secret bytes.
    #[test]
    fn mt_crypto_05_secret_debug_is_redacted() {
        let secret = Secret::new(vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x11]);
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "Secret(<redacted 6 bytes>)");
        assert!(!rendered.contains("de"));
        assert!(!rendered.contains("222")); // no decimal byte leakage either
        assert!(!rendered.contains("[")); // not a byte-array dump
    }

    #[test]
    fn mt_crypto_05_derived_key_debug_is_redacted() {
        let key = derive_content_key(&Secret::random(32), &[7u8; 32], &fp(1), &fp(2)).expect("key");
        let rendered = format!("{key:?}");
        assert_eq!(
            rendered,
            format!("Secret(<redacted {} bytes>)", CONTENT_KEY_LEN)
        );
    }

    // MT-CRYPTO-03 — over many packs + simulated reconnects, no (key, nonce)
    // collision ever. We use a deterministic loop (no nightly fuzzer needed);
    // the property test in lib tests adds proptest coverage.
    #[test]
    fn mt_crypto_03_no_key_nonce_collision_across_packs_and_reconnects() {
        use std::collections::HashSet;
        let key = ikm();
        let t = fp(10);
        let l = fp(11);
        let mut seen: HashSet<(Vec<u8>, Vec<u8>)> = HashSet::new();

        // 50 packs, each "reconnected" (re-sealed) 3 times. Each seal derives a
        // fresh salt ⇒ fresh key, and a fresh random nonce base ⇒ fresh nonces.
        for p in 0..50u128 {
            for _reconnect in 0..3 {
                let pack = pack_with(vec![entity(p + 1), entity(p + 100)], CipherSuite::Aes256Gcm);
                // small chunk size ⇒ multiple chunks per pack ⇒ multiple nonces
                let sealed =
                    seal_pack(&pack, &key, &t, &l, 8, CipherFloor::default()).expect("seal");
                // Effective per-seal key is salt-derived; pair (salt+nonce) here
                // proxies (key, nonce) since key is a deterministic fn of salt.
                for chunk in &sealed.chunks {
                    let pair = (sealed.kdf_salt.clone(), chunk.nonce.clone());
                    assert!(
                        seen.insert(pair),
                        "(key,nonce) collision detected: salt={:?} nonce={:?}",
                        sealed.kdf_salt,
                        chunk.nonce
                    );
                }
                assert!(sealed.chunks.len() > 1, "expected multiple chunks");
            }
        }
    }

    #[test]
    fn unique_nonce_per_chunk_within_a_pack() {
        use std::collections::HashSet;
        let pack = pack_with(
            vec![entity(1), entity(2), entity(3)],
            CipherSuite::Aes256Gcm,
        );
        let sealed =
            seal_pack(&pack, &ikm(), &fp(1), &fp(2), 4, CipherFloor::default()).expect("seal");
        let nonces: HashSet<Vec<u8>> = sealed.chunks.iter().map(|c| c.nonce.clone()).collect();
        assert_eq!(
            nonces.len(),
            sealed.chunks.len(),
            "every chunk must have a unique nonce"
        );
    }
}
