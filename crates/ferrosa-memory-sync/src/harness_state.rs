//! Module: Ask a harness what it is doing, rather than guessing from its pane.
//! Correctness: Correct when a harness that reports its own state is believed,
//! when one that does not is not pretended about, and when a slow or missing
//! CLI costs a tick rather than the pump.
//! Last revised: 2026-08-23
//! Last changed: New.
//!
//! # Why this beats reading the screen
//!
//! Everything else here works out what a job is doing by watching its pane, and
//! three successive rules for that were wrong against real panes — a static
//! footer, a ticking spinner, a spinner that scrolls between rows. The screen is
//! a rendering of the state, and rendering is lossy.
//!
//! Claude Code keeps the state itself and will hand it over: `claude agents
//! --json` reports `busy` or `idle` per running session. Idle, for an
//! interactive session, is precisely "waiting for the person" — the thing the
//! operator wants to be told about and the thing a pane cannot say, because a
//! pane waiting for input looks identical to a pane thinking.
//!
//! Suggested by Ben via `craftzdog/tmux-claude-session-manager`, which uses the
//! same source. Verified against the live CLI before being built on: `status` is
//! the field that carries it, not `state` — `state` is only populated for
//! background agents.
//!
//! # What about codex
//!
//! `codex` has no equivalent subcommand, checked. Sessions running it fall back
//! to the pane watcher and the quiet timer, and this module says `Unknown`
//! rather than inventing an answer.

use std::collections::HashMap;

use tokio::process::Command;

/// What a harness says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessState {
    /// Working. Not waiting for anybody.
    Busy,
    /// Sitting at its prompt: for an interactive session, waiting for a person.
    Waiting,
}

/// Every Claude session on this machine, by process id.
///
/// One call per tick rather than one per job — the CLI walks its own supervisor
/// and the cost is the same whether one job asks or six do.
///
/// An empty map on any failure. A harness that cannot be asked is not a harness
/// that is idle, and the caller distinguishes "no answer" from "idle" because
/// conflating them would announce every job as waiting the moment the CLI moved.
pub async fn claude_agents() -> HashMap<u32, HarnessState> {
    let Ok(output) = Command::new("claude")
        .args(["agents", "--json"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }
    parse_agents(&String::from_utf8_lossy(&output.stdout))
}

/// Pull `pid` and `status` out of what the CLI printed.
///
/// Split from the call so the shape can be tested without the CLI installed —
/// and because the shape is the part that will change out from under this.
pub fn parse_agents(json: &str) -> HashMap<u32, HarnessState> {
    let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
        return HashMap::new();
    };
    rows.iter()
        .filter_map(|row| {
            let pid = u32::try_from(row.get("pid")?.as_u64()?).ok()?;
            // `status`, not `state`. `state` is only set for background agents
            // and is null for every interactive one — reading it would report
            // nothing for exactly the sessions this is about.
            let state = match row.get("status")?.as_str()? {
                "busy" => HarnessState::Busy,
                "idle" => HarnessState::Waiting,
                // An unrecognised status is not a guess. A new value should
                // fall through to the pane watcher rather than be mapped to
                // whichever variant looks closest.
                _ => return None,
            };
            Some((pid, state))
        })
        .collect()
}

/// Every process under a tmux pane, including the pane's own shell.
///
/// A harness runs as a CHILD of the pane's shell, so matching on the pane pid
/// alone finds nothing. Two levels is enough for `sh -c claude` and for a
/// harness that re-execs once; deeper than that and the answer is better sought
/// from the harness itself.
pub async fn pane_process_tree(tmux: &std::path::Path, target: &str) -> Vec<u32> {
    let Ok(output) = Command::new(tmux)
        .args(["display-message", "-p", "-t", target, "#{pane_pid}"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    else {
        return Vec::new();
    };
    let Ok(root) = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
    else {
        return Vec::new();
    };

    let mut found = vec![root];
    let mut frontier = vec![root];
    for _ in 0..2 {
        let mut next = Vec::new();
        for parent in frontier.drain(..) {
            next.extend(children_of(parent).await);
        }
        found.extend(next.iter().copied());
        frontier = next;
    }
    found
}

async fn children_of(parent: u32) -> Vec<u32> {
    let Ok(output) = Command::new("pgrep")
        .args(["-P", &parent.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shape, copied from the live CLI.
    const SAMPLE: &str = r#"[
        {"id":"c4e61586","cwd":"/src/musicstand","kind":"background",
         "sessionId":"c4e6","name":"Port app","state":"blocked"},
        {"pid":8747,"cwd":"/src/ferrosa-suite","kind":"interactive",
         "state":null,"status":"busy","name":"Mobile"},
        {"pid":90996,"cwd":"/src/hippo","kind":"interactive",
         "state":null,"status":"idle","name":"Musicstand"}
    ]"#;

    /// `status` carries it, and `state` does not.
    ///
    /// The docs describe a `state` of busy/waiting/idle. On the live CLI every
    /// interactive row has `state: null` and a populated `status`; only
    /// background rows use `state`. Reading `state` would have reported nothing
    /// for exactly the sessions this exists for.
    #[test]
    fn status_is_read_and_state_is_not() {
        let found = parse_agents(SAMPLE);
        assert_eq!(found.get(&8747), Some(&HarnessState::Busy));
        assert_eq!(found.get(&90996), Some(&HarnessState::Waiting));
    }

    /// A background agent has no pid and is not a pane. Skipped rather than
    /// mapped onto some unrelated process.
    #[test]
    fn a_row_without_a_pid_is_skipped() {
        assert_eq!(parse_agents(SAMPLE).len(), 2);
    }

    /// An unrecognised status is no answer, not a guess.
    ///
    /// It falls through to the pane watcher. Mapping a new value to whichever
    /// variant looks closest is how a harness update would silently start
    /// reporting every job as waiting.
    #[test]
    fn an_unknown_status_is_not_guessed() {
        let json = r#"[{"pid":1,"status":"reticulating"}]"#;
        assert!(parse_agents(json).is_empty());
    }

    /// Output that is not the expected shape is an empty answer, not a panic.
    /// A CLI that changes or is not installed must cost a tick, not the pump.
    #[test]
    fn junk_output_is_survivable() {
        assert!(parse_agents("not json").is_empty());
        assert!(parse_agents("{}").is_empty());
        assert!(parse_agents("[]").is_empty());
        assert!(parse_agents(r#"[{"pid":"not a number","status":"idle"}]"#).is_empty());
    }
}
