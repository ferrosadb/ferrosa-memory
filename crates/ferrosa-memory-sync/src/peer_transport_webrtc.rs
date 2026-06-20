//! MAAS-T-25b — concrete `webrtc-rs` binding for the peer-transport seam.
//!
//! This module is compiled only under the `webrtc-transport` feature. It wires
//! the dependency-free transport core in [`super`] to a real `webrtc-rs`
//! `RTCDataChannel`:
//!
//! - [`RtcDataChannel`] implements [`super::DataChannel`] over an
//!   `Arc<RTCDataChannel>` — `ready_state()` → `is_open`,
//!   `buffered_amount` + `on_buffered_amount_low` → `wait_buffered_below`,
//!   `send(&Bytes)` → `send`. A [`super::PeerTransport`] built on it inherits
//!   the MR-P2P-03/04/05/06 guarantees unchanged.
//! - [`PackReceiver`] is the inbound half: it feeds each `on_message` frame
//!   through [`super::decode_frame`] → bounded [`super::ChunkAssembler`] →
//!   `PackRef` deserialize → [`crate::learner_ingest::ingest_pack`] (which
//!   AEAD-verifies before parse — T-33/T-27). Every outcome is recorded in a
//!   [`ReceiverHealth`] snapshot so failures are observable, never silent.
//!
//! # Wire protocol
//!
//! A sealed [`PackRef`] is serialized once and split into ≤`max_frame_payload`
//! byte chunks ([`pack_ref_to_wire_chunks`]); each chunk is framed and sent by
//! the transport core. The receiver reassembles the bytes (bounded), then
//! deserializes and ingests. The transport-layer chunking is independent of the
//! AEAD `SealedChunk`s *inside* the `PackRef` — framing is pure MTU sizing and
//! carries ciphertext only.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::indexing_slicing)]
#![deny(clippy::panic)]
#![deny(clippy::await_holding_lock)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

use std::sync::{Arc, Mutex as StdMutex};

use bytes::Bytes;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

use super::{
    AssemblerLimits, ChunkAssembler, DataChannel, PeerTransport, TransportError, decode_frame,
};
use crate::learner_ingest::{ChannelAttestation, PackApplyStore, ingest_pack};
use crate::pack::PackRef;
use crate::pack_crypto::{CipherFloor, Secret};

// ─────────────────────────────────────────────────────────────────────────
// Send side: RTCDataChannel → DataChannel
// ─────────────────────────────────────────────────────────────────────────

/// Concrete [`DataChannel`] backed by a `webrtc-rs` `RTCDataChannel`.
///
/// Construct with [`RtcDataChannel::attach`], which registers the
/// buffered-amount-low callback used for backpressure.
#[derive(Clone)]
pub struct RtcDataChannel {
    inner: Arc<RTCDataChannel>,
    /// Notified by the SCTP `on_buffered_amount_low` callback.
    low: Arc<Notify>,
    /// Hard cap on `wait_buffered_below` iterations (Power-of-10 R2: bounded).
    max_waits: u32,
}

impl RtcDataChannel {
    /// Attach to an open-or-opening `RTCDataChannel`, arming SCTP backpressure.
    ///
    /// `low_water` is the buffered-amount threshold below which the channel is
    /// considered drained; it should equal the [`super::SendLimits::max_buffered_bytes`]
    /// the [`PeerTransport`] is configured with, so the low-water callback fires
    /// exactly when [`wait_buffered_below`](DataChannel::wait_buffered_below)
    /// would unblock.
    pub async fn attach(inner: Arc<RTCDataChannel>, low_water: usize) -> Self {
        let low = Arc::new(Notify::new());
        inner.set_buffered_amount_low_threshold(low_water).await;
        let notify = low.clone();
        inner
            .on_buffered_amount_low(Box::new(move || {
                let notify = notify.clone();
                Box::pin(async move {
                    notify.notify_waiters();
                })
            }))
            .await;
        Self {
            inner,
            low,
            max_waits: 10_000,
        }
    }

    /// The wrapped channel, e.g. to query `label()` or `close()`.
    pub fn inner(&self) -> &Arc<RTCDataChannel> {
        &self.inner
    }
}

