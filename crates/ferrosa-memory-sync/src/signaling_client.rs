//! Client for the MaaS signaling broker (MAAS-T-36 ↔ T-29 contract).
//!
//! Mirrors the gateway's `/v1/p2p` surface: offers, single-use acceptance,
//! session views (which carry both vouched fingerprints), and SDP/ICE signal
//! relay. The [`SignalingApi`] trait is the seam the loopback harness mocks
//! with an in-process broker implementing the same state machine, so the peer
//! session logic is tested against the T-29 contract without a network.
//!
//! Fail-closed: every non-2xx response and every decode failure is an error —
//! there is no "assume accepted" or empty-result fallback anywhere.

// Fail-loud on untrusted input; production paths never unwrap. Tests assert
// on known-good fixtures and are exempt.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use ferrosa_memory_core::remote_identity::InstanceSigningIdentity;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A participant's view of a broker session (mirror of the gateway's
/// `SessionView`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerSessionView {
    /// Session (offer token) id.
    pub session_id: Uuid,
    /// `offered` | `accepted` | `closed`.
    pub phase: String,
    /// The offering account.
    pub teacher_account: Uuid,
    /// The accepting account.
    pub learner_account: Uuid,
    /// The pack this consent covers.
    pub pack_id: Uuid,
    /// The teacher's vouched DTLS/device fingerprint.
    pub teacher_fingerprint: String,
    /// The learner's vouched fingerprint (present once accepted).
    pub learner_fingerprint: Option<String>,
}

impl BrokerSessionView {
    /// Whether the learner has consented.
    pub fn is_accepted(&self) -> bool {
        self.phase == "accepted"
    }
}

/// Device-targeted control signaling session returned by `/v1/control`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlBrokerSessionView {
    /// Ephemeral offer/session token.
    pub session_id: Uuid,
    /// `offered` | `accepted` | `closed`.
    pub phase: String,
    /// Account owning the controller and server devices.
    pub account_id: Uuid,
    /// Mobile controller device fixed at offer time.
    pub controller_device_id: Uuid,
    /// Controller's gateway-vouched fingerprint.
    pub controller_fingerprint: String,
    /// Exact Ferrosa Memory server device fixed at offer time.
    pub server_device_id: Uuid,
    /// Server's gateway-vouched fingerprint, present after acceptance.
    pub server_fingerprint: Option<String>,
}

/// Registered gateway device view used by the operator enrollment command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayDeviceView {
    pub device_id: Uuid,
    pub public_key: String,
    pub fingerprint: String,
    pub label: String,
    pub revoked_at: Option<String>,
}

/// Authenticated account identity returned by `/v1/whoami`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayIdentityView {
    pub account_id: Uuid,
    pub key_id: Uuid,
}

impl ControlBrokerSessionView {
    /// Whether the exact target server accepted the session.
    pub fn is_accepted(&self) -> bool {
        self.phase == "accepted"
    }
}

/// One relayed signal (wire mirror of the gateway's `Signal`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind", content = "payload")]
pub enum BrokerSignal {
    /// SDP offer (teacher → learner).
    SdpOffer(String),
    /// SDP answer (learner → teacher).
    SdpAnswer(String),
    /// One ICE candidate line (unused in the non-trickle MVP flow).
    Ice(String),
}

/// Signaling client errors. All fail-closed.
#[derive(Debug, thiserror::Error)]
pub enum SignalingClientError {
    /// Transport-level failure reaching the broker.
    #[error("broker transport error: {0}")]
    Transport(String),
    /// The broker refused the request (4xx/5xx with its opaque category).
    #[error("broker refused: http {status}: {body}")]
    Refused { status: u16, body: String },
    /// The broker response did not decode as the expected shape.
    #[error("broker response decode error: {0}")]
    Decode(String),
}

/// The T-29 broker contract as seen by a peer.
///
/// RPITIT (like `PackApplyStore`) so implementations stay allocation-free and
/// the loopback mock is trivial; peer-session drivers are generic over it.
pub trait SignalingApi: Send + Sync {
    /// Offer `pack_id` to `learner_account`, vouching `fingerprint`.
    /// Returns the session id (single-use offer token).
    fn offer(
        &self,
        learner_account: Uuid,
        pack_id: Uuid,
        fingerprint: &str,
    ) -> impl std::future::Future<Output = Result<Uuid, SignalingClientError>> + Send;

