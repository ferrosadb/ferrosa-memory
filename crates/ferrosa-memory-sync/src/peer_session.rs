//! Peer session drivers (MAAS-T-36): broker-consented WebRTC pack transfer.
//!
//! Orchestrates one sealed-pack transfer end-to-end against the T-29 broker
//! contract ([`crate::signaling_client::SignalingApi`]):
//!
//! 1. **Consent** — the teacher's offer is accepted by the learner (broker
//!    enforces mutual-contact + single-use acceptance; both sides' device
//!    fingerprints are registry-vouched there).
//! 2. **Connect** — non-trickle SDP offer/answer through the broker, STUN-only
//!    ICE. A pair that cannot connect directly fails LOUD with
//!    [`PeerSessionError::NatTraversalFailed`] within the bounded timeout —
//!    never a hang (TURN relay is deliberately out of MVP scope).
//! 3. **Bind** — a signed-Hello exchange as the FIRST channel traffic. Each
//!    side signs (session id ‖ role ‖ ephemeral X25519 public key ‖ SHA-256 of
//!    both SDPs) with its device Ed25519 key via the existing
//!    [`SignedEnvelope`] machinery. The receiver requires the sender's
//!    self-carried public identity to hash to the fingerprint the BROKER
//!    vouched for that side. Signing the SDP hashes binds the exchange to
//!    this DTLS session (each SDP embeds its `a=fingerprint` cert line), so a
//!    relay-in-the-middle that terminates DTLS presents different SDPs and
//!    the signature check fails — with **zero pack bytes sent**.
//! 4. **Key** — IKM = X25519(local ephemeral secret, peer ephemeral public):
//!    signed-ephemeral DH, so pack confidentiality never depends on the
//!    broker and the content key has forward secrecy per session. The pack
//!    crypto additionally binds both vouched fingerprints
//!    ([`derive_content_key`]'s existing contract).
//! 5. **Transfer** — teacher seals and streams the pack over the channel; the
//!    learner's [`PackReceiver`] verifies-before-parse and applies with the
//!    channel-attested [`ChannelAttestation`] built from the vouched pair.
//!
//! [`derive_content_key`]: crate::pack_crypto::derive_content_key

// Fail-loud on untrusted input; production paths never unwrap. Tests assert
// on known-good fixtures and are exempt.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)
)]

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2_kdf::{Digest, Sha256};
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

use ferrosa_memory_core::remote_identity::{
    InstancePublicIdentity, InstanceSigningIdentity, PublicKeyFingerprint, SignedEnvelope,
};

use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

use crate::learner_ingest::{ChannelAttestation, PackApplyStore};
use crate::pack_crypto::{CipherFloor, Secret};
use crate::peer_transport::webrtc::{PackReceiver, ReceiverHealth, RtcDataChannel, send_pack_ref};
use crate::peer_transport::{AssemblerLimits, PeerTransport, SendLimits};
use crate::replication::{BuildReport, PackBuildParams, TeacherSelection, build_and_seal};
use crate::signaling_client::{BrokerSignal, SignalingApi, SignalingClientError};

/// Configuration for one peer session.
#[derive(Debug, Clone)]
pub struct PeerSessionConfig {
    /// STUN servers for ICE (STUN-only per D-MVP-1; no TURN).
    pub stun_urls: Vec<String>,
    /// How long the teacher waits for the learner to accept the offer.
    pub accept_timeout: Duration,
    /// Bound on SDP exchange + ICE + DTLS + channel-open. Exceeding it is
    /// [`PeerSessionError::NatTraversalFailed`].
    pub connect_timeout: Duration,
    /// Bound on the hello/bind exchange and on the pack transfer + apply.
    pub transfer_timeout: Duration,
    /// Broker polling interval.
    pub poll_interval: Duration,
    /// Gather loopback candidates (in-process tests only; keep `false` for
    /// real peers so only routable candidates are offered).
    pub allow_loopback: bool,
    /// Largest wire frame payload.
    pub max_frame_payload: usize,
    /// Send/assembly bounds.
    pub max_pack_bytes: usize,
}

