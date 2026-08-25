//! Module: The control-listener runtime, shared by every binary that hosts one.
//! Correctness: Correct when a binary can run a listener without restating any
//! of it, and when swapping the visual plugins is the only difference between
//! one binary and another.
//! Last revised: 2026-08-22
//! Last changed: Lifted out of main.rs so a second binary can reuse it.
//!
//! This lived in `main.rs` and was therefore reachable only by that one binary.
//! A second binary — one linking real capture plugins where this one links the
//! null implementation — would have had to restate the whole runtime, and two
//! copies of a session loop diverge in the direction of whichever was debugged
//! most recently.
//!
//! The amount of code a downstream `main` has to write is the measure of
//! whether the plugin seam is real. It should be argument parsing and a call.

#![cfg(feature = "webrtc-transport")]

use std::time::Duration;

use anyhow::Context as _;
use uuid::Uuid;

/// Which tenant's memory this machine serves to a controller.
///
/// `server.tenant_id` first, then `FERROSA_MEMORY_TENANT_ID`, then nothing.
///
/// There is deliberately no fallback. The obvious one is the task board's
/// tenant, because the board is a table on this same cluster and the listener
/// already holds its contact points — and that is exactly what this used to
/// do. The board's tenant is not the memory's: on the machine where this was
/// found the board held 118 entities and no source rows, while the memory held
/// 79,284 under a tenant derived from the authenticated principal. The tier
/// map came back reachable, with four tiers at zero, and looked precisely like
/// a machine nobody had seeded yet.
///
/// `viz.tenant_id` is NOT consulted. It is a real tenant in the config and it
/// is a plausible-looking answer, which makes it worse than no answer: viz is
/// the unauthenticated loopback view and on that same machine its tenant is
/// also empty. A wrong tenant reports zeros; an absent one reports that it
/// does not know.
fn memory_tenant(configured: Option<&str>) -> Option<Uuid> {
    let configured = configured
        .map(str::to_owned)
        .or_else(|| std::env::var("FERROSA_MEMORY_TENANT_ID").ok())?;
    match Uuid::parse_str(configured.trim()) {
        Ok(tenant) => Some(tenant),
        Err(error) => {
            // Loud, and then None. A malformed tenant is a typo in a config
            // file, and silently continuing without one would leave the phone
            // saying "no tenant configured" while the file plainly has one.
            eprintln!("memory tenant {configured:?} is not a UUID: {error}");
            None
        }
    }
}

/// How many control sessions one memory system serves at once.
///
/// A bound, not a target. Each session is a peer connection plus a durable
/// cursor block, so an unbounded fan-out would let anyone holding an account
/// key exhaust the host by opening offers. Eight covers a person's own devices
/// several times over; sessions past it stay pending on the broker and start as
/// slots free rather than being refused.
const MAX_CONCURRENT_CONTROL_SESSIONS: usize = 8;

/// Wait added per consecutive failed poll.
const POLL_BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Ceiling on the backoff multiplier, so a long outage retries every 5s rather
/// than drifting toward never.
const POLL_BACKOFF_MAX_STEPS: u32 = 10;

/// How long a peer may stay `Disconnected` before the session is given up on.
///
/// `Disconnected` is recoverable by design — a phone changing Wi-Fi bands, a
/// moment of packet loss — and WebRTC will return to `Connected` on its own if
/// the path comes back. So it cannot be treated as death outright. But it also
/// must not be ignored: `Disconnected` does not close the data channel, so a
/// session loop blocked on a read never wakes, and anything the session holds
/// runs on. That is not hypothetical — a screen capture was measured still
/// encoding at 21% CPU seven minutes after the viewer's connection dropped,
/// because nothing was watching this state.
///
/// Ten seconds: long enough to ride out a hiccup, short enough that nobody
/// pays for a stream nobody is receiving. ICE's own failure detection is
/// slower, so this is what actually ends the session in practice.
const PEER_DISCONNECT_GRACE: Duration = Duration::from_secs(10);

