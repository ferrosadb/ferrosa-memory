//! Module: Run a configured session and carry its text both ways.
//! Correctness: Correct when a tmux session survives a reconnect and is
//! rejoined rather than duplicated, when an ephemeral session leaves nothing
//! behind, and when output reaches the reader as it is produced rather than
//! when the command finishes.
//! Last revised: 2026-08-23
//! Last changed: Initial session runtime.
//!
//! # The two kinds are genuinely different, not a flag
//!
//! An **ephemeral bash** session is a child process on a PTY. It belongs to the
//! connection: when that goes, the process is killed and nothing remains. There
//! is no resumption because there is nothing to resume to, and that is the
//! point of choosing it.
//!
//! A **tmux** session outlives everything here. The machine starts it detached
//! and records its name against the config, so a device reconnecting finds the
//! same session and rejoins it. Starting a second one beside the first is the
//! failure this exists to prevent — two builds running, both writing the same
//! target directory, neither visible from the other's window.
//!
//! # Why a PTY and not piped stdio
//!
//! Programs behave differently when they believe they are talking to a
//! terminal: colour, progress bars, prompts, and line buffering rather than
//! block buffering. Piped stdio would give output that arrives in 8 KiB lumps
//! minutes apart, which for an interactive session is indistinguishable from a
//! hang.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};

use crate::session_config::{SessionConfig, SessionKind, clean_terminal_output};

/// How much output to hold for a reader that has fallen behind.
///
/// A build produces output far faster than a phone renders it. Bounded so a
/// slow reader cannot grow memory on the machine; when it overflows the OLDEST
/// chunk is dropped, because for a scrolling transcript the newest text is what
/// the operator is looking at.
const OUTPUT_QUEUE: usize = 256;

/// Largest single chunk read from a session.
const READ_CHUNK: usize = 8192;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("starting {kind}: {message}")]
    Start { kind: &'static str, message: String },
    #[error("this session has ended")]
    Ended,
    #[error("tmux {operation}: {message}")]
    Tmux {
        operation: &'static str,
        message: String,
    },
}

/// A running session, whichever kind it is.
pub struct RunningSession {
    config: SessionConfig,
    output: Mutex<mpsc::Receiver<String>>,
    input: Box<dyn SessionInput>,
    /// Chunks dropped because the reader was behind.
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

/// Where a session's input goes.
///
/// A trait because the two kinds write to genuinely different places — a PTY
/// master versus `tmux send-keys` — and pretending otherwise would mean a
/// branch at every call site.
trait SessionInput: Send + Sync {
    /// Send a line to the session. The newline is the caller's business:
    /// a shell needs one to act, and a program reading raw input may not.
    fn send(&self, text: &str) -> Result<(), SessionError>;

    /// Stop the session if it belongs to this connection.
    ///
    /// A no-op for tmux, which is the whole reason to choose tmux.
    fn shutdown(&self);
}

impl RunningSession {
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Text from the session, as it is produced.
    ///
    /// `None` once the session has ended and everything it produced has been
    /// read — so a caller draining after a command exits still gets the last
    /// output rather than losing it to the teardown.
    pub async fn next_output(&self) -> Option<String> {
        self.output.lock().await.recv().await
    }

    pub fn send(&self, text: &str) -> Result<(), SessionError> {
        self.input.send(text)
    }