impl Default for PeerSessionConfig {
    fn default() -> Self {
        Self {
            stun_urls: vec!["stun:stun.l.google.com:19302".to_string()],
            accept_timeout: Duration::from_secs(300),
            connect_timeout: Duration::from_secs(30),
            transfer_timeout: Duration::from_secs(120),
            poll_interval: Duration::from_millis(250),
            allow_loopback: false,
            max_frame_payload: 16 * 1024,
            max_pack_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Peer-session failures. Every variant is terminal and loud.
#[derive(Debug, thiserror::Error)]
pub enum PeerSessionError {
    /// Broker call failed.
    #[error("signaling: {0}")]
    Signaling(#[from] SignalingClientError),
    /// The peers could not establish a direct (STUN-only) connection within
    /// the bound — the MVP's explicit NAT-blocked failure (no TURN).
    #[error("direct peer connection failed within the bound (STUN-only, no TURN relay)")]
    NatTraversalFailed,
    /// A bounded wait elapsed.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    /// The peer's hello failed identity/channel binding — no pack bytes flow.
    #[error("channel attestation failed: {0}")]
    Attestation(String),
    /// WebRTC stack error.
    #[error("webrtc: {0}")]
    Rtc(String),
    /// Pack build/seal/send error.
    #[error("pack transfer: {0}")]
    Transfer(String),
    /// The learner reported the transfer failed (receiver health).
    #[error("learner failed to apply the pack: {0}")]
    LearnerFailed(String),
}

// ---------------------------------------------------------------------------
// Control protocol (text frames; the FIRST traffic on the channel)
// ---------------------------------------------------------------------------

/// The signed hello payload — the channel/identity/key binding statement.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelloPayload {
    /// The broker session this binding belongs to.
    pub session_id: Uuid,
    /// `"teacher"` or `"learner"`.
    pub role: String,
    /// The sender's ephemeral X25519 public key, hex.
    pub eph_pub_hex: String,
    /// SHA-256 (hex) of the SDP offer as this side observed it.
    pub offer_sdp_sha256: String,
    /// SHA-256 (hex) of the SDP answer as this side observed it.
    pub answer_sdp_sha256: String,
}

/// The hello frame body (sender identity + signed binding payload).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HelloFrame {
    identity: InstancePublicIdentity,
    envelope: SignedEnvelope<HelloPayload>,
}

/// Text control frames exchanged on the data channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "t")]
enum ControlFrame {
    /// The signed binding hello (first frame from each side). Boxed: it is
    /// far larger than the marker frames.
    Hello(Box<HelloFrame>),
    /// Learner is bound + receiver installed; teacher may stream.
    Ready,
    /// Learner applied the pack.
    Applied,
    /// Learner failed terminally.
    Failed { error: String },
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let digest = Sha256::digest(bytes);
    digest.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn hex32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn parse_hex32(hex: &str) -> Result<[u8; 32], PeerSessionError> {
    if hex.len() != 64 {
        return Err(PeerSessionError::Attestation(
            "ephemeral key must be 64 hex chars".into(),
        ));
    }
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let hi = hex_val(chunk.first().copied())
            .ok_or_else(|| PeerSessionError::Attestation("ephemeral key must be hex".into()))?;
        let lo = hex_val(chunk.get(1).copied())
            .ok_or_else(|| PeerSessionError::Attestation("ephemeral key must be hex".into()))?;
        if let Some(slot) = out.get_mut(i) {
            *slot = (hi << 4) | lo;
        }
    }
    Ok(out)
}

fn hex_val(c: Option<u8>) -> Option<u8> {
    match c? {
        c @ b'0'..=b'9' => Some(c - b'0'),
        c @ b'a'..=b'f' => Some(c - b'a' + 10),
        c @ b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Verify a peer hello: envelope signature over the payload, identity →
/// vouched fingerprint, session/role match, and SDP-hash channel binding.
/// Returns the peer's ephemeral X25519 public key.
fn verify_peer_hello(
    identity: &InstancePublicIdentity,
    envelope: &SignedEnvelope<HelloPayload>,
    vouched_fingerprint: &str,
    session_id: Uuid,
    expected_role: &str,
    offer_sdp_sha256: &str,
    answer_sdp_sha256: &str,
) -> Result<x25519_dalek::PublicKey, PeerSessionError> {
    // The self-carried identity must hash to the fingerprint the broker
    // vouched for this side of the session (registry ownership was enforced
    // there). This is what makes the hello unforgeable by a third party.
    if identity.public_key_fingerprint.0 != vouched_fingerprint {
        return Err(PeerSessionError::Attestation(format!(
            "peer identity fingerprint {} does not match the broker-vouched {}",
            identity.public_key_fingerprint.0, vouched_fingerprint
        )));
    }
    envelope
        .verify(identity)
        .map_err(|e| PeerSessionError::Attestation(format!("hello signature invalid: {e}")))?;
    let payload = &envelope.payload;
    if payload.session_id != session_id {
        return Err(PeerSessionError::Attestation(
            "hello bound to a different session".into(),
        ));
    }
    if payload.role != expected_role {
        return Err(PeerSessionError::Attestation(format!(
            "hello role {} where {expected_role} was required",
            payload.role
        )));
    }
    // Channel binding: both sides must have observed the SAME SDP pair. The
    // SDPs embed the DTLS certificate fingerprints, so a DTLS-terminating
    // middlebox cannot satisfy both legs.
    if payload.offer_sdp_sha256 != offer_sdp_sha256
        || payload.answer_sdp_sha256 != answer_sdp_sha256
    {
        return Err(PeerSessionError::Attestation(
            "SDP hash mismatch — channel binding failed".into(),
        ));
    }
    let bytes = parse_hex32(&payload.eph_pub_hex)?;
    Ok(x25519_dalek::PublicKey::from(bytes))
}

// ---------------------------------------------------------------------------
// Channel plumbing
// ---------------------------------------------------------------------------

/// Inbound routing installed as the channel's single permanent `on_message`
/// handler: text frames go to the control inbox; binary frames go to the pack
/// receiver once one is armed (early binary is a loud protocol violation).
struct InboundRouter<S: PackApplyStore + Send + Sync + 'static> {
    control_tx: mpsc::UnboundedSender<ControlFrame>,
    receiver: std::sync::Mutex<Option<Arc<PackReceiver<S>>>>,
}

impl<S: PackApplyStore + Send + Sync + 'static> InboundRouter<S> {
    fn new(control_tx: mpsc::UnboundedSender<ControlFrame>) -> Arc<Self> {
        Arc::new(Self {
            control_tx,
            receiver: std::sync::Mutex::new(None),
        })
    }

    fn arm(&self, receiver: Arc<PackReceiver<S>>) {
        if let Ok(mut slot) = self.receiver.lock() {
            *slot = Some(receiver);
        }
    }

    fn install(self: &Arc<Self>, dc: &Arc<RTCDataChannel>) {
        let router = self.clone();
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let router = router.clone();
            Box::pin(async move {
                if msg.is_string {
                    match serde_json::from_slice::<ControlFrame>(&msg.data) {
                        Ok(frame) => {
                            let _ = router.control_tx.send(frame);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "malformed control frame dropped");
                        }
                    }
                    return;
                }
                let receiver = match router.receiver.lock() {
                    Ok(slot) => slot.clone(),
                    Err(_) => None,
                };
                match receiver {
                    Some(r) => r.handle_frame_bytes(&msg.data).await,
                    // Binary before the bind completes is a protocol
                    // violation — dropped loudly, never buffered or parsed.
                    None => tracing::warn!("binary frame before channel bind — dropped"),
                }
            })
        }));
    }
}

