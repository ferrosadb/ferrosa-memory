//! Module: The wire surface for configured sessions.
//! Correctness: Correct when a device cannot run anything without a grant,
//! when a config survives a restart of the machine, and when output reaches
//! the device as it is produced rather than when the command finishes.
//! Last revised: 2026-08-23
//! Last changed: working_dir carried on add/update/list and persisted, so an agent's directory survives a restart.
//!
//! # Three capabilities wearing one grant, for now
//!
//! Creating a config, running one, and typing into a running session are
//! different amounts of power:
//!
//! - **Creating** is arbitrary code execution on the machine, just persisted.
//! - **Running** is bounded by what already exists, which is what makes it
//!   safe to hand to a device you half-trust.
//! - **Typing** is weaker again, though a config that runs `bash` collapses
//!   the distinction — its real bound is which configs exist.
//!
//! They are separate frame kinds so the grant can split along those lines
//! later without changing the protocol. Today one `shell_start` covers all
//! three, which is the same coarse gate input uses and is NOT shippable beyond
//! the operator's own devices. The check is one function per operation
//! precisely so swapping its body is the whole change.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::listener::{SessionExtension, SessionHandle};
use crate::session_config::{SessionConfig, SessionKind};
use crate::session_runtime::{RunningSession, SessionRuntime};

/// Configured sessions, and whoever is currently allowed to use them.
pub struct ShellExtension {
    /// The machine's configs. Shared by every device, which is the point:
    /// the tablet and the phone see one list, and a resumed tmux session can
    /// be attributed to the config that made it.
    configs: Arc<Mutex<Vec<SessionConfig>>>,
    /// Where configs are written, so they survive a restart.
    store_path: PathBuf,
    runtime: Arc<SessionRuntime>,
    /// Per session: whether input is granted, and what is open.
    ///
    /// The grant lives HERE rather than being implied by an open session, so
    /// it can be withdrawn while a session is running. A model where the grant
    /// lasted as long as the session would have to be unpicked when real
    /// capability grants land, because an owner revoking access mid-command
    /// must stop it.
    sessions: Mutex<HashMap<uuid::Uuid, ShellState>>,
}

#[derive(Default)]
struct ShellState {
    granted: bool,
    open: Option<Arc<RunningSession>>,
    /// Pumps output to the device until the session ends.
    pump: Option<tokio::task::JoinHandle<()>>,
    /// Ticks the roster's status frames while the control session lives.
    status_pump: Option<tokio::task::JoinHandle<()>>,
}