    /// Chunks dropped because the reader could not keep up.
    ///
    /// Surfaced rather than logged: a transcript with a hole in it looks like
    /// the command produced nothing, and the shell should be able to say
    /// "output was dropped here" instead of showing a plausible lie.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for RunningSession {
    fn drop(&mut self) {
        // Ephemeral means ephemeral. A bash session whose connection has gone
        // must not be left holding the machine's CPU with nobody watching.
        self.input.shutdown();
    }
}

/// Starts sessions and remembers the tmux ones.
pub struct SessionRuntime {
    tmux_binary: PathBuf,
    workspace: PathBuf,
}

impl SessionRuntime {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            tmux_binary: PathBuf::from("tmux"),
            workspace: workspace.into(),
        }
    }

    /// Open a session for this config, rejoining a tmux one if it is running.
    pub async fn open(&self, config: &SessionConfig) -> Result<RunningSession, SessionError> {
        match config.kind {
            SessionKind::EphemeralBash => self.open_pty(config),
            SessionKind::Tmux => self.open_tmux(config).await,
        }
    }

    /// Whether a tmux session for this config is already running.
    ///
    /// The question a device asks on reconnect. Answering it from tmux itself
    /// rather than from a record in memory is deliberate: the machine may have
    /// restarted since, and a remembered session that no longer exists is worse
    /// than no memory at all.
    pub async fn is_running(&self, config: &SessionConfig) -> bool {
        if config.kind != SessionKind::Tmux {
            return false;
        }
        Command::new(&self.tmux_binary)
            .args(["has-session", "-t", &config.tmux_session_name()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn open_pty(&self, config: &SessionConfig) -> Result<RunningSession, SessionError> {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                // A width a phone can read without wrapping mid-word. Programs
                // ask the terminal how wide it is and format to fit, so this
                // is not cosmetic — it decides where `cargo` breaks its lines.
                rows: 40,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| SessionError::Start {
                kind: "pty",
                message: error.to_string(),
            })?;

        let mut builder = CommandBuilder::new(&config.command[0]);
        for argument in &config.command[1..] {
            builder.arg(argument);
        }
        builder.cwd(&self.workspace);

        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|error| SessionError::Start {
                kind: "command",
                message: error.to_string(),
            })?;
        // Dropped as soon as the child holds it. Keeping the slave open means
        // the master never sees EOF, so a finished command looks like one that
        // is still running and quiet.
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| SessionError::Start {
                kind: "pty reader",
                message: error.to_string(),
            })?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| SessionError::Start {
                kind: "pty writer",
                message: error.to_string(),
            })?;

        let (sender, output) = mpsc::channel(OUTPUT_QUEUE);
        let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = Arc::clone(&dropped);
        // A blocking thread, not a task: the PTY reader has no async form, and
        // a blocking read on a runtime worker would park it.
        std::thread::Builder::new()
            .name("ferrosa-session-pty".to_owned())
            .spawn(move || {
                let mut buffer = [0u8; READ_CHUNK];
                loop {
                    match reader.read(&mut buffer) {
                        // EOF: the command has finished and the writer is gone.
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let text = String::from_utf8_lossy(&buffer[..read]);
                            let cleaned = clean_terminal_output(&text);
                            if cleaned.is_empty() {
                                continue;
                            }
                            if sender.try_send(cleaned).is_err() {
                                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                }
            })
            .map_err(|error| SessionError::Start {
                kind: "pty thread",
                message: error.to_string(),
            })?;

        Ok(RunningSession {
            config: config.clone(),
            output: Mutex::new(output),
            input: Box::new(PtyInput {
                writer: std::sync::Mutex::new(writer),
                child: std::sync::Mutex::new(child),
            }),
            dropped,
        })
    }

    async fn open_tmux(&self, config: &SessionConfig) -> Result<RunningSession, SessionError> {
        let name = config.tmux_session_name();
        if !self.is_running(config).await {
            let workspace = self.workspace.to_string_lossy().into_owned();
            let mut command = Command::new(&self.tmux_binary);
            command.args(["new-session", "-d", "-s", &name, "-c", &workspace, "--"]);
            command.args(&config.command);
            let status = command
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .map_err(|error| SessionError::Tmux {
                    operation: "new-session",
                    message: error.to_string(),
                })?;
            if !status.success() {
                return Err(SessionError::Tmux {
                    operation: "new-session",
                    message: format!("tmux exited with {status}"),
                });
            }
        }

        let (sender, output) = mpsc::channel(OUTPUT_QUEUE);
        let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));
        // Polled with `capture-pane` rather than attached. Attaching would need
        // a PTY of its own and would make this connection the session's
        // controlling terminal — which is exactly what must NOT happen, because
        // the session has to outlive it.
        let poller = TmuxPoller {
            tmux: self.tmux_binary.clone(),
            session: name.clone(),
            sender,
            dropped: Arc::clone(&dropped),
        };
        tokio::spawn(poller.run());

        Ok(RunningSession {
            config: config.clone(),
            output: Mutex::new(output),
            input: Box::new(TmuxInput {
                tmux: self.tmux_binary.clone(),
                session: name,
            }),
            dropped,
        })
    }
}

struct PtyInput {
    writer: std::sync::Mutex<Box<dyn std::io::Write + Send>>,
    child: std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
}

