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
use ferrosa_memory_sync::session_runtime::SessionRuntime;

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
    )
    .expect("valid config");

    let running = runtime_for(&workspace).open(&config).await.expect("opens");
    let seen = wait_for(&running, "REDTEXT", Duration::from_secs(6)).await;

    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &config.tmux_session_name()])
        .status();

    assert!(seen.contains("REDTEXT"), "the text never arrived; saw:\n{seen:?}");
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
