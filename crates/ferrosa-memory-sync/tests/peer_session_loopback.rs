//! MAAS-T-36 — full consent→bind→transfer loopback through a mock T-29 broker.
//!
//! Runs BOTH peer-session drivers concurrently against an in-process
//! [`MockBroker`] implementing the T-29 contract (server-ordered consent,
//! single-use acceptance, per-direction signal queues) and a REAL
//! ICE/DTLS/SCTP loopback connection. The teacher builds + seals a pack; the
//! learner receives, AEAD-opens with the DH-derived IKM, and applies it with
//! the channel-attested fingerprint pair.
//!
//! Marked `#[ignore]` like the transport loopback (UDP loopback,
//! timing-sensitive):
//!
//! ```text
//! cargo test -p ferrosa-memory-sync --features webrtc-transport \
//!   --test peer_session_loopback -- --ignored --nocapture
//! ```

#![cfg(feature = "webrtc-transport")]

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use ferrosa_memory_core::remote_identity::{InstanceId, InstanceSigningIdentity};
use ferrosa_memory_core::types::{EntityEntry, MemoryState};

use ferrosa_memory_sync::learner_ingest::{PackApplyStore, StagedPack};
use ferrosa_memory_sync::pack::{CipherSuite, PackProvenanceEnvelope};
use ferrosa_memory_sync::peer_session::{
    PeerSessionConfig, run_learner_session, run_teacher_session,
};
use ferrosa_memory_sync::replication::{PackBuildParams, TeacherSelection};
use ferrosa_memory_sync::signaling_client::{
    BrokerSessionView, BrokerSignal, SignalingApi, SignalingClientError,
};

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

// ---------------------------------------------------------------------------
// Mock broker (the T-29 contract, in process)
// ---------------------------------------------------------------------------

struct MockSession {
    view: BrokerSessionView,
    accepted_once: bool,
    to_teacher: VecDeque<BrokerSignal>,
    to_learner: VecDeque<BrokerSignal>,
}

#[derive(Default)]
struct MockBrokerState {
    sessions: std::collections::HashMap<Uuid, MockSession>,
}

/// One broker shared by both peers; each peer holds a handle that carries its
/// own account id (standing in for the gateway's authenticated session).
struct MockBroker {
    state: Mutex<MockBrokerState>,
}

impl MockBroker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MockBrokerState::default()),
        })
    }

    fn handle(self: &Arc<Self>, account: Uuid) -> MockBrokerHandle {
        MockBrokerHandle {
            broker: self.clone(),
            account,
        }
    }
}

struct MockBrokerHandle {
    broker: Arc<MockBroker>,
    account: Uuid,
}

fn refused(status: u16, why: &str) -> SignalingClientError {
    SignalingClientError::Refused {
        status,
        body: why.to_string(),
    }
}

impl SignalingApi for MockBrokerHandle {
    async fn offer(
        &self,
        learner_account: Uuid,
        pack_id: Uuid,
        fingerprint: &str,
    ) -> Result<Uuid, SignalingClientError> {
        let mut st = self.broker.state.lock().unwrap();
        let session_id = Uuid::new_v4();
        st.sessions.insert(
            session_id,
            MockSession {
                view: BrokerSessionView {
                    session_id,
                    phase: "offered".into(),
                    teacher_account: self.account,
                    learner_account,
                    pack_id,
                    teacher_fingerprint: fingerprint.to_string(),
                    learner_fingerprint: None,
                },
                accepted_once: false,
                to_teacher: VecDeque::new(),
                to_learner: VecDeque::new(),
            },
        );
        Ok(session_id)
    }

    async fn pending_offers(&self) -> Result<Vec<BrokerSessionView>, SignalingClientError> {
        let st = self.broker.state.lock().unwrap();
        Ok(st
            .sessions
            .values()
            .filter(|s| s.view.learner_account == self.account && s.view.phase == "offered")
            .map(|s| s.view.clone())
            .collect())
    }