async fn send_control(
    dc: &Arc<RTCDataChannel>,
    frame: &ControlFrame,
) -> Result<(), PeerSessionError> {
    let text = serde_json::to_string(frame)
        .map_err(|e| PeerSessionError::Transfer(format!("control encode: {e}")))?;
    dc.send_text(text)
        .await
        .map_err(|e| PeerSessionError::Rtc(format!("control send: {e}")))?;
    Ok(())
}

async fn next_control(
    rx: &mut mpsc::UnboundedReceiver<ControlFrame>,
    deadline: Duration,
    what: &'static str,
) -> Result<ControlFrame, PeerSessionError> {
    tokio::time::timeout(deadline, rx.recv())
        .await
        .map_err(|_| PeerSessionError::Timeout(what))?
        .ok_or(PeerSessionError::Rtc("channel closed".into()))
}

fn build_rtc_api(cfg: &PeerSessionConfig) -> Result<webrtc::api::API, PeerSessionError> {
    use webrtc::api::setting_engine::SettingEngine;
    use webrtc::ice::mdns::MulticastDnsMode;
    use webrtc::ice::network_type::NetworkType;

    let mut media = MediaEngine::default();
    let registry = register_default_interceptors(Registry::new(), &mut media)
        .map_err(|e| PeerSessionError::Rtc(format!("interceptors: {e}")))?;
    let mut se = SettingEngine::default();
    se.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
    se.set_network_types(vec![NetworkType::Udp4]);
    if cfg.allow_loopback {
        se.set_include_loopback_candidate(true);
    }
    Ok(APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(se)
        .build())
}