impl ShellExtension {
    pub fn new(workspace: impl Into<PathBuf>, store_path: impl Into<PathBuf>) -> Self {
        let store_path = store_path.into();
        let workspace = workspace.into();
        Self {
            configs: Arc::new(Mutex::new(load_configs(&store_path))),
            store_path,
            runtime: Arc::new(SessionRuntime::new(workspace)),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Whether this session may use configured sessions at all.
    ///
    /// One function, deliberately. When real capability grants land, its body
    /// changes from "did this session ask" to "does this device hold the
    /// capability" and nothing else moves. Checked per OPERATION rather than
    /// once at open, so a grant withdrawn mid-command takes effect.
    async fn permitted(&self, session_id: uuid::Uuid) -> bool {
        self.sessions
            .lock()
            .await
            .get(&session_id)
            .is_some_and(|state| state.granted)
    }

    async fn configs_frame(&self) -> serde_json::Value {
        let configs = self.configs.lock().await;
        let mut listed = Vec::with_capacity(configs.len());
        for config in configs.iter() {
            listed.push(serde_json::json!({
                "id": config.id.to_string(),
                "name": config.name,
                "kind": config.kind.as_wire(),
                "command": config.command,
                "working_dir": config.working_dir,
                // Whether a tmux session is already up, so the shell can say
                // "resume" rather than "run" and the operator knows a second
                // one is not about to start.
                "running": self.runtime.is_running(config).await,
            }));
        }
        serde_json::json!({ "type": "shell_configs", "configs": listed })
    }

    async fn add_config(&self, body: &serde_json::Value) -> Result<(), String> {
        let config = SessionConfig::create(
            text(body, "name"),
            text(body, "kind"),
            command_list(body),
            body.get("working_dir").and_then(serde_json::Value::as_str),
        )
        .map_err(|error| error.to_string())?;
        let mut configs = self.configs.lock().await;
        configs.push(config);
        save_configs(&self.store_path, &configs);
        Ok(())
    }

    /// Change an existing config.
    ///
    /// A tmux session already running keeps running the OLD command — it was
    /// started with it, and nothing here can retroactively change what a
    /// process was launched as. Reported so the operator knows the edit takes
    /// effect next time, rather than believing a running build picked up their
    /// change.
    async fn update_config(&self, body: &serde_json::Value) -> Result<bool, String> {
        let id = config_id(body)?;
        let name = text(body, "name");
        let kind = text(body, "kind");
        let command = command_list(body);

        let mut configs = self.configs.lock().await;
        let position = configs
            .iter()
            .position(|config| config.id == id)
            .ok_or_else(|| "no config with that id".to_owned())?;
        let updated = configs[position]
            .edited(
                name,
                kind,
                command,
                body.get("working_dir").and_then(serde_json::Value::as_str),
            )
            .map_err(|error| error.to_string())?;
        let was_running = self.runtime.is_running(&configs[position]).await;
        configs[position] = updated;
        save_configs(&self.store_path, &configs);
        Ok(was_running)
    }

    /// Remove a config.
    ///
    /// Refused while its tmux session is running. Deleting would leave a
    /// process alive that nothing can name, attribute or stop — an orphan the
    /// operator would find later in `tmux ls` with no idea what started it.
    /// Killing it silently is worse: "delete this config" is not "stop my
    /// build".
    async fn delete_config(&self, body: &serde_json::Value) -> Result<(), String> {
        let id = config_id(body)?;
        let mut configs = self.configs.lock().await;
        let position = configs
            .iter()
            .position(|config| config.id == id)
            .ok_or_else(|| "no config with that id".to_owned())?;
        if self.runtime.is_running(&configs[position]).await {
            return Err(
                "this config's session is still running — detach and stop it before deleting"
                    .to_owned(),
            );
        }
        configs.remove(position);
        save_configs(&self.store_path, &configs);
        Ok(())
    }

    /// Open a session and start pumping its output to the device.
    async fn open(&self, session: &SessionHandle, config_id: &str) -> Result<(), String> {
        let session_id = session.session_id();
        let wanted: uuid::Uuid = config_id
            .parse()
            .map_err(|_| "that is not a config id".to_owned())?;
        let config = self
            .configs
            .lock()
            .await
            .iter()
            .find(|config| config.id == wanted)
            .cloned()
            .ok_or_else(|| "no config with that id".to_owned())?;

        let running = Arc::new(
            self.runtime
                .open(&config)
                .await
                .map_err(|error| error.to_string())?,
        );

        // Its own task, so opening returns immediately and the control channel
        // keeps serving. A pump running inline would block every other frame
        // on this session for as long as the command ran.
        let pump = tokio::spawn(pump_output(
            Arc::clone(&running),
            session.clone(),
            config.id,
        ));

        let mut sessions = self.sessions.lock().await;
        let state = sessions.entry(session_id).or_default();
        // Replacing an open session closes the old one first. Two pumps
        // writing the same channel interleave their output into something
        // neither command said.
        if let Some(previous) = state.pump.take() {
            previous.abort();
        }
        state.open = Some(running);
        state.pump = Some(pump);
        Ok(())
    }

    /// Close whatever is open, if anything.
    ///
    /// For a tmux session this detaches; the session keeps running. For an
    /// ephemeral one, dropping it kills the process — which is the difference
    /// the operator chose between the two kinds.
    async fn close(&self, session_id: uuid::Uuid) {
        let mut sessions = self.sessions.lock().await;
        if let Some(state) = sessions.get_mut(&session_id) {
            if let Some(pump) = state.pump.take() {
                pump.abort();
            }
            state.open = None;
        }
    }
}

/// Forward a session's output to the device until it ends.
/// The most raw terminal bytes one `shell_output` frame may carry.
///
/// Deliberately small. A data channel is SCTP over DTLS over UDP, and SCTP
/// fragments a large message to the path MTU it believes in. On a path where
/// MTU discovery does not work — mobile carriers routinely drop the ICMP that
/// makes it work — those fragments are silently lost, so a big message never
/// arrives while small ones do.
///
/// That is the shape of the bug this bounds: video kept working (RTP packetises
/// to about 1200 bytes of its own accord) and small control frames kept
/// working, while terminal output alone vanished off WiFi. 512 bytes of raw
/// input becomes roughly 700 of base64 plus the envelope, which stays inside a
/// conservative MTU with room to spare.
///
/// The channel is reliable and ORDERED, so splitting changes nothing the
/// emulator sees: it receives the same bytes in the same sequence, in more
/// pieces.
const MAX_OUTPUT_BYTES_PER_FRAME: usize = 512;

/// How often the roster's status is refreshed.
///
/// Each tick runs one `capture-pane` per tmux job. Slow enough that a handful
/// of jobs costs nothing, fast enough that a glance at the roster is current.
const STATUS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Tell the device what every job is doing, until the control session ends.
/// What a job looked like on the last tick, so the next can send only changes.
type JobSnapshot = std::collections::HashMap<uuid::Uuid, (bool, Option<String>)>;

/// Read every job's current state.
async fn read_status(configs: &Mutex<Vec<SessionConfig>>, runtime: &SessionRuntime) -> JobSnapshot {
    let snapshot = configs.lock().await.clone();
    let mut out = JobSnapshot::with_capacity(snapshot.len());
    for config in &snapshot {
        let running = runtime.is_running(config).await;
        // Absent rather than empty when there is nothing yet: the device says
        // "no output yet" instead of rendering silence as a blank line.
        let line = if running {
            runtime.last_line(config).await
        } else {
            None
        };
        out.insert(config.id, (running, line));
    }
    out
}

/// Tell the device what changed, and when.
///
/// A DELTA, not a snapshot. Two reasons, and the second is the important one:
///
/// - Frames stay small. A full snapshot of every job's status on every tick
///   would grow past the datagram budget the output path had to be split to
///   respect, and would do it silently as jobs are added.
/// - A change is an EVENT. Sending only what moved, with the time it moved,
///   gives the device a stream it can interleave into a feed across jobs —
///   which a repeated snapshot cannot, because it cannot tell a line that just
///   appeared from one that has been there for an hour.
///
/// The first tick sends everything, because a device that just connected has
/// no prior state to diff against.
async fn pump_status(
    configs: Arc<Mutex<Vec<SessionConfig>>>,
    runtime: Arc<SessionRuntime>,
    session: SessionHandle,
) {
    let mut previous: Option<JobSnapshot> = None;

    loop {
        let current = read_status(&configs, &runtime).await;
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0);

        let changed: Vec<_> = current
            .iter()
            .filter(|(id, now)| {
                previous
                    .as_ref()
                    .is_none_or(|was| was.get(*id) != Some(now))
            })
            .map(|(id, (running, line))| {
                serde_json::json!({
                    "id": id.to_string(),
                    "running": running,
                    "last_line": line,
                    "changed_at": at,
                })
            })
            .collect();

        // Nothing moved. Say nothing — a tick that sends an empty list every
        // three seconds is a heartbeat pretending to be news.
        if !changed.is_empty() {
            let frame = serde_json::json!({
                "version": 1,
                "frame_id": uuid::Uuid::now_v7().to_string(),
                "body": {
                    "type": "shell_status",
                    // The device clears its state on a full send; on a delta it
                    // merges. Without this a reconnect would leave stale jobs
                    // in the roster forever.
                    "full": previous.is_none(),
                    "jobs": changed,
                },
            });
            if session.send(&frame.to_string()).await.is_err() {
                // The channel has gone; so has the reason to keep scraping.
                return;
            }
        }

        previous = Some(current);
        tokio::time::sleep(STATUS_INTERVAL).await;
    }
}

/// One `shell_output` frame carrying raw terminal bytes.
///
/// Base64, not a string. This is a raw terminal stream — escape sequences,
/// cursor moves, whatever encoding the program chose. A lossy UTF-8 conversion
/// would replace exactly the bytes the emulator needs to position anything.
async fn send_output(
    session: &SessionHandle,
    config_id: uuid::Uuid,
    bytes: &[u8],
) -> Result<(), ()> {
    use base64::Engine as _;

    let frame = serde_json::json!({
        "version": 1,
        "frame_id": uuid::Uuid::now_v7().to_string(),
        "body": {
            "type": "shell_output",
            "config_id": config_id.to_string(),
            "bytes": base64::engine::general_purpose::STANDARD.encode(bytes),
        },
    });
    session.send(&frame.to_string()).await.map_err(|_| ())
}

async fn pump_output(running: Arc<RunningSession>, session: SessionHandle, config_id: uuid::Uuid) {
    while let Some(bytes) = running.next_output().await {
        // Split rather than sent whole. See MAX_OUTPUT_BYTES_PER_FRAME.
        for piece in bytes.chunks(MAX_OUTPUT_BYTES_PER_FRAME) {
            if send_output(&session, config_id, piece).await.is_err() {
                // The channel has gone; so has the reason to keep reading.
                return;
            }
        }
    }
    // Ended. Said explicitly, because a transcript that simply stops is
    // indistinguishable from one that is still waiting.
    let dropped = running.dropped();
    let frame = serde_json::json!({
        "version": 1,
        "frame_id": uuid::Uuid::now_v7().to_string(),
        "body": {
            "type": "shell_ended",
            "config_id": config_id.to_string(),
            "resumable": running.config().kind.resumable(),
            // Surfaced so the shell can mark the gap rather than showing a
            // transcript with a silent hole, which reads as the command having
            // produced nothing.
            "dropped": dropped,
        },
    });
    let _ = session.send(&frame.to_string()).await;
}

#[async_trait::async_trait]
impl SessionExtension for ShellExtension {
    fn kinds(&self) -> &'static [&'static str] {
        &[
            "shell_start",
            "shell_stop",
            "shell_list",
            "shell_add_config",
            "shell_update_config",
            "shell_delete_config",
            "shell_open",
            "shell_close",
            "shell_delete_session",
            "shell_resize",
            "shell_input",
        ]
    }