/// A caller's extension to a control session.
///
/// A trait rather than a boxed closure so an implementation can hold state
/// across sessions — a pool, a registry — without smuggling it through captured
/// variables.
///
/// Four points, because an extension has a life and not just a birth. The
/// earlier version had `on_bound` alone, and the consequence was that anything
/// attached started at bind and ran until the process died: no way to ask for
/// it, no way to stop it, and no notification when the peer went away. An
/// extension that captures a screen would therefore capture every screen of
/// every session forever.
#[async_trait::async_trait]
pub trait SessionExtension: Send + Sync {
    /// Frame kinds this extension answers.
    ///
    /// A frame whose `body.type` appears here is routed to [`Self::on_request`]
    /// INSTEAD of the built-in dispatcher, which would otherwise reject it as
    /// an unknown kind and close the session. Kinds must not collide with the
    /// protocol's own (`command`, `ping`, `pong`, `subscribe`); a collision
    /// silently shadows a real frame, so keep extension kinds prefixed.
    fn kinds(&self) -> &'static [&'static str];

    /// Attach to a session whose hello has been verified.
    ///
    /// Register state here. Do NOT start work: nobody has asked for anything
    /// yet, and a session exists to carry control frames whether or not this
    /// extension is ever used.
    ///
    /// Errors are logged and the session continues WITHOUT whatever the caller
    /// was attaching. A failure to start an extension must not tear down a
    /// working control channel: the operator keeps the session they asked for
    /// and loses only the part that broke.
    async fn on_bound(&self, session: &SessionHandle) -> Result<(), String>;

    /// A frame of one of [`Self::kinds`] arrived.
    ///
    /// Errors are logged and the session survives, on the same reasoning as
    /// `on_bound`: a refused request is not a reason to drop a control channel.
    async fn on_request(
        &self,
        session: &SessionHandle,
        kind: &str,
        frame: &serde_json::Value,
    ) -> Result<(), String>;

    /// The session ended — for any reason, including one this extension never
    /// heard about.
    ///
    /// MUST be idempotent and MUST release everything held for the session. It
    /// runs on every exit path: an explicit stop, a peer that disconnected, a
    /// peer that went silent, and a protocol error. That is the whole point of
    /// having it — a peer that vanishes without saying goodbye is the normal
    /// case for a phone, not the exceptional one.
    async fn on_closed(&self, session_id: uuid::Uuid);
}

/// What an extension is given, and everything it is allowed to do.
///
/// Carries the peer connection and a way to write frames, which together are
/// enough to add a track and renegotiate for it over the channel that is
/// already up. Notably it does NOT carry the broker: renegotiation belongs on
/// the established, authenticated data channel, not on the signaling path used
/// to create it. See the note on [`SessionHandle::send`].
#[derive(Clone)]
pub struct SessionHandle {
    session_id: uuid::Uuid,
    peer: std::sync::Arc<webrtc::peer_connection::RTCPeerConnection>,
    sink: crate::control_session::ControlFrameSink,
}

impl SessionHandle {
    /// Which session this is.
    pub fn session_id(&self) -> uuid::Uuid {
        self.session_id
    }

    /// The peer connection, for attaching what this crate does not model.
    ///
    /// A caller that adds a track owns the renegotiation that follows, and
    /// [`Self::send`] is how it does that.
    pub fn peer(&self) -> std::sync::Arc<webrtc::peer_connection::RTCPeerConnection> {
        std::sync::Arc::clone(&self.peer)
    }

    /// Write one frame on the control channel.
    ///
    /// This is the renegotiation path. Re-offering through the broker would
    /// mean a second pass through a phase machine built for one handshake, at
    /// the broker's poll interval, with the offer and answer visible to the
    /// gateway. The data channel is already open, already authenticated, and
    /// already end-to-end — an offer sent on it reaches the peer in one round
    /// trip and is nobody else's business. Signaling servers exist to introduce
    /// peers that cannot yet talk; these two can.
    pub async fn send(&self, frame: &str) -> Result<(), String> {
        self.sink.send_text(frame).await.map_err(|e| e.to_string())
    }
}

/// What a caller attaches to the control sessions this listener serves.
///
/// Deliberately says nothing about what is attached. An earlier version was
/// called `VisualPlugins` and carried a capture factory and a surface provider
/// — media types, in a repository that does not have media as a capability, and
/// a factory the listener never once called.
///
/// Extensions are a `Vec` because video is not the only thing that will want
/// this. Audio, a metrics channel, file transfer: each is a `SessionExtension`,
/// and none of them needs this crate to know its name.
#[derive(Default, Clone)]
pub struct SessionExtensions {
    /// A WebRTC API built by the caller, when it needs one this crate would not
    /// build. `None` uses the data-channel-only default. Never inspected here.
    pub rtc_api: Option<std::sync::Arc<webrtc::api::API>>,
    /// Attached in order once a session's hello is attested, and driven for
    /// the life of the session.
    pub attach: Vec<std::sync::Arc<dyn SessionExtension>>,
}