fn rtc_config(cfg: &PeerSessionConfig) -> RTCConfiguration {
    RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: cfg.stun_urls.clone(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Poll `take_signals` until a matcher yields, within `deadline`.
async fn poll_signal<S: SignalingApi, T>(
    api: &S,
    session_id: Uuid,
    cfg: &PeerSessionConfig,
    deadline: Duration,
    what: &'static str,
    mut matcher: impl FnMut(BrokerSignal) -> Option<T>,
) -> Result<T, PeerSessionError> {
    let start = tokio::time::Instant::now();
    loop {
        for signal in api.take_signals(session_id).await? {
            if let Some(hit) = matcher(signal) {
                return Ok(hit);
            }
        }
        if start.elapsed() > deadline {
            return Err(PeerSessionError::Timeout(what));
        }
        tokio::time::sleep(cfg.poll_interval).await;
    }
}

fn fresh_ephemeral() -> ([u8; 32], x25519_dalek::PublicKey) {
    use rand::RngCore;
    let mut secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    let pk = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(secret));
    (secret, pk)
}

fn dh_ikm(local_secret: [u8; 32], peer_pub: &x25519_dalek::PublicKey) -> Secret {
    let shared = x25519_dalek::StaticSecret::from(local_secret).diffie_hellman(peer_pub);
    Secret::new(shared.as_bytes().to_vec())
}

// ---------------------------------------------------------------------------
// Teacher driver
// ---------------------------------------------------------------------------

/// Run the teacher side of an already-offered session: wait for acceptance,
/// connect, bind, seal, stream, and wait for the learner's applied receipt.
///
/// `params.provenance` fingerprints are overwritten with the broker-vouched
/// pair (self-claims never survive).
pub async fn run_teacher_session<S: SignalingApi>(
    api: &S,
    identity: &InstanceSigningIdentity,
    session_id: Uuid,
    selection: &TeacherSelection,
    params: &mut PackBuildParams,
    cfg: &PeerSessionConfig,
) -> Result<BuildReport, PeerSessionError> {
    // 1. Wait for consent.
    let start = tokio::time::Instant::now();
    let view = loop {
        let view = api.session(session_id).await?;
        if view.is_accepted() {
            break view;
        }
        if start.elapsed() > cfg.accept_timeout {
            return Err(PeerSessionError::Timeout("learner acceptance"));
        }
        tokio::time::sleep(cfg.poll_interval).await;
    };
    let my_fp = identity.public_identity().public_key_fingerprint;
    if view.teacher_fingerprint != my_fp.0 {
        return Err(PeerSessionError::Attestation(
            "session was vouched for a different teacher device".into(),
        ));
    }
    let learner_fp = view.learner_fingerprint.clone().ok_or_else(|| {
        PeerSessionError::Attestation("accepted session lacks learner fingerprint".into())
    })?;

    // 2. Connect (offerer, non-trickle).
    let rtc_api = build_rtc_api(cfg)?;
    let pc = Arc::new(
        rtc_api
            .new_peer_connection(rtc_config(cfg))
            .await
            .map_err(|e| PeerSessionError::Rtc(format!("new_peer_connection: {e}")))?,
    );
    let dc = pc
        .create_data_channel("maas-pack", None)
        .await
        .map_err(|e| PeerSessionError::Rtc(format!("create_data_channel: {e}")))?;
    let opened = Arc::new(Notify::new());
    {
        let opened = opened.clone();
        dc.on_open(Box::new(move || {
            let opened = opened.clone();
            Box::pin(async move {
                opened.notify_waiters();
            })
        }));
    }
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    // The teacher never receives pack frames; the router still guards them.
    let router: Arc<InboundRouter<NullApplyStore>> = InboundRouter::new(control_tx);
    router.install(&dc);

    let mut gather = pc.gathering_complete_promise().await;
    let offer = pc
        .create_offer(None)
        .await
        .map_err(|e| PeerSessionError::Rtc(format!("create_offer: {e}")))?;
    pc.set_local_description(offer)
        .await
        .map_err(|e| PeerSessionError::Rtc(format!("set_local_description: {e}")))?;
    let _ = gather.recv().await;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| PeerSessionError::Rtc("missing local description".into()))?;
    api.post_signal(session_id, &BrokerSignal::SdpOffer(local.sdp.clone()))
        .await?;

    let answer_sdp = poll_signal(
        api,
        session_id,
        cfg,
        cfg.connect_timeout,
        "sdp answer",
        |s| match s {
            BrokerSignal::SdpAnswer(sdp) => Some(sdp),
            _ => None,
        },
    )
    .await?;
    let answer = RTCSessionDescription::answer(answer_sdp.clone())
        .map_err(|e| PeerSessionError::Rtc(format!("answer sdp: {e}")))?;
    pc.set_remote_description(answer)
        .await
        .map_err(|e| PeerSessionError::Rtc(format!("set_remote_description: {e}")))?;

    if !wait_channel_open(&dc, &opened, cfg.connect_timeout).await {
        close_quietly(&pc).await;
        return Err(PeerSessionError::NatTraversalFailed);
    }

    // 3. Bind: signed hello exchange (first traffic, before any pack bytes).
    let offer_hash = sha256_hex(local.sdp.as_bytes());
    let answer_hash = sha256_hex(answer_sdp.as_bytes());
    let (eph_secret, eph_pub) = fresh_ephemeral();
    let payload = HelloPayload {
        session_id,
        role: "teacher".into(),
        eph_pub_hex: hex32(eph_pub.as_bytes()),
        offer_sdp_sha256: offer_hash.clone(),
        answer_sdp_sha256: answer_hash.clone(),
    };
    let envelope = identity
        .sign(payload)
        .map_err(|e| PeerSessionError::Attestation(format!("hello sign: {e}")))?;
    send_control(
        &dc,
        &ControlFrame::Hello(Box::new(HelloFrame {
            identity: identity.public_identity(),
            envelope,
        })),
    )
    .await?;

    let peer_eph = loop {
        match next_control(&mut control_rx, cfg.transfer_timeout, "learner hello").await? {
            ControlFrame::Hello(frame) => {
                break verify_peer_hello(
                    &frame.identity,
                    &frame.envelope,
                    &learner_fp,
                    session_id,
                    "learner",
                    &offer_hash,
                    &answer_hash,
                )?;
            }
            ControlFrame::Failed { error } => {
                return Err(PeerSessionError::LearnerFailed(error));
            }
            _ => continue,
        }
    };
    let ikm = dh_ikm(eph_secret, &peer_eph);

    // 4. Wait for the learner's receiver to be armed.
    loop {
        match next_control(&mut control_rx, cfg.transfer_timeout, "learner ready").await? {
            ControlFrame::Ready => break,
            ControlFrame::Failed { error } => {
                return Err(PeerSessionError::LearnerFailed(error));
            }
            _ => continue,
        }
    }

    // 5. Seal with the broker-vouched fingerprint pair and stream.
    params.provenance.teacher_fingerprint = my_fp.clone();
    params.provenance.learner_fingerprint = PublicKeyFingerprint(learner_fp.clone());
    let (pack_ref, report) = build_and_seal(
        selection,
        params,
        &ikm,
        cfg.max_frame_payload / 2,
        CipherFloor::default(),
    )
    .map_err(|e| PeerSessionError::Transfer(format!("build_and_seal: {e}")))?;

    let rtc = RtcDataChannel::attach(dc.clone(), 256 * 1024).await;
    let mut transport = PeerTransport::new(
        rtc,
        SendLimits {
            max_buffered_bytes: 256 * 1024,
            max_frame_payload: cfg.max_frame_payload,
            max_chunks: 8192,
        },
    );
    transport
        .mark_open()
        .map_err(|e| PeerSessionError::Transfer(format!("mark_open: {e}")))?;
    send_pack_ref(&mut transport, &pack_ref, cfg.max_frame_payload)
        .await
        .map_err(|e| PeerSessionError::Transfer(format!("send_pack_ref: {e}")))?;

    // 6. Wait for the applied receipt.
    loop {
        match next_control(&mut control_rx, cfg.transfer_timeout, "applied receipt").await? {
            ControlFrame::Applied => break,
            ControlFrame::Failed { error } => {
                return Err(PeerSessionError::LearnerFailed(error));
            }
            _ => continue,
        }
    }
    close_quietly(&pc).await;
    Ok(report)
}

