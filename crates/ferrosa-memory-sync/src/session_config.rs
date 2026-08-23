//! Module: Named session configurations, owned by the machine.
//! Correctness: Correct when a config round-trips the wire unchanged, when a
//! tmux session can be matched back to the config that made it after a
//! reconnect, and when text from a terminal renders as the operator would read
//! it rather than as escape codes.
//! Last revised: 2026-08-23
//! Last changed: Initial config model and terminal output cleaning.
//!
//! # Why the machine owns these
//!
//! A phone could hold its own list and send the whole command when it picks
//! one. It would be less code and it would be wrong twice over: the tablet and
//! the phone would drift apart, and a resumed tmux session could not be
//! matched to the config that started it — the machine would find a session
//! running and have no idea what it was for.
//!
//! It also means the wire carries a config ID rather than a command line. A
//! device asks to run something the machine already knows about, instead of
//! handing it a string to execute. That is a smaller surface, and it is the
//! shape a capability grant wants: the question becomes "may this device run
//! configs" rather than "may this device run this arbitrary text".

use std::time::Duration;

/// How a configured command is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A PTY that lives and dies with the connection.
    ///
    /// No resumption, deliberately. This is the one for a quick command where
    /// the point is that nothing is left behind.
    EphemeralBash,
    /// A tmux session, tracked so it survives a reconnect.
    ///
    /// The machine records the tmux session name against the config, so a
    /// device reconnecting finds the same session still running rather than
    /// starting a second one beside it.
    Tmux,
}

impl SessionKind {
    pub fn as_wire(self) -> &'static str {
        match self {
            SessionKind::EphemeralBash => "bash",
            SessionKind::Tmux => "tmux",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        match value {
            "bash" => Some(SessionKind::EphemeralBash),
            "tmux" => Some(SessionKind::Tmux),
            _ => None,
        }
    }

    /// Whether a session of this kind outlives the connection that opened it.
    pub fn resumable(self) -> bool {
        matches!(self, SessionKind::Tmux)
    }
}

/// One configured command the machine will run on request.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionConfig {
    /// Stable across renames, because a tmux session is tracked against it.
    /// Renaming a config must not orphan the session it started.
    pub id: uuid::Uuid,
    pub name: String,
    pub kind: SessionKind,
    /// The program and its arguments, already split.
    ///
    /// A list rather than a command line, so nothing here is passed through a
    /// shell for splitting. A single string would make quoting the difference
    /// between one argument and two, and on this path that difference is
    /// arbitrary code.
    pub command: Vec<String>,
}

/// Why a proposed config was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("a config needs a name")]
    NoName,
    #[error("a name may be at most {MAX_NAME_CHARS} characters")]
    NameTooLong,
    #[error("a config needs a command to run")]
    NoCommand,
    #[error("a command may have at most {MAX_ARGS} arguments")]
    TooManyArgs,
    #[error("unknown session kind")]
    UnknownKind,
}

const MAX_NAME_CHARS: usize = 64;
const MAX_ARGS: usize = 64;

impl SessionConfig {
    /// Build a config from what a device proposed, refusing what it cannot run.
    ///
    /// Validated HERE rather than at use, so a bad config is refused while
    /// someone is looking at the dialog that made it — not later, when a
    /// session mysteriously fails to open.
    pub fn create(name: &str, kind: &str, command: Vec<String>) -> Result<Self, ConfigError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ConfigError::NoName);
        }
        if name.chars().count() > MAX_NAME_CHARS {
            return Err(ConfigError::NameTooLong);
        }
        let kind = SessionKind::from_wire(kind).ok_or(ConfigError::UnknownKind)?;
        // Empty arguments are dropped, but an all-empty command is refused: it
        // would otherwise become a config that opens a session running nothing
        // and reports no error.
        let command: Vec<String> = command
            .into_iter()
            .map(|part| part.trim().to_owned())
            .filter(|part| !part.is_empty())
            .collect();
        if command.is_empty() {
            return Err(ConfigError::NoCommand);
        }
        if command.len() > MAX_ARGS {
            return Err(ConfigError::TooManyArgs);
        }
        Ok(Self {
            // v7 so the list has a natural creation order without storing one.
            id: uuid::Uuid::now_v7(),
            name: name.to_owned(),
            kind,
            command,
        })
    }

    /// Apply an edit, keeping the id so a running session stays attributable.
    ///
    /// Validated the same way a new config is, so an edit cannot produce
    /// something `create` would have refused.
    pub fn edited(
        &self,
        name: &str,
        kind: &str,
        command: Vec<String>,
    ) -> Result<Self, ConfigError> {
        let mut updated = SessionConfig::create(name, kind, command)?;
        // The id is IDENTITY, not a version. Keeping it is what lets a tmux
        // session started by this config still be recognised as its own after
        // an edit — a new id would orphan it, leaving a session running that
        // the machine could no longer attribute or stop.
        updated.id = self.id;
        Ok(updated)
    }

    /// The tmux session name for this config on this machine.
    ///
    /// Derived from the config id rather than the name, so renaming a config
    /// does not orphan a running session — the machine would otherwise find a
    /// session it could no longer attribute and leave it running forever.
    ///
    /// Prefixed so a `tmux ls` makes it obvious what created these, and so
    /// nothing here can collide with a session the operator started by hand.
    pub fn tmux_session_name(&self) -> String {
        format!("ferrosa-session-{}", self.id.simple())
    }
}