impl SessionExtensions {
    /// No extensions: a plain control session.
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether anything is attached. Drives what the listener reports at start.
    pub fn any(&self) -> bool {
        self.rtc_api.is_some() || !self.attach.is_empty()
    }
}

/// Everything a control listener needs, independent of which binary hosts it.
///
/// Shared so a downstream binary does not restate the argument list. Those
/// arguments were duplicated field-for-field between the two binaries, which is
/// the same divergence risk as duplicating the runtime, just quieter — a new
/// option added to one and not the other produces two tools that look alike and
/// behave differently.
#[derive(Debug, Clone)]
pub struct ListenerConfig {
    pub gateway: String,
    pub identity: std::path::PathBuf,
    pub workspace: std::path::PathBuf,
    pub contact_points: Vec<String>,
    pub existing_schema: bool,
}

/// Run a control listener until the process ends.
///
/// `visual` is the ONLY thing that differs between the public binary and one
/// linking real capture. If a second parameter ever has to be added to make a
/// downstream binary work, the seam has sprouted a second seam beside it.
pub async fn run_control_listener(
    config: &ListenerConfig,
    extensions: SessionExtensions,
) -> anyhow::Result<()> {
    let ListenerConfig {
        gateway,
        identity: identity_path,
        workspace,
        contact_points,
        existing_schema,
    } = config;
    let existing_schema = *existing_schema;
    use std::{sync::Arc, time::Duration};

    use crate::codex_runtime::{CodexTmuxConfig, CodexTmuxRuntime};
    use crate::control_session::{ControlRuntimeDispatcher, ControlSessionConfig};
    use crate::peer_cli;
    use crate::signaling_client::{ControlSignalingApi, Credential, HttpSignalingClient};
    use ferrosa_memory_core::config::load_config_with_dbaas;
    use ferrosa_memory_core::control_store::CqlControlStore;

    let identity = Arc::new(peer_cli::load_identity(identity_path)?);
    let public = identity.public_identity();
    let fingerprint = public.public_key_fingerprint.0;
    // Device-signed, not API-key. The gateway resolves the account from the
    // signature, so the listener carries nothing that would still be useful to
    // someone who copied it off this disk.
    let api = Arc::new(HttpSignalingClient::with_credential(
        gateway,
        Credential::device(Arc::clone(&identity)),
    ));
    let config = Arc::new(ControlSessionConfig::default());
    let mut memory_config = load_config_with_dbaas()
        .context("loading Ferrosa Memory config for durable mobile control")?;
    if !contact_points.is_empty() {
        memory_config.ferrosa.contact_points = contact_points.to_vec();
    }
    let store = if existing_schema {
        CqlControlStore::connect_existing(&memory_config.ferrosa).await
    } else {
        CqlControlStore::connect(&memory_config.ferrosa).await
    }
    .context("connecting durable mobile control store")?;
    let control_store = Arc::new(store);
    let runtime = CodexTmuxRuntime::new(CodexTmuxConfig::new(workspace, fingerprint.clone()))
        .context("configuring Codex tmux-light runtime")?;
    let dispatcher = Arc::new(ControlRuntimeDispatcher::new(
        Arc::clone(&control_store),
        runtime,
    ));

    // Sessions run CONCURRENTLY, one task each.
    //
    // They used to run in the poll loop itself, so the listener served exactly
    // one controller at a time: a phone that was accepted and then went away
    // pinned it for the whole bind timeout, and a phone that connected pinned
    // it for the entire session. Every other device queued behind and failed
    // with a client-side timeout that said nothing about why. With a phone, a
    // tablet and an iPad on one account that is the normal case, not an edge.
    let in_flight: Arc<tokio::sync::Mutex<std::collections::HashSet<Uuid>>> =
        Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));
    // Bounded so a flood of offers cannot exhaust the host. Sessions beyond the
    // cap stay pending on the broker and are picked up as slots free.
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_CONTROL_SESSIONS));
    println!("control listener device fingerprint: {fingerprint}");
    println!("managed Codex workspace: {}", workspace.display());
    let rtc_api = extensions.rtc_api.clone();
    // Configured sessions are attached by the LISTENER, not supplied by the
    // binary. Running a command is not media and needs nothing the caller has
    // to provide — and every binary that hosts a listener wants it, so making
    // each one remember to attach it is a way for one of them to forget.
    let mut hooks = extensions.attach.clone();
    hooks.push(std::sync::Arc::new(
        crate::shell_extension::ShellExtension::new(
            workspace.clone(),
            session_config_store(workspace),
            // The same cluster this listener already uses. The deferred-work
            // board is a table on it, so the machine can answer "what is
            // outstanding for the repository this agent works in" without the
            // phone needing a route to the database or a credential for it.
            memory_config.ferrosa.contact_points.clone(),
            memory_tenant(memory_config.server.tenant_id.as_deref()),
        ),
    ));

    if extensions.any() {
        println!("{} session extension(s) attached", extensions.attach.len());
    } else {
        // Said out loud. A plain control session is the normal case for the
        // public binary, and silence would leave an operator unable to tell
        // "nothing is attached" from "something failed to attach".
        println!("no session extensions attached");
    }
    println!("polling {gateway} for device-targeted control offers");

    let mut poll_failures: u32 = 0;
    loop {
        // A failed poll is not a reason to stop listening.
        //
        // This propagated with `?`, so one transient broker error ended the
        // process — taking every live session with it, including sessions that
        // were working perfectly. Observed killing a listener mid-stream on a
        // single request failure.
        //
        // Backs off on repeated failures rather than hammering a broker that is
        // already struggling, and says so each time: a listener that has been
        // unable to reach the gateway for a minute is not the same as a quiet
        // one, and silence would make those identical.
        let pending = match api.control_pending_offers(&fingerprint).await {
            Ok(pending) => {
                poll_failures = 0;
                pending
            }
            Err(error) => {
                poll_failures = poll_failures.saturating_add(1);
                let backoff = POLL_BACKOFF_BASE * poll_failures.min(POLL_BACKOFF_MAX_STEPS);
                tracing::warn!(
                    %error,
                    consecutive = poll_failures,
                    retry_in_ms = backoff.as_millis() as u64,
                    "polling the broker for offers failed; still listening"
                );
                tokio::time::sleep(backoff).await;
                continue;
            }
        };
        if pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        for offer in pending {
            // The broker keeps listing an offer until it is accepted, and the
            // accept happens inside the spawned task — so without this the
            // 500ms poll would spawn the same session again and again.
            if !in_flight.lock().await.insert(offer.session_id) {
                continue;
            }
            let permit = match Arc::clone(&slots).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => break,
            };
            let api = Arc::clone(&api);
            let identity = Arc::clone(&identity);
            let config = Arc::clone(&config);
            let dispatcher = Arc::clone(&dispatcher);
            let control_store = Arc::clone(&control_store);
            let in_flight = Arc::clone(&in_flight);
            let fingerprint = fingerprint.clone();
            let rtc = rtc_api.clone();
            let attach = hooks.clone();
            // Kept because `offer` moves into the call below, and the id is
            // needed afterwards to release the in-flight slot.
            let session_id = offer.session_id;
            tokio::spawn(async move {
                // Held for the life of the session, then dropped with it.
                let _permit = permit;
                serve_control_session(
                    api.as_ref(),
                    identity.as_ref(),
                    config.as_ref(),
                    dispatcher.as_ref(),
                    control_store.as_ref(),
                    &fingerprint,
                    offer,
                    rtc,
                    attach,
                )
                .await;
                in_flight.lock().await.remove(&session_id);
            });
        }
    }
}

