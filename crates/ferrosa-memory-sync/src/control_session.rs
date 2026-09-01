//! Signed, bounded WebRTC binding for the separate Ferrosa mobile control path.
//!
//! Correctness: the peer identity is bound to the exact SDP pair, inbound
//! queues and frames are bounded, and typed commands persist before execution.
//! Last revised: 2026-08-25
//! Last changed: Split durable replay at the frame bound without skipping events.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use ferrosa_memory_core::control_store::{
    CommandInsert, ControlCommand, ControlCommandState, ControlCommandUpdate, ControlEvent,
    ControlEventDraft, ControlStore, MAX_CONTROL_REPLAY_EVENTS,
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
    ///
    /// This one ENDS the session, so it means exactly what it says: the peer
    /// sent something this protocol cannot interpret. A local dependency
    /// failing is not a protocol violation -- see [`Self::CapabilityUnavailable`].
    #[error("control protocol violation: {0}")]
    Protocol(String),
    /// A database-backed capability could not be served right now.
    ///
    /// The peer did nothing wrong and the channel is healthy, so this must not
    /// end the session. Separated from [`Self::Protocol`] because every
    /// durable-store failure used to be reported as one, and the frame loop
    /// closes the channel on a protocol error: a hot database therefore
    /// disconnected the operator's terminal. WebRTC, authentication and tmux
    /// do not need the database, and a session that can still serve them must
    /// stay up and say which capability is missing.
    #[error("control capability unavailable: {0}")]
    CapabilityUnavailable(String),
    /// The peer sent a frame kind nothing on this machine serves.
    ///
    /// Not a protocol violation. A newer app asking for something this build
    /// does not have is the ordinary state of a fleet mid-upgrade, and the
    /// answer is to say so — not to drop a channel that is carrying a working
    /// terminal.
    ///
    /// This was fatal, and it took down every session: the phone gained a
    /// Knowledge tab whose four frame kinds the extension had handlers for but
    /// never listed in `kinds()`, so they reached this dispatcher instead. Once
    /// the app began loading claims on connect rather than on opening the tab,
    /// the first frame of every session closed it.
    #[error("no capability serves {0} on this machine")]
    UnknownKind(String),
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

/// A send-only handle on a bound control channel.
///
/// Exists because receiving needs `&mut` and sending does not, so an extension
/// that only ever writes should not have to contend for the loop that reads.
/// Cloneable and cheap: the alternative was handing extensions the whole
/// channel behind a lock, where a slow writer would stall the reader that
/// detects the peer going away.
#[derive(Clone)]
pub struct ControlFrameSink {
    data_channel: Arc<RTCDataChannel>,
    max_frame_bytes: usize,
}

impl ControlFrameSink {
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

    /// A send-only handle, for a caller that writes frames but does not read
    /// them.
    pub fn frame_sink(&self) -> ControlFrameSink {
        ControlFrameSink {
            data_channel: Arc::clone(&self.data_channel),
            max_frame_bytes: self.max_frame_bytes,
        }
    }

    /// Close the direct peer connection.
    pub async fn close(&self) -> Result<(), ControlSessionError> {
        self.peer_connection
            .close()
            .await
            .map_err(|error| ControlSessionError::Rtc(format!("close: {error}")))
    }
}

/// Anything that owns an ICE agent and therefore must be closed on every exit.
///
/// A trait rather than the concrete `RTCPeerConnection` for one reason: the
/// guarantee is only observable, without it, by counting file descriptors on a
/// machine that has been up for hours. Behind a trait it is a millisecond
/// assertion against a fake.
#[async_trait::async_trait]
pub(crate) trait ClosablePeer: Send + Sync {
    async fn close_peer(&self) -> Result<(), String>;
}

/// Close `peer` when `result` is an error, and return `result` either way.
///
/// Dropping an `RTCPeerConnection` does NOT release its ICE agent's UDP
/// sockets — only `close()` does. Every `?` on a path that has already built a
/// peer connection therefore leaks one socket per gathered candidate, and the
/// failure paths are the common ones: a controller that goes away mid-handshake
/// hits them on every attempt.
async fn close_peer_on_error<T, E>(peer: &impl ClosablePeer, result: Result<T, E>) -> Result<T, E> {
    if result.is_err() {
        // The close error is deliberately swallowed. The caller is already
        // holding a real failure, and replacing it with "close: ..." would
        // report the cleanup instead of the cause.
        if let Err(close_error) = peer.close_peer().await {
            tracing::debug!(%close_error, "closing a failed peer connection also failed");
        }
    }
    result
}