/// How long to wait for a config's first output before saying so.
///
/// Not a failure — a command may legitimately be silent — but the shells need
/// to distinguish "still starting" from "running and quiet", and a bare
/// spinner forever is the worst of both.
pub const FIRST_OUTPUT_HINT: Duration = Duration::from_secs(3);

/// Strip terminal control sequences, keeping the text a person would read.
///
/// Not a terminal emulator, and deliberately not: the shells render this in
/// the design system as text. What that costs is anything which redraws in
/// place — a progress bar, `top`, vim — since without a cursor model those
/// become repeated lines rather than one line changing.
///
/// The one concession is carriage return: a `\r` with no newline means "start
/// this line again", which is exactly what a progress bar does. Honouring just
/// that turns the most common redraw into a single updating line for almost no
/// complexity, and without it a thirty-second build produces hundreds of
/// near-identical lines.
pub fn clean_terminal_output(raw: &str) -> String {
    let stripped = strip_escapes(raw);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for character in stripped.chars() {
        match character {
            '\n' => {
                lines.push(std::mem::take(&mut current));
            }
            // Back to the start of this line: what follows overwrites it.
            '\r' => current.clear(),
            // Backspace, which some tools use to erase a spinner character.
            '\u{8}' => {
                current.pop();
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines.join("\n")
}

/// Remove ANSI escape sequences.
///
/// Handles CSI (`ESC [ ... final`) and OSC (`ESC ] ... BEL` or `ESC \`), which
/// between them cover colour, cursor movement and window-title setting — the
/// sequences a build or a shell prompt actually emits. Anything else beginning
/// with ESC drops the ESC and its next byte, which is right for the two-byte
/// sequences and harmless for the rest.
fn strip_escapes(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then a final byte in @..~
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            // OSC: runs until BEL or ST (ESC \)
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        // ST — consume the backslash too.
                        let _ = chars.next();
                        break;
                    }
                }
            }
            // A two-byte sequence; the byte after ESC is already consumed.
            Some(_) | None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_round_trips_its_kind() {
        for kind in [SessionKind::EphemeralBash, SessionKind::Tmux] {
            assert_eq!(SessionKind::from_wire(kind.as_wire()), Some(kind));
        }
    }

    /// Only tmux survives a reconnect, and the shells show that differently —
    /// an ephemeral session vanishing is expected, a tmux one vanishing is a
    /// fault.
    #[test]
    fn only_tmux_is_resumable() {
        assert!(SessionKind::Tmux.resumable());
        assert!(!SessionKind::EphemeralBash.resumable());
    }

    #[test]
    fn a_valid_config_is_accepted() {
        let config = SessionConfig::create("build", "tmux", vec!["cargo".into(), "test".into()])
            .expect("valid");
        assert_eq!(config.name, "build");
        assert_eq!(config.kind, SessionKind::Tmux);
        assert_eq!(config.command, vec!["cargo", "test"]);
    }

    /// A config that runs nothing would open a session and report no error,
    /// which reads as the machine ignoring the request.
    #[test]
    fn a_config_with_no_command_is_refused() {
        assert_eq!(
            SessionConfig::create("empty", "bash", vec![]),
            Err(ConfigError::NoCommand)
        );
        assert_eq!(
            SessionConfig::create("blank", "bash", vec!["  ".into(), "".into()]),
            Err(ConfigError::NoCommand)
        );
    }

    #[test]
    fn a_config_with_no_name_is_refused() {
        assert_eq!(
            SessionConfig::create("   ", "bash", vec!["ls".into()]),
            Err(ConfigError::NoName)
        );
    }

    #[test]
    fn an_unknown_kind_is_refused() {
        assert_eq!(
            SessionConfig::create("odd", "powershell", vec!["ls".into()]),
            Err(ConfigError::UnknownKind)
        );
    }

    /// The tmux name comes from the ID, so renaming cannot orphan a running
    /// session. If it came from the name, a rename would leave the machine
    /// holding a session it could no longer attribute.
    #[test]
    fn renaming_a_config_does_not_change_its_tmux_session() {
        let mut config =
            SessionConfig::create("first", "tmux", vec!["htop".into()]).expect("valid");
        let before = config.tmux_session_name();
        config.name = "renamed".to_owned();
        assert_eq!(config.tmux_session_name(), before);
    }

    /// An edit keeps the id, so a running session stays attributable. A new
    /// id would orphan it — still running, no longer recognised.
    #[test]
    fn editing_keeps_the_id_and_so_the_running_session() {
        let original = SessionConfig::create("build", "tmux", vec!["cargo".into(), "build".into()])
            .expect("valid");
        let edited = original
            .edited("build", "tmux", vec!["cargo".into(), "test".into()])
            .expect("valid");
        assert_eq!(edited.id, original.id);
        assert_eq!(edited.tmux_session_name(), original.tmux_session_name());
        assert_eq!(edited.command, vec!["cargo", "test"]);
    }

    /// An edit is validated like a creation, so it cannot produce something
    /// `create` would have refused.
    #[test]
    fn an_edit_to_nothing_is_refused() {
        let original = SessionConfig::create("build", "tmux", vec!["ls".into()]).expect("valid");
        assert_eq!(
            original.edited("", "tmux", vec!["ls".into()]),
            Err(ConfigError::NoName)
        );
        assert_eq!(
            original.edited("build", "tmux", vec![]),
            Err(ConfigError::NoCommand)
        );
    }

    /// Two configs must never share a tmux session, or selecting one attaches
    /// to the other's process.
    #[test]
    fn two_configs_get_distinct_tmux_sessions() {
        let one = SessionConfig::create("a", "tmux", vec!["ls".into()]).expect("valid");
        let two = SessionConfig::create("b", "tmux", vec!["ls".into()]).expect("valid");
        assert_ne!(one.tmux_session_name(), two.tmux_session_name());
    }

    // --- terminal output ---

    #[test]
    fn colour_is_stripped_leaving_the_text() {
        let raw = "\u{1b}[31merror\u{1b}[0m: it broke";
        assert_eq!(clean_terminal_output(raw), "error: it broke");
    }

    /// A window-title sequence is emitted by most shell prompts. Left in, it
    /// appears as a line of punctuation before every command.
    #[test]
    fn a_window_title_sequence_is_stripped() {
        let raw = "\u{1b}]0;bkearns@mac\u{7}hello";
        assert_eq!(clean_terminal_output(raw), "hello");
    }

    /// The reason `\r` is honoured. A progress bar emits the same line
    /// repeatedly; without this a thirty-second build is hundreds of
    /// near-identical lines and the useful output scrolls away.
    #[test]
    fn a_carriage_return_rewrites_the_line_rather_than_adding_one() {
        let raw = "Building 10%\rBuilding 50%\rBuilding 100%\ndone";
        assert_eq!(clean_terminal_output(raw), "Building 100%\ndone");
    }

    #[test]
    fn backspace_erases_a_character() {
        assert_eq!(clean_terminal_output("abc\u{8}d"), "abd");
    }

    /// Cursor movement is discarded rather than acted on. This is the case
    /// that renders badly and is accepted: a TUI redrawing at coordinates
    /// arrives as its text in emission order.
    #[test]
    fn cursor_movement_is_discarded() {
        assert_eq!(
            clean_terminal_output("\u{1b}[2J\u{1b}[Htop line"),
            "top line"
        );
    }

    #[test]
    fn ordinary_text_is_untouched() {
        let raw = "cargo test\n   Compiling ferrosa\ntest result: ok. 57 passed";
        assert_eq!(clean_terminal_output(raw), raw);
    }

    /// A sequence split across two reads must not leave a stray ESC in the
    /// output. This cleans per-chunk, so a truncated escape drops what it has
    /// rather than emitting control bytes into the transcript.
    #[test]
    fn a_truncated_escape_does_not_leak_control_bytes() {
        let cleaned = clean_terminal_output("text\u{1b}[3");
        assert!(!cleaned.contains('\u{1b}'), "escape survived: {cleaned:?}");
    }
}
