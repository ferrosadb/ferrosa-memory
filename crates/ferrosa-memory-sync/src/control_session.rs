//! Signed, bounded WebRTC binding for the separate Ferrosa mobile control path.
//!
//! Correctness: the peer identity is bound to the exact SDP pair, inbound
//! queues and frames are bounded, and typed commands persist before execution.
//! Last revised: 2026-08-19
//! Last changed: Added idempotent agent-launch dispatch and durable completion.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use ferrosa_memory_core::control_store::{
    CommandInsert, ControlCommand, ControlCommandState, ControlCommandUpdate, ControlEventDraft,
    ControlStore, MAX_CONTROL_REPLAY_EVENTS,
};
use ferrosa_memory_core::remote_identity::{
    InstancePublicIdentity, InstanceSigningIdentity, SignedEnvelope,
};
use ferrosa_memory_core::types::TenantContext;
use serde::{Deserialize, Serialize};
use sha2_kdf::{Digest, Sha256};
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;
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

use crate::codex_runtime::{CodexRunResult, CodexTmuxRuntime, MAX_CODEX_INSTRUCTION_BYTES};
use crate::pack_crypto::Secret;
use crate::signaling_client::{BrokerSignal, ControlSignalingApi, SignalingClientError};

/// Exact RTC data-channel label for interactive control traffic.
pub const CONTROL_CHANNEL_LABEL: &str = "ferrosa-control-v1";
/// First supported signed binding and application protocol version.
pub const CONTROL_PROTOCOL_VERSION: u16 = 1;
/// Bound on capability identifiers carried in the signed hello.
pub const MAX_CONTROL_CAPABILITIES: usize = 32;
/// Maximum application or binding frame accepted from the data channel.
pub const MAX_CONTROL_FRAME_BYTES: usize = 64 * 1024;
/// Maximum frames retained for a temporarily slow local consumer.
pub const MAX_CONTROL_INBOUND_FRAMES: usize = 256;

/// Output text bound leaves room for JSON keys and identifiers inside the
/// control store's 16 KiB payload limit.
const MAX_AGENT_RESULT_TEXT_BYTES: usize = 12 * 1024;

/// Direct-first WebRTC control-session bounds.
#[derive(Debug, Clone)]
pub struct ControlSessionConfig {
    /// STUN servers used for direct ICE gathering. TURN URLs are not accepted
    /// on this unpaid direct path.
    pub stun_urls: Vec<String>,
    /// Bound on acceptance, SDP exchange, ICE, DTLS, and channel open.
    pub connect_timeout: Duration,
    /// Bound on the first signed hello exchange.
    pub bind_timeout: Duration,
    /// Gateway signaling poll interval.
    pub poll_interval: Duration,
    /// Include host loopback candidates for the ignored real-RTC test only.
    pub allow_loopback: bool,
    /// Maximum complete text frame.
    pub max_frame_bytes: usize,
    /// Bounded inbound queue capacity.
    pub inbound_capacity: usize,
}

impl Default for ControlSessionConfig {
    fn default() -> Self {
        Self {
            stun_urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            connect_timeout: Duration::from_secs(30),
            bind_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(250),
            allow_loopback: false,
            max_frame_bytes: MAX_CONTROL_FRAME_BYTES,
            inbound_capacity: MAX_CONTROL_INBOUND_FRAMES,
        }
    }
}

/// Authenticated role of one control-channel peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlPeerRole {
    /// Native mobile device directing work.
    Controller,
    /// Ferrosa Memory process on the selected development machine.
    Server,
}

/// Signed statement binding identity, broker session, SDP pair, version, role,
/// and ephemeral key to the exact control data channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlHelloPayload {
    /// Ephemeral gateway session this channel belongs to.
    pub session_id: Uuid,
    /// Sender's authenticated peer role.
    pub role: ControlPeerRole,
    /// Application protocol version selected by the sender.
    pub protocol_version: u16,
    /// Must be exactly [`CONTROL_CHANNEL_LABEL`].
    pub channel_label: String,
    /// Sender's ephemeral X25519 public key as 64 lowercase hex characters.
    pub eph_pub_hex: String,
    /// SHA-256 of the SDP offer observed by this peer.
    pub offer_sdp_sha256: String,
    /// SHA-256 of the SDP answer observed by this peer.
    pub answer_sdp_sha256: String,
    /// Bounded capability identifiers supported by this peer.
    pub capabilities: Vec<String>,
}

/// First frame sent over a control data channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlHelloFrame {
    /// Sender's public signing identity.
    pub identity: InstancePublicIdentity,
    /// Signed channel-binding statement.
    pub envelope: SignedEnvelope<ControlHelloPayload>,
}