    async fn accept(
        &self,
        session_id: Uuid,
        fingerprint: &str,
    ) -> Result<BrokerSessionView, SignalingClientError> {
        let mut st = self.broker.state.lock().unwrap();
        let s = st
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| refused(404, "session_not_found"))?;
        if s.view.learner_account != self.account {
            return Err(refused(404, "session_not_found"));
        }
        if s.view.phase != "offered" || s.accepted_once {
            return Err(refused(409, "invalid_transition"));
        }
        s.accepted_once = true;
        s.view.phase = "accepted".into();
        s.view.learner_fingerprint = Some(fingerprint.to_string());
        Ok(s.view.clone())
    }

    async fn session(&self, session_id: Uuid) -> Result<BrokerSessionView, SignalingClientError> {
        let st = self.broker.state.lock().unwrap();
        let s = st
            .sessions
            .get(&session_id)
            .ok_or_else(|| refused(404, "session_not_found"))?;
        if s.view.teacher_account != self.account && s.view.learner_account != self.account {
            return Err(refused(404, "session_not_found"));
        }
        Ok(s.view.clone())
    }

    async fn post_signal(
        &self,
        session_id: Uuid,
        signal: &BrokerSignal,
    ) -> Result<(), SignalingClientError> {
        let mut st = self.broker.state.lock().unwrap();
        let s = st
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| refused(404, "session_not_found"))?;
        if s.view.phase != "accepted" {
            return Err(refused(409, "invalid_transition"));
        }
        if self.account == s.view.teacher_account {
            s.to_learner.push_back(signal.clone());
        } else if self.account == s.view.learner_account {
            s.to_teacher.push_back(signal.clone());
        } else {
            return Err(refused(404, "session_not_found"));
        }
        Ok(())
    }

    async fn take_signals(
        &self,
        session_id: Uuid,
    ) -> Result<Vec<BrokerSignal>, SignalingClientError> {
        let mut st = self.broker.state.lock().unwrap();
        let s = st
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| refused(404, "session_not_found"))?;
        let queue = if self.account == s.view.teacher_account {
            &mut s.to_teacher
        } else if self.account == s.view.learner_account {
            &mut s.to_learner
        } else {
            return Err(refused(404, "session_not_found"));
        };
        Ok(queue.drain(..).collect())
    }
}

// ---------------------------------------------------------------------------
// Learner apply store
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemStore {
    applied: Mutex<std::collections::HashMap<Uuid, u64>>,
    staged_entities: Mutex<usize>,
}

