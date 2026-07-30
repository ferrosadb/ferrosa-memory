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

/// HTTPS client for a live gateway broker, authenticated by API key.
pub struct HttpSignalingClient {
    base: String,
    api_key: String,
    http: reqwest::Client,
}

impl HttpSignalingClient {
    /// Build a client for `base` (e.g. `https://gw.example`) with `api_key`.
    pub fn new(base: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
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
            .http
            .get(self.url(path))
            .header("Api-Key", &self.api_key)
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
        let resp = self
            .http
            .post(self.url(path))
            .header("Api-Key", &self.api_key)
            .json(body)
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
        let resp = self
            .http
            .post(self.url(path))
            .header("Api-Key", &self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|e| SignalingClientError::Transport(e.to_string()))?;
        Self::check(resp).await.map(|_| ())
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
}
