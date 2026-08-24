//! Module: Run a configured session and carry its text both ways.
//! Correctness: Correct when a tmux session survives a reconnect and is
//! rejoined rather than duplicated, when an ephemeral session leaves nothing
//! behind, and when output reaches the reader as it is produced rather than
//! when the command finishes.
//! Last revised: 2026-08-23
//! Last changed: Sessions start in their config's working directory, falling back to the machine's workspace when none is set.
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

use crate::session_config::{SessionConfig, SessionKind};

/// How much output to hold for a reader that has fallen behind.
///
/// A build produces output far faster than a phone renders it. Bounded so a
/// slow reader cannot grow memory on the machine; when it overflows the OLDEST
/// chunk is dropped, because for a scrolling transcript the newest text is what
/// the operator is looking at.
const OUTPUT_QUEUE: usize = 256;

/// Largest single chunk read from a session.
const READ_CHUNK: usize = 8192;

/// How much of a status line the roster shows. A glance, not a transcript.
const STATUS_LINE_CHARS: usize = 120;

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
    /// An ephemeral session has no scrollback on the machine to move through.
    #[error("this session has no scrollback on the machine — scroll it in the terminal instead")]
    NotScrollable,
    #[error("scrolling: {0}")]
    ScrollRefused(String),
}

/// A running session, whichever kind it is.
pub struct RunningSession {
    config: SessionConfig,
    output: Mutex<mpsc::Receiver<Vec<u8>>>,
    input: Box<dyn SessionInput>,
    /// Chunks dropped because the reader was behind.
    dropped: Arc<std::sync::atomic::AtomicU64>,
    /// Whether a scroll from here left the pane in tmux's copy mode.
    ///
    /// It matters because copy mode swallows typing: keys go to copy-mode
    /// bindings rather than to the program, so after scrolling the session
    /// appears to stop accepting input. Tracked rather than asked, because
    /// asking tmux costs a process spawn and this would be on the path of
    /// every keystroke.
    in_copy_mode: std::sync::atomic::AtomicBool,
}

/// What pressing Return actually puts on the wire.
///
/// A terminal sends carriage return (0x0d) for the Return key, not line feed
/// (0x0a). A cooked shell accepts either, because the line discipline turns CR
/// into NL — which is why sending "\n" LOOKED correct against bash. A program
/// in raw mode does not get that translation: crossterm and friends read 0x0d
/// as Return and 0x0a as Ctrl-J, so a TUI sent "\n" shows the typed text and
/// then does nothing — indistinguishable from input not being wired at all.
const RETURN: &str = "\r";

/// Runs the operator's command, reports how it ended, and keeps the pane.
///
/// `"$@"` is the operator's argv, passed to `sh` as arguments rather than
/// pasted into this string — so nothing they type is ever parsed as shell
/// syntax. The trailing wait is what makes a session outlive the thing it ran:
/// the pane, and therefore its scrollback, stays readable until the session is
/// deleted.
///
/// The exit line is deliberately printed rather than only recorded. It is the
/// first thing an operator debugging "why did this not start" needs, and it
/// belongs in the transcript they are already reading.
const KEEPER_SCRIPT: &str = r#"
"$@"
status=$?
if [ "$status" -eq 0 ]; then
  printf '\n[%s exited normally]\n' "$1"
else
  printf '\n[%s exited with status %s]\n' "$1" "$status"
fi
printf '[the session is still here — delete it when you are done]\n'
while :; do sleep 3600; done
"#;

/// A key that is not a character.
///
/// The set is deliberately small and explicit rather than "any string the
/// device sends". These names cross the wire from a phone, and `send-keys`
/// without `-l` interprets its argument as a tmux command language — an
/// unvalidated name there is arbitrary tmux control, not a keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Escape,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    /// Interrupt. The single most important one for a harness that is stuck.
    CtrlC,
    /// End of input.
    CtrlD,
    /// Suspend.
    CtrlZ,
}

impl NamedKey {
    /// Parse a name the device sent. Unknown names are refused, not guessed.
    pub fn from_wire(name: &str) -> Option<Self> {
        Some(match name {
            "enter" | "return" => Self::Enter,
            "escape" | "esc" => Self::Escape,
            "tab" => Self::Tab,
            "backspace" => Self::Backspace,
            "up" => Self::Up,
            "down" => Self::Down,
            "left" => Self::Left,
            "right" => Self::Right,
            "ctrl-c" => Self::CtrlC,
            "ctrl-d" => Self::CtrlD,
            "ctrl-z" => Self::CtrlZ,
            _ => return None,
        })
    }

    /// The bytes a terminal actually sends for it.
    ///
    /// Used for the PTY path, which has no key names — only bytes. The arrows
    /// are the standard CSI sequences; a program in raw mode recognises
    /// nothing else as an arrow.
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Enter => b"\r",
            Self::Escape => b"\x1b",
            Self::Tab => b"\t",
            Self::Backspace => b"\x7f",
            Self::Up => b"\x1b[A",
            Self::Down => b"\x1b[B",
            Self::Right => b"\x1b[C",
            Self::Left => b"\x1b[D",
            Self::CtrlC => b"\x03",
            Self::CtrlD => b"\x04",
            Self::CtrlZ => b"\x1a",
        }
    }
}

/// Where a session's input goes.
///
/// A trait because the two kinds write to genuinely different places — a PTY
/// master versus `tmux send-keys` — and pretending otherwise would mean a
/// branch at every call site.
trait SessionInput: Send + Sync {
    /// Send text to the session.
    ///
    /// A `\n` in `text` means "the operator pressed Return here", and each
    /// implementation is responsible for delivering that as a real Return —
    /// see [`RETURN`] for why a literal line feed is not one.
    fn send(&self, text: &str) -> Result<(), SessionError>;

