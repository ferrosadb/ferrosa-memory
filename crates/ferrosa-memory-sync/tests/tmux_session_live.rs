//! Module: Does a tmux session actually take input from the control channel?
//! Correctness: Correct when a command sent through `RunningSession::send`
//! runs in the detached pane and its output comes back out of the session's
//! own output stream — not merely when the code compiles.
//! Last revised: 2026-08-23
//! Last changed: Added after input to a launched tmux config did nothing on
//! the device, to find out which side of the wire was at fault.
//!
//! Ignored by default because it needs a real `tmux` and leaves a session
//! running for as long as the test does. Run with:
//!
//! ```text
//! cargo test -p ferrosa-memory-sync --test tmux_session_live -- --ignored
//! ```

use std::time::Duration;

use ferrosa_memory_sync::session_config::SessionConfig;
use ferrosa_memory_sync::session_runtime::{ScrollMotion, SessionRuntime};

/// Read from the session until `needle` shows up, or give up.
///
/// Polling with a deadline rather than a fixed sleep: tmux is being scraped on
/// an interval, so the first look is expected to be empty and a fixed wait
/// would either be flaky or slow.
async fn wait_for(
    running: &ferrosa_memory_sync::session_runtime::RunningSession,
    needle: &str,
    within: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + within;
    let mut seen = String::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, running.next_output()).await {
            Ok(Some(bytes)) => {
                // Lossy is fine HERE and only here: the test is looking for
                // ASCII markers in a stream that also carries escape
                // sequences. The production path never converts.
                seen.push_str(&String::from_utf8_lossy(&bytes));
                if seen.contains(needle) {
                    return seen;
                }
            }
            // The session ended, or we ran out of time. Either way the caller
            // gets what arrived, and asserts against it.
            Ok(None) | Err(_) => break,
        }
    }
    seen
}

#[tokio::test]
#[ignore = "needs a real tmux"]
async fn input_sent_to_a_tmux_session_runs_in_the_pane() {
    let workspace = std::env::temp_dir().join("ferrosa-tmux-input-probe");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let runtime = SessionRuntime::new(workspace);

    // A shell, so there is something to type AT. A config whose command is a
    // one-shot program has nowhere to put input, which is a different bug from
    // the one this test is about.
    let config = SessionConfig::create(
        "input-probe",
        "tmux",
        vec!["bash".into(), "--noprofile".into(), "--norc".into()],
        None,
    )
    .expect("valid config");

    let running = runtime.open(&config).await.expect("tmux session opens");

    // Wait for the prompt before typing. Sending input to a pane whose shell
    // has not started yet is dropped by the terminal, and would look exactly
    // like the bug being investigated.
    let _ = wait_for(&running, "$", Duration::from_secs(5)).await;

    running
        .send("echo FERROSA_INPUT_OK\n")
        .expect("input is accepted");

    let seen = wait_for(&running, "FERROSA_INPUT_OK", Duration::from_secs(5)).await;

    // Kill the session before asserting, so a failure does not leave a stray
    // tmux session behind on the developer's machine.
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &config.tmux_session_name()])
        .status();

    assert!(
        seen.contains("FERROSA_INPUT_OK"),
        "the pane never ran what was sent to it; saw:\n{seen}"
    );
}

/// The case a plain shell hides.
///
/// bash accepts a line feed as Return because the line discipline translates
/// CR to NL for it. A program in raw mode gets no such translation: it reads
/// 0x0d as Return and 0x0a as Ctrl-J. Sending "\n" therefore worked against
/// bash and did nothing against a TUI — the pane showed the typed text and sat
/// there, which looks exactly like input not being wired.
///
/// This asserts on the BYTES the program received, because that is the only
/// level at which the two are distinguishable.
#[tokio::test]
#[ignore = "needs a real tmux and python3"]
async fn a_raw_mode_program_receives_a_return_not_a_line_feed() {
    let workspace = std::env::temp_dir().join("ferrosa-tmux-raw-probe");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let log = workspace.join("received.log");
    let _ = std::fs::remove_file(&log);

    // Reads one byte at a time in raw mode and records what arrived — a stand
    // in for any TUI, without depending on one being installed.
    let reader = workspace.join("raw_reader.py");
    std::fs::write(
        &reader,
        format!(
            r#"import sys, tty, termios
fd = sys.stdin.fileno()
saved = termios.tcgetattr(fd)
tty.setraw(fd)
log = open({log:?}, "w")
try:
    while True:
        char = sys.stdin.read(1)
        if not char or char == "q":
            break
        log.write("%02x " % ord(char))
        log.flush()
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, saved)
"#,
            log = log.to_string_lossy()
        ),
    )
    .expect("probe script");

    let config = SessionConfig::create(
        "raw-probe",
        "tmux",
        vec!["python3".into(), reader.to_string_lossy().into_owned()],
        None,
    )
    .expect("valid config");

    let running = runtime_for(&workspace).open(&config).await.expect("opens");
    tokio::time::sleep(Duration::from_millis(800)).await;

    running.send("a\n").expect("input is accepted");
    tokio::time::sleep(Duration::from_millis(600)).await;

    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &config.tmux_session_name()])
        .status();

    let received = std::fs::read_to_string(&log).unwrap_or_default();
    let received = received.trim();
    assert_eq!(
        received, "61 0d",
        "expected 'a' then Return (0x0d); a line feed (0x0a) here is the bug \
         that made a TUI look unresponsive"
    );
}