    /// The caller's pending offers (learner side).
    fn pending_offers(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<BrokerSessionView>, SignalingClientError>> + Send;

    /// Accept an offer, vouching the learner `fingerprint` (single-use).
    fn accept(
        &self,
        session_id: Uuid,
        fingerprint: &str,
    ) -> impl std::future::Future<Output = Result<BrokerSessionView, SignalingClientError>> + Send;

    /// A participant's current session view.
    fn session(
        &self,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = Result<BrokerSessionView, SignalingClientError>> + Send;

    /// Post one signal to the peer.
    fn post_signal(
        &self,
        session_id: Uuid,
        signal: &BrokerSignal,
    ) -> impl std::future::Future<Output = Result<(), SignalingClientError>> + Send;

    /// Drain the caller's inbox.
    fn take_signals(
        &self,
        session_id: Uuid,
    ) -> impl std::future::Future<Output = Result<Vec<BrokerSignal>, SignalingClientError>> + Send;
}

/// Device-targeted signaling contract for the separate mobile control data
/// channel. Application commands and events do not transit this API.
pub trait ControlSignalingApi: Send + Sync {
    /// Controller offers a session to one exact Ferrosa Memory server device.
    fn control_offer(
        &self,
        server_device_id: Uuid,
        controller_fingerprint: &str,
    ) -> impl std::future::Future<Output = Result<Uuid, SignalingClientError>> + Send;

    /// Server lists offers addressed to its own vouched device fingerprint.
    fn control_pending_offers(
        &self,
        fingerprint: &str,
    ) -> impl std::future::Future<
        Output = Result<Vec<ControlBrokerSessionView>, SignalingClientError>,
    > + Send;

    /// Exact target server accepts once with its own vouched fingerprint.
    fn control_accept(
        &self,
        session_id: Uuid,
        fingerprint: &str,
    ) -> impl std::future::Future<Output = Result<ControlBrokerSessionView, SignalingClientError>> + Send;

    /// Exact participant reads the current session view.
    fn control_session(
        &self,
        session_id: Uuid,
        fingerprint: &str,
    ) -> impl std::future::Future<Output = Result<ControlBrokerSessionView, SignalingClientError>> + Send;

    /// Exact participant posts one SDP/ICE signal to the peer.
    fn control_post_signal(
        &self,
        session_id: Uuid,
        fingerprint: &str,
        signal: &BrokerSignal,
    ) -> impl std::future::Future<Output = Result<(), SignalingClientError>> + Send;

    /// Exact participant drains SDP/ICE signals posted by the peer.
    fn control_take_signals(
        &self,
        session_id: Uuid,
        fingerprint: &str,
    ) -> impl std::future::Future<Output = Result<Vec<BrokerSignal>, SignalingClientError>> + Send;
}

/// How a client proves who it is.
///
/// Device identities do NOT use API keys. A key is a bearer secret that sits on
/// the machine and authorises everything until it is revoked; a device signs
/// each request, so a captured one cannot be lifted onto another call and there
/// is nothing on disk worth stealing. The API-key arm remains only for callers
/// that predate enrolment and have no identity yet.
pub enum Credential {
    /// Legacy account-scoped bearer key.
    ApiKey(String),
    /// A device identity, signing every request (`X-Device-*`).
    ///
    /// `Arc` rather than an owned value so a caller that also needs the
    /// identity — the control session signs its hello with the same key —
    /// shares ONE copy. Cloning it would put a second copy of the secret in
    /// memory to satisfy a borrow checker, and re-reading the file would put it
    /// through the filesystem twice.
    Device(std::sync::Arc<InstanceSigningIdentity>),
}

impl Credential {
    /// Build a device credential from an enrolled identity.
    pub fn device(identity: std::sync::Arc<InstanceSigningIdentity>) -> Self {
        Self::Device(identity)
    }

    /// Authenticate one request.
    ///
    /// `body` must be the exact bytes being sent: the signature covers their
    /// hash, so hashing one serialization and sending another would sign
    /// something the server never receives.
    fn apply(
        &self,
        req: reqwest::RequestBuilder,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> reqwest::RequestBuilder {
        match self {
            Self::ApiKey(key) => req.header("Api-Key", key),
            Self::Device(identity) => {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64);
                let nonce = Uuid::new_v4().to_string();
                crate::device_request::sign_request(identity, method, path, body, timestamp, &nonce)
                    .apply(req)
            }
        }
    }
}

/// HTTPS client for a live gateway broker.
pub struct HttpSignalingClient {
    base: String,
    credential: Credential,
    http: reqwest::Client,
}

impl HttpSignalingClient {
    /// Build a client for `base` (e.g. `https://gw.example`) with `api_key`.
    pub fn new(base: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::with_credential(base, Credential::ApiKey(api_key.into()))
    }

    /// Build a client that signs every request with a device identity.
    pub fn with_credential(base: impl Into<String>, credential: Credential) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            credential,
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The path the signature covers: everything before any query string.
    ///
    /// The gateway verifies against `uri().path()`, so signing the query too
    /// would 401 every request that carries one — and it would look like a
    /// credential fault rather than a formatting one.
    fn signed_path(path: &str) -> &str {
        match path.split_once('?') {
            Some((before, _)) => before,
            None => path,
        }
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response, SignalingClientError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(SignalingClientError::Refused {
            status: status.as_u16(),
            body,
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, SignalingClientError> {
        let resp = self
            .credential
            .apply(
                self.http.get(self.url(path)),
                "GET",
                Self::signed_path(path),
                b"",
            )
            .send()
            .await
            .map_err(|e| SignalingClientError::Transport(e.to_string()))?;
        Self::check(resp)
            .await?
            .json::<T>()
            .await
            .map_err(|e| SignalingClientError::Decode(e.to_string()))
    }

    async fn post_json<B: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, SignalingClientError> {
        // Serialized ONCE. The signature covers the hash of these bytes, so
        // letting reqwest re-serialize the value would sign something other
        // than what is transmitted.
        let bytes =
            serde_json::to_vec(body).map_err(|e| SignalingClientError::Decode(e.to_string()))?;
        let resp = self
            .credential
            .apply(
                self.http
                    .post(self.url(path))
                    .header("content-type", "application/json"),
                "POST",
                Self::signed_path(path),
                &bytes,
            )
            .body(bytes.clone())
            .send()
            .await
            .map_err(|e| SignalingClientError::Transport(e.to_string()))?;
        Self::check(resp)
            .await?
            .json::<T>()
            .await
            .map_err(|e| SignalingClientError::Decode(e.to_string()))
    }

    async fn post_json_no_body<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<(), SignalingClientError> {
        let bytes =
            serde_json::to_vec(body).map_err(|e| SignalingClientError::Decode(e.to_string()))?;
        let resp = self
            .credential
            .apply(
                self.http
                    .post(self.url(path))
                    .header("content-type", "application/json"),
                "POST",
                Self::signed_path(path),
                &bytes,
            )
            .body(bytes.clone())
            .send()
            .await
            .map_err(|e| SignalingClientError::Transport(e.to_string()))?;
        Self::check(resp).await.map(|_| ())
    }

    /// Return the authenticated account identity used in approval signatures.
    pub async fn whoami(&self) -> Result<GatewayIdentityView, SignalingClientError> {
        self.get_json("/v1/whoami").await
    }

    /// List all registered devices for the authenticated account.
    pub async fn devices(&self) -> Result<Vec<GatewayDeviceView>, SignalingClientError> {
        self.get_json("/v1/devices").await
    }

    /// List devices still awaiting an enrolled-device signature.
    pub async fn pending_devices(&self) -> Result<Vec<GatewayDeviceView>, SignalingClientError> {
        self.get_json("/v1/devices/pending").await
    }

    /// Approve one exact device with an already-enrolled device signature.
    pub async fn approve_device(
        &self,
        target_device_id: Uuid,
        approved_by_device_id: Uuid,
        signature: &str,
    ) -> Result<(), SignalingClientError> {
        self.post_json_no_body(
            &format!("/v1/devices/{target_device_id}/approve"),
            &serde_json::json!({
                "approved_by_device_id": approved_by_device_id,
                "signature": signature,
            }),
        )
        .await
    }
}

#[derive(Debug, Deserialize)]
struct OfferCreated {
    session_id: Uuid,
}

impl SignalingApi for HttpSignalingClient {
    async fn offer(
        &self,
        learner_account: Uuid,
        pack_id: Uuid,
        fingerprint: &str,
    ) -> Result<Uuid, SignalingClientError> {
        let created: OfferCreated = self
            .post_json(
                "/v1/p2p/offers",
                &serde_json::json!({
                    "learner_account_id": learner_account,
                    "pack_id": pack_id,
                    "fingerprint": fingerprint,
                }),
            )
            .await?;
        Ok(created.session_id)
    }

    async fn pending_offers(&self) -> Result<Vec<BrokerSessionView>, SignalingClientError> {
        self.get_json("/v1/p2p/offers").await
    }

    async fn accept(
        &self,
        session_id: Uuid,
        fingerprint: &str,
    ) -> Result<BrokerSessionView, SignalingClientError> {
        self.post_json(
            &format!("/v1/p2p/offers/{session_id}/accept"),
            &serde_json::json!({"fingerprint": fingerprint}),
        )
        .await
    }

    async fn session(&self, session_id: Uuid) -> Result<BrokerSessionView, SignalingClientError> {
        self.get_json(&format!("/v1/p2p/sessions/{session_id}"))
            .await
    }

    async fn post_signal(
        &self,
        session_id: Uuid,
        signal: &BrokerSignal,
    ) -> Result<(), SignalingClientError> {
        self.post_json_no_body(&format!("/v1/p2p/sessions/{session_id}/signals"), signal)
            .await
    }

    async fn take_signals(
        &self,
        session_id: Uuid,
    ) -> Result<Vec<BrokerSignal>, SignalingClientError> {
        self.get_json(&format!("/v1/p2p/sessions/{session_id}/signals"))
            .await
    }
}

impl ControlSignalingApi for HttpSignalingClient {
    async fn control_offer(
        &self,
        server_device_id: Uuid,
        controller_fingerprint: &str,
    ) -> Result<Uuid, SignalingClientError> {
        let created: OfferCreated = self
            .post_json(
                "/v1/control/offers",
                &serde_json::json!({
                    "server_device_id": server_device_id,
                    "controller_fingerprint": controller_fingerprint,
                }),
            )
            .await?;
        Ok(created.session_id)
    }

    async fn control_pending_offers(
        &self,
        fingerprint: &str,
    ) -> Result<Vec<ControlBrokerSessionView>, SignalingClientError> {
        self.get_json(&format!("/v1/control/offers?fingerprint={fingerprint}"))
            .await
    }

    async fn control_accept(
        &self,
        session_id: Uuid,
        fingerprint: &str,
    ) -> Result<ControlBrokerSessionView, SignalingClientError> {
        self.post_json(
            &format!("/v1/control/offers/{session_id}/accept"),
            &serde_json::json!({"fingerprint": fingerprint}),
        )
        .await
    }

    async fn control_session(
        &self,
        session_id: Uuid,
        fingerprint: &str,
    ) -> Result<ControlBrokerSessionView, SignalingClientError> {
        self.get_json(&format!(
            "/v1/control/sessions/{session_id}?fingerprint={fingerprint}"
        ))
        .await
    }

    async fn control_post_signal(
        &self,
        session_id: Uuid,
        fingerprint: &str,
        signal: &BrokerSignal,
    ) -> Result<(), SignalingClientError> {
        self.post_json_no_body(
            &format!("/v1/control/sessions/{session_id}/signals"),
            &serde_json::json!({
                "fingerprint": fingerprint,
                "signal": signal,
            }),
        )
        .await
    }

    async fn control_take_signals(
        &self,
        session_id: Uuid,
        fingerprint: &str,
    ) -> Result<Vec<BrokerSignal>, SignalingClientError> {
        self.get_json(&format!(
            "/v1/control/sessions/{session_id}/signals?fingerprint={fingerprint}"
        ))
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_signal_wire_format_matches_the_gateway() {
        // The gateway serializes Signal with tag "kind" / content "payload"
        // in snake_case. This wire contract must not drift.
        let s = BrokerSignal::SdpOffer("v=0".into());
        let json = serde_json::to_value(&s).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({"kind": "sdp_offer", "payload": "v=0"})
        );
        let back: BrokerSignal =
            serde_json::from_value(serde_json::json!({"kind": "ice", "payload": "candidate x"}))
                .expect("deserialize");
        assert_eq!(back, BrokerSignal::Ice("candidate x".into()));
    }

    #[test]
    fn session_view_decodes_the_gateway_shape() {
        let json = serde_json::json!({
            "session_id": Uuid::new_v4(),
            "phase": "accepted",
            "teacher_account": Uuid::new_v4(),
            "learner_account": Uuid::new_v4(),
            "pack_id": Uuid::new_v4(),
            "teacher_fingerprint": "ab".repeat(32),
            "learner_fingerprint": null,
        });
        let view: BrokerSessionView = serde_json::from_value(json).expect("decode");
        assert!(view.is_accepted());
        assert!(view.learner_fingerprint.is_none());
    }

    #[test]
    fn control_session_view_decodes_gateway_shape() {
        let controller_device_id = Uuid::new_v4();
        let server_device_id = Uuid::new_v4();
        let json = serde_json::json!({
            "session_id": Uuid::new_v4(),
            "phase": "accepted",
            "account_id": Uuid::new_v4(),
            "controller_device_id": controller_device_id,
            "controller_fingerprint": "ab".repeat(32),
            "server_device_id": server_device_id,
            "server_fingerprint": "cd".repeat(32),
        });

        let view: ControlBrokerSessionView = serde_json::from_value(json).expect("decode");

        assert!(view.is_accepted());
        assert_eq!(view.controller_device_id, controller_device_id);
        assert_eq!(view.server_device_id, server_device_id);
    }
}