/// Fail-closed signed binding errors.
#[derive(Debug, thiserror::Error)]
pub enum ControlSessionError {
    /// Gateway signaling failed closed.
    #[error("control signaling failed: {0}")]
    Signaling(#[from] SignalingClientError),
    /// Broker or signed-identity binding did not match this channel.
    #[error("control channel attestation failed: {0}")]
    Attestation(String),
    /// WebRTC stack operation failed.
    #[error("control WebRTC failed: {0}")]
    Rtc(String),
    /// A bounded acceptance, connection, or bind wait elapsed.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    /// Peer violated the first-frame or text-only protocol.
    #[error("control protocol violation: {0}")]
    Protocol(String),
    /// Local consumer did not keep up with the bounded inbound queue.
    #[error("control inbound queue is full")]
    Backpressure,
    /// A frame exceeds the negotiated hard size limit.
    #[error("control frame is {actual} bytes and exceeds {limit}")]
    FrameTooLarge { actual: usize, limit: usize },
    /// Peer closed before the next expected frame.
    #[error("control data channel closed")]
    ChannelClosed,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
enum BindingFrame {
    Hello { hello: Box<ControlHelloFrame> },
}

/// Successfully signed and bound direct control data channel.
pub struct BoundControlChannel {
    peer_connection: Arc<RTCPeerConnection>,
    data_channel: Arc<RTCDataChannel>,
    inbound: mpsc::Receiver<Vec<u8>>,
    session_key: Secret,
    max_frame_bytes: usize,
}

impl BoundControlChannel {
    /// Send one complete UTF-8 application frame with an explicit size bound.
    pub async fn send_text(&self, text: &str) -> Result<(), ControlSessionError> {
        if text.len() > self.max_frame_bytes {
            return Err(ControlSessionError::FrameTooLarge {
                actual: text.len(),
                limit: self.max_frame_bytes,
            });
        }
        self.data_channel
            .send_text(text.to_owned())
            .await
            .map(|_| ())
            .map_err(|error| ControlSessionError::Rtc(format!("send text: {error}")))
    }

    /// Receive one complete post-binding UTF-8 application frame.
    pub async fn recv_text(&mut self) -> Result<String, ControlSessionError> {
        let bytes = self
            .inbound
            .recv()
            .await
            .ok_or(ControlSessionError::ChannelClosed)?;
        String::from_utf8(bytes)
            .map_err(|_| ControlSessionError::Protocol("frame is not UTF-8".to_owned()))
    }

    /// Length of the signed ephemeral shared key without exposing it.
    pub fn session_key_len(&self) -> usize {
        self.session_key.len()
    }

    /// The underlying peer connection.
    ///
    /// Exposed so a caller can attach things this crate does not model — a
    /// media track, for one. Deliberately returns the connection rather than
    /// growing an `add_video_track` method here: the moment this crate has a
    /// method with "video" in the name, media has become a capability of a
    /// repository that is not supposed to have one.
    ///
    /// A caller that adds a track owns the renegotiation that follows.
    pub fn peer_connection(&self) -> Arc<RTCPeerConnection> {
        Arc::clone(&self.peer_connection)
    }

    /// Close the direct peer connection.
    pub async fn close(&self) -> Result<(), ControlSessionError> {
        self.peer_connection
            .close()
            .await
            .map_err(|error| ControlSessionError::Rtc(format!("close: {error}")))
    }
}

/// Verify the peer's signed hello and return its ephemeral X25519 public key.
pub fn verify_control_hello(
    identity: &InstancePublicIdentity,
    envelope: &SignedEnvelope<ControlHelloPayload>,
    vouched_fingerprint: &str,
    session_id: Uuid,
    expected_role: ControlPeerRole,
    offer_sdp_sha256: &str,
    answer_sdp_sha256: &str,
) -> Result<x25519_dalek::PublicKey, ControlSessionError> {
    if identity.public_key_fingerprint.0 != vouched_fingerprint {
        return Err(ControlSessionError::Attestation(
            "public identity does not match broker-vouched fingerprint".to_owned(),
        ));
    }
    envelope
        .verify(identity)
        .map_err(|error| ControlSessionError::Attestation(format!("signature invalid: {error}")))?;
    let payload = &envelope.payload;
    if payload.session_id != session_id {
        return Err(ControlSessionError::Attestation(
            "hello is bound to another broker session".to_owned(),
        ));
    }
    if payload.role != expected_role {
        return Err(ControlSessionError::Attestation(
            "peer role does not match channel direction".to_owned(),
        ));
    }
    if payload.protocol_version != CONTROL_PROTOCOL_VERSION {
        return Err(ControlSessionError::Attestation(format!(
            "unsupported protocol version {}",
            payload.protocol_version
        )));
    }
    if payload.channel_label != CONTROL_CHANNEL_LABEL {
        return Err(ControlSessionError::Attestation(
            "unexpected data-channel label".to_owned(),
        ));
    }
    if payload.offer_sdp_sha256 != offer_sdp_sha256
        || payload.answer_sdp_sha256 != answer_sdp_sha256
    {
        return Err(ControlSessionError::Attestation(
            "SDP hash mismatch".to_owned(),
        ));
    }
    if payload.capabilities.len() > MAX_CONTROL_CAPABILITIES
        || payload
            .capabilities
            .iter()
            .any(|capability| capability.is_empty() || capability.len() > 64)
    {
        return Err(ControlSessionError::Attestation(
            "invalid capability set".to_owned(),
        ));
    }
    let bytes = parse_hex_32(&payload.eph_pub_hex)?;
    Ok(x25519_dalek::PublicKey::from(bytes))
}

fn parse_hex_32(value: &str) -> Result<[u8; 32], ControlSessionError> {
    if value.len() != 64 {
        return Err(ControlSessionError::Attestation(
            "ephemeral key must be 64 hex characters".to_owned(),
        ));
    }
    let mut output = [0_u8; 32];
    // The length check above guarantees 32 whole pairs and an empty remainder;
    // `as_chunks` yields `&[u8; 2]`, so the pair length is known at compile time.
    let (pairs, _remainder) = value.as_bytes().as_chunks::<2>();
    for (index, pair) in pairs.iter().enumerate() {
        let high = hex_value(pair.first().copied()).ok_or_else(|| {
            ControlSessionError::Attestation("ephemeral key must be hexadecimal".to_owned())
        })?;
        let low = hex_value(pair.get(1).copied()).ok_or_else(|| {
            ControlSessionError::Attestation("ephemeral key must be hexadecimal".to_owned())
        })?;
        if let Some(slot) = output.get_mut(index) {
            *slot = (high << 4) | low;
        }
    }
    Ok(output)
}

fn hex_value(value: Option<u8>) -> Option<u8> {
    match value? {
        value @ b'0'..=b'9' => Some(value - b'0'),
        value @ b'a'..=b'f' => Some(value - b'a' + 10),
        value @ b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

/// Establish the offerer/controller half of a previously created gateway
/// session, then return only after the signed server hello verifies.
pub async fn run_control_controller_session<S: ControlSignalingApi>(
    api: &S,
    identity: &InstanceSigningIdentity,
    session_id: Uuid,
    config: &ControlSessionConfig,
) -> Result<BoundControlChannel, ControlSessionError> {
    validate_config(config)?;
    let own_fingerprint = identity.public_identity().public_key_fingerprint.0;
    let view = wait_for_acceptance(api, session_id, &own_fingerprint, config).await?;
    if view.controller_fingerprint != own_fingerprint {
        return Err(ControlSessionError::Attestation(
            "gateway session is vouched for another controller".to_owned(),
        ));
    }
    let server_fingerprint = view.server_fingerprint.ok_or_else(|| {
        ControlSessionError::Attestation("accepted session lacks server fingerprint".to_owned())
    })?;

    let rtc_api = build_rtc_api(config)?;
    let peer_connection = Arc::new(
        rtc_api
            .new_peer_connection(rtc_config(config))
            .await
            .map_err(|error| ControlSessionError::Rtc(format!("peer connection: {error}")))?,
    );
    let data_channel = peer_connection
        .create_data_channel(CONTROL_CHANNEL_LABEL, None)
        .await
        .map_err(|error| ControlSessionError::Rtc(format!("create data channel: {error}")))?;
    let opened = install_open_notification(&data_channel);
    let mut inbound = install_bounded_inbound(
        &data_channel,
        config.inbound_capacity,
        config.max_frame_bytes,
    );

    let mut gather = peer_connection.gathering_complete_promise().await;
    let offer = peer_connection
        .create_offer(None)
        .await
        .map_err(|error| ControlSessionError::Rtc(format!("create offer: {error}")))?;
    peer_connection
        .set_local_description(offer)
        .await
        .map_err(|error| ControlSessionError::Rtc(format!("set local offer: {error}")))?;
    let _ = gather.recv().await;
    let local = peer_connection
        .local_description()
        .await
        .ok_or_else(|| ControlSessionError::Rtc("missing local offer".to_owned()))?;
    api.control_post_signal(
        session_id,
        &own_fingerprint,
        &BrokerSignal::SdpOffer(local.sdp.clone()),
    )
    .await?;

    let answer_sdp = poll_control_signal(
        api,
        session_id,
        &own_fingerprint,
        config,
        "SDP answer",
        |signal| match signal {
            BrokerSignal::SdpAnswer(value) => Some(value),
            _ => None,
        },
    )
    .await?;
    let answer = RTCSessionDescription::answer(answer_sdp.clone())
        .map_err(|error| ControlSessionError::Rtc(format!("answer SDP: {error}")))?;
    peer_connection
        .set_remote_description(answer)
        .await
        .map_err(|error| ControlSessionError::Rtc(format!("set remote answer: {error}")))?;
    wait_channel_open(&data_channel, &opened, config.connect_timeout).await?;

    let offer_hash = sha256_hex(local.sdp.as_bytes());
    let answer_hash = sha256_hex(answer_sdp.as_bytes());
    let (ephemeral_secret, ephemeral_public) = fresh_ephemeral();
    let hello = signed_hello(
        identity,
        session_id,
        ControlPeerRole::Controller,
        &ephemeral_public,
        &offer_hash,
        &answer_hash,
    )?;
    send_binding_hello(&data_channel, &hello, config.max_frame_bytes).await?;
    let peer_hello = recv_binding_hello(&mut inbound, config.bind_timeout).await?;
    let peer_public = verify_control_hello(
        &peer_hello.identity,
        &peer_hello.envelope,
        &server_fingerprint,
        session_id,
        ControlPeerRole::Server,
        &offer_hash,
        &answer_hash,
    )?;
    let session_key = derive_session_key(ephemeral_secret, &peer_public);

    Ok(BoundControlChannel {
        peer_connection,
        data_channel,
        inbound,
        session_key,
        max_frame_bytes: config.max_frame_bytes,
    })
}

/// Accept and establish the exact-target Ferrosa Memory server half, returning
/// only after the controller's first signed hello verifies.
/// Run the server half of one control session.
///
/// `rtc` is built by the CALLER and used here without inspection. That is the
/// point: a caller that needs media registers its own codecs and hands the
/// result in, and this crate never names a codec, a track or a media engine.
/// Media is not a capability of this repository.
///
/// `None` builds the data-channel-only API this crate has always used, so a
/// caller that does not care negotiates exactly what it did before.
pub async fn run_control_server_session<S: ControlSignalingApi>(
    api: &S,
    identity: &InstanceSigningIdentity,
    session_id: Uuid,
    config: &ControlSessionConfig,
    rtc: Option<Arc<webrtc::api::API>>,
) -> Result<BoundControlChannel, ControlSessionError> {
    validate_config(config)?;
    let own_fingerprint = identity.public_identity().public_key_fingerprint.0;
    let view = api.control_accept(session_id, &own_fingerprint).await?;
    if view.server_fingerprint.as_deref() != Some(own_fingerprint.as_str()) {
        return Err(ControlSessionError::Attestation(
            "gateway session is vouched for another server".to_owned(),
        ));
    }
    let controller_fingerprint = view.controller_fingerprint;

    // Caller-supplied when the caller needs something this crate does not
    // provide; otherwise the data-channel-only default.
    let rtc_api = match rtc {
        Some(rtc) => rtc,
        None => Arc::new(build_rtc_api(config)?),
    };
    let peer_connection = Arc::new(
        rtc_api
            .new_peer_connection(rtc_config(config))
            .await
            .map_err(|error| ControlSessionError::Rtc(format!("peer connection: {error}")))?,
    );
    let (channel_sender, mut channel_receiver) = mpsc::channel(1);
    let inbound_capacity = config.inbound_capacity;
    let max_frame_bytes = config.max_frame_bytes;
    peer_connection.on_data_channel(Box::new(move |data_channel: Arc<RTCDataChannel>| {
        let channel_sender = channel_sender.clone();
        Box::pin(async move {
            if data_channel.label() != CONTROL_CHANNEL_LABEL {
                let _ = data_channel.close().await;
                return;
            }
            let opened = install_open_notification(&data_channel);
            let inbound = install_bounded_inbound(&data_channel, inbound_capacity, max_frame_bytes);
            let _ = channel_sender.try_send((data_channel, opened, inbound));
        })
    }));

    let offer_sdp = poll_control_signal(
        api,
        session_id,
        &own_fingerprint,
        config,
        "SDP offer",
        |signal| match signal {
            BrokerSignal::SdpOffer(value) => Some(value),
            _ => None,
        },
    )
    .await?;
    tracing::info!(
        candidate_count = offer_sdp
            .lines()
            .filter(|line| line.starts_with("a=candidate:"))
            .count(),
        "received gathered control offer"
    );
    let offer = RTCSessionDescription::offer(offer_sdp.clone())
        .map_err(|error| ControlSessionError::Rtc(format!("offer SDP: {error}")))?;
    peer_connection
        .set_remote_description(offer)
        .await
        .map_err(|error| ControlSessionError::Rtc(format!("set remote offer: {error}")))?;
    let mut gather = peer_connection.gathering_complete_promise().await;
    let answer = peer_connection
        .create_answer(None)
        .await
        .map_err(|error| ControlSessionError::Rtc(format!("create answer: {error}")))?;
    peer_connection
        .set_local_description(answer)
        .await
        .map_err(|error| ControlSessionError::Rtc(format!("set local answer: {error}")))?;
    let _ = gather.recv().await;
    let local = peer_connection
        .local_description()
        .await
        .ok_or_else(|| ControlSessionError::Rtc("missing local answer".to_owned()))?;
    tracing::info!(
        candidate_count = local
            .sdp
            .lines()
            .filter(|line| line.starts_with("a=candidate:"))
            .count(),
        "sending gathered control answer"
    );
    api.control_post_signal(
        session_id,
        &own_fingerprint,
        &BrokerSignal::SdpAnswer(local.sdp.clone()),
    )
    .await?;

    let (data_channel, opened, mut inbound) =
        tokio::time::timeout(config.connect_timeout, channel_receiver.recv())
            .await
            .map_err(|_| ControlSessionError::Timeout("control data channel"))?
            .ok_or(ControlSessionError::ChannelClosed)?;
    wait_channel_open(&data_channel, &opened, config.connect_timeout).await?;

    let offer_hash = sha256_hex(offer_sdp.as_bytes());
    let answer_hash = sha256_hex(local.sdp.as_bytes());
    let peer_hello = recv_binding_hello(&mut inbound, config.bind_timeout).await?;
    let peer_public = verify_control_hello(
        &peer_hello.identity,
        &peer_hello.envelope,
        &controller_fingerprint,
        session_id,
        ControlPeerRole::Controller,
        &offer_hash,
        &answer_hash,
    )?;
    let (ephemeral_secret, ephemeral_public) = fresh_ephemeral();
    let hello = signed_hello(
        identity,
        session_id,
        ControlPeerRole::Server,
        &ephemeral_public,
        &offer_hash,
        &answer_hash,
    )?;
    send_binding_hello(&data_channel, &hello, config.max_frame_bytes).await?;
    let session_key = derive_session_key(ephemeral_secret, &peer_public);

    Ok(BoundControlChannel {
        peer_connection,
        data_channel,
        inbound,
        session_key,
        max_frame_bytes: config.max_frame_bytes,
    })
}

fn validate_config(config: &ControlSessionConfig) -> Result<(), ControlSessionError> {
    if config.max_frame_bytes == 0 || config.max_frame_bytes > MAX_CONTROL_FRAME_BYTES {
        return Err(ControlSessionError::Protocol(
            "invalid control frame limit".to_owned(),
        ));
    }
    if config.inbound_capacity == 0 || config.inbound_capacity > MAX_CONTROL_INBOUND_FRAMES {
        return Err(ControlSessionError::Protocol(
            "invalid control inbound capacity".to_owned(),
        ));
    }
    if config.stun_urls.iter().any(|url| !url.starts_with("stun:")) {
        return Err(ControlSessionError::Protocol(
            "direct control path accepts STUN URLs only".to_owned(),
        ));
    }
    Ok(())
}

async fn wait_for_acceptance<S: ControlSignalingApi>(
    api: &S,
    session_id: Uuid,
    fingerprint: &str,
    config: &ControlSessionConfig,
) -> Result<crate::signaling_client::ControlBrokerSessionView, ControlSessionError> {
    let start = tokio::time::Instant::now();
    loop {
        let view = api.control_session(session_id, fingerprint).await?;
        if view.is_accepted() {
            return Ok(view);
        }
        if start.elapsed() >= config.connect_timeout {
            return Err(ControlSessionError::Timeout("target server acceptance"));
        }
        tokio::time::sleep(config.poll_interval).await;
    }
}

async fn poll_control_signal<S, T>(
    api: &S,
    session_id: Uuid,
    fingerprint: &str,
    config: &ControlSessionConfig,
    description: &'static str,
    mut select: impl FnMut(BrokerSignal) -> Option<T>,
) -> Result<T, ControlSessionError>
where
    S: ControlSignalingApi,
{
    let start = tokio::time::Instant::now();
    loop {
        for signal in api.control_take_signals(session_id, fingerprint).await? {
            if let Some(value) = select(signal) {
                return Ok(value);
            }
        }
        if start.elapsed() >= config.connect_timeout {
            return Err(ControlSessionError::Timeout(description));
        }
        tokio::time::sleep(config.poll_interval).await;
    }
}

fn build_rtc_api(config: &ControlSessionConfig) -> Result<webrtc::api::API, ControlSessionError> {
    use webrtc::api::setting_engine::SettingEngine;
    use webrtc::ice::mdns::MulticastDnsMode;
    use webrtc::ice::network_type::NetworkType;

    // The workspace enables both Rustls providers through independent HTTP
    // and WebRTC dependencies, so auto-selection deliberately refuses. Ring
    // is already the provider used by the real RTC loopback tests; installing
    // it is process-global and idempotent (an existing provider wins).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut media = MediaEngine::default();
    let registry = register_default_interceptors(Registry::new(), &mut media)
        .map_err(|error| ControlSessionError::Rtc(format!("interceptors: {error}")))?;
    let mut settings = SettingEngine::default();
    settings.set_ice_multicast_dns_mode(MulticastDnsMode::Disabled);
    settings.set_network_types(vec![NetworkType::Udp4]);
    if config.allow_loopback {
        settings.set_include_loopback_candidate(true);
    }
    Ok(APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(settings)
        .build())
}

fn rtc_config(config: &ControlSessionConfig) -> RTCConfiguration {
    RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: config.stun_urls.clone(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn install_open_notification(data_channel: &Arc<RTCDataChannel>) -> Arc<Notify> {
    let opened = Arc::new(Notify::new());
    let notification = opened.clone();
    data_channel.on_open(Box::new(move || {
        let notification = notification.clone();
        Box::pin(async move {
            notification.notify_waiters();
        })
    }));
    opened
}

fn install_bounded_inbound(
    data_channel: &Arc<RTCDataChannel>,
    capacity: usize,
    max_frame_bytes: usize,
) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel(capacity);
    let channel = data_channel.clone();
    data_channel.on_message(Box::new(move |message: DataChannelMessage| {
        let sender = sender.clone();
        let channel = channel.clone();
        Box::pin(async move {
            if !message.is_string || message.data.len() > max_frame_bytes {
                let _ = channel.close().await;
                return;
            }
            if sender.try_send(message.data.to_vec()).is_err() {
                let _ = channel.close().await;
            }
        })
    }));
    receiver
}

async fn wait_channel_open(
    data_channel: &Arc<RTCDataChannel>,
    opened: &Arc<Notify>,
    deadline: Duration,
) -> Result<(), ControlSessionError> {
    let start = tokio::time::Instant::now();
    loop {
        let notified = opened.notified();
        if data_channel.ready_state() == RTCDataChannelState::Open {
            return Ok(());
        }
        let remaining = deadline
            .checked_sub(start.elapsed())
            .filter(|value| !value.is_zero())
            .ok_or(ControlSessionError::Timeout("control data channel open"))?;
        let _ = tokio::time::timeout(remaining.min(Duration::from_millis(250)), notified).await;
    }
}

fn signed_hello(
    identity: &InstanceSigningIdentity,
    session_id: Uuid,
    role: ControlPeerRole,
    ephemeral_public: &x25519_dalek::PublicKey,
    offer_hash: &str,
    answer_hash: &str,
) -> Result<ControlHelloFrame, ControlSessionError> {
    let capabilities = match role {
        ControlPeerRole::Controller => vec![
            "agent_control".to_owned(),
            "memory_read".to_owned(),
            "approval_decide".to_owned(),
        ],
        ControlPeerRole::Server => vec![
            "agent_control".to_owned(),
            "memory_read".to_owned(),
            "approval_decide".to_owned(),
            "durable_resume".to_owned(),
        ],
    };
    let payload = ControlHelloPayload {
        session_id,
        role,
        protocol_version: CONTROL_PROTOCOL_VERSION,
        channel_label: CONTROL_CHANNEL_LABEL.to_owned(),
        eph_pub_hex: hex_32(ephemeral_public.as_bytes()),
        offer_sdp_sha256: offer_hash.to_owned(),
        answer_sdp_sha256: answer_hash.to_owned(),
        capabilities,
    };
    let envelope = identity.sign(payload).map_err(|error| {
        ControlSessionError::Attestation(format!("hello signing failed: {error}"))
    })?;
    Ok(ControlHelloFrame {
        identity: identity.public_identity(),
        envelope,
    })
}

async fn send_binding_hello(
    data_channel: &Arc<RTCDataChannel>,
    hello: &ControlHelloFrame,
    max_frame_bytes: usize,
) -> Result<(), ControlSessionError> {
    let encoded = serde_json::to_string(&BindingFrame::Hello {
        hello: Box::new(hello.clone()),
    })
    .map_err(|error| ControlSessionError::Protocol(format!("hello encode: {error}")))?;
    if encoded.len() > max_frame_bytes {
        return Err(ControlSessionError::FrameTooLarge {
            actual: encoded.len(),
            limit: max_frame_bytes,
        });
    }
    data_channel
        .send_text(encoded)
        .await
        .map(|_| ())
        .map_err(|error| ControlSessionError::Rtc(format!("send hello: {error}")))
}

async fn recv_binding_hello(
    inbound: &mut mpsc::Receiver<Vec<u8>>,
    deadline: Duration,
) -> Result<ControlHelloFrame, ControlSessionError> {
    let bytes = tokio::time::timeout(deadline, inbound.recv())
        .await
        .map_err(|_| ControlSessionError::Timeout("signed peer hello"))?
        .ok_or(ControlSessionError::ChannelClosed)?;
    let frame: BindingFrame = serde_json::from_slice(&bytes)
        .map_err(|error| ControlSessionError::Protocol(format!("first frame: {error}")))?;
    match frame {
        BindingFrame::Hello { hello } => Ok(*hello),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        })
}

fn hex_32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write;

    bytes
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        })
}

fn fresh_ephemeral() -> ([u8; 32], x25519_dalek::PublicKey) {
    use rand::RngCore;

    let mut secret = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    let public = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(secret));
    (secret, public)
}