impl DataChannel for RtcDataChannel {
    fn is_open(&self) -> bool {
        self.inner.ready_state() == RTCDataChannelState::Open
    }

    async fn wait_buffered_below(&self, threshold: usize) {
        // Bounded wait (R2): re-check buffered_amount, sleeping on the low-water
        // notification or a short timeout (defends against a missed wakeup).
        let mut waits = 0u32;
        while self.inner.buffered_amount().await > threshold {
            tokio::select! {
                _ = self.low.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
            waits += 1;
            if waits >= self.max_waits {
                tracing::warn!(
                    threshold,
                    "wait_buffered_below hit iteration cap; proceeding (best-effort backpressure)"
                );
                return;
            }
        }
    }

    async fn send(&self, frame: &[u8]) -> Result<(), TransportError> {
        let bytes = Bytes::copy_from_slice(frame);
        self.inner
            .send(&bytes)
            .await
            .map(|_sent| ())
            .map_err(|e| TransportError::ChannelSend(e.to_string()))
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Wire helpers (producer side)
// ─────────────────────────────────────────────────────────────────────────

/// Errors producing wire chunks from a [`PackRef`].
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("failed to serialize PackRef for the wire: {0}")]
    Serialize(String),
    #[error("max_frame_payload must be non-zero")]
    ZeroChunkSize,
}

/// Serialize a sealed [`PackRef`] and split it into ≤`max_frame_payload`-byte
/// chunks ready for [`PeerTransport::send_pack`]. Carries ciphertext + metadata
/// only (the `PackRef` itself holds no plaintext or key).
pub fn pack_ref_to_wire_chunks(
    pack_ref: &PackRef,
    max_frame_payload: usize,
) -> Result<Vec<Vec<u8>>, WireError> {
    if max_frame_payload == 0 {
        return Err(WireError::ZeroChunkSize);
    }
    let bytes = serde_json::to_vec(pack_ref).map_err(|e| WireError::Serialize(e.to_string()))?;
    Ok(bytes
        .chunks(max_frame_payload)
        .map(|c| c.to_vec())
        .collect())
}

/// Errors from [`send_pack_ref`].
#[derive(Debug, thiserror::Error)]
pub enum SendPackError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// Serialize, chunk, and send a sealed [`PackRef`] over a [`PeerTransport`].
/// Inherits the transport's ordering, bounds, backpressure, and
/// cancellation-safety guarantees.
pub async fn send_pack_ref<C: DataChannel>(
    transport: &mut PeerTransport<C>,
    pack_ref: &PackRef,
    max_frame_payload: usize,
) -> Result<(), SendPackError> {
    let chunks = pack_ref_to_wire_chunks(pack_ref, max_frame_payload)?;
    transport.send_pack(&chunks).await?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────
// Receive side: on_message → assemble → ingest
// ─────────────────────────────────────────────────────────────────────────

/// Observable health of a [`PackReceiver`]. Every frame and pack outcome is
/// counted, and the most recent error is retained so a failing receive path is
/// visible in a health check rather than silently dropping data.
#[derive(Debug, Default, Clone)]
pub struct ReceiverHealth {
    /// Frames handed to the receiver.
    pub frames_received: u64,
    /// Frames rejected at decode or assembly (bounded/ malformed input).
    pub frames_rejected: u64,
    /// Packs fully reassembled and ingested.
    pub packs_applied: u64,
    /// Packs that reassembled but failed deserialize/AEAD/ingest.
    pub packs_failed: u64,
    /// Most recent error message, if any.
    pub last_error: Option<String>,
}

/// Inbound pack receiver: bounded reassembly + AEAD-verified ingest.
///
/// Generic over the learner [`PackApplyStore`]. Shareable via `Arc` and
/// installable as an `RTCDataChannel` `on_message` handler ([`install`]).
///
/// [`install`]: PackReceiver::install
pub struct PackReceiver<S: PackApplyStore> {
    assembler: AsyncMutex<ChunkAssembler>,
    limits: AssemblerLimits,
    max_frame_payload: usize,
    store: S,
    ikm: Secret,
    attestation: ChannelAttestation,
    floor: CipherFloor,
    health: StdMutex<ReceiverHealth>,
}

impl<S: PackApplyStore> PackReceiver<S> {
    /// Create a receiver. `ikm` is the shared input keying material the pack was
    /// sealed with; `attestation` carries the channel-vouched peer fingerprints
    /// (T-29) the ingest path trusts over the pack's self-claim.
    pub fn new(
        store: S,
        ikm: Secret,
        attestation: ChannelAttestation,
        floor: CipherFloor,
        limits: AssemblerLimits,
        max_frame_payload: usize,
    ) -> Self {
        Self {
            assembler: AsyncMutex::new(ChunkAssembler::new(limits)),
            limits,
            max_frame_payload,
            store,
            ikm,
            attestation,
            floor,
            health: StdMutex::new(ReceiverHealth::default()),
        }
    }

    /// Snapshot the current health counters.
    pub fn health(&self) -> ReceiverHealth {
        match self.health.lock() {
            Ok(h) => h.clone(),
            // A poisoned health lock is itself a fault worth surfacing.
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn record(&self, f: impl FnOnce(&mut ReceiverHealth)) {
        if let Ok(mut h) = self.health.lock() {
            f(&mut h);
        }
    }

    /// Handle one framed wire message: decode → bounded-assemble → (on
    /// completion) deserialize + ingest. All failures are recorded in health and
    /// drop the offending input; a tampered or oversized peer can never apply
    /// memory or exhaust it.
    pub async fn handle_frame_bytes(&self, raw: &[u8]) {
        self.record(|h| h.frames_received += 1);

        let frame = match decode_frame(raw, self.max_frame_payload) {
            Ok(f) => f,
            Err(e) => {
                self.record(|h| {
                    h.frames_rejected += 1;
                    h.last_error = Some(format!("decode: {e}"));
                });
                return;
            }
        };

        // Bounded reassembly. The async lock guard is held only for synchronous
        // assembler ops and dropped before any ingest await (no lock across
        // await — MR-P2P transport actor rule).
        let assembled: Option<Vec<u8>> = {
            let mut asm = self.assembler.lock().await;
            if let Err(e) = asm.accept(frame) {
                drop(asm);
                self.record(|h| {
                    h.frames_rejected += 1;
                    h.last_error = Some(format!("assemble: {e}"));
                });
                return;
            }
            if asm.is_complete() {
                let done = std::mem::replace(&mut *asm, ChunkAssembler::new(self.limits));
                match done.into_assembled() {
                    Ok(bytes) => Some(bytes),
                    Err(e) => {
                        self.record(|h| {
                            h.packs_failed += 1;
                            h.last_error = Some(format!("reassemble: {e}"));
                        });
                        None
                    }
                }
            } else {
                None
            }
        };

        let Some(bytes) = assembled else {
            return;
        };

        let pack_ref: PackRef = match serde_json::from_slice(&bytes) {
            Ok(p) => p,
            Err(e) => {
                self.record(|h| {
                    h.packs_failed += 1;
                    h.last_error = Some(format!("deserialize PackRef: {e}"));
                });
                return;
            }
        };

        // ingest_pack AEAD-verifies before parse and cross-checks the attested
        // peers (T-33/T-27). It is the only place a pack becomes visible.
        match ingest_pack(
            &self.store,
            &pack_ref,
            &self.ikm,
            &self.attestation,
            self.floor,
        )
        .await
        {
            Ok(_outcome) => self.record(|h| h.packs_applied += 1),
            Err(e) => self.record(|h| {
                h.packs_failed += 1;
                h.last_error = Some(format!("ingest: {e}"));
            }),
        }
    }
}

impl<S: PackApplyStore + Send + Sync + 'static> PackReceiver<S> {
    /// Install this receiver as the `on_message` handler of a data channel.
    /// Each inbound message is routed through [`handle_frame_bytes`].
    ///
    /// [`handle_frame_bytes`]: PackReceiver::handle_frame_bytes
    pub fn install(self: Arc<Self>, dc: &Arc<RTCDataChannel>) {
        let recv = self;
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let recv = recv.clone();
            Box::pin(async move {
                recv.handle_frame_bytes(&msg.data).await;
            })
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use chrono::Utc;
    use ferrosa_memory_core::remote_identity::{
        InstanceId, InstanceSigningIdentity, PublicKeyFingerprint,
    };
    use ferrosa_memory_core::types::{EntityEntry, MemoryState};
    use uuid::Uuid;

    use crate::learner_ingest::StagedPack;
    use crate::pack::{CipherSuite, PackProvenanceEnvelope};
    use crate::peer_transport::SendLimits;
    use crate::replication::{PackBuildParams, TeacherSelection, build_and_seal};

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn fingerprint(seed: u128) -> PublicKeyFingerprint {
        InstanceSigningIdentity::generate(InstanceId(id(seed)))
            .public_identity()
            .public_key_fingerprint
    }

    fn entity(entity_id: Uuid) -> EntityEntry {
        EntityEntry {
            tenant_id: id(99),
            entity_id,
            session_id: id(1),
            entity_name: format!("e{entity_id}"),
            entity_type: "concept".into(),
            context_snippet: "ctx".into(),
            entity_embedding: None,
            confidence: 0.9,
            state: MemoryState::Active,
            created_at: Utc::now(),
            ..Default::default()
        }
    }

    /// Minimal in-memory apply store.
    #[derive(Default)]
    struct MemStore {
        applied: StdMutex<HashMap<Uuid, u64>>,
        flips: StdMutex<u64>,
    }

    impl PackApplyStore for MemStore {
        async fn last_applied_version(&self, pack_id: Uuid) -> anyhow::Result<Option<u64>> {
            Ok(self.applied.lock().unwrap().get(&pack_id).copied())
        }
        async fn stage(&self, _staged: &StagedPack) -> anyhow::Result<()> {
            Ok(())
        }
        async fn flip(&self, staged: &StagedPack) -> anyhow::Result<()> {
            self.applied
                .lock()
                .unwrap()
                .insert(staged.pack_id, staged.pack_version);
            *self.flips.lock().unwrap() += 1;
            Ok(())
        }
    }

    fn envelope() -> PackProvenanceEnvelope {
        PackProvenanceEnvelope {
            teacher_instance_id: InstanceId(id(2)),
            teacher_fingerprint: fingerprint(2),
            learner_fingerprint: fingerprint(3),
            request_id: None,
            source_namespace: "ns".into(),
        }
    }

    /// Build + seal a real pack and return (PackRef, ikm bytes, attestation).
    fn sealed_pack() -> (PackRef, Vec<u8>, ChannelAttestation) {
        let env = envelope();
        let created = Utc::now();
        let params = PackBuildParams {
            pack_id: id(1000),
            pack_version: 3,
            cipher_suite: CipherSuite::Aes256Gcm,
            engine_version: "ferrosa-test".into(),
            embedding_model: "test-embed".into(),
            embedding_dim: 4,
            summary_first: false,
            created_at: created,
            ttl_expires_at: Some(created + chrono::Duration::hours(1)),
            provenance: env.clone(),
        };
        let selection = TeacherSelection {
            entities: vec![entity(id(1)), entity(id(2))],
            ..Default::default()
        };
        let ikm_bytes = vec![7u8; 32];
        let (pack_ref, _report) = build_and_seal(
            &selection,
            &params,
            &Secret::new(ikm_bytes.clone()),
            64,
            CipherFloor::default(),
        )
        .expect("seal");
        let attestation = ChannelAttestation {
            attested_teacher: env.teacher_fingerprint.clone(),
            attested_learner: env.learner_fingerprint.clone(),
            remote_id: id(555),
        };
        (pack_ref, ikm_bytes, attestation)
    }

    fn receiver(ikm_bytes: Vec<u8>, attestation: ChannelAttestation) -> PackReceiver<MemStore> {
        PackReceiver::new(
            MemStore::default(),
            Secret::new(ikm_bytes),
            attestation,
            CipherFloor::default(),
            AssemblerLimits {
                max_chunks: 4096,
                max_total_bytes: 8 * 1024 * 1024,
                max_frame_payload: 16 * 1024,
            },
            512,
        )
    }

    // Full path: seal → wire-chunk → frame → decode → assemble → ingest.
    #[tokio::test]
    async fn full_path_applies_pack() {
        let (pack_ref, ikm, att) = sealed_pack();
        let recv = receiver(ikm, att);

        // Producer: serialize + chunk + frame exactly as the transport would.
        let payload_chunks = pack_ref_to_wire_chunks(&pack_ref, 512).expect("chunks");
        assert!(payload_chunks.len() > 1, "pack should span multiple frames");
        let total = payload_chunks.len() as u32;
        for (i, payload) in payload_chunks.iter().enumerate() {
            let framed = super::super::encode_frame(i as u32, total, payload, 16 * 1024).unwrap();
            recv.handle_frame_bytes(&framed).await;
        }

        let h = recv.health();
        assert_eq!(h.packs_applied, 1, "pack applied exactly once: {h:?}");
        assert_eq!(h.packs_failed, 0);
        assert_eq!(*recv.store.flips.lock().unwrap(), 1);
        assert_eq!(
            recv.store.applied.lock().unwrap().get(&id(1000)).copied(),
            Some(3)
        );
    }

    // Tampering a ciphertext byte must fail AEAD inside ingest → no apply.
    #[tokio::test]
    async fn tampered_ciphertext_does_not_apply() {
        let (mut pack_ref, ikm, att) = sealed_pack();
        // Flip a byte of the first sealed chunk's ciphertext.
        pack_ref.chunks[0].ciphertext[0] ^= 0xff;
        let recv = receiver(ikm, att);

        let payload_chunks = pack_ref_to_wire_chunks(&pack_ref, 512).expect("chunks");
        let total = payload_chunks.len() as u32;
        for (i, payload) in payload_chunks.iter().enumerate() {
            let framed = super::super::encode_frame(i as u32, total, payload, 16 * 1024).unwrap();
            recv.handle_frame_bytes(&framed).await;
        }

        let h = recv.health();
        assert_eq!(h.packs_applied, 0, "tampered pack must not apply");
        assert_eq!(h.packs_failed, 1);
        assert!(h.last_error.unwrap().contains("ingest"));
        assert_eq!(*recv.store.flips.lock().unwrap(), 0);
    }

    // An oversized declared total is rejected by the bounded assembler; memory
    // stays bounded and nothing applies.
    #[tokio::test]
    async fn oversized_total_rejected_bounded() {
        let (_pack_ref, ikm, att) = sealed_pack();
        let recv = PackReceiver::new(
            MemStore::default(),
            Secret::new(ikm),
            att,
            CipherFloor::default(),
            AssemblerLimits {
                max_chunks: 4, // tiny cap
                max_total_bytes: 64,
                max_frame_payload: 64,
            },
            512,
        );
        // Frame declares total = 9 chunks, above the assembler's max_chunks (4).
        let framed = super::super::encode_frame(0, 9, b"some-bytes", 16 * 1024).unwrap();
        recv.handle_frame_bytes(&framed).await;

        let h = recv.health();
        assert_eq!(h.frames_received, 1);
        assert_eq!(h.frames_rejected, 1);
        assert_eq!(h.packs_applied, 0);
        assert!(h.last_error.unwrap().contains("assemble"));
    }

    // A malformed frame (bad magic) is dropped, not panicked, and never applies.
    #[tokio::test]
    async fn malformed_frame_dropped() {
        let (_p, ikm, att) = sealed_pack();
        let recv = receiver(ikm, att);
        recv.handle_frame_bytes(b"not a frame at all").await;
        let h = recv.health();
        assert_eq!(h.frames_rejected, 1);
        assert_eq!(h.packs_applied, 0);
        assert!(h.last_error.unwrap().contains("decode"));
    }

    // SendLimits/AssemblerLimits are re-exported sanity (compile-time wiring).
    #[test]
    fn limit_types_wire_together() {
        let _ = SendLimits {
            max_buffered_bytes: 1024,
            max_frame_payload: 512,
            max_chunks: 16,
        };
    }
}