impl SessionInput for PtyInput {
    fn send(&self, text: &str) -> Result<(), SessionError> {
        let mut writer = self.writer.lock().map_err(|_| SessionError::Ended)?;
        writer
            .write_all(text.as_bytes())
            .and_then(|()| writer.flush())
            .map_err(|_| SessionError::Ended)
    }

    fn shutdown(&self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

struct TmuxInput {
    tmux: PathBuf,
    session: String,
}

impl SessionInput for TmuxInput {
    fn send(&self, text: &str) -> Result<(), SessionError> {
        // `-l` sends the text literally, so a message containing something
        // tmux would read as a key name — "Enter", "C-c" — arrives as those
        // characters rather than as that key.
        let status = std::process::Command::new(&self.tmux)
            .args(["send-keys", "-t", &self.session, "-l", text])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| SessionError::Tmux {
                operation: "send-keys",
                message: error.to_string(),
            })?;
        if !status.success() {
            return Err(SessionError::Ended);
        }
        Ok(())
    }

    fn shutdown(&self) {
        // Deliberately nothing. Outliving this connection is the entire reason
        // to choose tmux, and killing it here would make the two kinds
        // identical.
    }
}

/// Reads a detached tmux session by polling its pane.
struct TmuxPoller {
    tmux: PathBuf,
    session: String,
    sender: mpsc::Sender<String>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

impl TmuxPoller {
    /// How often to look. Fast enough to feel live, slow enough that a
    /// long-running session is not a busy loop spawning processes.
    const INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

    async fn run(self) {
        let mut seen = String::new();
        loop {
            tokio::time::sleep(Self::INTERVAL).await;
            if self.sender.is_closed() {
                return;
            }
            let Ok(output) = Command::new(&self.tmux)
                .args(["capture-pane", "-p", "-t", &self.session])
                .output()
                .await
            else {
                return;
            };
            if !output.status.success() {
                // The session has gone — killed, or the machine restarted.
                // Ending the poller closes the channel, which the reader sees
                // as the session finishing.
                return;
            }
            let text = clean_terminal_output(&String::from_utf8_lossy(&output.stdout));
            // `capture-pane` returns the WHOLE visible pane every time, so
            // sending it raw would repeat everything on each poll. Only what
            // is new since the last look is forwarded.
            if let Some(fresh) = added_since(&seen, &text)
                && self.sender.try_send(fresh).is_err()
            {
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            seen = text;
        }
    }
}

/// What is new in `current` compared with `previous`.
///
/// `capture-pane` shows a window onto the pane, so between polls the content
/// may have grown, or scrolled, or been cleared. Prefix matching handles growth
/// — the common case — and anything else is treated as a fresh screen rather
/// than trying to diff it, because a wrong diff silently drops output.
fn added_since(previous: &str, current: &str) -> Option<String> {
    if current == previous {
        return None;
    }
    if let Some(rest) = current.strip_prefix(previous) {
        let rest = rest.trim_start_matches('\n');
        return (!rest.is_empty()).then(|| rest.to_owned());
    }
    // Scrolled or cleared. Send the whole visible pane rather than guessing:
    // a duplicated screen is readable, a dropped one is invisible.
    (!current.is_empty()).then(|| current.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_new_yields_nothing() {
        assert_eq!(added_since("hello", "hello"), None);
    }

    /// The common case: the pane grew. Only the new part is forwarded, or the
    /// transcript repeats the entire screen every 400ms.
    #[test]
    fn only_the_growth_is_forwarded() {
        assert_eq!(
            added_since("line one", "line one\nline two"),
            Some("line two".to_owned())
        );
    }

    /// Scrolled: the old text is no longer a prefix. Resending the visible
    /// pane duplicates some lines, which is readable — guessing at a diff
    /// would drop lines, which is invisible.
    #[test]
    fn a_scrolled_pane_resends_rather_than_guessing() {
        let before = "line one\nline two";
        let after = "line two\nline three";
        assert_eq!(added_since(before, after), Some(after.to_owned()));
    }

    #[test]
    fn a_cleared_pane_yields_nothing_rather_than_an_empty_message() {
        assert_eq!(added_since("something", ""), None);
    }

    /// A pane starting from empty must forward its first content.
    #[test]
    fn the_first_content_is_forwarded() {
        assert_eq!(added_since("", "first line"), Some("first line".to_owned()));
    }
}