#[cfg(feature = "webrtc-transport")]
/// Bind one accepted session and serve it until the peer goes away.
///
/// Returns rather than propagating: this runs in its own task, and one
/// controller's failure must not take down the listener for every other
/// device. Every exit path logs why.
#[allow(clippy::too_many_arguments)]
async fn serve_control_session<S, R, T>(
    api: &S,
    identity: &ferrosa_memory_core::remote_identity::InstanceSigningIdentity,
    config: &crate::control_session::ControlSessionConfig,
    dispatcher: &crate::control_session::ControlRuntimeDispatcher<T, R>,
    control_store: &T,
    fingerprint: &str,
    offer: crate::signaling_client::ControlBrokerSessionView,
    rtc: Option<std::sync::Arc<webrtc::api::API>>,
    attach: Vec<std::sync::Arc<dyn SessionExtension>>,
) where
    S: crate::signaling_client::ControlSignalingApi,
    R: crate::control_session::AgentRuntime,
    T: ferrosa_memory_core::control_store::ControlStore + 'static,
{
    use crate::control_session::run_control_server_session;
    println!(
        "accepting control session {} from controller device {}",
        offer.session_id, offer.controller_device_id
    );
    let mut channel =
        match run_control_server_session(api, identity, offer.session_id, config, rtc).await {
            Ok(channel) => channel,
            Err(error) => {
                tracing::warn!(
                    session_id = %offer.session_id,
                    error = %error,
                    "control session bind failed"
                );
                return;
            }
        };
    println!("control session {} bound directly", offer.session_id);
    let peer_state = watch_peer_state(&channel.peer_connection());
    let handle = SessionHandle {
        session_id: offer.session_id,
        peer: channel.peer_connection(),
        sink: channel.frame_sink(),
    };
    for extension in &attach {
        // Logged, not fatal, and the next extension still runs. One failing
        // must not cost the operator the others, nor the control channel
        // underneath them.
        if let Err(error) = extension.on_bound(&handle).await {
            tracing::warn!(session_id = %offer.session_id, %error, "session extension failed");
        }
    }
    // Every path out of the work below — clean disconnect, cursor failure,
    // protocol error, peer that simply stopped answering — has to reach the
    // teardown. Hence the inner future and the unconditional close after it,
    // rather than a `return` at each failure: an extension holding a screen
    // capture must not keep it because a cursor reservation failed.
    session_work(
        &mut channel,
        dispatcher,
        control_store,
        fingerprint,
        &offer,
        &handle,
        &attach,
        peer_state,
    )
    .await;
    for extension in &attach {
        extension.on_closed(offer.session_id).await;
    }
    tracing::info!(session_id = %offer.session_id, "control session closed");
}