// ---------------------------------------------------------------------------
// Learner driver
// ---------------------------------------------------------------------------

/// Run the learner side: accept the offer (vouching this device), connect,
/// bind, receive + apply the pack, and send the applied receipt.
///
/// `remote_id` labels provenance rows for this teacher relationship.
pub async fn run_learner_session<S, ST>(
    api: &S,
    identity: &InstanceSigningIdentity,
    session_id: Uuid,
    store: ST,
    remote_id: Uuid,
    cfg: &PeerSessionConfig,
) -> Result<ReceiverHealth, PeerSessionError>
where
    S: SignalingApi,
    ST: PackApplyStore + Send + Sync + 'static,
{
    let my_fp = identity.public_identity().public_key_fingerprint;
    // 1. Consent (single-use; the broker re-checks mutual contact).
    let view = api.accept(session_id, &my_fp.0).await?;
    let teacher_fp = view.teacher_fingerprint.clone();

    // 2. Connect (answerer, non-trickle).
    let rtc_api = build_rtc_api(cfg)?;
    let pc = Arc::new(
        rtc_api
            .new_peer_connection(rtc_config(cfg))
            .await
            .map_err(|e| PeerSessionError::Rtc(format!("new_peer_connection: {e}")))?,
    );
    let (control_tx, mut control_rx) = mpsc::unbounded_channel();
    let router: Arc<InboundRouter<ST>> = InboundRouter::new(control_tx);
    let (dc_tx, mut dc_rx) = mpsc::unbounded_channel::<Arc<RTCDataChannel>>();
    let opened = Arc::new(Notify::new());
    {
        let router = router.clone();
        let opened = opened.clone();
        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let router = router.clone();
            let opened = opened.clone();
            let dc_tx = dc_tx.clone();
            Box::pin(async move {
                router.install(&dc);
                let opened_inner = opened.clone();
                dc.on_open(Box::new(move || {
                    let opened_inner = opened_inner.clone();
                    Box::pin(async move {
                        opened_inner.notify_waiters();
                    })
                }));
                let _ = dc_tx.send(dc);
            })
        }));
    }

    let offer_sdp = poll_signal(
        api,
        session_id,
        cfg,
        cfg.connect_timeout,
        "sdp offer",
        |s| match s {
            BrokerSignal::SdpOffer(sdp) => Some(sdp),
            _ => None,
        },
    )
    .await?;
    let offer = RTCSessionDescription::offer(offer_sdp.clone())
        .map_err(|e| PeerSessionError::Rtc(format!("offer sdp: {e}")))?;
    pc.set_remote_description(offer)
        .await
        .map_err(|e| PeerSessionError::Rtc(format!("set_remote_description: {e}")))?;
    let mut gather = pc.gathering_complete_promise().await;
    let answer = pc
        .create_answer(None)
        .await
        .map_err(|e| PeerSessionError::Rtc(format!("create_answer: {e}")))?;
    pc.set_local_description(answer)
        .await
        .map_err(|e| PeerSessionError::Rtc(format!("set_local_description: {e}")))?;
    let _ = gather.recv().await;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| PeerSessionError::Rtc("missing local description".into()))?;
    api.post_signal(session_id, &BrokerSignal::SdpAnswer(local.sdp.clone()))
        .await?;

    let dc = match tokio::time::timeout(cfg.connect_timeout, dc_rx.recv()).await {
        Ok(Some(dc)) => dc,
        _ => {
            close_quietly(&pc).await;
            return Err(PeerSessionError::NatTraversalFailed);
        }
    };
    if !wait_channel_open(&dc, &opened, cfg.connect_timeout).await {
        close_quietly(&pc).await;
        return Err(PeerSessionError::NatTraversalFailed);
    }

    // 3. Bind.
    let offer_hash = sha256_hex(offer_sdp.as_bytes());
    let answer_hash = sha256_hex(local.sdp.as_bytes());
    let (eph_secret, eph_pub) = fresh_ephemeral();
    let payload = HelloPayload {
        session_id,
        role: "learner".into(),
        eph_pub_hex: hex32(eph_pub.as_bytes()),
        offer_sdp_sha256: offer_hash.clone(),
        answer_sdp_sha256: answer_hash.clone(),
    };
    let envelope = identity
        .sign(payload)
        .map_err(|e| PeerSessionError::Attestation(format!("hello sign: {e}")))?;
    send_control(
        &dc,
        &ControlFrame::Hello(Box::new(HelloFrame {
            identity: identity.public_identity(),
            envelope,
        })),
    )
    .await?;

    let peer_eph = loop {
        match next_control(&mut control_rx, cfg.transfer_timeout, "teacher hello").await? {
            ControlFrame::Hello(frame) => {
                match verify_peer_hello(
                    &frame.identity,
                    &frame.envelope,
                    &teacher_fp,
                    session_id,
                    "teacher",
                    &offer_hash,
                    &answer_hash,
                ) {
                    Ok(pk) => break pk,
                    Err(e) => {
                        // Tell the teacher (zero pack bytes have flowed) and abort.
                        let _ = send_control(
                            &dc,
                            &ControlFrame::Failed {
                                error: e.to_string(),
                            },
                        )
                        .await;
                        close_quietly(&pc).await;
                        return Err(e);
                    }
                }
            }
            _ => continue,
        }
    };
    let ikm = dh_ikm(eph_secret, &peer_eph);

    // 4. Arm the receiver with the channel-attested fingerprint pair, then
    //    tell the teacher to stream.
    let attestation = ChannelAttestation {
        attested_teacher: PublicKeyFingerprint(teacher_fp),
        attested_learner: my_fp,
        remote_id,
    };
    let receiver = Arc::new(PackReceiver::new(
        store,
        ikm,
        attestation,
        CipherFloor::default(),
        AssemblerLimits {
            max_chunks: 8192,
            max_total_bytes: cfg.max_pack_bytes,
            max_frame_payload: cfg.max_frame_payload,
        },
        cfg.max_frame_payload,
    ));
    router.arm(receiver.clone());
    send_control(&dc, &ControlFrame::Ready).await?;

    // 5. Wait for the pack to apply (bounded), then send the receipt.
    let start = tokio::time::Instant::now();
    let health = loop {
        let health = receiver.health();
        if health.packs_applied >= 1 {
            break health;
        }
        if health.packs_failed >= 1 {
            let error = health
                .last_error
                .clone()
                .unwrap_or_else(|| "pack apply failed".to_string());
            let _ = send_control(
                &dc,
                &ControlFrame::Failed {
                    error: error.clone(),
                },
            )
            .await;
            close_quietly(&pc).await;
            return Err(PeerSessionError::LearnerFailed(error));
        }
        if start.elapsed() > cfg.transfer_timeout {
            close_quietly(&pc).await;
            return Err(PeerSessionError::Timeout("pack transfer"));
        }
        tokio::time::sleep(cfg.poll_interval).await;
    };
    send_control(&dc, &ControlFrame::Applied).await?;
    close_quietly(&pc).await;
    Ok(health)
}