fn runtime_for(workspace: &std::path::Path) -> SessionRuntime {
    SessionRuntime::new(workspace.to_path_buf())
}

/// The case that sent Ben looking: an agent whose command does not work.
///
/// A command that exits immediately used to take its pane and its session with
/// it, so whatever it printed was gone before the poller's first look — on
/// screen, a session that appeared and vanished saying nothing. The output, the
/// exit status and the session itself must all survive.
#[tokio::test]
#[ignore = "needs a real tmux"]
async fn a_command_that_fails_leaves_its_output_and_its_session_behind() {
    let workspace = std::env::temp_dir().join("ferrosa-tmux-failure-probe");
    std::fs::create_dir_all(&workspace).expect("workspace");

    // Fails instantly and says something first, which is what a broken harness
    // does. `false` alone would prove the status but not that output survives.
    let config = SessionConfig::create(
        "broken",
        "tmux",
        vec![
            "sh".into(),
            "-c".into(),
            "echo BOOM_BEFORE_EXIT >&2; exit 3".into(),
        ],
        None,
    )
    .expect("valid config");

    let running = runtime_for(&workspace).open(&config).await.expect("opens");

    let seen = wait_for(&running, "exited with status 3", Duration::from_secs(6)).await;

    let still_there = std::process::Command::new("tmux")
        .args(["has-session", "-t", &config.tmux_session_name()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &config.tmux_session_name()])
        .status();

    assert!(
        seen.contains("BOOM_BEFORE_EXIT"),
        "the failing command's own output was lost; saw:\n{seen}"
    );
    assert!(
        seen.contains("exited with status 3"),
        "the exit status never reached the transcript; saw:\n{seen}"
    );
    assert!(
        still_there,
        "the session vanished with the command — it must stay until deleted"
    );
}

/// Named keys reach a raw-mode program as the right bytes.
///
/// The case Ben hit: a CLI waiting for confirmation needs a bare Return, and
/// there is no text that means one. Ctrl-C is the other half — a harness that
/// hangs has no other way out from a phone.
#[tokio::test]
#[ignore = "needs a real tmux and python3"]
async fn named_keys_reach_the_program_as_the_right_bytes() {
    use ferrosa_memory_sync::session_runtime::NamedKey;

    let workspace = std::env::temp_dir().join("ferrosa-tmux-keys-probe");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let log = workspace.join("keys.log");
    let _ = std::fs::remove_file(&log);

    let reader = workspace.join("key_reader.py");
    std::fs::write(
        &reader,
        format!(
            r#"import sys, tty, termios
fd = sys.stdin.fileno()
saved = termios.tcgetattr(fd)
tty.setraw(fd)
log = open({log:?}, "w")
try:
    while True:
        char = sys.stdin.read(1)
        if not char or char == "q":
            break
        log.write("%02x " % ord(char))
        log.flush()
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, saved)
"#,
            log = log.to_string_lossy()
        ),
    )
    .expect("probe script");

    let config = SessionConfig::create(
        "keys-probe",
        "tmux",
        vec!["python3".into(), reader.to_string_lossy().into_owned()],
        None,
    )
    .expect("valid config");

    let running = runtime_for(&workspace).open(&config).await.expect("opens");
    tokio::time::sleep(Duration::from_millis(900)).await;

    // A bare Return, with no text at all — the thing the send button could not
    // express because it is disabled on an empty field.
    running.send_key(NamedKey::Enter).expect("enter accepted");
    running.send_key(NamedKey::CtrlC).expect("ctrl-c accepted");
    running.send_key(NamedKey::Up).expect("up accepted");
    tokio::time::sleep(Duration::from_millis(700)).await;

    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &config.tmux_session_name()])
        .status();

    let received = std::fs::read_to_string(&log).unwrap_or_default();
    let received = received.trim();
    // CR, then the interrupt byte, then the CSI sequence for Up.
    assert_eq!(
        received, "0d 03 1b 5b 41",
        "expected Return (0d), Ctrl-C (03) and Up (1b 5b 41); got: {received:?}"
    );
}