/// The body of a bound session.
///
/// Split out so that [`serve_control_session`] has exactly one exit path and
/// can always run the extension teardown. Returns nothing: every failure in
/// here ends this session and only this session, and is logged where it occurs.
#[allow(clippy::too_many_arguments)]
async fn session_work<R, T>(
    channel: &mut crate::control_session::BoundControlChannel,
    dispatcher: &crate::control_session::ControlRuntimeDispatcher<T, R>,
    control_store: &T,
    fingerprint: &str,
    offer: &crate::signaling_client::ControlBrokerSessionView,
    handle: &SessionHandle,
    attach: &[std::sync::Arc<dyn SessionExtension>],
    peer_state: tokio::sync::watch::Receiver<
        webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState,
    >,
) where
    R: crate::control_session::AgentRuntime,
    T: ferrosa_memory_core::control_store::ControlStore + 'static,
{
    use ferrosa_memory_core::control_store::ControlEventDraft;
    use ferrosa_memory_core::types::TenantContext;
    let tenant = TenantContext {
        tenant_id: offer.account_id,
        session_origin: format!("mobile-control:{}", offer.session_id),
    };
    // An AUDIT record that a session began. Nothing below reads the cursor and
    // nothing about serving the operator depends on this row existing.
    //
    // It used to `return` on failure, which ended a perfectly good session
    // because a bookkeeping write did not land. Observed 2026-08-23: two
    // sessions in quick succession collided on the same cursor
    // ("event cursor 4289 is already occupied") and the second was destroyed
    // moments after connecting — the operator saw a machine that connected and
    // then had no agents, because the session was gone before the list could
    // reach them.
    //
    // Losing the record is a real loss and is logged at ERROR so it cannot pass
    // unnoticed. Losing the SESSION over it is a worse one, and is the operator's
    // problem rather than the log's.
    match control_store
        .reserve_cursor_block(&tenant, fingerprint, 64)
        .await
    {
        Err(error) => {
            tracing::error!(
                session_id = %offer.session_id,
                error = %error,
                "reserving a control cursor block failed; this session is NOT \
                 recorded in the durable event log, but continues to serve"
            );
        }
        Ok(block) => {
            if let Err(error) = control_store
                .append_event(
                    &tenant,
                    fingerprint,
                    ControlEventDraft {
                        cursor: block.start,
                        event_id: Uuid::now_v7(),
                        command_id: None,
                        kind: "heartbeat".to_owned(),
                        payload: serde_json::json!({
                            "session_id": offer.session_id,
                            "controller_device_id": offer.controller_device_id,
                        }),
                        created_at: chrono::Utc::now(),
                    },
                )
                .await
            {
                tracing::error!(
                    session_id = %offer.session_id,
                    cursor = block.start,
                    error = %error,
                    "persisting the control-session heartbeat failed; this session \
                     is NOT in the durable event log, but continues to serve"
                );
            }
        }
    }
    // Read a frame, OR notice the peer has gone. Reading alone was not enough:
    // a peer that stops answering without closing leaves this await pending
    // forever, and the session — with everything attached to it — never ends.
    //
    // `recv_text` is cancel-safe (it awaits `mpsc::Receiver::recv`), so losing
    // the race drops no frame.
    let mut lost = std::pin::pin!(peer_lost(peer_state, PEER_DISCONNECT_GRACE));
    loop {
        let frame = tokio::select! {
            received = channel.recv_text() => match received {
                Ok(frame) => frame,
                Err(error) => {
                    tracing::info!(
                        session_id = %offer.session_id,
                        error = %error,
                        "control session disconnected"
                    );
                    break;
                }
            },
            state = &mut lost => {
                tracing::info!(
                    session_id = %offer.session_id,
                    ?state,
                    "peer went away; ending the session"
                );
                break;
            }
        };
        // Extensions get first refusal. Without this the dispatcher rejects
        // any kind it does not know and CLOSES the session, so an extension
        // frame would not merely go unanswered — it would drop the channel it
        // arrived on.
        if let Some((extension, kind)) = claim(attach, &frame) {
            if let Err(error) = extension
                .on_request(handle, &kind, &frame_json(&frame))
                .await
            {
                // The request failed; the session did not. A screen that could
                // not be captured is a screen that could not be captured, not a
                // reason to disconnect a working control channel.
                tracing::warn!(
                    session_id = %offer.session_id,
                    %kind,
                    %error,
                    "session extension refused a request"
                );
            }
            continue;
        }
        match dispatcher.reply(&tenant, fingerprint, &frame).await {
            Ok(Some(reply)) => {
                if let Err(error) = channel.send_text(&reply).await {
                    tracing::info!(
                        session_id = %offer.session_id,
                        error = %error,
                        "control pong send failed"
                    );
                    break;
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    session_id = %offer.session_id,
                    error = %error,
                    "invalid control application frame"
                );
                let _ = channel.close().await;
                break;
            }
        }
    }
}