fn derive_session_key(local_secret: [u8; 32], peer_public: &x25519_dalek::PublicKey) -> Secret {
    let shared = x25519_dalek::StaticSecret::from(local_secret).diffie_hellman(peer_public);
    Secret::new(shared.as_bytes().to_vec())
}

/// Produce the version-1 pong for a mobile ping, or `None` for another valid
/// version-1 body that belongs to the command/snapshot dispatcher.
pub fn liveness_reply(input: &str) -> Result<Option<String>, ControlSessionError> {
    if input.len() > MAX_CONTROL_FRAME_BYTES {
        return Err(ControlSessionError::FrameTooLarge {
            actual: input.len(),
            limit: MAX_CONTROL_FRAME_BYTES,
        });
    }
    let value: serde_json::Value = serde_json::from_str(input)
        .map_err(|error| ControlSessionError::Protocol(format!("frame JSON: {error}")))?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| ControlSessionError::Protocol("missing version".to_owned()))?;
    if version != u64::from(CONTROL_PROTOCOL_VERSION) {
        return Err(ControlSessionError::Protocol(format!(
            "unsupported protocol version {version}"
        )));
    }
    let frame_id = value
        .get("frame_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| ControlSessionError::Protocol("invalid frame id".to_owned()))?;
    let body = value
        .get("body")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| ControlSessionError::Protocol("missing body".to_owned()))?;
    match body.get("type").and_then(serde_json::Value::as_str) {
        Some("ping") => {
            let nonce = body
                .get("nonce")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 256)
                .ok_or_else(|| ControlSessionError::Protocol("invalid ping nonce".to_owned()))?;
            serde_json::to_string(&serde_json::json!({
                "version": CONTROL_PROTOCOL_VERSION,
                "frame_id": frame_id,
                "body": {"type": "pong", "nonce": nonce},
            }))
            .map(Some)
            .map_err(|error| ControlSessionError::Protocol(format!("pong encode: {error}")))
        }
        Some(_) => Ok(None),
        None => Err(ControlSessionError::Protocol(
            "missing body type".to_owned(),
        )),
    }
}

