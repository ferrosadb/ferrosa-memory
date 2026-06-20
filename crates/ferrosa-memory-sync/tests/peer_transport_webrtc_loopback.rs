//! MAAS-T-25b — loopback integration test for the concrete `webrtc-rs` path.
//!
//! Stands up two in-process `RTCPeerConnection`s and performs **real**
//! ICE/DTLS/SCTP over UDP loopback: the offerer seals a pack and sends it
//! through [`RtcDataChannel`] + [`PeerTransport`]; the answerer's
//! [`PackReceiver`] reassembles, AEAD-opens, and ingests it.
//!
//! Marked `#[ignore]` — it needs UDP loopback and is timing-sensitive, so it is
//! not part of the default gate. Run it explicitly:
//!
//! ```text
//! cargo test -p ferrosa-memory-sync --features webrtc-transport \
//!   --test peer_transport_webrtc_loopback -- --ignored --nocapture
//! ```

#![cfg(feature = "webrtc-transport")]

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use ferrosa_memory_core::remote_identity::{
    InstanceId, InstanceSigningIdentity, PublicKeyFingerprint,
};
use ferrosa_memory_core::types::{EntityEntry, MemoryState};
use uuid::Uuid;

use ferrosa_memory_sync::learner_ingest::{ChannelAttestation, PackApplyStore, StagedPack};
use ferrosa_memory_sync::pack::{CipherSuite, PackProvenanceEnvelope, PackRef};
use ferrosa_memory_sync::pack_crypto::{CipherFloor, Secret};
use ferrosa_memory_sync::peer_transport::webrtc::{PackReceiver, RtcDataChannel, send_pack_ref};
use ferrosa_memory_sync::peer_transport::{AssemblerLimits, PeerTransport, SendLimits};
use ferrosa_memory_sync::replication::{PackBuildParams, TeacherSelection, build_and_seal};

use tokio::sync::Notify;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;

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

#[derive(Default)]
struct MemStore {
    applied: std::sync::Mutex<std::collections::HashMap<Uuid, u64>>,
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
        Ok(())
    }
}

/// Build + seal a real pack; return (PackRef, ikm bytes, attestation).
fn sealed_pack() -> (PackRef, Vec<u8>, ChannelAttestation) {
    let env = PackProvenanceEnvelope {
        teacher_instance_id: InstanceId(id(2)),
        teacher_fingerprint: fingerprint(2),
        learner_fingerprint: fingerprint(3),
        request_id: None,
        source_namespace: "ns".into(),
    };
    let created = Utc::now();
    let params = PackBuildParams {
        pack_id: id(1000),
        pack_version: 5,
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
        entities: vec![entity(id(1)), entity(id(2)), entity(id(3))],
        ..Default::default()
    };
    let ikm = vec![9u8; 32];
    let (pack_ref, _report) = build_and_seal(
        &selection,
        &params,
        &Secret::new(ikm.clone()),
        256,
        CipherFloor::default(),
    )
    .expect("seal");
    let attestation = ChannelAttestation {
        attested_teacher: env.teacher_fingerprint.clone(),
        attested_learner: env.learner_fingerprint.clone(),
        remote_id: id(555),
    };
    (pack_ref, ikm, attestation)
}