/// Which extension, if any, claims this frame.
///
/// Reads `body.type` only. A frame that does not parse, or carries no body
/// type, is left for the dispatcher to reject with its own error — this is a
/// routing question, not a validation one, and two places rejecting the same
/// malformed frame differently is how error messages start lying.
fn claim(
    attach: &[std::sync::Arc<dyn SessionExtension>],
    frame: &str,
) -> Option<(std::sync::Arc<dyn SessionExtension>, String)> {
    let value: serde_json::Value = serde_json::from_str(frame).ok()?;
    let kind = value.pointer("/body/type")?.as_str()?.to_owned();
    let extension = attach
        .iter()
        .find(|extension| extension.kinds().contains(&kind.as_str()))?;
    Some((std::sync::Arc::clone(extension), kind))
}

/// Reparse for the extension.
///
/// [`claim`] parses to find the kind and discards the result; parsing twice
/// costs one pass over a frame bounded by `max_frame_bytes`, and is what keeps
/// `claim` a pure predicate rather than a function that returns a routing
/// decision AND a payload. Revisit if frames ever get large enough to notice.
fn frame_json(frame: &str) -> serde_json::Value {
    serde_json::from_str(frame).unwrap_or(serde_json::Value::Null)
}

/// Where a machine keeps its session configs.
///
/// Beside the workspace rather than in a global config directory: the configs
/// are commands to run in THIS workspace, and a machine serving two workspaces
/// should not offer one's build command while sitting in the other.
fn session_config_store(workspace: &std::path::Path) -> std::path::PathBuf {
    workspace.join(".ferrosa").join("sessions.json")
}

