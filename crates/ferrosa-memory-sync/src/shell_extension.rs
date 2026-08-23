//! Module: The wire surface for configured sessions.
//! Correctness: Correct when a device cannot run anything without a grant,
//! when a config survives a restart of the machine, and when output reaches
//! the device as it is produced rather than when the command finishes.
//! Last revised: 2026-08-23
//! Last changed: Initial shell extension.
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
    configs: Mutex<Vec<SessionConfig>>,
    /// Where configs are written, so they survive a restart.
    store_path: PathBuf,
    runtime: SessionRuntime,
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
}

impl ShellExtension {
    pub fn new(workspace: impl Into<PathBuf>, store_path: impl Into<PathBuf>) -> Self {
        let store_path = store_path.into();
        let workspace = workspace.into();
        Self {
            configs: Mutex::new(load_configs(&store_path)),
            store_path,
            runtime: SessionRuntime::new(workspace),
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
                // Whether a tmux session is already up, so the shell can say
                // "resume" rather than "run" and the operator knows a second
                // one is not about to start.
                "running": self.runtime.is_running(config).await,
            }));
        }
        serde_json::json!({ "type": "shell_configs", "configs": listed })
    }

    async fn add_config(&self, body: &serde_json::Value) -> Result<(), String> {
        let name = body
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let kind = body
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let command: Vec<String> = body
            .get("command")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let config =
            SessionConfig::create(name, kind, command).map_err(|error| error.to_string())?;
        let mut configs = self.configs.lock().await;
        configs.push(config);
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
async fn pump_output(running: Arc<RunningSession>, session: SessionHandle, config_id: uuid::Uuid) {
    while let Some(text) = running.next_output().await {
        let frame = serde_json::json!({
            "version": 1,
            "frame_id": uuid::Uuid::now_v7().to_string(),
            "body": {
                "type": "shell_output",
                "config_id": config_id.to_string(),
                "text": text,
            },
        });
        if session.send(&frame.to_string()).await.is_err() {
            // The channel has gone; so has the reason to keep reading.
            return;
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
            "shell_open",
            "shell_close",
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
            sessions.entry(session_id).or_default().granted = true;
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
            "shell_input" => {
                let text = body
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "input needs text".to_owned())?;
                let open = self
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .and_then(|state| state.open.clone())
                    .ok_or_else(|| "no session is open".to_owned())?;
                open.send(text).map_err(|error| error.to_string())
            }
            other => Err(format!("claimed {other} and cannot serve it")),
        }
    }

    async fn on_closed(&self, session_id: uuid::Uuid) {
        self.close(session_id).await;
        // The grant goes with the session. An ephemeral session is killed by
        // dropping it; a tmux one keeps running, which is why it was chosen.
        self.sessions.lock().await.remove(&session_id);
    }
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
            SessionConfig::create("build", "tmux", vec!["cargo".into(), "test".into()])
                .expect("valid"),
            SessionConfig::create("shell", "bash", vec!["bash".into(), "-l".into()])
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
        let good = SessionConfig::create("keep", "bash", vec!["ls".into()]).expect("valid");
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