    /// Write raw bytes straight to the terminal.
    ///
    /// What a client-side emulator produces: already exactly what a terminal
    /// would put on the wire, including the escape sequences for arrows and
    /// the control bytes for Ctrl-C. Passing these through `send` would
    /// newline-translate them and corrupt any sequence containing 0x0a.
    fn send_bytes(&self, bytes: &[u8]) -> Result<(), SessionError>;

    /// Press a key that is not a character.
    ///
    /// Separate from [`Self::send`] because there is no text encoding that
    /// means "Ctrl-C" — a byte that happens to be 0x03 inside a string would
    /// arrive as data on one transport and as an interrupt on the other.
    fn send_key(&self, key: NamedKey) -> Result<(), SessionError>;

    /// Tell the program how big the operator's terminal is.
    ///
    /// Not cosmetic: a TUI lays out to the size it is told, so a wrong one
    /// wraps or truncates every frame it draws. Resizing the master is also
    /// what sends SIGWINCH, which is the only way a running program learns the
    /// size changed.
    fn resize(&self, rows: u16, cols: u16) -> Result<(), SessionError>;

    /// Stop the session if it belongs to this connection.
    ///
    /// A no-op for tmux, which is the whole reason to choose tmux.
    fn shutdown(&self);

    /// End the session permanently, whatever kind it is.
    ///
    /// The counterpart to [`Self::shutdown`]: that one asks "does this session
    /// belong to the connection going away", this one is the operator saying
    /// they are done. For tmux the difference is everything — detaching leaves
    /// a build running, this does not.
    ///
    /// Synchronous, like the rest of this trait, because it is boxed: a trait
    /// with an `async fn` cannot be used behind `dyn`.
    fn destroy(&self) -> Result<(), SessionError>;
}

/// How far to move through the scrollback, in the operator's terms.
///
/// A closed set for the same reason `NamedKey` is one: the wire value ends up
/// in a tmux command, and an unvalidated string there is remote tmux control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollMotion {
    PageUp,
    PageDown,
    LineUp,
    LineDown,
    Top,
    /// Back to the live pane, leaving copy mode.
    Bottom,
}

impl ScrollMotion {
    pub fn from_wire(name: &str) -> Option<Self> {
        match name {
            "page-up" => Some(Self::PageUp),
            "page-down" => Some(Self::PageDown),
            "line-up" => Some(Self::LineUp),
            "line-down" => Some(Self::LineDown),
            "top" => Some(Self::Top),
            "bottom" => Some(Self::Bottom),
            _ => None,
        }
    }
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
    pub async fn next_output(&self) -> Option<Vec<u8>> {
        self.output.lock().await.recv().await
    }

    pub fn send(&self, text: &str) -> Result<(), SessionError> {
        if is_typing(text.as_bytes()) {
            self.leave_copy_mode();
        }
        self.input.send(text)
    }

    /// Press a named key — Return on its own, Ctrl-C, an arrow.
    pub fn send_key(&self, key: NamedKey) -> Result<(), SessionError> {
        if is_typing(key.bytes()) {
            self.leave_copy_mode();
        }
        self.input.send_key(key)
    }

    /// Write raw bytes from a client-side terminal emulator.
    pub fn send_bytes(&self, bytes: &[u8]) -> Result<(), SessionError> {
        if is_typing(bytes) {
            self.leave_copy_mode();
        }
        self.input.send_bytes(bytes)
    }

    /// Whether the pane is showing history rather than the live output.
    ///
    /// Read by the caller so the device's LIVE control can show which state it
    /// is in. A button that sets a mode but never reflects it leaves the
    /// operator guessing whether they are looking at now.
    pub fn is_scrolled(&self) -> bool {
        self.in_copy_mode.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Come back to the live pane before delivering input.
    ///
    /// Scrolling puts tmux into copy mode, and copy mode routes keys to its own
    /// bindings instead of to the program — so after scrolling, a session looks
    /// like it has stopped accepting typing. It has not; the keys are going
    /// somewhere else.
    ///
    /// Typing means "I am done reading", which is how every other terminal
    /// behaves: type and the view snaps to the bottom. Doing it here rather
    /// than on the device means it holds for every client and for the key bar
    /// as well as the keyboard.
    ///
    /// Costs a tmux call only on the FIRST input after a scroll, because the
    /// flag is cleared by leaving.
    fn leave_copy_mode(&self) {
        use std::sync::atomic::Ordering;
        if !self.in_copy_mode.swap(false, Ordering::Relaxed) {
            return;
        }
        let _ = std::process::Command::new("tmux")
            .args([
                "send-keys",
                "-X",
                "-t",
                &self.config.tmux_session_name(),
                "cancel",
            ])
            .status();
    }

    /// Tell the program the size of the terminal showing it.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), SessionError> {
        self.input.resize(rows, cols)
    }

