//! # ferrosa-memory-sync
//!
//! Memory replication CLI **and** the MaaS teacher/learner P2P transfer layer.
//!
//! The binary (`main.rs`) replicates memories between two Ferrosa clusters.
//! This library hosts the security-critical P2P transfer modules:
//!
//! - [`pack`] — MAAS-T-24: versioned [`pack::KnowledgePack`] schema + manifest,
//!   and the host-side ciphertext-only [`pack::PackRef`].
//! - [`pack_crypto`] — MAAS-T-33: end-to-end AEAD sealing/opening, fail-closed,
//!   with per-pack HKDF keys bound to teacher+learner fingerprints and unique
//!   per-chunk nonces.
//! - [`learner_ingest`] — MAAS-T-27: verify → atomic+idempotent apply →
//!   provenance + TTL.
//!
//! ## T-29 seam
//!
//! Key derivation binds to **two [`ferrosa_memory_core::remote_identity::PublicKeyFingerprint`]
//! values** (teacher + learner). MAAS-T-29 (DTLS-vouched peer identity, in
//! ferrosa-dbaas) will supply those fingerprints through the existing
//! [`pack_crypto::derive_content_key`] / [`learner_ingest::ChannelAttestation`]
//! interfaces. The fingerprint binding is real today; only the *source* (DTLS
//! vouch) is deferred.

pub mod learner_ingest;
pub mod pack;
pub mod pack_crypto;