/// A `PackApplyStore` for the teacher-side router type parameter — the teacher
/// never receives pack frames; if one arrives it fails loud.
pub struct NullApplyStore;

impl PackApplyStore for NullApplyStore {
    async fn last_applied_version(&self, _pack_id: Uuid) -> anyhow::Result<Option<u64>> {
        anyhow::bail!("teacher side never applies packs")
    }
    async fn stage(&self, _staged: &crate::learner_ingest::StagedPack) -> anyhow::Result<()> {
        anyhow::bail!("teacher side never applies packs")
    }
    async fn flip(&self, _staged: &crate::learner_ingest::StagedPack) -> anyhow::Result<()> {
        anyhow::bail!("teacher side never applies packs")
    }
}

/// Wait for a data channel to be open, immune to the lost-notification race:
/// `Notify::notify_waiters` only wakes CURRENT waiters, and SCTP can open the
/// channel before the driver reaches its await — so subscribe first, then
/// check `ready_state`, then wait.
async fn wait_channel_open(
    dc: &Arc<RTCDataChannel>,
    opened: &Arc<Notify>,
    deadline: Duration,
) -> bool {
    let start = tokio::time::Instant::now();
    loop {
        let notified = opened.notified();
        if dc.ready_state() == RTCDataChannelState::Open {
            return true;
        }
        let remaining = match deadline.checked_sub(start.elapsed()) {
            Some(r) if !r.is_zero() => r,
            _ => return false,
        };
        // Re-check state at least every 250ms in case the open notification
        // fired on a handler registered after the state change.
        let slice = remaining.min(Duration::from_millis(250));
        let _ = tokio::time::timeout(slice, notified).await;
    }
}