    /// Move through the pane's scrollback.
    ///
    /// NOT key bytes. Scrollback belongs to tmux, not to the program in the
    /// pane, and keys sent into the pane go to that program — Page Up in a TUI
    /// scrolls the TUI, or does nothing, and never reaches tmux's history. So
    /// this drives tmux directly, which also means it works while a full-screen
    /// program is running and grabbing every key.
    ///
    /// Refused for an ephemeral session: there is no tmux to ask, and its
    /// scrollback lives in the client's own emulator where the client can
    /// scroll it without involving the machine at all.
    pub fn scroll(&self, motion: ScrollMotion) -> Result<(), SessionError> {
        if self.config.kind != SessionKind::Tmux {
            return Err(SessionError::NotScrollable);
        }
        let target = self.config.tmux_session_name();
        let status = match motion {
            // `-u` both ENTERS copy mode and scrolls a page up, so the first
            // press and every press after it are the same command. Entering
            // first and then scrolling would swallow the first press.
            ScrollMotion::PageUp => std::process::Command::new("tmux")
                .args(["copy-mode", "-u", "-t", &target])
                .status(),
            ScrollMotion::PageDown => std::process::Command::new("tmux")
                .args(["send-keys", "-X", "-t", &target, "page-down"])
                .status(),
            ScrollMotion::LineUp => std::process::Command::new("tmux")
                .args([
                    "copy-mode",
                    "-t",
                    &target,
                    ";",
                    "send-keys",
                    "-X",
                    "-t",
                    &target,
                    "scroll-up",
                ])
                .status(),
            ScrollMotion::LineDown => std::process::Command::new("tmux")
                .args(["send-keys", "-X", "-t", &target, "scroll-down"])
                .status(),
            ScrollMotion::Top => std::process::Command::new("tmux")
                .args([
                    "copy-mode",
                    "-t",
                    &target,
                    ";",
                    "send-keys",
                    "-X",
                    "-t",
                    &target,
                    "history-top",
                ])
                .status(),
            // Leaving copy mode IS returning to the bottom: tmux jumps back to
            // the live pane. Two words for one action would let the operator
            // sit in copy mode believing they were live, watching output that
            // had stopped updating.
            ScrollMotion::Bottom => std::process::Command::new("tmux")
                .args(["send-keys", "-X", "-t", &target, "cancel"])
                .status(),
        };
        match status {
            Ok(status) if status.success() => {
                use std::sync::atomic::Ordering;
                // `Bottom` IS leaving copy mode; everything else enters or
                // stays in it.
                self.in_copy_mode
                    .store(motion != ScrollMotion::Bottom, Ordering::Relaxed);
                Ok(())
            }
            // A non-zero exit is usually "not in copy mode" for a motion that
            // needs it. Reported rather than swallowed: a scroll that silently
            // does nothing reads as a dead button.
            Ok(status) => Err(SessionError::ScrollRefused(format!(
                "tmux exited with {status}"
            ))),
            Err(error) => Err(SessionError::ScrollRefused(error.to_string())),
        }
    }

