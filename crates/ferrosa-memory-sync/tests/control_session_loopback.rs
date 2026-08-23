//! Real loopback proof for the separate signed Ferrosa mobile control channel.

#![cfg(feature = "webrtc-transport")]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferrosa_memory_core::remote_identity::{InstanceId, InstanceSigningIdentity};
use ferrosa_memory_sync::control_session::{
    ControlSessionConfig, run_control_controller_session, run_control_server_session,
};
use ferrosa_memory_sync::signaling_client::{
    BrokerSignal, ControlBrokerSessionView, ControlSignalingApi, SignalingClientError,
};
use uuid::Uuid;

struct Session {
    view: ControlBrokerSessionView,
    to_controller: VecDeque<BrokerSignal>,
    to_server: VecDeque<BrokerSignal>,
}

#[derive(Default)]
struct BrokerState {
    sessions: HashMap<Uuid, Session>,
}

#[derive(Default)]
struct Broker {
    state: Mutex<BrokerState>,
}

impl Broker {
    fn handle(self: &Arc<Self>, account_id: Uuid, device_id: Uuid) -> BrokerHandle {
        BrokerHandle {
            broker: self.clone(),
            account_id,
            device_id,
        }
    }
}

struct BrokerHandle {
    broker: Arc<Broker>,
    account_id: Uuid,
    device_id: Uuid,
}

fn refused(status: u16, body: &str) -> SignalingClientError {
    SignalingClientError::Refused {
        status,
        body: body.to_owned(),
    }
}

impl ControlSignalingApi for BrokerHandle {
    async fn control_offer(
        &self,
        server_device_id: Uuid,
        controller_fingerprint: &str,
    ) -> Result<Uuid, SignalingClientError> {
        let session_id = Uuid::new_v4();
        let session = Session {
            view: ControlBrokerSessionView {
                session_id,
                phase: "offered".to_owned(),
                account_id: self.account_id,
                controller_device_id: self.device_id,
                controller_fingerprint: controller_fingerprint.to_owned(),
                server_device_id,
                server_fingerprint: None,
            },
            to_controller: VecDeque::new(),
            to_server: VecDeque::new(),
        };
        self.broker
            .state
            .lock()
            .expect("state")
            .sessions
            .insert(session_id, session);
        Ok(session_id)
    }

    async fn control_pending_offers(
        &self,
        _fingerprint: &str,
    ) -> Result<Vec<ControlBrokerSessionView>, SignalingClientError> {
        let state = self.broker.state.lock().expect("state");
        Ok(state
            .sessions
            .values()
            .filter(|session| {
                session.view.server_device_id == self.device_id && session.view.phase == "offered"
            })
            .map(|session| session.view.clone())
            .collect())
    }

    async fn control_accept(
        &self,
        session_id: Uuid,
        fingerprint: &str,
    ) -> Result<ControlBrokerSessionView, SignalingClientError> {
        let mut state = self.broker.state.lock().expect("state");
        let session = state
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| refused(404, "session_not_found"))?;
        if session.view.server_device_id != self.device_id || session.view.phase != "offered" {
            return Err(refused(409, "invalid_transition"));
        }
        session.view.phase = "accepted".to_owned();
        session.view.server_fingerprint = Some(fingerprint.to_owned());
        Ok(session.view.clone())
    }

    async fn control_session(
        &self,
        session_id: Uuid,
        _fingerprint: &str,
    ) -> Result<ControlBrokerSessionView, SignalingClientError> {
        let state = self.broker.state.lock().expect("state");
        state
            .sessions
            .get(&session_id)
            .filter(|session| {
                session.view.controller_device_id == self.device_id
                    || session.view.server_device_id == self.device_id
            })
            .map(|session| session.view.clone())
            .ok_or_else(|| refused(404, "session_not_found"))
    }

    async fn control_post_signal(
        &self,
        session_id: Uuid,
        _fingerprint: &str,
        signal: &BrokerSignal,
    ) -> Result<(), SignalingClientError> {
        let mut state = self.broker.state.lock().expect("state");
        let session = state
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| refused(404, "session_not_found"))?;
        if session.view.phase != "accepted" {
            return Err(refused(409, "invalid_transition"));
        }
        if session.view.controller_device_id == self.device_id {
            session.to_server.push_back(signal.clone());
        } else if session.view.server_device_id == self.device_id {
            session.to_controller.push_back(signal.clone());
        } else {
            return Err(refused(404, "session_not_found"));
        }
        Ok(())
    }

    async fn control_take_signals(
        &self,
        session_id: Uuid,
        _fingerprint: &str,
    ) -> Result<Vec<BrokerSignal>, SignalingClientError> {
        let mut state = self.broker.state.lock().expect("state");
        let session = state
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| refused(404, "session_not_found"))?;
        let queue = if session.view.controller_device_id == self.device_id {
            &mut session.to_controller
        } else if session.view.server_device_id == self.device_id {
            &mut session.to_server
        } else {
            return Err(refused(404, "session_not_found"));
        };
        Ok(queue.drain(..).collect())
    }
}

fn config() -> ControlSessionConfig {
    ControlSessionConfig {
        stun_urls: Vec::new(),
        connect_timeout: Duration::from_secs(20),
        bind_timeout: Duration::from_secs(20),
        poll_interval: Duration::from_millis(25),
        allow_loopback: true,
        ..ControlSessionConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real ICE/DTLS/SCTP over UDP loopback; run with --ignored"]
async fn direct_control_bind_and_ping_pong() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let broker = Arc::new(Broker::default());
    let account_id = Uuid::new_v4();
    let controller_device_id = Uuid::new_v4();
    let server_device_id = Uuid::new_v4();
    let controller_api = broker.handle(account_id, controller_device_id);
    let server_api = broker.handle(account_id, server_device_id);
    let controller_identity = InstanceSigningIdentity::generate(InstanceId::new());
    let server_identity = InstanceSigningIdentity::generate(InstanceId::new());
    let controller_fingerprint = controller_identity
        .public_identity()
        .public_key_fingerprint
        .0;
    let session_id = controller_api
        .control_offer(server_device_id, &controller_fingerprint)
        .await
        .expect("offer");

    let controller_config = config();
    let controller_task = tokio::spawn(async move {
        run_control_controller_session(
            &controller_api,
            &controller_identity,
            session_id,
            &controller_config,
        )
        .await
    });
    let server_config = config();
    let server_task = tokio::spawn(async move {
        run_control_server_session(
            &server_api,
            &server_identity,
            session_id,
            &server_config,
            None,
        )
        .await
    });

    let mut controller = controller_task
        .await
        .expect("controller join")
        .expect("controller bind");
    let mut server = server_task
        .await
        .expect("server join")
        .expect("server bind");

    controller
        .send_text(r#"{"version":1,"body":{"type":"ping"}}"#)
        .await
        .expect("ping");
    assert_eq!(
        server.recv_text().await.expect("receive ping"),
        r#"{"version":1,"body":{"type":"ping"}}"#
    );
    server
        .send_text(r#"{"version":1,"body":{"type":"pong"}}"#)
        .await
        .expect("pong");
    assert_eq!(
        controller.recv_text().await.expect("receive pong"),
        r#"{"version":1,"body":{"type":"pong"}}"#
    );
}