    async fn on_bound(&self, session: &SessionHandle) -> Result<(), String> {
        // Registered, not granted. Binding a control session is not asking to
        // run commands on the machine.
        self.sessions
            .lock()
            .await
            .insert(session.session_id(), ShellState::default());
        Ok(())
    }

    async fn on_request(
        &self,
        session: &SessionHandle,
        kind: &str,
        frame: &serde_json::Value,
    ) -> Result<(), String> {
        let session_id = session.session_id();
        let body = frame
            .get("body")
            .ok_or_else(|| "frame has no body".to_owned())?;

        // Granting is the one thing that does not require a grant.
        if kind == "shell_start" {
            let mut sessions = self.sessions.lock().await;
            let state = sessions.entry(session_id).or_default();
            state.granted = true;
            // One pump per control session. Re-granting must not stack a second
            // one — two pumps means two status frames per tick and a roster
            // that flickers between them.
            if state.status_pump.is_none() {
                state.status_pump = Some(tokio::spawn(pump_status(
                    Arc::clone(&self.configs),
                    Arc::clone(&self.runtime),
                    session.clone(),
                )));
            }
            drop(sessions);
            tracing::info!(%session_id, "shell granted");
            return session.send(&envelope(self.configs_frame().await)).await;
        }
        if kind == "shell_stop" {
            self.close(session_id).await;
            if let Some(state) = self.sessions.lock().await.get_mut(&session_id) {
                state.granted = false;
            }
            tracing::info!(%session_id, "shell revoked");
            return Ok(());
        }

        // Everything else is checked per operation, not once at open, so a
        // withdrawn grant takes effect on a session already running.
        if !self.permitted(session_id).await {
            return Err("shell access has not been granted for this session".to_owned());
        }

        match kind {
            "shell_list" => session.send(&envelope(self.configs_frame().await)).await,
            "shell_add_config" => {
                self.add_config(body).await?;
                // The new list, not just an acknowledgement — the device asked
                // to change the list, so what it needs back is the list.
                session.send(&envelope(self.configs_frame().await)).await
            }
            "shell_update_config" => {
                let was_running = self.update_config(body).await?;
                session.send(&envelope(self.configs_frame().await)).await?;
                if was_running {
                    // Said plainly. A running process cannot retroactively
                    // become the command it was just edited to be, and an
                    // operator who believes otherwise will trust output from a
                    // build they think they changed.
                    session
                        .send(&envelope(serde_json::json!({
                            "type": "shell_notice",
                            "text": "The running session still uses the previous command. \
                                     The change applies next time it starts.",
                        })))
                        .await?;
                }
                Ok(())
            }
            "shell_delete_config" => {
                self.delete_config(body).await?;
                session.send(&envelope(self.configs_frame().await)).await
            }
            "shell_open" => {
                let config_id = body
                    .get("config_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "open needs a config_id".to_owned())?;
                self.open(session, config_id).await
            }
            "shell_close" => {
                self.close(session_id).await;
                Ok(())
            }
            "shell_resize" => {
                // A TUI lays out to the size it is told, so a wrong one wraps
                // or truncates every frame it draws. The device is the only
                // thing that knows how big its terminal is.
                let rows = body.get("rows").and_then(serde_json::Value::as_u64);
                let cols = body.get("cols").and_then(serde_json::Value::as_u64);
                let (Some(rows), Some(cols)) = (rows, cols) else {
                    return Err("resize needs rows and cols".to_owned());
                };
                // Refused rather than clamped. A zero here means the device
                // measured nothing, and telling the program it has a
                // zero-column terminal makes it draw nothing at all.
                if rows == 0 || cols == 0 {
                    return Err("a terminal cannot be zero rows or columns".to_owned());
                }
                let open = self
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .and_then(|state| state.open.clone())
                    .ok_or_else(|| "no session is open".to_owned())?;
                open.resize(
                    u16::try_from(rows.min(u64::from(u16::MAX))).unwrap_or(u16::MAX),
                    u16::try_from(cols.min(u64::from(u16::MAX))).unwrap_or(u16::MAX),
                )
                .map_err(|error| error.to_string())
            }
            "shell_delete_session" => {
                // Deliberately distinct from shell_close. Closing a persistent
                // session detaches and leaves it running, which is the whole
                // reason to choose that kind; this is the operator saying the
                // work is finished and the session should stop existing.
                let open = self
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .and_then(|state| state.open.clone())
                    .ok_or_else(|| "no session is open".to_owned())?;
                open.destroy().map_err(|error| error.to_string())?;
                self.close(session_id).await;
                // The list changes — the agent stops being "in use" — so send
                // it rather than leaving the device to guess.
                session.send(&envelope(self.configs_frame().await)).await
            }
            "shell_input" => {
                let open = self
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .and_then(|state| state.open.clone())
                    .ok_or_else(|| "no session is open".to_owned())?;

                // Text and keys are separate fields, and a frame may carry
                // either or both. A bare Return — no text at all — is how a
                // CLI waiting for confirmation is answered, so "empty means
                // nothing to do" would make the app unable to say yes.
                if let Some(text) = body.get("text").and_then(serde_json::Value::as_str)
                    && !text.is_empty()
                {
                    open.send(text).map_err(|error| error.to_string())?;
                }

                // Raw bytes from a client-side emulator. This is the normal
                // path now that the device runs a real terminal; `text` and
                // `keys` remain for the on-screen key bar and for any client
                // without an emulator.
                let raw = body.get("bytes").and_then(serde_json::Value::as_str);
                if let Some(encoded) = raw {
                    use base64::Engine as _;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|error| format!("input bytes are not base64: {error}"))?;
                    open.send_bytes(&bytes).map_err(|error| error.to_string())?;
                }

                let keys = body.get("keys").and_then(serde_json::Value::as_array);
                if let Some(keys) = keys {
                    for name in keys.iter().filter_map(serde_json::Value::as_str) {
                        // Refused, not guessed. `send-keys` reads its argument
                        // as tmux's command language, so forwarding an unknown
                        // name would hand the device remote tmux control rather
                        // than a keypress.
                        let key = crate::session_runtime::NamedKey::from_wire(name)
                            .ok_or_else(|| format!("{name} is not a key this machine sends"))?;
                        open.send_key(key).map_err(|error| error.to_string())?;
                    }
                }

                if body.get("text").is_none() && keys.is_none() && raw.is_none() {
                    return Err("input needs bytes, text or keys".to_owned());
                }
                Ok(())
            }
            other => Err(format!("claimed {other} and cannot serve it")),
        }
    }