    /// The whole scrollback, as bytes, with its colour.
    ///
    /// `-e` keeps the escape sequences, so a reader can feed this to a terminal
    /// emulator and see what the operator would have seen. Plain text renders
    /// more cheaply and is not what was on screen. `-S -` takes the history
    /// from its beginning; without it capture-pane returns the visible pane
    /// only, which for an archive would be the last screenful of a day's work.
    ///
    /// `-J` is deliberately NOT used: it rejoins wrapped lines, which changes
    /// the bytes from what the terminal actually received.
    ///
    /// Returns the bytes and whether tmux hit its history limit — a capture cut
    /// short must be able to say so rather than pass as the whole thing.
    pub fn capture_scrollback(&self) -> Result<(Vec<u8>, bool), SessionError> {
        if self.config.kind != SessionKind::Tmux {
            return Err(SessionError::NotScrollable);
        }
        let target = self.config.tmux_session_name();
        let output = std::process::Command::new("tmux")
            .args(["capture-pane", "-p", "-e", "-S", "-", "-t", &target])
            .output()
            .map_err(|error| SessionError::Tmux {
                operation: "capture-pane",
                message: error.to_string(),
            })?;
        if !output.status.success() {
            return Err(SessionError::Tmux {
                operation: "capture-pane",
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        // At the limit means older output has already been discarded by tmux,
        // so this capture cannot be the whole history however it looks.
        let truncated = tmux_number(&target, "#{history_size}")
            .zip(tmux_number(&target, "#{history_limit}"))
            .is_some_and(|(size, limit)| size >= limit);
        Ok((output.stdout, truncated))
    }

    /// End this session for good.
    ///
    /// Distinct from dropping the handle, which for a persistent session only
    /// detaches. This is the operator saying they are finished with it: the
    /// tmux session and its scrollback go away, and nothing is left to resume.
    pub fn destroy(&self) -> Result<(), SessionError> {
        self.input.destroy()
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

    /// Which tmux this runtime drives, for callers that need to ask it
    /// something this type does not wrap.
    pub fn tmux_binary(&self) -> &std::path::Path {
        &self.tmux_binary
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
    /// Whether a session is running ON THE MACHINE, independently of anyone
    /// being connected.
    ///
    /// Only ever true for tmux, and that is the honest answer: an ephemeral
    /// session exists only as a process this listener holds, so nothing on the
    /// machine outlives the connection to be asked about.
    ///
    /// Which means this is NOT the same question as "is this job running", and
    /// callers that want that must also ask whether they are holding one. The
    /// status pump did not, so an ephemeral session was reported off-shift for
    /// its whole life — invisible on a home screen that hides off-shift jobs.
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

    /// The last thing this session printed, for the roster.
    ///
    /// Read with `capture-pane` rather than from the attached stream, because
    /// the roster needs a line for EVERY job and only one is attached at a
    /// time. Scraping for a one-line status is cheap; scraping to reconstruct
    /// output is what the PTY attach replaced, and this does not reintroduce
    /// it — nothing here is fed to a terminal.
    ///
    /// Returns `None` when the session is gone or has printed nothing, which
    /// the caller must render as "no output yet" rather than as silence.
    /// The pane as it currently reads, whole.
    ///
    /// The caller diffs it. "The last non-empty line" was the previous answer
    /// and it is wrong for exactly the programs this is for: a full-screen TUI
    /// redraws in place and ends with a STATIC footer — codex sits on
    /// `gpt-5.6-sol high · /` forever — so the last line never changed, no
    /// delta was ever sent, and a session doing real work looked asleep.
    pub async fn pane_text(&self, config: &SessionConfig) -> Option<String> {
        if config.kind != SessionKind::Tmux {
            return None;
        }
        let output = Command::new(&self.tmux_binary)
            .args(["capture-pane", "-p", "-t", &config.tmux_session_name()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn open_pty(&self, config: &SessionConfig) -> Result<RunningSession, SessionError> {
        let mut command = std::process::Command::new(&config.command[0]);
        command.args(&config.command[1..]);
        self.spawn_pty(command, config, PtyKind::Ephemeral)
    }

    /// Run something in a PTY and stream its raw bytes.
    ///
    /// Shared by both kinds, because after attaching to tmux they ARE the same
    /// thing: a pseudo-terminal with a program on the far end. The only
    /// difference left is what closing it means, which is what [`PtyKind`]
    /// carries.
    fn spawn_pty(
        &self,
        command: std::process::Command,
        config: &SessionConfig,
        kind: PtyKind,
    ) -> Result<RunningSession, SessionError> {
        use portable_pty::{CommandBuilder, PtySize, native_pty_system};

        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                // The device resizes this as soon as it knows its own terminal
                // size. These are only what the program sees for the first few
                // milliseconds, and a program that measures once at startup —
                // which most TUIs do — would otherwise be stuck with them.
                rows: 40,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| SessionError::Start {
                kind: "pty",
                message: error.to_string(),
            })?;

        let mut builder = CommandBuilder::new(command.get_program());
        for argument in command.get_args() {
            builder.arg(argument);
        }
        // The config's directory if it named one, else the machine's own
        // workspace — which is what every config did before the field existed.
        builder.cwd(config.working_dir.as_deref().map_or_else(
            || self.workspace.to_string_lossy().into_owned(),
            str::to_owned,
        ));
        // Declared, because a program asks $TERM what it may draw with. Without
        // it a TUI assumes a dumb terminal and either refuses to run or falls
        // back to output no emulator can lay out. This is the value the client
        // emulator implements.
        builder.env("TERM", "xterm-256color");

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
                        // EOF: the program has finished and the writer is gone.
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            // RAW. Nothing is stripped or interpreted here —
                            // the device runs a terminal emulator, and an
                            // escape sequence removed on the way past is one it
                            // can never act on. Cleaning these bytes is what
                            // made a redrawing CLI "jump around".
                            let chunk = buffer[..read].to_vec();
                            if sender.try_send(chunk).is_err() {
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
                master: std::sync::Mutex::new(pair.master),
                kind,
                tmux: self.tmux_binary.clone(),
            }),
            dropped,
            // A fresh session is live, not scrolled.
            in_copy_mode: std::sync::atomic::AtomicBool::new(false),
        })
    }

    async fn open_tmux(&self, config: &SessionConfig) -> Result<RunningSession, SessionError> {
        let name = config.tmux_session_name();
        if !self.is_running(config).await {
            let workspace = config
                .working_dir
                .clone()
                .unwrap_or_else(|| self.workspace.to_string_lossy().into_owned());
            let mut command = Command::new(&self.tmux_binary);
            command.args(["new-session", "-d", "-s", &name, "-c", &workspace, "--"]);
            // Run under a keeper, not directly.
            //
            // A command that exits takes its pane and its session with it, so a
            // harness that fails on startup is GONE before anything can read
            // what it printed — which looks exactly like a session that never
            // started. The keeper runs the command, prints its exit status into
            // the pane, and then holds the pane open so the output stays
            // readable until the operator deletes the session.
            //
            // The user's argv is passed as ARGUMENTS to `sh`, never
            // interpolated into the script. `"$@"` expands to exactly the words
            // the operator entered, so a command containing quotes, semicolons
            // or backticks runs as one command with those characters in it
            // rather than becoming several.
            command.args(["/bin/sh", "-c", KEEPER_SCRIPT, "ferrosa-session"]);
            command.args(&config.command);
            let output = command
                .stdout(Stdio::piped())
                // KEPT, not discarded. This was `Stdio::null()`, so a failure
                // reported "tmux exited with exit status: 1" and nothing else —
                // the one line that says whether the command does not exist,
                // the directory is missing, or tmux itself is unhappy.
                .stderr(Stdio::piped())
                .output()
                .await
                .map_err(|error| SessionError::Tmux {
                    operation: "new-session",
                    message: error.to_string(),
                })?;
            if !output.status.success() {
                let complaint = String::from_utf8_lossy(&output.stderr);
                let complaint = complaint.trim();
                return Err(SessionError::Tmux {
                    operation: "new-session",
                    message: if complaint.is_empty() {
                        format!("tmux exited with {} and said nothing", output.status)
                    } else {
                        complaint.to_owned()
                    },
                });
            }
        }

        // Applied on EVERY open, not only on creation.
        //
        // A session created before these settings changed keeps whatever it was
        // made with, for as long as it runs — and these sessions run for days.
        // Setting them on rejoin is what repairs one, and setting an option to
        // the value it already has costs nothing.
        //
        // Per session, never globally: this process does not get to
        // reconfigure the operator's own tmux.
        for option in [
            // No status bar. It costs a row on a phone and describes tmux
            // rather than the work.
            ["status", "off"],
            // Mouse reporting OFF, and this matters more than it sounds.
            //
            // It was ON, to make a drag scroll tmux's history. That is not how
            // scrolling works here any more — `shell_scroll` drives copy mode
            // directly — and leaving it on did real damage. With mouse
            // reporting active, SwiftTerm turns every TAP into a mouse escape
            // sequence and sends it as INPUT: the "weird ascii characters that
            // had to be deleted before I could type a command". Worse, a tap
            // that becomes a mouse report is a tap that no longer focuses the
            // terminal, so typing stopped working altogether.
            //
            // A program that genuinely wants mouse events can still request
            // them itself; this only stops tmux asking on its behalf.
            ["mouse", "off"],
            // A history worth scrolling. The default is 2000 lines, which a
            // build log passes in seconds.
            ["history-limit", "50000"],
        ] {
            let _ = Command::new(&self.tmux_binary)
                .args(["set-option", "-t", &name, option[0], option[1]])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }

        // ATTACHED through a PTY, not scraped with `capture-pane`.
        //
        // Scraping produced cleaned text with the escape sequences stripped,
        // which is fine for a command that emits lines and useless for anything
        // that redraws — a TUI arrived as its repainted screens concatenated in
        // emission order. Attaching gives the real byte stream in both
        // directions, so the device can run an actual terminal emulator.
        //
        // Attaching does NOT endanger the session. tmux is a server; this
        // creates a client, and when the PTY goes away the client detaches and
        // the session keeps running. That is exactly what detaching means.
        let mut attach = std::process::Command::new(&self.tmux_binary);
        attach.args(["attach-session", "-t", &name]);
        self.spawn_pty(attach, config, PtyKind::Tmux { session: name })
    }
}
/// What closing a PTY means for the thing on the far end.
enum PtyKind {
    /// Killing the child ends the work. There is nothing to outlive us.
    Ephemeral,
    /// The PTY holds a tmux CLIENT. Dropping it detaches; the session and its
    /// scrollback keep going until someone deletes them.
    Tmux { session: String },
}

struct PtyInput {
    writer: std::sync::Mutex<Box<dyn std::io::Write + Send>>,
    child: std::sync::Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Kept so the terminal can be resized after it is running. A program that
    /// asked the terminal its size at startup only learns about a change from
    /// SIGWINCH, which is what resizing the master sends.
    master: std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    kind: PtyKind,
    tmux: PathBuf,
}

impl SessionInput for PtyInput {
    fn send(&self, text: &str) -> Result<(), SessionError> {
        // Translated on the way in, because a terminal sends CR when Return is
        // pressed. A cooked shell gets NL anyway (the line discipline's ICRNL
        // does it), and a raw-mode program gets the Return it is waiting for
        // instead of a Ctrl-J it will ignore.
        let keyed = text.replace('\n', RETURN);
        self.write(keyed.as_bytes())
    }

    fn send_key(&self, key: NamedKey) -> Result<(), SessionError> {
        self.write(key.bytes())
    }

    fn send_bytes(&self, bytes: &[u8]) -> Result<(), SessionError> {
        // Untouched. These already ARE terminal bytes; translating anything
        // here would corrupt a multi-byte escape sequence that happens to
        // contain the byte being translated.
        self.write(bytes)
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<(), SessionError> {
        let master = self.master.lock().map_err(|_| SessionError::Ended)?;
        master
            .resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| SessionError::Start {
                kind: "pty resize",
                message: error.to_string(),
            })
    }

    fn destroy(&self) -> Result<(), SessionError> {
        match &self.kind {
            // These are the same act for an ephemeral session. It dies with the
            // connection anyway; asking for it early changes only the timing.
            PtyKind::Ephemeral => {
                self.shutdown();
                Ok(())
            }
            // The one place a persistent session is actually killed. Everything
            // else — closing the transcript, losing the connection, quitting
            // the app — only detaches, which is the point of choosing tmux.
            PtyKind::Tmux { session } => {
                let result = std::process::Command::new(&self.tmux)
                    .args(["kill-session", "-t", session])
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .output()
                    .map_err(|error| SessionError::Tmux {
                        operation: "kill-session",
                        message: error.to_string(),
                    })?;
                if !result.status.success() {
                    let complaint = String::from_utf8_lossy(&result.stderr);
                    let complaint = complaint.trim();
                    // A session already gone is the outcome the operator asked
                    // for, so it is not an error to report at them.
                    if complaint.contains("can't find session") {
                        return Ok(());
                    }
                    return Err(SessionError::Tmux {
                        operation: "kill-session",
                        message: complaint.to_owned(),
                    });
                }
                // The attached client goes too, so the reader sees EOF.
                self.shutdown();
                Ok(())
            }
        }
    }

    fn shutdown(&self) {
        // Kills whatever is on the far end of THIS pty. For tmux that is the
        // attached client, not the session — detaching, which is exactly what
        // losing a connection should do.
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

impl PtyInput {
    fn write(&self, bytes: &[u8]) -> Result<(), SessionError> {
        let mut writer = self.writer.lock().map_err(|_| SessionError::Ended)?;
        writer
            .write_all(bytes)
            .and_then(|()| writer.flush())
            .map_err(|_| SessionError::Ended)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The discovery that cost a debugging round, kept as a test even though
    /// the code that first encoded it is gone.
    ///
    /// A terminal sends carriage return (0x0d) for Return, not line feed
    /// (0x0a). bash accepts either because the line discipline translates CR
    /// to NL for it — which is why sending "\n" LOOKED correct. A program in
    /// raw mode gets no translation: crossterm reads 0x0d as Return and 0x0a
    /// as Ctrl-J, so a TUI sent "\n" shows the typed text and then does
    /// nothing.
    #[test]
    fn a_newline_is_written_as_a_carriage_return() {
        assert_eq!("echo hi\n".replace('\n', RETURN), "echo hi\r");
    }

    /// A paste keeps one Return per line, not one at the end.
    #[test]
    fn every_line_of_a_paste_keeps_its_return() {
        assert_eq!("one\ntwo\n".replace('\n', RETURN), "one\rtwo\r");
    }

    /// Return on its own is a real thing to send: it is how a CLI waiting for
    /// confirmation is answered.
    #[test]
    fn return_is_a_single_carriage_return() {
        assert_eq!(NamedKey::Enter.bytes(), b"\r");
    }

    /// The interrupt byte, which is the only way out of a stuck harness from a
    /// phone.
    #[test]
    fn ctrl_c_is_the_interrupt_byte() {
        assert_eq!(NamedKey::CtrlC.bytes(), b"\x03");
    }

    /// Arrows are CSI sequences. A program in raw mode recognises nothing else
    /// as an arrow, so the exact bytes matter.
    #[test]
    fn the_arrows_are_csi_sequences() {
        assert_eq!(NamedKey::Up.bytes(), b"\x1b[A");
        assert_eq!(NamedKey::Down.bytes(), b"\x1b[B");
        assert_eq!(NamedKey::Right.bytes(), b"\x1b[C");
        assert_eq!(NamedKey::Left.bytes(), b"\x1b[D");
    }

    /// Names come off the wire from a phone, so anything unrecognised is
    /// refused rather than passed along.
    #[test]
    fn an_unknown_key_name_is_refused() {
        assert!(NamedKey::from_wire("enter").is_some());
        assert!(NamedKey::from_wire("ctrl-c").is_some());
        assert!(NamedKey::from_wire("kill-server").is_none());
        assert!(NamedKey::from_wire("").is_none());
    }

    /// Both spellings of the same key, because the two shells disagree about
    /// what to call it and neither should have to know about the other.
    #[test]
    fn return_and_enter_are_the_same_key() {
        assert_eq!(NamedKey::from_wire("return"), NamedKey::from_wire("enter"));
        assert_eq!(NamedKey::from_wire("esc"), NamedKey::from_wire("escape"));
    }
}

/// One numeric tmux format value for a session, or `None` if it cannot be read.
fn tmux_number(target: &str, format: &str) -> Option<u64> {
    let output = std::process::Command::new("tmux")
        .args(["display-message", "-p", "-t", target, format])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Whether these bytes are a person typing, as opposed to the terminal talking.
///
/// # Why this has to be told apart
///
/// Typing leaves tmux's copy mode, so that scrolling and then typing does what
/// it does in every other terminal. But a terminal emulator sends input on its
/// own account: it answers Device Attributes and cursor-position queries, and
/// it reports focus changes. Entering copy mode makes tmux REDRAW, the redraw
/// contains queries, and the emulator's replies arrived here as input — so copy
/// mode was cancelled a few milliseconds after it started and scrolling
/// appeared to jump straight back to live.
///
/// The distinction is not "did bytes arrive" but "did a person cause them".
/// Escape sequences are skipped whole; what is left is what was typed. An
/// unterminated sequence is treated as a sequence rather than as its characters
/// — a truncated reply is still not typing.
///
/// Cursor keys are deliberately NOT typing. In copy mode they scroll, which is
/// what an operator pressing them while reading history means.
pub fn is_typing(bytes: &[u8]) -> bool {
    const ESC: u8 = 0x1b;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != ESC {
            let byte = bytes[index];
            // Anything a key produces: printable ASCII, UTF-8 text, Return,
            // Tab, Backspace, and control bytes like Ctrl-C — all of which are
            // deliberate presses. Only NUL is ignored, as padding.
            if byte != 0 {
                return true;
            }
            index += 1;
            continue;
        }
        index += 1;
        let Some(&next) = bytes.get(index) else {
            // A lone ESC at the end. That IS a keypress — Escape is how a TUI
            // is dismissed — and it is not a query reply, which always carries
            // a body.
            return true;
        };
        index += 1;
        match next {
            // CSI: parameters, then a final byte in @..~.
            b'[' => {
                while index < bytes.len() && !(0x40..=0x7e).contains(&bytes[index]) {
                    index += 1;
                }
                index += 1;
            }
            // OSC and friends: run to BEL or ST.
            b']' | b'P' | b'X' | b'^' | b'_' => {
                while index < bytes.len() && bytes[index] != 0x07 {
                    if bytes[index] == ESC && bytes.get(index + 1) == Some(&b'\\') {
                        index += 1;
                        break;
                    }
                    index += 1;
                }
                index += 1;
            }
            // Two-byte sequences: ESC O A for a cursor key in application
            // mode, ESC = , ESC > and the rest.
            b'O' => index += 1,
            _ => {}
        }
    }
    false
}

/// Watches one pane and reports what it SAID, not what it redrew.
///
/// # Three rules, and the first two were wrong against real panes
///
/// "The last non-empty line": a full-screen TUI ends every frame with a static
/// footer — codex sits on `gpt-5.6-sol high · /` forever — so the answer never
/// changed, no delta was sent, and a session doing real work looked asleep.
///
/// "The last line that CHANGED": better for codex, wrong for Claude, whose
/// status line carries a spinner, a token count and an elapsed timer. It
/// changes every tick and sits at the bottom, so it won every time.
///
/// "The last CHANGED row that does not change too often": measured churn per
/// row index. Watching the live panes killed it — a transcript that scrolls
/// moves the spinner between row indices, so no index accumulates the churn
/// that would mark it as chrome, and the reported line was
/// `✽ Canoodling… (4m 34s · ↓ 19.3k tokens)`.
///
/// # What actually separates speech from chrome
///
/// Speech STAYS. A model writes a line and it remains on screen; a spinner
/// writes a line that is gone next tick, because the glyph or the elapsed time
/// has moved on. So a line is reported when it is present now, was present on
/// the previous reading, and was absent on the one before that — new, and
/// still there.
///
/// Content-based rather than position-based, so scrolling does not confuse it.
/// It costs one tick of delay, which at a three-second pump nobody can see.
pub struct PaneWatcher {
    /// Non-empty lines from the previous reading, and the one before it.
    previous: Option<std::collections::HashSet<String>>,
    before: Option<std::collections::HashSet<String>>,
    /// The last thing reported, so the same line is not reported twice as it
    /// scrolls up the pane.
    said: Option<String>,
}

impl Default for PaneWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneWatcher {
    pub fn new() -> Self {
        Self {
            previous: None,
            before: None,
            said: None,
        }
    }

    /// Take a reading. `None` when nothing worth saying appeared.
    pub fn observe(&mut self, pane: &str) -> Option<String> {
        let lines: Vec<String> = pane
            .lines()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty())
            .collect();
        let now: std::collections::HashSet<String> = lines.iter().cloned().collect();

        let chosen = match (&self.previous, &self.before) {
            // The first reading only PRIMES. It has nothing to compare
            // against, so "the last non-empty line" is the only answer
            // available — and that is the footer, which is the very thing this
            // exists to stop reporting. One tick later there is a real answer.
            (None, _) => None,
            // The second reading primes too. "Anything that survived the
            // first reading" sounds like it excludes spinners, and it does —
            // but the footer and the input box survive as well, so it reports
            // exactly the chrome this exists to suppress. Two readings of
            // history are needed to say "new AND stayed", so two readings is
            // what it waits for.
            (Some(_), None) => None,
            (Some(previous), Some(before)) => lines
                .iter()
                .rev()
                // Present now AND on the last reading: it stayed, so it is not
                // a spinner frame.
                .filter(|line| previous.contains(*line))
                // Absent two readings ago: it is new, so it is not the static
                // footer or the input box.
                .find(|line| !before.contains(*line))
                .cloned(),
        };

        self.before = self.previous.take();
        self.previous = Some(now);

        // Not the same thing twice. A line scrolling up the pane is still the
        // line already reported.
        match chosen {
            Some(line) if self.said.as_ref() != Some(&line) => {
                let mut text = line;
                if text.chars().count() > STATUS_LINE_CHARS {
                    text = text.chars().take(STATUS_LINE_CHARS).collect::<String>() + "…";
                }
                self.said = Some(text.clone());
                Some(text)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod typing_tests {
    use super::is_typing;

    /// The bug: entering copy mode makes tmux redraw, the redraw asks the
    /// emulator questions, and the emulator's ANSWERS arrived as input. Treated
    /// as typing, they cancelled copy mode milliseconds after a scroll — so
    /// scrolling "jumped back to live".
    #[test]
    fn a_device_attributes_reply_is_not_typing() {
        assert!(!is_typing(b"\x1b[?62;1;6;9;15;22c"));
        assert!(!is_typing(b"\x1b[?1;2c"));
    }

    /// A cursor-position report is the same kind of thing.
    #[test]
    fn a_cursor_position_report_is_not_typing() {
        assert!(!is_typing(b"\x1b[24;80R"));
    }

    /// So are focus events, which arrive whenever the app comes forward.
    #[test]
    fn focus_events_are_not_typing() {
        assert!(!is_typing(b"\x1b[I"));
        assert!(!is_typing(b"\x1b[O"));
    }

    /// And what a person types IS typing.
    #[test]
    fn characters_are_typing() {
        assert!(is_typing(b"hello"));
        assert!(is_typing(b"y"));
        assert!(is_typing(" ".as_bytes()));
        assert!(is_typing("é".as_bytes()), "UTF-8 text is typing");
    }

    /// Return is the one that matters most: it is how a waiting CLI is
    /// answered, and it must return the pane to live.
    #[test]
    fn return_is_typing() {
        assert!(is_typing(b"\r"));
        assert!(is_typing(b"\n"));
    }

    /// Deliberate control keys count.
    #[test]
    fn control_keys_are_typing() {
        assert!(is_typing(b"\x03"), "Ctrl-C");
        assert!(is_typing(b"\x04"), "Ctrl-D");
        assert!(is_typing(b"\t"), "Tab");
        assert!(is_typing(b"\x7f"), "Backspace");
        assert!(is_typing(b"\x1b"), "a lone Escape is a keypress");
    }

    /// Cursor keys are NOT, on purpose: in copy mode they scroll, which is what
    /// pressing them while reading history means.
    #[test]
    fn cursor_keys_do_not_leave_copy_mode() {
        assert!(!is_typing(b"\x1b[A"));
        assert!(!is_typing(b"\x1b[B"));
        assert!(!is_typing(b"\x1bOA"), "application-mode cursor key");
    }

    /// A reply arriving in the same write as a keystroke still counts as
    /// typing — the person did type.
    #[test]
    fn a_keystroke_mixed_with_a_reply_is_typing() {
        assert!(is_typing(b"\x1b[?62ca"));
    }

    /// A truncated escape sequence is still not typing. A reply split across
    /// two writes must not be read as its characters.
    #[test]
    fn a_truncated_sequence_is_not_typing() {
        assert!(!is_typing(b"\x1b[?62"));
        assert!(!is_typing(b"\x1b]0;some title"));
    }

    #[test]
    fn nothing_is_not_typing() {
        assert!(!is_typing(b""));
        assert!(!is_typing(b"\0"));
    }
}

#[cfg(test)]
mod activity_tests {
    use super::PaneWatcher;

    /// Feed a watcher several readings and collect what it reported.
    fn watch(panes: &[&str]) -> Vec<String> {
        let mut watcher = PaneWatcher::new();
        panes
            .iter()
            .filter_map(|pane| watcher.observe(pane))
            .collect()
    }

    /// Failure one: a TUI whose last line is a static footer.
    ///
    /// codex ends every frame with the same model line, so "the last non-empty
    /// line" returned that forever and the roster showed nothing happening.
    #[test]
    fn a_static_footer_does_not_hide_the_work_above_it() {
        let said = watch(&[
            "reading files\ngpt-5.6-sol high · /",
            "reading files\ngpt-5.6-sol high · /",
            "writing the patch\ngpt-5.6-sol high · /",
            "writing the patch\ngpt-5.6-sol high · /",
        ]);
        assert!(
            said.contains(&"writing the patch".to_owned()),
            "never reported the work: {said:?}"
        );
        assert!(
            !said.iter().any(|line| line.contains("gpt-5.6-sol")),
            "reported the footer: {said:?}"
        );
    }

    /// Failure two and three, from the live panes: Claude's status line
    /// changes every tick AND moves between rows as the transcript scrolls, so
    /// neither "last changed row" nor per-row churn suppressed it. The reported
    /// line was `✽ Canoodling… (4m 34s · ↓ 19.3k tokens)`.
    #[test]
    fn a_ticking_spinner_is_never_reported() {
        let said = watch(&[
            "the model is thinking\n✻ Canoodling… (4m 31s · ↓ 19.0k tokens)\n⏵⏵ bypass permissions on",
            "the model is thinking\n· Canoodling… (4m 33s · ↓ 19.0k tokens)\n⏵⏵ bypass permissions on",
            "the model is thinking\n✽ Canoodling… (4m 34s · ↓ 19.3k tokens)\n⏵⏵ bypass permissions on",
            "the model is thinking\n✻ Canoodling… (4m 36s · ↓ 19.6k tokens)\n⏵⏵ bypass permissions on",
        ]);
        assert!(
            !said.iter().any(|line| line.contains("Canoodling")),
            "reported the spinner: {said:?}"
        );
    }

    /// And what the model actually says IS reported, from among the same
    /// churning chrome.
    #[test]
    fn what_the_model_says_is_reported_despite_the_spinner() {
        let said = watch(&[
            "thinking\n✻ Canoodling… (1s)\n⏵⏵ bypass permissions on",
            "thinking\n· Canoodling… (2s)\n⏵⏵ bypass permissions on",
            "thinking\nhere is the plan for the schema\n✽ Canoodling… (3s)\n⏵⏵ bypass permissions on",
            "thinking\nhere is the plan for the schema\n✻ Canoodling… (4s)\n⏵⏵ bypass permissions on",
        ]);
        assert!(
            said.contains(&"here is the plan for the schema".to_owned()),
            "did not report what the model said: {said:?}"
        );
    }

    /// A static input box is not news either, however permanent it looks.
    #[test]
    fn a_static_prompt_is_not_reported_twice() {
        let pane = "Ask Codex to do anything\ngpt-5.6-sol high · /";
        let said = watch(&[pane, pane, pane, pane]);
        assert!(said.len() <= 1, "repeated a static pane: {said:?}");
    }

    /// The first reading only primes; the second speaks.
    ///
    /// The first has nothing to compare against, so its only possible answer is
    /// the last non-empty line — which is the footer, the thing this exists to
    /// stop reporting. One tick of the pump later there is a real answer, and a
    /// roster row reading "started — no output yet" for three seconds is true
    /// rather than wrong.
    #[test]
    fn the_first_two_readings_prime_and_the_third_speaks() {
        let pane = "starting up\ncompiling";
        assert!(
            watch(&[pane]).is_empty(),
            "the first reading must not guess"
        );
        assert!(
            watch(&[pane, pane]).is_empty(),
            "the second cannot tell a footer from a fact either"
        );
        // A third reading with something NEW in it. A pane that has not moved
        // in three readings has nothing to say, and the waiting flag is what
        // covers that case.
        assert_eq!(
            watch(&[
                pane,
                pane,
                "starting up\ncompiling\nlinking",
                "starting up\ncompiling\nlinking"
            ]),
            vec!["linking".to_owned()]
        );
    }

    /// The same line is not reported again as it scrolls up the pane.
    #[test]
    fn a_line_scrolling_upward_is_reported_once() {
        let said = watch(&[
            "a\nb",
            "a\nb\nDONE",
            "a\nb\nDONE",
            "b\nDONE\nc",
            "DONE\nc\nd",
        ]);
        assert_eq!(
            said.iter().filter(|line| *line == "DONE").count(),
            1,
            "reported the same line more than once: {said:?}"
        );
    }

    /// Long lines are cut, because the roster shows one row.
    #[test]
    fn a_very_long_line_is_truncated() {
        let long = "x".repeat(500);
        let filler = "priming";
        let combined = format!("{filler}\n{long}");
        let said = watch(&[filler, filler, &combined, &combined]);
        let reported = said.first().expect("a line");
        assert!(reported.chars().count() <= super::STATUS_LINE_CHARS + 1);
        assert!(reported.ends_with('…'));
    }
}