/// Publish the peer connection's state to anyone waiting on it.
fn watch_peer_state(
    peer: &std::sync::Arc<webrtc::peer_connection::RTCPeerConnection>,
) -> tokio::sync::watch::Receiver<
    webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState,
> {
    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
    let (sender, receiver) = tokio::sync::watch::channel(RTCPeerConnectionState::New);
    peer.on_peer_connection_state_change(Box::new(move |state| {
        let sender = sender.clone();
        Box::pin(async move {
            // `send_replace`, not `send`: a state change matters even when
            // nobody is currently listening, and `send` fails with no
            // receivers.
            sender.send_replace(state);
        })
    }));
    receiver
}

/// Resolve once the peer should be considered gone.
///
/// `Failed` and `Closed` are terminal and reported at once. `Disconnected` is
/// given [`PEER_DISCONNECT_GRACE`] to recover, and if the state changes in that
/// window the wait restarts against the new state — so a connection that
/// flickers Disconnected → Connected keeps its session.
async fn peer_lost(
    mut states: tokio::sync::watch::Receiver<
        webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState,
    >,
    grace: Duration,
) -> webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState {
    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState as State;
    loop {
        let current = *states.borrow_and_update();
        match current {
            State::Failed | State::Closed => return current,
            State::Disconnected => {
                match tokio::time::timeout(grace, states.changed()).await {
                    // Still disconnected when the grace ran out.
                    Err(_) => return State::Disconnected,
                    // Changed — loop and judge the new state.
                    Ok(Ok(())) => {}
                    // The sender is gone, which means the connection is too.
                    Ok(Err(_)) => return State::Closed,
                }
            }
            _ => {
                if states.changed().await.is_err() {
                    return State::Closed;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Claims(&'static [&'static str]);

    #[async_trait::async_trait]
    impl SessionExtension for Claims {
        fn kinds(&self) -> &'static [&'static str] {
            self.0
        }
        async fn on_bound(&self, _: &SessionHandle) -> Result<(), String> {
            Ok(())
        }
        async fn on_request(
            &self,
            _: &SessionHandle,
            _: &str,
            _: &serde_json::Value,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn on_closed(&self, _: uuid::Uuid) {}
    }

    fn attach(kinds: &'static [&'static str]) -> Vec<std::sync::Arc<dyn SessionExtension>> {
        vec![std::sync::Arc::new(Claims(kinds))]
    }

    fn frame(kind: &str) -> String {
        format!(r#"{{"version":1,"frame_id":"f1","body":{{"type":"{kind}"}}}}"#)
    }

    #[test]
    fn a_declared_kind_is_claimed() {
        let claimed = claim(&attach(&["visual_start"]), &frame("visual_start"));
        assert_eq!(
            claimed.map(|(_, kind)| kind).as_deref(),
            Some("visual_start")
        );
    }

    /// The reason routing exists at all. `command` reaching an extension
    /// instead of the dispatcher would silently break every command the app
    /// sends.
    #[test]
    fn an_undeclared_kind_is_left_for_the_dispatcher() {
        assert!(claim(&attach(&["visual_start"]), &frame("command")).is_none());
    }

    /// With nothing attached — the public binary — routing must be inert.
    #[test]
    fn nothing_is_claimed_when_nothing_is_attached() {
        assert!(claim(&[], &frame("visual_start")).is_none());
    }

    /// A malformed frame belongs to the dispatcher, which already has an error
    /// for it. Claiming it here would report the wrong reason for the failure.
    #[test]
    fn a_frame_that_does_not_parse_is_not_claimed() {
        assert!(claim(&attach(&["visual_start"]), "{not json").is_none());
        assert!(claim(&attach(&["visual_start"]), r#"{"body":{}}"#).is_none());
    }

    // --- peer_lost ---
    //
    // These pin the behaviour that was missing entirely: nothing watched the
    // peer state, so a viewer whose connection dropped left the session — and
    // its screen capture — running indefinitely.

    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState as State;

    const GRACE: Duration = Duration::from_secs(10);

    /// Terminal states end the session with no waiting.
    #[tokio::test]
    async fn a_failed_peer_is_lost_at_once() {
        let (tx, rx) = tokio::sync::watch::channel(State::Failed);
        assert_eq!(peer_lost(rx, GRACE).await, State::Failed);
        drop(tx);
    }

    #[tokio::test]
    async fn a_closed_peer_is_lost_at_once() {
        let (tx, rx) = tokio::sync::watch::channel(State::Closed);
        assert_eq!(peer_lost(rx, GRACE).await, State::Closed);
        drop(tx);
    }

    /// A healthy connection must never resolve. Without this the session would
    /// be torn down while it was working.
    #[tokio::test(start_paused = true)]
    async fn a_connected_peer_is_not_lost() {
        let (_tx, rx) = tokio::sync::watch::channel(State::Connected);
        assert!(
            tokio::time::timeout(GRACE * 10, peer_lost(rx, GRACE))
                .await
                .is_err(),
            "a connected peer must not be reported lost"
        );
    }

    /// The measured bug: Disconnected persisted and nothing noticed.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_stays_disconnected_is_lost_after_the_grace() {
        let (_tx, rx) = tokio::sync::watch::channel(State::Disconnected);
        assert_eq!(peer_lost(rx, GRACE).await, State::Disconnected);
    }

    /// And the reason the grace exists: a flicker must not kill the session.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_recovers_within_the_grace_is_kept() {
        let (tx, rx) = tokio::sync::watch::channel(State::Disconnected);
        tokio::spawn(async move {
            tokio::time::sleep(GRACE / 2).await;
            tx.send_replace(State::Connected);
            // Held so the channel does not close, which would itself count as
            // loss and pass the test for the wrong reason.
            tokio::time::sleep(GRACE * 20).await;
            drop(tx);
        });
        assert!(
            tokio::time::timeout(GRACE * 5, peer_lost(rx, GRACE))
                .await
                .is_err(),
            "a peer that came back must not be reported lost"
        );
    }

    /// A drop-out that recovers and then drops again is lost on the second one
    /// — the grace restarts, it is not a one-time allowance.
    #[tokio::test(start_paused = true)]
    async fn a_second_disconnect_after_recovery_is_still_lost() {
        let (tx, rx) = tokio::sync::watch::channel(State::Disconnected);
        tokio::spawn(async move {
            tokio::time::sleep(GRACE / 2).await;
            tx.send_replace(State::Connected);
            tokio::time::sleep(GRACE / 2).await;
            tx.send_replace(State::Disconnected);
            tokio::time::sleep(GRACE * 20).await;
            drop(tx);
        });
        assert_eq!(peer_lost(rx, GRACE).await, State::Disconnected);
    }

    /// First match wins, deterministically by attachment order — so a collision
    /// is a stable bug rather than an intermittent one.
    #[test]
    fn the_first_extension_declaring_a_kind_wins() {
        let both: Vec<std::sync::Arc<dyn SessionExtension>> = vec![
            std::sync::Arc::new(Claims(&["shared"])),
            std::sync::Arc::new(Claims(&["shared"])),
        ];
        let (winner, _) = claim(&both, &frame("shared")).expect("claimed");
        assert!(std::sync::Arc::ptr_eq(&winner, &both[0]));
    }

    /// The regression this function exists for.
    ///
    /// A config with no tenant must resolve to None, NOT to the task board's
    /// tenant. Asserting the absence is the whole point: the bug it replaces
    /// returned a perfectly valid UUID that simply pointed somewhere else.
    #[test]
    fn an_unset_memory_tenant_resolves_to_nothing() {
        // The resolver reads this variable, so a value left by the environment
        // would decide the assertion instead of the config.
        unsafe { std::env::remove_var("FERROSA_MEMORY_TENANT_ID") };
        assert_eq!(memory_tenant(None), None);
        assert_ne!(
            memory_tenant(None),
            Some(Uuid::from_u128(1)),
            "the board's tenant is not a default for the memory's"
        );
    }

    #[test]
    fn a_configured_memory_tenant_is_used_verbatim() {
        assert_eq!(
            memory_tenant(Some("9a5f8fbf-d842-4d30-8ea5-1aa931e618a8")),
            Some(Uuid::parse_str("9a5f8fbf-d842-4d30-8ea5-1aa931e618a8").unwrap())
        );
    }

    /// A typo must not read as "nobody configured one".
    #[test]
    fn a_malformed_memory_tenant_is_refused_rather_than_guessed() {
        assert_eq!(memory_tenant(Some("not-a-uuid")), None);
    }
}