async fn close_quietly(pc: &Arc<RTCPeerConnection>) {
    if let Err(e) = pc.close().await {
        tracing::debug!(error = %e, "peer connection close");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrosa_memory_core::remote_identity::InstanceId;

    fn identity(seed: u128) -> InstanceSigningIdentity {
        InstanceSigningIdentity::generate(InstanceId(Uuid::from_u128(seed)))
    }

    fn hello(
        id: &InstanceSigningIdentity,
        session: Uuid,
        role: &str,
        eph: &x25519_dalek::PublicKey,
        offer_hash: &str,
        answer_hash: &str,
    ) -> (InstancePublicIdentity, SignedEnvelope<HelloPayload>) {
        let payload = HelloPayload {
            session_id: session,
            role: role.into(),
            eph_pub_hex: hex32(eph.as_bytes()),
            offer_sdp_sha256: offer_hash.into(),
            answer_sdp_sha256: answer_hash.into(),
        };
        let envelope = id.sign(payload).expect("sign");
        (id.public_identity(), envelope)
    }

    #[test]
    fn hello_verification_accepts_the_genuine_peer() {
        let session = Uuid::new_v4();
        let learner = identity(1);
        let (eph_secret, eph_pub) = fresh_ephemeral();
        let vouched = learner.public_identity().public_key_fingerprint.0;
        let (pid, env) = hello(&learner, session, "learner", &eph_pub, "oh", "ah");
        let got = verify_peer_hello(&pid, &env, &vouched, session, "learner", "oh", "ah")
            .expect("verify");
        assert_eq!(got.as_bytes(), eph_pub.as_bytes());
        // And the DH agrees from both directions.
        let (peer_secret, peer_pub) = fresh_ephemeral();
        let a = dh_ikm(eph_secret, &peer_pub);
        let b = dh_ikm(peer_secret, &eph_pub);
        assert_eq!(a.expose(), b.expose());
    }

    #[test]
    fn hello_verification_rejects_wrong_identity_session_role_and_binding() {
        let session = Uuid::new_v4();
        let learner = identity(1);
        let mallory = identity(2);
        let (_, eph_pub) = fresh_ephemeral();
        let vouched = learner.public_identity().public_key_fingerprint.0;

        // Mallory's identity does not hash to the vouched fingerprint.
        let (pid, env) = hello(&mallory, session, "learner", &eph_pub, "oh", "ah");
        assert!(matches!(
            verify_peer_hello(&pid, &env, &vouched, session, "learner", "oh", "ah"),
            Err(PeerSessionError::Attestation(_))
        ));

        // Wrong session.
        let (pid, env) = hello(&learner, Uuid::new_v4(), "learner", &eph_pub, "oh", "ah");
        assert!(matches!(
            verify_peer_hello(&pid, &env, &vouched, session, "learner", "oh", "ah"),
            Err(PeerSessionError::Attestation(_))
        ));

        // Wrong role.
        let (pid, env) = hello(&learner, session, "teacher", &eph_pub, "oh", "ah");
        assert!(matches!(
            verify_peer_hello(&pid, &env, &vouched, session, "learner", "oh", "ah"),
            Err(PeerSessionError::Attestation(_))
        ));

        // SDP binding mismatch (a DTLS-terminating middlebox).
        let (pid, env) = hello(&learner, session, "learner", &eph_pub, "OTHER", "ah");
        assert!(matches!(
            verify_peer_hello(&pid, &env, &vouched, session, "learner", "oh", "ah"),
            Err(PeerSessionError::Attestation(_))
        ));

        // Tampered payload breaks the signature.
        let (pid, mut env) = hello(&learner, session, "learner", &eph_pub, "oh", "ah");
        env.payload.answer_sdp_sha256 = "tampered".into();
        assert!(matches!(
            verify_peer_hello(&pid, &env, &vouched, session, "learner", "oh", "ah"),
            Err(PeerSessionError::Attestation(_))
        ));
    }

    #[test]
    fn ephemeral_key_hex_roundtrips_and_rejects_garbage() {
        let (_, pk) = fresh_ephemeral();
        let hex = hex32(pk.as_bytes());
        let back = parse_hex32(&hex).expect("parse");
        assert_eq!(&back, pk.as_bytes());
        assert!(parse_hex32("zz").is_err());
        assert!(parse_hex32(&"zz".repeat(32)).is_err());
    }
}