/// Dispatch one post-binding application frame. Ping/pong remains ephemeral;
/// subscribe reads only the server-owned durable event log and returns a page
/// whose high-water cursor never advances past an undelivered bounded event.
pub async fn control_application_reply<S: ControlStore>(
    store: &S,
    ctx: &TenantContext,
    server_fingerprint: &str,
    input: &str,
) -> Result<Option<String>, ControlSessionError> {
    if let Some(reply) = liveness_reply(input)? {
        return Ok(Some(reply));
    }
    let value: serde_json::Value = serde_json::from_str(input)
        .map_err(|error| ControlSessionError::Protocol(format!("frame JSON: {error}")))?;
    let frame_id = value
        .get("frame_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| ControlSessionError::Protocol("invalid frame id".to_owned()))?;
    let body = value
        .get("body")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| ControlSessionError::Protocol("missing body".to_owned()))?;
    if body.get("type").and_then(serde_json::Value::as_str) != Some("subscribe") {
        return Err(ControlSessionError::Protocol(
            "unsupported control body type".to_owned(),
        ));
    }
    let after_cursor = match body.get("after_cursor") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            ControlSessionError::Protocol("after_cursor must be an unsigned integer".to_owned())
        })?),
    };
    let capabilities = body
        .get("capabilities")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ControlSessionError::Protocol("capabilities must be an array".to_owned()))?;
    if capabilities.len() > MAX_CONTROL_CAPABILITIES
        || capabilities.iter().any(|value| {
            !matches!(
                value.as_str(),
                Some("agent_control" | "memory_read" | "approval_decide")
            )
        })
    {
        return Err(ControlSessionError::Protocol(
            "unsupported or oversized capability set".to_owned(),
        ));
    }
    let page = store
        .events_after(
            ctx,
            server_fingerprint,
            after_cursor,
            MAX_CONTROL_REPLAY_EVENTS,
        )
        .await
        .map_err(|error| {
            ControlSessionError::Protocol(format!("durable replay failed: {error}"))
        })?;
    let events: Vec<_> = page
        .events
        .into_iter()
        .map(|event| {
            serde_json::json!({
                "cursor": event.cursor,
                "event_id": event.event_id.to_string(),
                "command_id": event.command_id.map(|value| value.to_string()),
                "kind": event.kind,
                "payload": event.payload,
                "created_at": event.created_at,
            })
        })
        .collect();
    serde_json::to_string(&serde_json::json!({
        "version": CONTROL_PROTOCOL_VERSION,
        "frame_id": frame_id,
        "body": {
            "type": "event_batch",
            "high_water_cursor": page.high_water_cursor,
            "events": events,
        },
    }))
    .map(Some)
    .map_err(|error| ControlSessionError::Protocol(format!("event batch encode: {error}")))
}