/// A name the machine does not know is refused rather than forwarded.
///
/// `tmux send-keys` without `-l` reads its argument as tmux's command language,
/// so passing an arbitrary string through would hand a phone remote tmux
/// control rather than a keypress.
#[test]
fn an_unknown_key_name_is_refused() {
    use ferrosa_memory_sync::session_runtime::NamedKey;

    assert!(NamedKey::from_wire("enter").is_some());
    assert!(NamedKey::from_wire("ctrl-c").is_some());
    assert!(NamedKey::from_wire("kill-server").is_none());
    assert!(NamedKey::from_wire("C-c ; kill-server").is_none());
}

/// The whole point of attaching instead of scraping: escape sequences survive.
///
/// `capture-pane` gave cleaned text with the control codes stripped, so a
/// program that positions its cursor arrived as its repainted screens
/// concatenated — which is what made a redrawing CLI "jump around". A terminal
/// emulator on the device can only work if these bytes reach it intact.
#[tokio::test]
#[ignore = "needs a real tmux"]
async fn escape_sequences_reach_the_device_intact() {
    let workspace = std::env::temp_dir().join("ferrosa-tmux-escape-probe");
    std::fs::create_dir_all(&workspace).expect("workspace");

    // Clears the screen, homes the cursor, then writes in colour — the three
    // things a TUI does constantly and none of which survive stripping.
    let config = SessionConfig::create(
        "escape-probe",
        "tmux",
        vec![
            "sh".into(),
            "-c".into(),
            "printf '\\033[2J\\033[H\\033[31mREDTEXT\\033[0m\\n'; sleep 30".into(),
        ],
        None,
    )
    .expect("valid config");

    let running = runtime_for(&workspace).open(&config).await.expect("opens");
    let seen = wait_for(&running, "REDTEXT", Duration::from_secs(6)).await;

    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &config.tmux_session_name()])
        .status();

    assert!(
        seen.contains("REDTEXT"),
        "the text never arrived; saw:\n{seen:?}"
    );
    assert!(
        seen.contains('\u{1b}'),
        "no escape byte in the stream — it is being stripped somewhere, and an \
         emulator cannot position anything without them; saw:\n{seen:?}"
    );
}

/// Resizing reaches the program, which is how a TUI knows how wide to draw.
#[tokio::test]
#[ignore = "needs a real tmux"]
async fn a_resize_reaches_the_program() {
    let workspace = std::env::temp_dir().join("ferrosa-tmux-resize-probe");
    std::fs::create_dir_all(&workspace).expect("workspace");

    // Reports its width whenever the size changes. `tput cols` asks the
    // terminal, so a stale answer means the resize never landed.
    let config = SessionConfig::create(
        "resize-probe",
        "tmux",
        vec![
            "sh".into(),
            "-c".into(),
            "trap 'echo WIDTH=$(tput cols)' WINCH; echo WIDTH=$(tput cols); \
             while :; do sleep 1; done"
                .into(),
        ],
        None,
    )
    .expect("valid config");

    let running = runtime_for(&workspace).open(&config).await.expect("opens");
    let _ = wait_for(&running, "WIDTH=", Duration::from_secs(5)).await;

    running.resize(30, 132).expect("resize accepted");
    let seen = wait_for(&running, "WIDTH=132", Duration::from_secs(5)).await;

    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &config.tmux_session_name()])
        .status();

    assert!(
        seen.contains("WIDTH=132"),
        "the program never learned the new width; saw:\n{seen}"
    );
}