/// The real peer connection. `close()` is what releases the ICE agent's
/// sockets; `drop` is not.
#[async_trait::async_trait]
impl ClosablePeer for Arc<RTCPeerConnection> {
    async fn close_peer(&self) -> Result<(), String> {
        RTCPeerConnection::close(self)
            .await
            .map_err(|error| error.to_string())
    }
}

/// Why a gather produced nothing, phrased for whoever reads the log next.
///
/// `None` when the count is healthy — a warning on every session is a warning
/// nobody reads.
///
/// Zero candidates earns its own message because the consequence is so
/// misleading: the answer is still sent, the session still negotiates, and it
/// fails thirty seconds later as a generic ICE timeout. Nothing downstream says
/// the local host could not open a socket, so the failure gets attributed to
/// the network, the phone, or the broker. It was attributed to the broker.
fn gathering_verdict(candidate_count: usize) -> Option<String> {
    (candidate_count == 0).then(|| {
        "gathered 0 ICE candidates: this host could not bind a local socket. \
         The usual cause is file descriptor exhaustion - check the process's \
         open descriptor count against its RLIMIT_NOFILE soft limit. The answer \
         will negotiate and then time out."
            .to_owned()
    })
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

    // Same guarantee as the server half, and the same reason: everything below
    // can fail with the peer connection already built and gathering candidates.
    let bind = async {
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
            peer_connection: Arc::clone(&peer_connection),
            data_channel,
            inbound,
            session_key,
            max_frame_bytes: config.max_frame_bytes,
        })
    }
    .await;

    close_peer_on_error(&peer_connection, bind).await
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

    // Everything below can fail, and by this point the peer connection exists
    // and is about to gather candidates — a UDP socket per candidate. A bare
    // `?` here drops it WITHOUT closing it, and dropping an RTCPeerConnection
    // does not release its ICE agent's sockets.
    //
    // So the rest of the bind runs in an inner future whose result goes through
    // `close_peer_on_error`. That is deliberately not a `?` chain: the failure
    // paths are the COMMON ones — a controller that goes away mid-handshake
    // hits them on every attempt — and each one used to cost sockets
    // permanently.
    let bind = async {
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
        let candidate_count = local
            .sdp
            .lines()
            .filter(|line| line.starts_with("a=candidate:"))
            .count();
        if let Some(fault) = gathering_verdict(candidate_count) {
            tracing::error!(session_id = %session_id, "{fault}");
        }
        tracing::info!(candidate_count, "sending gathered control answer");
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
            peer_connection: Arc::clone(&peer_connection),
            data_channel,
            inbound,
            session_key,
            max_frame_bytes: config.max_frame_bytes,
        })
    }
    .await;

    // The single exit. Every `?` inside `bind` now lands here with the peer
    // connection still alive, so it can be closed before the error leaves.
    close_peer_on_error(&peer_connection, bind).await
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
    // `liveness_reply` above already refuses a body carrying no type, so a
    // frame reaching here HAS one. Stated as a guard rather than a default
    // string: a placeholder would have read like a real kind in the refusal,
    // and would go quietly wrong if that earlier check ever moved.
    let Some(body_type) = body.get("type").and_then(serde_json::Value::as_str) else {
        return Err(ControlSessionError::Protocol(
            "missing body type".to_owned(),
        ));
    };
    if body_type != "subscribe" {
        // Refused, not fatal: an unclaimed kind means no extension serves it,
        // which is a missing capability rather than a peer sending nonsense.
        return Err(ControlSessionError::UnknownKind(body_type.to_owned()));
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
    encode_event_batch(frame_id, page.high_water_cursor, page.events).map(Some)
}