/// Runtime seam for typed agent commands. The control protocol never exposes
/// a generic process or shell API.
#[allow(clippy::manual_async_fn)]
pub trait AgentRuntime: Send + Sync + 'static {
    fn launch(
        &self,
        instruction: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    fn wait(&self) -> impl std::future::Future<Output = anyhow::Result<CodexRunResult>> + Send;
}

impl AgentRuntime for CodexTmuxRuntime {
    async fn launch(&self, instruction: &str) -> anyhow::Result<()> {
        CodexTmuxRuntime::launch(self, instruction)
            .await
            .map_err(anyhow::Error::from)
    }

    async fn wait(&self) -> anyhow::Result<CodexRunResult> {
        CodexTmuxRuntime::wait(self)
            .await
            .map_err(anyhow::Error::from)
    }
}

/// Dispatches mobile application frames into one durable store and one typed
/// agent runtime. Both are shared so terminal monitoring survives peer loss.
pub struct ControlRuntimeDispatcher<S, R> {
    store: Arc<S>,
    runtime: Arc<R>,
}

impl<S, R> ControlRuntimeDispatcher<S, R>
where
    S: ControlStore + 'static,
    R: AgentRuntime,
{
    pub fn new(store: Arc<S>, runtime: R) -> Self {
        Self {
            store,
            runtime: Arc::new(runtime),
        }
    }

    /// Reply to liveness, replay, or the first typed agent command. A duplicate
    /// command id returns the stored projection and never executes twice.
    pub async fn reply(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        input: &str,
    ) -> Result<Option<String>, ControlSessionError> {
        if let Some(reply) = liveness_reply(input)? {
            return Ok(Some(reply));
        }
        let value: serde_json::Value = serde_json::from_str(input)
            .map_err(|error| ControlSessionError::Protocol(format!("frame JSON: {error}")))?;
        let body_type = value
            .pointer("/body/type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ControlSessionError::Protocol("missing body type".to_owned()))?;
        if body_type != "command" {
            return control_application_reply(self.store.as_ref(), ctx, server_fingerprint, input)
                .await;
        }
        self.dispatch_command(ctx, server_fingerprint, &value).await
    }

    async fn dispatch_command(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        frame: &serde_json::Value,
    ) -> Result<Option<String>, ControlSessionError> {
        let frame_id = frame
            .get("frame_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 128)
            .ok_or_else(|| ControlSessionError::Protocol("invalid frame id".to_owned()))?;
        let command_id = frame
            .pointer("/body/command_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .filter(|value| value.get_version_num() == 7)
            .ok_or_else(|| {
                ControlSessionError::Protocol("command_id must be a UUIDv7".to_owned())
            })?;
        let command_type = frame
            .pointer("/body/command_type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ControlSessionError::Protocol("missing command_type".to_owned()))?;
        if command_type != "agent_launch" {
            return Err(ControlSessionError::Protocol(
                "unsupported command_type".to_owned(),
            ));
        }
        let payload = frame
            .pointer("/body/payload")
            .cloned()
            .ok_or_else(|| ControlSessionError::Protocol("missing command payload".to_owned()))?;
        let instruction = payload
            .get("instruction")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty() && value.len() <= MAX_CODEX_INSTRUCTION_BYTES)
            .ok_or_else(|| ControlSessionError::Protocol("invalid agent instruction".to_owned()))?
            .to_owned();
        let now = Utc::now();
        let command = ControlCommand {
            command_id,
            command_type: command_type.to_owned(),
            request: payload,
            state: ControlCommandState::Queued,
            result: None,
            result_cursor: None,
            created_at: now,
            updated_at: now,
        };
        let inserted = self
            .store
            .put_command_if_absent(ctx, server_fingerprint, &command)
            .await
            .map_err(control_store_error)?;
        if let CommandInsert::Duplicate(existing) = inserted {
            return command_result_reply(frame_id, &existing).map(Some);
        }

        let accepted_cursor = append_control_event(
            self.store.as_ref(),
            ctx,
            server_fingerprint,
            command_id,
            "command_accepted",
            serde_json::json!({"state": "queued", "command_type": command_type}),
        )
        .await?;
        if let Err(error) = self.runtime.launch(&instruction).await {
            let failed = persist_terminal_command(
                self.store.as_ref(),
                ctx,
                server_fingerprint,
                command_id,
                ControlCommandState::Failed,
                "agent_failed",
                serde_json::json!({"error": bounded_text(&error.to_string())}),
            )
            .await?;
            return command_result_reply(frame_id, &failed).map(Some);
        }
        let running = self
            .store
            .update_command(
                ctx,
                server_fingerprint,
                command_id,
                ControlCommandUpdate {
                    state: ControlCommandState::Running,
                    result: None,
                    result_cursor: None,
                    updated_at: Utc::now(),
                },
            )
            .await
            .map_err(control_store_error)?;

        let store = Arc::clone(&self.store);
        let runtime = Arc::clone(&self.runtime);
        let owned_ctx = ctx.clone();
        let owned_fingerprint = server_fingerprint.to_owned();
        tokio::spawn(async move {
            let completion = match runtime.wait().await {
                Ok(result) if result.success => {
                    let payload = serde_json::json!({
                        "thread_id": result.thread_id,
                        "message": result.final_message.map(|value| bounded_text(&value)),
                    });
                    persist_terminal_command(
                        store.as_ref(),
                        &owned_ctx,
                        &owned_fingerprint,
                        command_id,
                        ControlCommandState::Succeeded,
                        "agent_completed",
                        payload,
                    )
                    .await
                }
                Ok(result) => {
                    let payload = serde_json::json!({
                        "thread_id": result.thread_id,
                        "error": bounded_text(result.error.as_deref().unwrap_or("Codex turn failed")),
                    });
                    persist_terminal_command(
                        store.as_ref(),
                        &owned_ctx,
                        &owned_fingerprint,
                        command_id,
                        ControlCommandState::Failed,
                        "agent_failed",
                        payload,
                    )
                    .await
                }
                Err(error) => {
                    persist_terminal_command(
                        store.as_ref(),
                        &owned_ctx,
                        &owned_fingerprint,
                        command_id,
                        ControlCommandState::Failed,
                        "agent_failed",
                        serde_json::json!({"error": bounded_text(&error.to_string())}),
                    )
                    .await
                }
            };
            if let Err(error) = completion {
                tracing::error!(%command_id, %error, "persisting agent completion failed");
            }
        });

        command_result_reply_with_cursor(frame_id, &running, Some(accepted_cursor)).map(Some)
    }
}

async fn persist_terminal_command<S: ControlStore>(
    store: &S,
    ctx: &TenantContext,
    server_fingerprint: &str,
    command_id: Uuid,
    state: ControlCommandState,
    event_kind: &str,
    payload: serde_json::Value,
) -> Result<ControlCommand, ControlSessionError> {
    let cursor = append_control_event(
        store,
        ctx,
        server_fingerprint,
        command_id,
        event_kind,
        payload.clone(),
    )
    .await?;
    store
        .update_command(
            ctx,
            server_fingerprint,
            command_id,
            ControlCommandUpdate {
                state,
                result: Some(payload),
                result_cursor: Some(cursor),
                updated_at: Utc::now(),
            },
        )
        .await
        .map_err(control_store_error)
}

async fn append_control_event<S: ControlStore>(
    store: &S,
    ctx: &TenantContext,
    server_fingerprint: &str,
    command_id: Uuid,
    kind: &str,
    payload: serde_json::Value,
) -> Result<u64, ControlSessionError> {
    let cursor = store
        .reserve_cursor_block(ctx, server_fingerprint, 1)
        .await
        .map_err(control_store_error)?
        .start;
    store
        .append_event(
            ctx,
            server_fingerprint,
            ControlEventDraft {
                cursor,
                event_id: Uuid::now_v7(),
                command_id: Some(command_id),
                kind: kind.to_owned(),
                payload,
                created_at: Utc::now(),
            },
        )
        .await
        .map_err(control_store_error)?;
    Ok(cursor)
}

fn command_result_reply(
    frame_id: &str,
    command: &ControlCommand,
) -> Result<String, ControlSessionError> {
    command_result_reply_with_cursor(frame_id, command, None)
}

fn command_result_reply_with_cursor(
    frame_id: &str,
    command: &ControlCommand,
    accepted_cursor: Option<u64>,
) -> Result<String, ControlSessionError> {
    serde_json::to_string(&serde_json::json!({
        "version": CONTROL_PROTOCOL_VERSION,
        "frame_id": frame_id,
        "body": {
            "type": "command_result",
            "command_id": command.command_id,
            "state": command.state,
            "result": command.result,
            "result_cursor": command.result_cursor,
            "accepted_cursor": accepted_cursor,
        },
    }))
    .map_err(|error| ControlSessionError::Protocol(format!("command result encode: {error}")))
}

fn bounded_text(value: &str) -> String {
    if value.len() <= MAX_AGENT_RESULT_TEXT_BYTES {
        return value.to_owned();
    }
    let mut end = MAX_AGENT_RESULT_TEXT_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn control_store_error(error: anyhow::Error) -> ControlSessionError {
    ControlSessionError::Protocol(format!("durable control operation failed: {error}"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::Utc;
    use ferrosa_memory_core::control_store::{
        ControlCommandState, ControlEventDraft, ControlStore, InMemoryControlStore,
    };
    use ferrosa_memory_core::remote_identity::{InstanceId, InstanceSigningIdentity};
    use uuid::Uuid;

    use super::*;
    use crate::codex_runtime::CodexRunResult;

    #[derive(Clone)]
    struct FakeAgentRuntime {
        launches: Arc<AtomicUsize>,
    }

    impl AgentRuntime for FakeAgentRuntime {
        async fn launch(&self, _instruction: &str) -> anyhow::Result<()> {
            self.launches.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn wait(&self) -> anyhow::Result<CodexRunResult> {
            Ok(CodexRunResult {
                thread_id: Some("01a01dcf-3c01-76e1-96c8-59a2eb7637c9".to_owned()),
                final_message: Some("ferrosa-mobile proof of life".to_owned()),
                error: None,
                success: true,
                captured_jsonl: String::new(),
            })
        }
    }

    fn identity() -> InstanceSigningIdentity {
        InstanceSigningIdentity::generate(InstanceId::new())
    }

    /// `parse_hex_32` decodes every byte value, and rejects malformed input.
    ///
    /// Cover for the `as_chunks::<2>()` rewrite: the decoder walks fixed-size
    /// pairs now, so a mistake here would silently mis-decode an ephemeral key
    /// rather than fail to compile.
    #[test]
    fn parse_hex_32_roundtrips_every_byte_value_and_rejects_garbage() {
        let mut key = [0_u8; 32];
        for (i, slot) in key.iter_mut().enumerate() {
            // 0, 8, 16 ... 248 — spans the low and high nibble ranges.
            *slot = (i as u8).wrapping_mul(8);
        }
        let encoded = hex_32(&key);
        assert_eq!(encoded.len(), 64);
        assert_eq!(parse_hex_32(&encoded).expect("valid hex decodes"), key);

        assert!(parse_hex_32("").is_err(), "empty input");
        assert!(parse_hex_32(&encoded[..62]).is_err(), "too short");
        assert!(parse_hex_32(&format!("{encoded}00")).is_err(), "too long");
        assert!(parse_hex_32(&"zz".repeat(32)).is_err(), "non-hex digits");
    }

    #[test]
    fn control_hello_rejects_version_label_and_role_mismatch() {
        let signer = identity();
        let public = signer.public_identity();
        let session_id = Uuid::new_v4();
        let payload = ControlHelloPayload {
            session_id,
            role: ControlPeerRole::Controller,
            protocol_version: CONTROL_PROTOCOL_VERSION,
            channel_label: CONTROL_CHANNEL_LABEL.to_owned(),
            eph_pub_hex: "11".repeat(32),
            offer_sdp_sha256: "offer".to_owned(),
            answer_sdp_sha256: "answer".to_owned(),
            capabilities: vec!["agent_control".to_owned()],
        };
        let envelope = signer.sign(payload).expect("sign");
        let vouched = public.public_key_fingerprint.0.clone();

        verify_control_hello(
            &public,
            &envelope,
            &vouched,
            session_id,
            ControlPeerRole::Controller,
            "offer",
            "answer",
        )
        .expect("valid hello");

        let mut wrong_version = envelope.clone();
        wrong_version.payload.protocol_version = CONTROL_PROTOCOL_VERSION + 1;
        wrong_version = signer.sign(wrong_version.payload).expect("resign");
        assert!(
            verify_control_hello(
                &public,
                &wrong_version,
                &vouched,
                session_id,
                ControlPeerRole::Controller,
                "offer",
                "answer",
            )
            .is_err()
        );

        let mut wrong_label = envelope.clone();
        wrong_label.payload.channel_label = "maas-pack".to_owned();
        wrong_label = signer.sign(wrong_label.payload).expect("resign");
        assert!(
            verify_control_hello(
                &public,
                &wrong_label,
                &vouched,
                session_id,
                ControlPeerRole::Controller,
                "offer",
                "answer",
            )
            .is_err()
        );

        assert!(
            verify_control_hello(
                &public,
                &envelope,
                &vouched,
                session_id,
                ControlPeerRole::Server,
                "offer",
                "answer",
            )
            .is_err()
        );
    }

    #[test]
    fn liveness_reply_matches_mobile_wire_contract() {
        let ping = r#"{"version":1,"frame_id":"frame-1","body":{"type":"ping","nonce":"proof-1"}}"#;

        let pong = liveness_reply(ping)
            .expect("valid ping")
            .expect("ping must produce a reply");
        let value: serde_json::Value = serde_json::from_str(&pong).expect("pong json");

        assert_eq!(value["version"], serde_json::json!(1));
        assert_eq!(value["frame_id"], serde_json::json!("frame-1"));
        assert_eq!(value["body"]["type"], serde_json::json!("pong"));
        assert_eq!(value["body"]["nonce"], serde_json::json!("proof-1"));

        let future =
            r#"{"version":2,"frame_id":"frame-2","body":{"type":"ping","nonce":"proof-2"}}"#;
        assert!(liveness_reply(future).is_err());
    }

    #[tokio::test]
    async fn subscribe_replays_durable_events_after_cursor() {
        let store = InMemoryControlStore::default();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "control-test".to_owned(),
        };
        let server = "server-fingerprint";
        let block = store
            .reserve_cursor_block(&ctx, server, 8)
            .await
            .expect("reserve cursors");
        for cursor in [block.start, block.start + 1] {
            store
                .append_event(
                    &ctx,
                    server,
                    ControlEventDraft {
                        cursor,
                        event_id: Uuid::now_v7(),
                        command_id: None,
                        kind: "heartbeat".to_owned(),
                        payload: serde_json::json!({}),
                        created_at: Utc::now(),
                    },
                )
                .await
                .expect("append event");
        }
        let request = format!(
            r#"{{"version":1,"frame_id":"sub-1","body":{{"type":"subscribe","after_cursor":{},"capabilities":["agent_control"]}}}}"#,
            block.start
        );
        let reply = control_application_reply(&store, &ctx, server, &request)
            .await
            .expect("valid subscribe")
            .expect("subscribe reply");
        let value: serde_json::Value = serde_json::from_str(&reply).expect("reply JSON");
        assert_eq!(value["frame_id"], "sub-1");
        assert_eq!(value["body"]["type"], "event_batch");
        assert_eq!(value["body"]["high_water_cursor"], block.end);
        assert_eq!(value["body"]["events"].as_array().unwrap().len(), 1);
        assert_eq!(value["body"]["events"][0]["cursor"], block.start + 1);
    }

    #[tokio::test]
    async fn agent_launch_command_executes_once_and_persists_terminal_event() {
        let store = Arc::new(InMemoryControlStore::default());
        let launches = Arc::new(AtomicUsize::new(0));
        let dispatcher = ControlRuntimeDispatcher::new(
            Arc::clone(&store),
            FakeAgentRuntime {
                launches: Arc::clone(&launches),
            },
        );
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "control-runtime-test".to_owned(),
        };
        let server = "server-fingerprint";
        let command_id = Uuid::now_v7();
        let request = serde_json::json!({
            "version": CONTROL_PROTOCOL_VERSION,
            "frame_id": "command-1",
            "body": {
                "type": "command",
                "command_id": command_id,
                "command_type": "agent_launch",
                "payload": {"instruction": "report proof of life"},
            },
        })
        .to_string();

        let accepted = dispatcher
            .reply(&ctx, server, &request)
            .await
            .expect("launch accepted")
            .expect("command reply");
        let accepted: serde_json::Value = serde_json::from_str(&accepted).expect("reply json");
        assert_eq!(accepted["body"]["type"], "command_result");
        assert_eq!(accepted["body"]["state"], "running");

        for _ in 0..100 {
            let command = store
                .get_command(&ctx, server, command_id)
                .await
                .expect("read command")
                .expect("command exists");
            if command.state == ControlCommandState::Succeeded {
                assert_eq!(
                    command.result.as_ref().unwrap()["message"],
                    "ferrosa-mobile proof of life"
                );
                assert!(command.result_cursor.is_some());
                break;
            }
            tokio::task::yield_now().await;
        }
        let command = store
            .get_command(&ctx, server, command_id)
            .await
            .expect("read completed command")
            .expect("command exists");
        assert_eq!(command.state, ControlCommandState::Succeeded);
        assert_eq!(launches.load(Ordering::SeqCst), 1);

        let duplicate = dispatcher
            .reply(&ctx, server, &request)
            .await
            .expect("duplicate accepted")
            .expect("duplicate reply");
        let duplicate: serde_json::Value = serde_json::from_str(&duplicate).expect("reply json");
        assert_eq!(duplicate["body"]["state"], "succeeded");
        assert_eq!(launches.load(Ordering::SeqCst), 1);

        let replay = store
            .events_after(&ctx, server, None, MAX_CONTROL_REPLAY_EVENTS)
            .await
            .expect("replay terminal event");
        assert!(replay.events.iter().any(|event| {
            event.command_id == Some(command_id)
                && event.kind == "agent_completed"
                && event.payload["message"] == "ferrosa-mobile proof of life"
        }));
    }
}
