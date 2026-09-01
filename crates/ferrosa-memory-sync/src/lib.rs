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
//! - [`replication`] — MAAS-T-26: teacher-side selective pack build + emit
//!   (exact selection, no neighbour bleed, summary-first, provenance on build).
//! - [`peer_transport`] — MAAS-T-25: bounded, backpressured, cancellation-safe
//!   WebRTC pack transport over a [`peer_transport::DataChannel`] seam.
//!
//! ## T-29 seam
//!
//! Key derivation binds to **two [`ferrosa_memory_core::remote_identity::PublicKeyFingerprint`]
//! values** (teacher + learner). MAAS-T-29 (DTLS-vouched peer identity, in
//! ferrosa-dbaas) will supply those fingerprints through the existing
//! [`pack_crypto::derive_content_key`] / [`learner_ingest::ChannelAttestation`]
//! interfaces. The fingerprint binding is real today; only the *source* (DTLS
//! vouch) is deferred.

pub mod artifact_view;
pub mod codex_runtime;
#[cfg(feature = "webrtc-transport")]
pub mod control_session;
pub mod coordinator_client;
pub mod control_frame;
pub mod coordinator_command;
pub mod device_request;
/// The control-listener runtime, so every binary hosting one shares it.
#[cfg(feature = "webrtc-transport")]
pub mod harness_state;
pub mod knowledge_view;
pub mod learner_ingest;
pub mod listener;
pub mod memory_view;
pub mod pack;
pub mod pack_crypto;
pub mod peer_cli;
#[cfg(feature = "webrtc-transport")]
pub mod peer_session;
pub mod peer_transport;
pub mod replication;
pub mod rules_view;
pub mod session_config;
/// Runs a configured session and carries its text both ways.
pub mod session_runtime;
/// Named session configurations, owned by the machine.
#[cfg(feature = "webrtc-transport")]
/// The wire surface for configured sessions.
pub mod shell_extension;
#[cfg(feature = "webrtc-transport")]
pub mod signaling_client;
pub mod task_board;