/// Encode the largest ordered event prefix that fits one control frame.
///
/// Each durable payload is bounded independently, but their JSON envelope can
/// still exceed the data-channel limit when a replay page contains several
/// terminal events. The returned high-water cursor advances only through the
/// last encoded event, so the next subscription receives every omitted event.
fn encode_event_batch(
    frame_id: &str,
    final_high_water_cursor: u64,
    events: Vec<ControlEvent>,
) -> Result<String, ControlSessionError> {
    let frame_id = serde_json::to_string(frame_id)
        .map_err(|error| ControlSessionError::Protocol(format!("frame id encode: {error}")))?;
    let envelope_bytes = event_batch_json(&frame_id, u64::MAX, "").len();
    let event_count = events.len();
    let mut encoded_events = Vec::with_capacity(event_count);
    let mut encoded_bytes = envelope_bytes;
    let mut last_cursor = None;

    for event in events {
        let cursor = event.cursor;
        let encoded = serde_json::to_string(&serde_json::json!({
            "cursor": cursor,
            "event_id": event.event_id.to_string(),
            "command_id": event.command_id.map(|value| value.to_string()),
            "kind": event.kind,
            "payload": event.payload,
            "created_at": event.created_at,
        }))
        .map_err(|error| ControlSessionError::Protocol(format!("event encode: {error}")))?;
        let delimiter_bytes = if encoded_events.is_empty() { 0 } else { 1 };
        let candidate_bytes = encoded_bytes + delimiter_bytes + encoded.len();
        if candidate_bytes > MAX_CONTROL_FRAME_BYTES {
            if encoded_events.is_empty() {
                return Err(ControlSessionError::FrameTooLarge {
                    actual: candidate_bytes,
                    limit: MAX_CONTROL_FRAME_BYTES,
                });
            }
            break;
        }
        encoded_bytes = candidate_bytes;
        last_cursor = Some(cursor);
        encoded_events.push(encoded);
    }

    let high_water_cursor = if encoded_events.len() == event_count {
        final_high_water_cursor
    } else {
        last_cursor.expect("a truncated batch contains at least one event")
    };
    let events = encoded_events.join(",");
    let reply = event_batch_json(&frame_id, high_water_cursor, &events);
    debug_assert!(reply.len() <= MAX_CONTROL_FRAME_BYTES);
    Ok(reply)
}