/// Raw bytes from a client-side emulator reach the program unchanged.
///
/// This is the path the terminal actually uses, and it shipped broken: the app
/// sent `bytes` and the machine only understood `text` and `keys`, so every
/// keypress was refused with "input needs text or keys" and nothing typed
/// reached the console.
///
/// Asserts on the bytes the program received, because "unchanged" is only
/// checkable at that level — a translation applied on the way through would
/// corrupt any escape sequence containing the translated byte.
#[tokio::test]
#[ignore = "needs a real tmux and python3"]
async fn raw_bytes_from_an_emulator_reach_the_program_unchanged() {
    let workspace = std::env::temp_dir().join("ferrosa-tmux-rawinput-probe");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let log = workspace.join("raw_in.log");
    let _ = std::fs::remove_file(&log);

    let reader = workspace.join("raw_in.py");
    std::fs::write(
        &reader,
        format!(
            r#"import sys, tty, termios
fd = sys.stdin.fileno()
saved = termios.tcgetattr(fd)
tty.setraw(fd)
log = open({log:?}, "w")
try:
    while True:
        char = sys.stdin.read(1)
        if not char or char == "q":
            break
        log.write("%02x " % ord(char))
        log.flush()
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, saved)
"#,
            log = log.to_string_lossy()
        ),
    )
    .expect("probe script");

    let config = SessionConfig::create(
        "rawinput-probe",
        "tmux",
        vec!["python3".into(), reader.to_string_lossy().into_owned()],
        None,
    )
    .expect("valid config");

    let running = runtime_for(&workspace).open(&config).await.expect("opens");
    tokio::time::sleep(Duration::from_millis(900)).await;

    // Exactly what an emulator emits for: 'h', Return, Ctrl-C, Up arrow.
    running
        .send_bytes(&[0x68, 0x0d, 0x03, 0x1b, 0x5b, 0x41])
        .expect("bytes accepted");
    // A lone line feed. `send` would turn "\n" into a carriage return, and
    // this path must NOT — that translation is what would corrupt an escape
    // sequence containing 0x0a.
    running.send_bytes(&[0x0a]).expect("bytes accepted");
    tokio::time::sleep(Duration::from_millis(700)).await;

    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &config.tmux_session_name()])
        .status();

    let received = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(
        received.trim(),
        "68 0d 03 1b 5b 41 0a",
        "bytes were altered in transit; a 0a arriving as 0d means this path is \
         newline-translating and will corrupt escape sequences"
    );
}

/// An agent starts where its config says, not where the machine happens to be.
///
/// Asserted by asking the shell itself, because "it started somewhere" is the
/// kind of thing that silently does not happen — the wrong directory produces a
/// working agent doing the wrong work, with no error anywhere.
#[tokio::test]
#[ignore = "needs a real tmux"]
async fn a_config_starts_in_its_own_working_directory() {
    let machine_workspace = std::env::temp_dir().join("ferrosa-cwd-machine");
    let agent_dir = std::env::temp_dir().join("ferrosa-cwd-agent");
    std::fs::create_dir_all(&machine_workspace).expect("machine workspace");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");

    let config = SessionConfig::create(
        "cwd-probe",
        "tmux",
        vec!["sh".into(), "-c".into(), "pwd; sleep 30".into()],
        Some(&agent_dir.to_string_lossy()),
    )
    .expect("valid config");

    // The runtime is given the MACHINE's workspace, which is deliberately not
    // the agent's — so a pass cannot be the fallback quietly working.
    let running = SessionRuntime::new(machine_workspace.clone())
        .open(&config)
        .await
        .expect("opens");

    let seen = wait_for(&running, "ferrosa-cwd", Duration::from_secs(6)).await;

    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &config.tmux_session_name()])
        .status();

    assert!(
        seen.contains("ferrosa-cwd-agent"),
        "started in the wrong directory; saw:\n{seen}"
    );
    assert!(
        !seen.contains("ferrosa-cwd-machine"),
        "fell back to the machine workspace despite the config naming one:\n{seen}"
    );
}