    async fn on_closed(&self, session_id: uuid::Uuid) {
        // The status pump outlives an open session but not the control session.
        // Leaving it running would scrape tmux forever for a device that has
        // gone, which is invisible and never stops.
        if let Some(state) = self.sessions.lock().await.get_mut(&session_id)
            && let Some(pump) = state.status_pump.take()
        {
            pump.abort();
        }
        self.close(session_id).await;
        // The grant goes with the session. An ephemeral session is killed by
        // dropping it; a tmux one keeps running, which is why it was chosen.
        self.sessions.lock().await.remove(&session_id);
    }
}

fn config_id(body: &serde_json::Value) -> Result<uuid::Uuid, String> {
    body.get("config_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| "that is not a config id".to_owned())
}

fn text<'a>(body: &'a serde_json::Value, key: &str) -> &'a str {
    body.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

fn command_list(body: &serde_json::Value) -> Vec<String> {
    body.get("command")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn envelope(body: serde_json::Value) -> String {
    serde_json::json!({
        "version": 1,
        "frame_id": uuid::Uuid::now_v7().to_string(),
        "body": body,
    })
    .to_string()
}

/// Read the configs from disk, tolerating their absence.
///
/// A missing or unreadable store yields an empty list rather than refusing to
/// start: a machine with no configs yet is the normal first run, and a
/// listener that would not start because of it is worse than one with nothing
/// configured.
fn load_configs(path: &PathBuf) -> Vec<SessionConfig> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        tracing::warn!(path = %path.display(), "the session config store is not valid JSON; starting empty");
        return Vec::new();
    };
    value
        .as_array()
        .map(|items| items.iter().filter_map(config_from_json).collect())
        .unwrap_or_default()
}