fn event_batch_json(frame_id: &str, high_water_cursor: u64, events: &str) -> String {
    format!(
        r#"{{"version":{CONTROL_PROTOCOL_VERSION},"frame_id":{frame_id},"body":{{"type":"event_batch","high_water_cursor":{high_water_cursor},"events":[{events}]}}}}"#
    )
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
    /// The coordinator beside this listener, when one is installed.
    ///
    /// `None` is a normal state, not a degraded one: a machine that only serves
    /// memory has no coordinator, and team commands answer "not available"
    /// rather than failing.
    #[cfg(feature = "webrtc-transport")]
    coordinator: Option<Arc<crate::coordinator_client::CoordinatorClient>>,
    /// What the ACCOUNT has paid for.
    ///
    /// An entitlement turns functionality off; it is not a permission check.
    /// An account without `teams` sees a machine with no team functionality,
    /// which is the same answer as a machine that has none installed.
    entitlements: Vec<String>,
    /// What THIS DEVICE was granted.
    ///
    /// The actual security decision, and the server's view of it. It must never
    /// be the capability list the client asked for in its own subscribe frame.
    granted_capabilities: Vec<String>,
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
            #[cfg(feature = "webrtc-transport")]
            coordinator: None,
            // Empty by default, which denies every coordinator command. A
            // dispatcher that has not been told what the peer may do must not
            // assume it may do anything.
            entitlements: Vec::new(),
            granted_capabilities: Vec::new(),
        }
    }

    /// Attach the coordinator and the server's view of what this peer may do.
    ///
    /// Both grants are supplied by the CALLER, which reads them from the
    /// account and the device record. They are deliberately not derived from
    /// anything the client sent.
    #[cfg(feature = "webrtc-transport")]
    #[must_use]
    pub fn with_coordinator(
        mut self,
        coordinator: Option<Arc<crate::coordinator_client::CoordinatorClient>>,
        entitlements: Vec<String>,
        granted_capabilities: Vec<String>,
    ) -> Self {
        self.coordinator = coordinator;
        self.entitlements = entitlements;
        self.granted_capabilities = granted_capabilities;
        self
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

    /// Answer a coordinator command, or say plainly why it cannot be answered.
    ///
    /// Three refusals, kept apart because they need different things from the
    /// reader. `NotAvailable` covers both "this host runs no coordinator" and
    /// "the account has no teams entitlement" -- ONE answer on purpose, since
    /// both mean "nothing here" from the caller's side and the difference is of
    /// use only to somebody mapping which hosts run one.
    ///
    /// Reads answer straight from the coordinator and record NOTHING durable: a
    /// list is safe to repeat, and writing an event per poll would bury the
    /// events that describe real changes. Writes are the opposite -- they are
    /// what the log exists for.
    async fn dispatch_coordinator_command(
        &self,
        ctx: &TenantContext,
        server_fingerprint: &str,
        frame: &serde_json::Value,
        frame_id: &str,
        command: crate::coordinator_command::CoordinatorCommand,
    ) -> Result<Option<String>, ControlSessionError> {
        use crate::coordinator_command::{Effect, RefusedReason, authorize, teams_available};

        let available = teams_available(self.coordinator.is_some(), &self.entitlements);
        if let Err(reason) = authorize(command, &self.granted_capabilities, available) {
            let message = match reason {
                RefusedReason::NotAvailable => "teams are not available on this machine",
                RefusedReason::MissingCapability { .. } => {
                    "this device is not permitted to drive the coordinator"
                }
            };
            // A refusal is not a protocol violation: the frame was well formed
            // and the answer is no. Killing the session over it would take down
            // everything else the peer is doing.
            return Err(ControlSessionError::CapabilityUnavailable(
                message.to_owned(),
            ));
        }

        let Some(coordinator) = self.coordinator.as_ref() else {
            return Err(ControlSessionError::CapabilityUnavailable(
                "teams are not available on this machine".to_owned(),
            ));
        };

        // NOTE: the frame is never logged from here on. For SecretFulfil it
        // carries a credential, and the redaction that protects it on the app
        // side does not travel with the JSON.
        let payload = frame.pointer("/body/payload").cloned().unwrap_or_default();

        let outcome = match command {
            crate::coordinator_command::CoordinatorCommand::TeammateList => {
                coordinator.teammates().await
            }
            crate::coordinator_command::CoordinatorCommand::SecretPendingList => {
                coordinator.pending_secrets().await
            }
            crate::coordinator_command::CoordinatorCommand::VmList => coordinator.vms().await,
            crate::coordinator_command::CoordinatorCommand::VmHibernate => {
                let id = payload
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        ControlSessionError::Protocol("vm_hibernate needs an id".to_owned())
                    })?;
                coordinator.hibernate_vm(id).await
            }
            crate::coordinator_command::CoordinatorCommand::VmResume => {
                let id = payload
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        ControlSessionError::Protocol("vm_resume needs an id".to_owned())
                    })?;
                coordinator.resume_vm(id).await
            }
            crate::coordinator_command::CoordinatorCommand::CoordinatorOffer => {
                coordinator.offering().await
            }
            crate::coordinator_command::CoordinatorCommand::SecretFulfil => {
                let request_id = payload
                    .get("request_id")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| {
                        ControlSessionError::Protocol("secret_fulfil needs a request_id".to_owned())
                    })?;
                // Taken as an owned String and moved straight into the call, so
                // it is not retained here after the request.
                let value = payload
                    .get("value")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        ControlSessionError::Protocol("secret_fulfil needs a value".to_owned())
                    })?
                    .to_owned();
                coordinator.fulfil_secret(request_id, value).await
            }
            crate::coordinator_command::CoordinatorCommand::SecretDeny => {
                let request_id = payload
                    .get("request_id")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| {
                        ControlSessionError::Protocol("secret_deny needs a request_id".to_owned())
                    })?;
                coordinator
                    .deny_secret(request_id)
                    .await
                    .map(|()| serde_json::json!({"denied": request_id}))
            }
        };

        let result = outcome.map_err(|e| {
            // The coordinator being unreachable costs this capability, not the
            // session: the peer keeps its memory and agent access.
            ControlSessionError::CapabilityUnavailable(format!("coordinator: {e}"))
        })?;

        if command.effect() == Effect::Write {
            // Writes belong in the durable record, so the log says who answered
            // a credential prompt and when. The RESULT is recorded, which for a
            // fulfilment is a state and a path -- never the value.
            let _ = append_control_event(
                self.store.as_ref(),
                ctx,
                server_fingerprint,
                Uuid::now_v7(),
                "coordinator_command",
                serde_json::json!({
                    "command_type": command.as_wire(),
                    "result": result.clone(),
                }),
            )
            .await?;
        }

        let reply = serde_json::json!({
            "version": CONTROL_PROTOCOL_VERSION,
            "frame_id": frame_id,
            "body": {
                "type": "command_result",
                "command_id": frame.pointer("/body/command_id").cloned().unwrap_or_default(),
                "state": "succeeded",
                "result": result,
            }
        });
        Ok(Some(reply.to_string()))
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
        // Coordinator commands take a different path: they are answered by the
        // coordinator beside this listener rather than by the agent runtime,
        // and reads among them produce no durable command at all.
        //
        // Recognised BEFORE the agent_launch check so an unknown command still
        // reports "unsupported" rather than being mistaken for one.
        if let Some(command) =
            crate::coordinator_command::CoordinatorCommand::from_wire(command_type)
        {
            return self
                .dispatch_coordinator_command(ctx, server_fingerprint, frame, frame_id, command)
                .await;
        }
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