/// A directory that does not exist is refused when the config is WRITTEN.
///
/// Catching it at first run means an agent that dies with a message nobody
/// reads, or worse starts somewhere unintended. FMEA F12.
#[test]
fn a_missing_working_directory_is_refused_at_authoring() {
    let result = SessionConfig::create(
        "bad-cwd",
        "tmux",
        vec!["ls".into()],
        Some("/definitely/not/a/real/path"),
    );
    assert!(result.is_err(), "a nonexistent directory was accepted");

    // A file is not a directory, and the message should say which is wrong.
    let file = std::env::temp_dir().join("ferrosa-cwd-not-a-dir");
    std::fs::write(&file, b"x").expect("fixture");
    let result = SessionConfig::create(
        "file-cwd",
        "tmux",
        vec!["ls".into()],
        Some(&file.to_string_lossy()),
    );
    assert!(
        result.is_err(),
        "a file was accepted as a working directory"
    );
}

/// Scrolling reaches tmux's real scrollback, and the device sees it.
///
/// Asserted on the OUTPUT STREAM, which is what actually reaches the phone —
/// not on `capture-pane`, which reads the LIVE pane buffer and reports the
/// bottom of the history no matter where copy mode is looking. The first
/// version of this test used capture-pane and failed against a scroll that was
/// working correctly (tmux reported pane_in_mode=1, scroll_position=377).
///
/// Sending Page Up as KEY BYTES cannot pass this: those keys go to the program
/// in the pane and never reach tmux's history.
#[tokio::test]
#[ignore = "needs a real tmux"]
async fn scrolling_up_reaches_output_that_left_the_screen() {
    let config = SessionConfig::create(
        "scrollback",
        "tmux",
        // Numbered so a specific line can be looked for, and far more than a
        // pane holds so the early ones are definitely off-screen.
        vec![
            "sh".into(),
            "-c".into(),
            // Zero-padded, so "LINE-0001" cannot match LINE-100 and friends.
            // tmux redraws with cursor positioning rather than padding, so a
            // trailing space is NOT a reliable delimiter — the first version of
            // this test looked for "LINE-1 " and never found it.
            "for i in $(seq 1 400); do printf 'LINE-%04d\\n' $i; done; sleep 60".into(),
        ],
        None,
    )
    .expect("valid config");

    let running = SessionRuntime::new(std::env::temp_dir())
        .open(&config)
        .await
        .expect("opens");

    // Let it finish printing before scrolling, or the pane is still moving.
    let _ = wait_for(&running, "LINE-0400", Duration::from_secs(10)).await;

    running
        .scroll(ScrollMotion::Top)
        .expect("scrolling to the top is accepted");
    // The redraw tmux sends its attached client is the evidence: the first line
    // ever printed is only drawn if the history really moved.
    let at_top = wait_for(&running, "LINE-0001", Duration::from_secs(8)).await;
    let scrolled_position = tmux_var(&config, "#{scroll_position}");

    running
        .scroll(ScrollMotion::Bottom)
        .expect("returns to live");
    let back_live = wait_for(&running, "LINE-0400", Duration::from_secs(8)).await;
    let in_mode = tmux_var(&config, "#{pane_in_mode}");

    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &config.tmux_session_name()])
        .status();

    assert_ne!(
        scrolled_position, "0",
        "tmux did not move through the history at all"
    );
    assert!(
        at_top.contains("LINE-0001"),
        "the top of the history never reached the device"
    );
    assert!(
        back_live.contains("LINE-0400"),
        "leaving copy mode did not redraw the live pane"
    );
    assert_eq!(in_mode, "0", "the pane was left in copy mode, not live");
}

/// An ephemeral session says so rather than failing silently.
///
/// Its scrollback lives in the client's own emulator, and there is no tmux to
/// ask. A scroll that quietly did nothing would read as a dead button.
#[tokio::test]
#[ignore = "needs a real tmux"]
async fn scrolling_an_ephemeral_session_is_refused_with_a_reason() {
    let config = SessionConfig::create(
        "ephemeral",
        "bash",
        vec!["sh".into(), "-c".into(), "sleep 5".into()],
        None,
    )
    .expect("valid config");

    let running = SessionRuntime::new(std::env::temp_dir())
        .open(&config)
        .await
        .expect("opens");

    let refused = running.scroll(ScrollMotion::PageUp);
    let message = refused.expect_err("must be refused").to_string();
    assert!(
        message.contains("scrollback"),
        "the refusal should say why, not just fail; got: {message}"
    );
}

/// Ask tmux directly, for the assertions the output stream cannot make.
fn tmux_var(config: &SessionConfig, format: &str) -> String {
    let output = std::process::Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            &config.tmux_session_name(),
            format,
        ])
        .output()
        .expect("display-message runs");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}