fn config_from_json(value: &serde_json::Value) -> Option<SessionConfig> {
    Some(SessionConfig {
        id: value.get("id")?.as_str()?.parse().ok()?,
        name: value.get("name")?.as_str()?.to_owned(),
        kind: SessionKind::from_wire(value.get("kind")?.as_str()?)?,
        // Absent in a file written before the field existed, which correctly
        // means "the machine's workspace" — the behaviour those configs had.
        working_dir: value
            .get("working_dir")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        command: value
            .get("command")?
            .as_array()?
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
    })
}

/// Write the configs, reporting a failure rather than losing it.
///
/// A config that was accepted and then silently not persisted comes back after
/// a restart as a config the operator is sure they created.
fn save_configs(path: &PathBuf, configs: &[SessionConfig]) {
    let listed: Vec<serde_json::Value> = configs
        .iter()
        .map(|config| {
            serde_json::json!({
                "id": config.id.to_string(),
                "name": config.name,
                "kind": config.kind.as_wire(),
                "command": config.command,
                "working_dir": config.working_dir,
            })
        })
        .collect();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(&listed) {
        Ok(text) => {
            if let Err(error) = std::fs::write(path, text) {
                tracing::error!(path = %path.display(), %error, "could not save session configs");
            }
        }
        Err(error) => {
            tracing::error!(%error, "could not serialise session configs");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> PathBuf {
        std::env::temp_dir().join(format!("ferrosa-configs-{}.json", uuid::Uuid::now_v7()))
    }

    /// A first run has no store, and must start rather than refuse.
    #[test]
    fn a_missing_store_starts_empty() {
        assert!(load_configs(&store()).is_empty());
    }

    /// Corruption must not stop the listener booting. Reported, then empty.
    #[test]
    fn an_unreadable_store_starts_empty() {
        let path = store();
        std::fs::write(&path, "{ not json").expect("write");
        assert!(load_configs(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// The property that matters: a config created today is there tomorrow.
    #[test]
    fn configs_survive_a_round_trip_through_the_store() {
        let path = store();
        let original = vec![
            SessionConfig::create("build", "tmux", vec!["cargo".into(), "test".into()], None)
                .expect("valid"),
            SessionConfig::create("shell", "bash", vec!["bash".into(), "-l".into()], None)
                .expect("valid"),
        ];
        save_configs(&path, &original);
        assert_eq!(load_configs(&path), original);
        let _ = std::fs::remove_file(&path);
    }

    /// One bad entry must not discard the rest — a hand-edited store with a
    /// typo would otherwise lose every config the operator had.
    #[test]
    fn one_malformed_entry_does_not_lose_the_others() {
        let path = store();
        let good = SessionConfig::create("keep", "bash", vec!["ls".into()], None).expect("valid");
        let text = format!(
            r#"[{{"id":"{}","name":"keep","kind":"bash","command":["ls"]}},{{"name":"broken"}}]"#,
            good.id
        );
        std::fs::write(&path, text).expect("write");
        let loaded = load_configs(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "keep");
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod output_framing_tests {
    use super::*;
    use base64::Engine as _;

    /// A conservative floor for path MTU.
    ///
    /// IPv6 guarantees 1280, and DTLS, UDP, IP and SCTP headers all come out of
    /// that. Staying under this means SCTP never has to fragment an output
    /// frame, which is the thing that was failing on a path where MTU discovery
    /// does not work.
    const SAFE_DATAGRAM_BYTES: usize = 1100;

    fn frame_len(payload: &[u8]) -> usize {
        serde_json::json!({
            "version": 1,
            "frame_id": uuid::Uuid::now_v7().to_string(),
            "body": {
                "type": "shell_output",
                "config_id": uuid::Uuid::now_v7().to_string(),
                "bytes": base64::engine::general_purpose::STANDARD.encode(payload),
            },
        })
        .to_string()
        .len()
    }

    /// The invariant that matters is the SERIALIZED frame, not the chunk size.
    ///
    /// Base64 inflates by a third and the envelope carries two UUIDs, so a
    /// chunk limit chosen without measuring the whole frame is a guess. This
    /// fails if anyone raises the chunk size or adds a field to the envelope
    /// without rechecking.
    #[test]
    fn a_full_output_frame_fits_in_one_datagram() {
        let payload = vec![0xffu8; MAX_OUTPUT_BYTES_PER_FRAME];
        let length = frame_len(&payload);
        assert!(
            length <= SAFE_DATAGRAM_BYTES,
            "a full output frame is {length} bytes, over the {SAFE_DATAGRAM_BYTES} \
             budget — SCTP will fragment it and it will vanish on a path with a \
             small MTU, which is the bug this bounds"
        );
    }

    /// Splitting must not lose or reorder anything: the channel is reliable and
    /// ordered, so reassembling the pieces has to give back the original.
    #[test]
    fn splitting_preserves_the_byte_stream() {
        let original: Vec<u8> = (0..5000u32).map(|byte| (byte % 256) as u8).collect();
        let rejoined: Vec<u8> = original
            .chunks(MAX_OUTPUT_BYTES_PER_FRAME)
            .flat_map(<[u8]>::to_vec)
            .collect();
        assert_eq!(rejoined, original);
    }

    /// Every piece is within budget, including the last short one.
    #[test]
    fn no_piece_exceeds_the_limit() {
        let original = vec![0u8; 5000];
        for piece in original.chunks(MAX_OUTPUT_BYTES_PER_FRAME) {
            assert!(piece.len() <= MAX_OUTPUT_BYTES_PER_FRAME);
            assert!(frame_len(piece) <= SAFE_DATAGRAM_BYTES);
        }
    }
}
