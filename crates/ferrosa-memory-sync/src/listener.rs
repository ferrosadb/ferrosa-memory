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

use anyhow::Context as _;
use uuid::Uuid;

use crate::visual::{SurfaceProvider, VisualCapture};

/// How many control sessions one memory system serves at once.
///
/// A bound, not a target. Each session is a peer connection plus a durable
/// cursor block, so an unbounded fan-out would let anyone holding an account
/// key exhaust the host by opening offers. Eight covers a person's own devices
/// several times over; sessions past it stay pending on the broker and start as
/// slots free rather than being refused.
const MAX_CONCURRENT_CONTROL_SESSIONS: usize = 8;

/// A caller's hook for an attested session.
///
/// A trait rather than a boxed closure so an implementation can hold state
/// across sessions — an encoder pool, a session registry — without smuggling it
/// through captured variables.
pub trait SessionBound: Send + Sync {
    /// Attach to a session whose hello has been verified.
    ///
    /// Errors are logged and the session continues WITHOUT whatever the caller
    /// was attaching. A failure to start video must not tear down a working
    /// control channel: the operator keeps the session they asked for and loses
    /// only the part that broke.
    fn on_bound(
        &self,
        peer: std::sync::Arc<webrtc::peer_connection::RTCPeerConnection>,
        session_id: uuid::Uuid,
    ) -> Result<(), String>;
}

/// The visual-streaming plugins a listener runs with.
///
/// Passed in rather than constructed, because choosing them is the ONLY
/// difference between the public binary and a private one that links real
/// capture. Anything else a binary needs to customize belongs here too, or the
/// seam will grow a second one beside it.
pub struct VisualPlugins {
    /// A WebRTC API built by the plugin, when it needs one this crate would not
    /// build — codecs for a media track, for instance.
    ///
    /// `None` uses the data-channel-only default, which is what a host with no
    /// capture plugin should negotiate. This crate never inspects it.
    pub rtc_api: Option<std::sync::Arc<webrtc::api::API>>,
    /// Called once a session's signed hello has been attested.
    ///
    /// The moment — and the only moment — at which a caller may attach things
    /// this crate does not model. It runs AFTER attestation deliberately:
    /// attaching earlier would start sending to a peer whose identity has not
    /// been proven.
    ///
    /// Takes the peer connection, not a media type. This crate does not know
    /// what the caller does with it, and must not.
    pub on_session_bound: Option<std::sync::Arc<dyn SessionBound>>,
    pub provider: Box<dyn SurfaceProvider>,
    /// Builds a capture per session. A factory rather than an instance because
    /// sessions are concurrent and each needs its own.
    pub capture: Box<dyn Fn() -> Box<dyn VisualCapture> + Send + Sync>,
}

impl VisualPlugins {
    /// The default: a host that cannot stream.
    ///
    /// Reports no capability, so a client hides the control rather than
    /// offering a session that cannot start.
    pub fn null() -> Self {
        Self {
            rtc_api: None,
            on_session_bound: None,
            provider: Box::new(crate::visual::NullSurfaceProvider),
            capture: Box::new(|| Box::new(crate::visual::NullCapture::new())),
        }
    }
}

impl Default for VisualPlugins {
    fn default() -> Self {
        Self::null()
    }
}

#[cfg(feature = "webrtc-transport")]
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
    visual: VisualPlugins,
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
    let rtc_api = visual.rtc_api.clone();
    let bound_hook = visual.on_session_bound.clone();
    let caps = visual.provider.capabilities();
    if caps.any() {
        println!(
            "visual streaming available: browser={} desktop={} up to {}x{}@{}",
            caps.browser, caps.desktop, caps.max_width, caps.max_height, caps.max_fps
        );
    } else {
        // Said out loud. A host with no capture is the normal case for the
        // public binary, and silence here would leave an operator wondering
        // whether streaming failed or was never present.
        println!("visual streaming not available on this host (no capture plugin)");
    }
    let _visual = visual;
    println!("polling {gateway} for device-targeted control offers");

    loop {
        let pending = api.control_pending_offers(&fingerprint).await?;
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
            let on_bound = bound_hook.clone();
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
                    on_bound,
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
    on_bound: Option<std::sync::Arc<dyn SessionBound>>,
) where
    S: crate::signaling_client::ControlSignalingApi,
    R: crate::control_session::AgentRuntime,
    T: ferrosa_memory_core::control_store::ControlStore + 'static,
{
    use crate::control_session::run_control_server_session_with_rtc;
    use ferrosa_memory_core::control_store::ControlEventDraft;
    use ferrosa_memory_core::types::TenantContext;

    println!(
        "accepting control session {} from controller device {}",
        offer.session_id, offer.controller_device_id
    );
    let mut channel =
        match run_control_server_session_with_rtc(api, identity, offer.session_id, config, rtc)
            .await
        {
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
    if let Some(hook) = on_bound
        && let Err(error) = hook.on_bound(channel.peer_connection(), offer.session_id)
    {
        // Logged, not fatal. Whatever the caller was attaching failed; the
        // control channel underneath it still works, and taking the whole
        // session down would lose the part that was fine.
        tracing::warn!(session_id = %offer.session_id, %error, "session-bound hook failed");
    }
    let tenant = TenantContext {
        tenant_id: offer.account_id,
        session_origin: format!("mobile-control:{}", offer.session_id),
    };
    let cursor = match control_store
        .reserve_cursor_block(&tenant, fingerprint, 64)
        .await
    {
        Ok(block) => block.start,
        Err(error) => {
            // Ends THIS session, not the listener. A cursor failure used to
            // propagate out of the poll loop and stop the process, taking every
            // other device's session with it.
            tracing::warn!(
                session_id = %offer.session_id,
                error = %error,
                "reserving durable mobile control cursor block failed"
            );
            return;
        }
    };
    if let Err(error) = control_store
        .append_event(
            &tenant,
            fingerprint,
            ControlEventDraft {
                cursor,
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
        tracing::warn!(
            session_id = %offer.session_id,
            error = %error,
            "persisting control-session heartbeat failed"
        );
        return;
    }
    loop {
        let frame = match channel.recv_text().await {
            Ok(frame) => frame,
            Err(error) => {
                tracing::info!(
                    session_id = %offer.session_id,
                    error = %error,
                    "control session disconnected"
                );
                break;
            }
        };
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