impl PackApplyStore for &'static MemStore {
    async fn last_applied_version(&self, pack_id: Uuid) -> anyhow::Result<Option<u64>> {
        Ok(self.applied.lock().unwrap().get(&pack_id).copied())
    }
    async fn stage(&self, staged: &StagedPack) -> anyhow::Result<()> {
        *self.staged_entities.lock().unwrap() = staged.entities.len();
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

fn loopback_config() -> PeerSessionConfig {
    PeerSessionConfig {
        // No STUN needed on loopback; host candidates connect directly.
        stun_urls: vec![],
        accept_timeout: Duration::from_secs(20),
        connect_timeout: Duration::from_secs(20),
        transfer_timeout: Duration::from_secs(30),
        poll_interval: Duration::from_millis(50),
        allow_loopback: true,
        ..Default::default()
    }
}

fn params(teacher: &InstanceSigningIdentity, pack_id: Uuid) -> PackBuildParams {
    let created = Utc::now();
    let public = teacher.public_identity();
    PackBuildParams {
        pack_id,
        pack_version: 1,
        cipher_suite: CipherSuite::Aes256Gcm,
        engine_version: "t36-test".into(),
        embedding_model: "test-embed".into(),
        embedding_dim: 4,
        summary_first: false,
        created_at: created,
        ttl_expires_at: Some(created + chrono::Duration::hours(1)),
        provenance: PackProvenanceEnvelope {
            teacher_instance_id: public.instance_id,
            // Placeholder fingerprints — run_teacher_session overwrites them
            // with the broker-vouched pair (self-claims never survive).
            teacher_fingerprint: public.public_key_fingerprint.clone(),
            learner_fingerprint: public.public_key_fingerprint,
            request_id: None,
            source_namespace: "t36".into(),
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real ICE/DTLS/SCTP over UDP loopback; run with --ignored"]
async fn full_consent_bind_transfer_through_the_mock_broker() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let broker = MockBroker::new();
    let (teacher_account, learner_account) = (id(10), id(20));
    let teacher_api = broker.handle(teacher_account);
    let learner_api = broker.handle(learner_account);

    let teacher_id = InstanceSigningIdentity::generate(InstanceId(id(2)));
    let learner_id = InstanceSigningIdentity::generate(InstanceId(id(3)));
    let teacher_fp = teacher_id.public_identity().public_key_fingerprint.0;

    // Teacher offers through the broker (the real gateway would have vouched
    // the fingerprint against the registry + checked mutual contact here).
    let pack_id = id(1000);
    let session_id = teacher_api
        .offer(learner_account, pack_id, &teacher_fp)
        .await
        .expect("offer");

    // The learner discovers the pending offer like the CLI would.
    let pending = learner_api.pending_offers().await.expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].session_id, session_id);

    let selection = TeacherSelection {
        entities: vec![entity(id(1)), entity(id(2)), entity(id(3))],
        ..Default::default()
    };
    let mut build_params = params(&teacher_id, pack_id);
    let cfg = loopback_config();

    static STORE: once_cell_store::Lazy<MemStore> = once_cell_store::Lazy::new(MemStore::default);

    let teacher_cfg = cfg.clone();
    let teacher_task = tokio::spawn(async move {
        run_teacher_session(
            &teacher_api,
            &teacher_id,
            session_id,
            &selection,
            &mut build_params,
            &teacher_cfg,
        )
        .await
    });
    let learner_cfg = cfg.clone();
    let learner_task = tokio::spawn(async move {
        run_learner_session(
            &learner_api,
            &learner_id,
            session_id,
            &*STORE,
            id(555),
            &learner_cfg,
        )
        .await
    });

    let report = teacher_task
        .await
        .expect("teacher join")
        .expect("teacher session");
    let health = learner_task
        .await
        .expect("learner join")
        .expect("learner session");

    // The pack applied exactly once, with the entities intact.
    assert_eq!(report.dropped_edges, 0);
    assert_eq!(health.packs_applied, 1);
    assert_eq!(health.packs_failed, 0);
    assert_eq!(
        STORE.applied.lock().unwrap().get(&pack_id).copied(),
        Some(1)
    );
    assert_eq!(*STORE.staged_entities.lock().unwrap(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real ICE/DTLS/SCTP over UDP loopback; run with --ignored"]
async fn replayed_accept_is_refused_by_the_broker_contract() {
    let broker = MockBroker::new();
    let (teacher_account, learner_account) = (id(10), id(20));
    let teacher_api = broker.handle(teacher_account);
    let learner_api = broker.handle(learner_account);
    let session_id = teacher_api
        .offer(learner_account, id(1), "fp")
        .await
        .expect("offer");
    learner_api.accept(session_id, "lfp").await.expect("accept");
    let err = learner_api.accept(session_id, "lfp").await.unwrap_err();
    match err {
        SignalingClientError::Refused { status, .. } => assert_eq!(status, 409),
        other => panic!("expected Refused, got {other:?}"),
    }
}

/// Minimal `Lazy` so the `&'static MemStore` apply-store borrow works without
/// adding a once_cell dependency.
mod once_cell_store {
    pub struct Lazy<T> {
        cell: std::sync::OnceLock<T>,
        init: fn() -> T,
    }
    impl<T> Lazy<T> {
        pub const fn new(init: fn() -> T) -> Self {
            Self {
                cell: std::sync::OnceLock::new(),
                init,
            }
        }
    }
    impl<T> std::ops::Deref for Lazy<T> {
        type Target = T;
        fn deref(&self) -> &T {
            self.cell.get_or_init(self.init)
        }
    }
}