/// A durable-store failure, reported as a missing capability rather than as a
/// protocol violation.
///
/// It was the latter, and the frame loop closes the channel on a protocol
/// error. So an overloaded database did not merely fail the command feed --
/// it tore down the control channel, and the terminal the operator was
/// watching went away with it. Nothing about the peer was wrong.
fn control_store_error(error: anyhow::Error) -> ControlSessionError {
    ControlSessionError::CapabilityUnavailable(format!("durable control store: {error}"))
}

/// Tell the caller a capability is down, on the frame they asked about.
///
/// Sent instead of closing. A reply the device can correlate is what lets it
/// mark one control unavailable and keep the rest of the session, which
/// silence -- or a dropped channel -- does not.
pub fn capability_unavailable_reply(frame_id: &str, reason: &str) -> Option<String> {
    serde_json::to_string(&serde_json::json!({
        "version": CONTROL_PROTOCOL_VERSION,
        "frame_id": frame_id,
        "body": {
            "type": "capability_unavailable",
            "reason": bounded_text(reason),
        },
    }))
    .ok()
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

    async fn append_large_terminal_events(
        store: &InMemoryControlStore,
        ctx: &TenantContext,
        server: &str,
    ) -> ferrosa_memory_core::control_store::CursorBlock {
        let block = store
            .reserve_cursor_block(ctx, server, 8)
            .await
            .expect("reserve cursors");
        for offset in 0..6 {
            store
                .append_event(
                    ctx,
                    server,
                    ControlEventDraft {
                        cursor: block.start + offset,
                        event_id: Uuid::now_v7(),
                        command_id: None,
                        kind: "terminal".to_owned(),
                        payload: serde_json::json!({
                            "text": "x".repeat(MAX_AGENT_RESULT_TEXT_BYTES),
                        }),
                        created_at: Utc::now(),
                    },
                )
                .await
                .expect("append bounded terminal event");
        }
        block
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

    /// An unrecognised body type is a MISSING CAPABILITY, not a violation.
    ///
    /// The distinction is the whole outage: four shell_knowledge kinds reached
    /// this parser because the extension never claimed them, and calling them
    /// protocol violations closed the channel — every session, within a second.
    #[tokio::test]
    async fn an_unrecognised_body_type_is_a_missing_capability() {
        let store = InMemoryControlStore::default();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "control-test".to_owned(),
        };
        let request = r#"{"version":1,"frame_id":"k-1","body":{"type":"shell_knowledge_claims"}}"#;
        let error = control_application_reply(&store, &ctx, "server-fingerprint", request)
            .await
            .expect_err("nothing serves that kind");
        assert!(
            matches!(error, ControlSessionError::UnknownKind(ref kind)
                     if kind == "shell_knowledge_claims"),
            "an unserved kind must name itself and stay non-fatal; got {error:?}"
        );
    }

    /// The other side of it. A body with no type at all is not a newer client
    /// asking for something — it is a peer speaking wrongly, and reclassifying
    /// that too would let it hold a session open forever.
    ///
    /// Enforced in `liveness_reply`, which runs first; the guard in
    /// `control_application_reply` is the belt to its braces. Pinned here
    /// because the two are easy to move apart, and it is the ONLY thing
    /// separating "your build is old" from "you are speaking nonsense".
    #[tokio::test]
    async fn a_body_with_no_type_is_still_a_protocol_violation() {
        let store = InMemoryControlStore::default();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "control-test".to_owned(),
        };
        for request in [
            r#"{"version":1,"frame_id":"bad-1","body":{}}"#,
            r#"{"version":1,"frame_id":"bad-2","body":{"type":42}}"#,
        ] {
            let error = control_application_reply(&store, &ctx, "server-fingerprint", request)
                .await
                .expect_err("a body with no type is malformed");
            assert!(
                matches!(error, ControlSessionError::Protocol(_)),
                "{request} must stay fatal; got {error:?}"
            );
        }
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
    async fn subscribe_splits_event_batches_at_the_control_frame_limit() {
        let store = InMemoryControlStore::default();
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "bounded-replay-test".to_owned(),
        };
        let server = "server-fingerprint";
        let block = append_large_terminal_events(&store, &ctx, server).await;

        let first_request = r#"{"version":1,"frame_id":"sub-large-1","body":{"type":"subscribe","after_cursor":null,"capabilities":["agent_control"]}}"#;
        let first_reply = control_application_reply(&store, &ctx, server, first_request)
            .await
            .expect("valid first subscribe")
            .expect("first subscribe reply");
        assert!(
            first_reply.len() <= MAX_CONTROL_FRAME_BYTES,
            "a replay reply must fit the channel frame; got {} bytes",
            first_reply.len()
        );
        let first: serde_json::Value =
            serde_json::from_str(&first_reply).expect("first reply JSON");
        let first_events = first["body"]["events"]
            .as_array()
            .expect("first event batch");
        assert!(!first_events.is_empty());
        assert!(first_events.len() < 6, "the fixture must require two pages");
        let first_high_water = first["body"]["high_water_cursor"]
            .as_u64()
            .expect("first high-water cursor");
        assert_eq!(
            first_high_water,
            first_events.last().unwrap()["cursor"].as_u64().unwrap(),
            "the cursor must not advance past an event omitted from this frame"
        );

        let second_request = format!(
            r#"{{"version":1,"frame_id":"sub-large-2","body":{{"type":"subscribe","after_cursor":{first_high_water},"capabilities":["agent_control"]}}}}"#
        );
        let second_reply = control_application_reply(&store, &ctx, server, &second_request)
            .await
            .expect("valid second subscribe")
            .expect("second subscribe reply");
        assert!(second_reply.len() <= MAX_CONTROL_FRAME_BYTES);
        let second: serde_json::Value =
            serde_json::from_str(&second_reply).expect("second reply JSON");
        let second_events = second["body"]["events"]
            .as_array()
            .expect("second event batch");
        assert_eq!(first_events.len() + second_events.len(), 6);
        assert_eq!(second["body"]["high_water_cursor"], block.end);
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

    /// A store that behaves like an overloaded cluster: every call fails.
    ///
    /// Not a timeout, because the classification is what is under test and it
    /// is the same for a timeout as for any other store error. A test that
    /// actually waited 30 s would be measuring tokio.
    struct OverloadedControlStore;

    impl ControlStore for OverloadedControlStore {
        async fn reserve_cursor_block(
            &self,
            _ctx: &TenantContext,
            _server_fingerprint: &str,
            _size: u64,
        ) -> anyhow::Result<ferrosa_memory_core::control_store::CursorBlock> {
            anyhow::bail!("Request timeout: Request took longer than 30000ms")
        }

        async fn append_event(
            &self,
            _ctx: &TenantContext,
            _server_fingerprint: &str,
            _draft: ControlEventDraft,
        ) -> anyhow::Result<ferrosa_memory_core::control_store::ControlEvent> {
            anyhow::bail!("Request timeout: Request took longer than 30000ms")
        }

        async fn events_after(
            &self,
            _ctx: &TenantContext,
            _server_fingerprint: &str,
            _after_cursor: Option<u64>,
            _limit: usize,
        ) -> anyhow::Result<ferrosa_memory_core::control_store::ControlReplayPage> {
            anyhow::bail!("Request timeout: Request took longer than 30000ms")
        }

        async fn put_command_if_absent(
            &self,
            _ctx: &TenantContext,
            _server_fingerprint: &str,
            _command: &ControlCommand,
        ) -> anyhow::Result<ferrosa_memory_core::control_store::CommandInsert> {
            anyhow::bail!("Request timeout: Request took longer than 30000ms")
        }

        async fn get_command(
            &self,
            _ctx: &TenantContext,
            _server_fingerprint: &str,
            _command_id: Uuid,
        ) -> anyhow::Result<Option<ControlCommand>> {
            anyhow::bail!("Request timeout: Request took longer than 30000ms")
        }

        async fn update_command(
            &self,
            _ctx: &TenantContext,
            _server_fingerprint: &str,
            _command_id: Uuid,
            _update: ferrosa_memory_core::control_store::ControlCommandUpdate,
        ) -> anyhow::Result<ControlCommand> {
            anyhow::bail!("Request timeout: Request took longer than 30000ms")
        }
    }

    /// The regression. A hot database must not read as a protocol violation,
    /// because the frame loop CLOSES the channel on one -- which is how an
    /// overloaded cluster made the operator's terminal disappear.
    #[tokio::test]
    async fn a_failing_store_is_a_missing_capability_not_a_protocol_violation() {
        let dispatcher = ControlRuntimeDispatcher::new(
            Arc::new(OverloadedControlStore),
            FakeAgentRuntime {
                launches: Arc::new(AtomicUsize::new(0)),
            },
        );
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "control-runtime-test".to_owned(),
        };
        let request = serde_json::json!({
            "version": CONTROL_PROTOCOL_VERSION,
            "frame_id": "command-1",
            "body": {
                "type": "command",
                "command_id": Uuid::now_v7(),
                "command_type": "agent_launch",
                "payload": {"instruction": "report proof of life"},
            },
        })
        .to_string();

        let error = dispatcher
            .reply(&ctx, "server-fingerprint", &request)
            .await
            .expect_err("an overloaded store cannot serve the command feed");
        assert!(
            matches!(error, ControlSessionError::CapabilityUnavailable(_)),
            "a store failure must not be reported as a protocol violation, \
             because that tears down the channel; got {error:?}"
        );
    }

    /// The other half of the boundary: a peer that really does speak wrongly
    /// still ends the session. Without this the first test could be satisfied
    /// by reclassifying everything, which would leave a malformed peer able to
    /// hold a session open forever.
    #[tokio::test]
    async fn a_malformed_frame_is_still_a_protocol_violation() {
        let dispatcher = ControlRuntimeDispatcher::new(
            Arc::new(InMemoryControlStore::default()),
            FakeAgentRuntime {
                launches: Arc::new(AtomicUsize::new(0)),
            },
        );
        let ctx = TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "control-runtime-test".to_owned(),
        };
        let error = dispatcher
            .reply(&ctx, "server-fingerprint", "{not json")
            .await
            .expect_err("a malformed frame is refused");
        assert!(
            matches!(error, ControlSessionError::Protocol(_)),
            "got {error:?}"
        );
    }

    /// The degraded reply has to be correlatable, or the device cannot tell
    /// which request went unserved and shows a control that never resolves.
    #[test]
    fn a_capability_reply_carries_the_frame_it_answers() {
        let reply = capability_unavailable_reply("command-1", "durable control store: hot")
            .expect("encodes");
        let value: serde_json::Value = serde_json::from_str(&reply).expect("json");
        assert_eq!(value["frame_id"], "command-1");
        assert_eq!(value["body"]["type"], "capability_unavailable");
        assert!(
            value["body"]["reason"]
                .as_str()
                .expect("reason")
                .contains("durable control store")
        );
    }

    /// A recording stand-in for the peer connection.
    ///
    /// The leak this guards against went unnoticed precisely because the only
    /// way to observe it was to count file descriptors on a machine that had
    /// been up for hours. Counting calls on a fake makes it a millisecond
    /// assertion instead.
    struct RecordingPeer {
        closes: AtomicUsize,
        close_result: Result<(), String>,
    }

    impl RecordingPeer {
        fn new() -> Self {
            Self {
                closes: AtomicUsize::new(0),
                close_result: Ok(()),
            }
        }

        fn failing() -> Self {
            Self {
                closes: AtomicUsize::new(0),
                close_result: Err("ICE agent already gone".to_owned()),
            }
        }

        fn closes(&self) -> usize {
            self.closes.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ClosablePeer for RecordingPeer {
        async fn close_peer(&self) -> Result<(), String> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            self.close_result.clone()
        }
    }

    /// A success is passed straight through, and nothing is closed.
    ///
    /// The starter case, and a real requirement: closing a peer connection that
    /// is about to serve a session would end the session it just established.
    #[tokio::test]
    async fn a_successful_result_is_not_closed() {
        let peer = RecordingPeer::new();
        let result: Result<u8, String> = close_peer_on_error(&peer, Ok(7)).await;
        assert_eq!(result, Ok(7));
        assert_eq!(peer.closes(), 0, "a healthy session must not be torn down");
    }

    /// THE BUG. A failed bind must close the peer connection it already built.
    ///
    /// `run_control_server_session` creates the peer connection, lets it gather
    /// candidates — binding a UDP socket per candidate — and then has a bare `?`
    /// after it. Every failure there dropped the connection without closing it.
    /// Dropping does not release the sockets; only `close()` does.
    ///
    /// Measured on a real machine: 226 leaked UDP sockets against a 256
    /// descriptor limit after ~11 hours, after which the process could gather no
    /// candidates at all and reported the gateway as unreachable.
    #[tokio::test]
    async fn a_failed_bind_closes_the_peer_connection_it_built() {
        let peer = RecordingPeer::new();
        let result: Result<u8, String> = close_peer_on_error(
            &peer,
            Err("timed out waiting for control data channel".to_owned()),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(peer.closes(), 1, "the failed session leaked its ICE agent");
    }

    /// Closing must not swallow or replace the real failure. The cleanup is
    /// never what the operator needs to read; the cause is.
    #[tokio::test]
    async fn the_original_error_survives_the_close() {
        let peer = RecordingPeer::new();
        let result: Result<u8, String> =
            close_peer_on_error(&peer, Err("broker transport error".to_owned())).await;
        assert_eq!(result, Err("broker transport error".to_owned()));
    }

    /// A close that itself fails still leaves the original error intact.
    #[tokio::test]
    async fn a_failing_close_does_not_mask_the_cause() {
        let peer = RecordingPeer::failing();
        let result: Result<u8, String> =
            close_peer_on_error(&peer, Err("attestation rejected".to_owned())).await;
        assert_eq!(result, Err("attestation rejected".to_owned()));
        assert_eq!(peer.closes(), 1, "close must still be attempted");
    }

    /// Exactly once. A double close races the first teardown and logs an error
    /// that reads like a new fault.
    #[tokio::test]
    async fn a_failed_session_is_closed_exactly_once() {
        let peer = RecordingPeer::new();
        let _: Result<u8, String> = close_peer_on_error(&peer, Err("x".to_owned())).await;
        assert_eq!(peer.closes(), 1);
    }

    /// Zero gathered candidates must be reported, not sent.
    ///
    /// An answer with no candidates negotiates, looks fine, and dies half a
    /// minute later as a generic timeout — while the real cause (no descriptors
    /// left to bind a socket) is never mentioned anywhere.
    #[test]
    fn no_gathered_candidates_is_reported_as_a_fault() {
        let verdict = gathering_verdict(0);
        assert!(verdict.is_some(), "zero candidates must not pass silently");
        let message = verdict.unwrap_or_default();
        assert!(
            message.contains("descriptor"),
            "the message must name the likely cause: {message}"
        );
    }

    /// A healthy gather says nothing. A warning on every session is a warning
    /// nobody reads.
    #[test]
    fn a_healthy_gather_is_not_flagged() {
        assert_eq!(gathering_verdict(8), None);
        assert_eq!(gathering_verdict(1), None);
    }
}