fn build_api() -> webrtc::api::API {
    use webrtc::api::setting_engine::SettingEngine;
    use webrtc::ice::mdns::MulticastDnsMode;
    use webrtc::ice::network_type::NetworkType;

    let mut media = MediaEngine::default();
    let registry =
        register_default_interceptors(Registry::new(), &mut media).expect("interceptors");
    // Disable mDNS so host candidates are real loopback/LAN IPs (mDNS `.local`
    // candidates don't resolve in-process), and pin to UDP4 for determinism.
    let mut se = SettingEngine::default();
    se.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
    se.set_network_types(vec![NetworkType::Udp4]);
    // Same-machine: gather 127.0.0.1 candidates so ICE checks can complete even
    // when the host has no usable routable interface in the test sandbox.
    se.set_include_loopback_candidate(true);
    APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(se)
        .build()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real ICE/DTLS/SCTP over UDP loopback; run with --ignored"]
async fn loopback_full_pack_send_assemble_open_ingest() {
    // rustls 0.23 needs an explicit process-global CryptoProvider when both
    // ring and aws-lc-rs are present in the dependency tree (they are here).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let (pack_ref, ikm, attestation) = sealed_pack();

    let api = build_api();
    let config = RTCConfiguration::default();
    let pc_off = Arc::new(
        api.new_peer_connection(config.clone())
            .await
            .expect("off pc"),
    );
    let pc_answerer = Arc::new(api.new_peer_connection(config).await.expect("answerer pc"));

    // Answerer: install a PackReceiver on the inbound data channel.
    let receiver = Arc::new(PackReceiver::new(
        MemStore::default(),
        Secret::new(ikm),
        attestation,
        CipherFloor::default(),
        AssemblerLimits {
            max_chunks: 4096,
            max_total_bytes: 8 * 1024 * 1024,
            max_frame_payload: 16 * 1024,
        },
        16 * 1024,
    ));
    let recv_for_cb = receiver.clone();
    pc_answerer.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let recv = recv_for_cb.clone();
        Box::pin(async move {
            recv.install(&dc);
        })
    }));

    // Offerer: create the data channel and wait for it to open.
    let dc = pc_off
        .create_data_channel("pack", None)
        .await
        .expect("create dc");
    let opened = Arc::new(Notify::new());
    let opened_sig = opened.clone();
    dc.on_open(Box::new(move || {
        let opened_sig = opened_sig.clone();
        Box::pin(async move {
            opened_sig.notify_waiters();
        })
    }));

    // SDP offer/answer dance — NON-trickle: wait for ICE gathering to complete
    // so all host candidates are embedded in the SDP (robust on loopback; avoids
    // trickle-candidate ordering races).
    let mut gather_off = pc_off.gathering_complete_promise().await;
    let offer = pc_off.create_offer(None).await.expect("offer");
    pc_off.set_local_description(offer).await.expect("off sld");
    let _ = gather_off.recv().await;
    let local_off = pc_off.local_description().await.expect("off local desc");
    pc_answerer
        .set_remote_description(local_off)
        .await
        .expect("answerer srd");

    let mut gather_answerer = pc_answerer.gathering_complete_promise().await;
    let answer = pc_answerer.create_answer(None).await.expect("answer");
    pc_answerer
        .set_local_description(answer)
        .await
        .expect("answerer sld");
    let _ = gather_answerer.recv().await;
    let local_answerer = pc_answerer
        .local_description()
        .await
        .expect("answerer local desc");
    pc_off
        .set_remote_description(local_answerer)
        .await
        .expect("off srd");

    // Wait for the channel to open (bounded).
    tokio::time::timeout(Duration::from_secs(15), opened.notified())
        .await
        .expect("data channel opened");

    // Send the sealed pack over the real channel through the transport core.
    let max_frame_payload = 16 * 1024usize;
    let rtc = RtcDataChannel::attach(dc.clone(), 256 * 1024).await;
    let mut transport = PeerTransport::new(
        rtc,
        SendLimits {
            max_buffered_bytes: 256 * 1024,
            max_frame_payload,
            max_chunks: 8192,
        },
    );
    transport.mark_open().expect("mark open");
    send_pack_ref(&mut transport, &pack_ref, max_frame_payload)
        .await
        .expect("send pack");

    // Poll the receiver until it applies the pack (bounded).
    let mut applied = false;
    for _ in 0..100 {
        if receiver.health().packs_applied == 1 {
            applied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let h = receiver.health();
    assert!(
        applied,
        "receiver should apply exactly one pack; health={h:?}"
    );
    assert_eq!(h.packs_failed, 0, "no failures expected; health={h:?}");

    let _ = pc_off.close().await;
    let _ = pc_answerer.close().await;
}
