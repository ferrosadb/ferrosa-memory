//! Module: Find the deliverables an agent produced, in the session that made them.
//! Correctness: Correct when every pull request an agent opened is found with
//! the title it was opened under, when nothing is invented for a session that
//! produced nothing, and when the same deliverable is reported once.
//! Last revised: 2026-08-25
//! Last changed: new — claims had no source; nothing wrote them.
//!
//! # Why read the transcript rather than ask GitHub
//!
//! A claim is a thing an AGENT produced and nobody has judged, so the question
//! is what this agent did, not what exists on a server. Listing open pull
//! requests from GitHub answers a different question: it includes work by
//! people, misses anything not yet pushed, and knows nothing about which
//! session to send feedback to. The session is where authorship lives, and
//! authorship is what makes a claim reviewable rather than merely present.
//!
//! # What counts as a deliverable
//!
//! A pull request, for now. It is the one artifact with an unambiguous signal
//! in the transcript — Claude Code writes a `pr-link` record when one is opened
//! — and an address a reviewer can act on. Files written are a much noisier
//! signal: an agent writes a hundred files a session and almost none of them
//! are things a person should be asked to approve.
//!
//! # Where the title comes from
//!
//! The `pr-link` record carries a number, a URL and a repository, but no title,
//! and a claim with no title is not reviewable. The title is recovered from the
//! `gh pr create --title` that opened it, paired to its URL through the tool
//! result that command produced — an exact pairing rather than a guess by
//! ordering, because a session can open several.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

/// One thing an agent produced that a person could be asked to judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deliverable {
    /// `pull_request` today. Kept open because approvals are one kind of
    /// decision among several — command runs and access grants are coming.
    pub kind: String,
    pub title: String,
    pub url: String,
    /// `owner/name`, as the transcript recorded it.
    pub repo: Option<String>,
    /// The session that made it, so feedback reaches the agent that can act on
    /// it — or a replacement can pick up where it left off.
    pub session_id: Option<Uuid>,
    pub at: Option<DateTime<Utc>>,
}

/// A `pr-link` record, which Claude Code writes when a pull request is opened.
#[derive(Deserialize)]
struct PrLink {
    #[serde(rename = "prUrl")]
    pr_url: String,
    #[serde(rename = "prRepository")]
    pr_repository: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    timestamp: Option<String>,
}

/// Find every pull request opened in one transcript.
///
/// Two passes over one read: titles are paired to URLs through the tool result
/// of the command that opened them, and `pr-link` records supply the
/// authoritative URL, repository and time. A URL is reported once however many
/// times the transcript mentions it — the same link is re-recorded on every
/// later turn of the session.
pub fn scan_transcript<I, S>(lines: I) -> Vec<Deliverable>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    // tool_use id -> the title that command opened a pull request under.
    let mut titles_by_call: HashMap<String, String> = HashMap::new();
    // url -> title, once the tool result names the URL that command produced.
    let mut titles_by_url: HashMap<String, String> = HashMap::new();
    let mut links: Vec<PrLink> = Vec::new();

    for line in lines {
        let line = line.as_ref();
        // A cheap reject first: these transcripts run to hundreds of megabytes
        // and almost every line is neither of the two things wanted here.
        let has_create = line.contains("gh pr create");
        let has_link = line.contains("\"pr-link\"");
        let has_result = line.contains("/pull/");
        if !has_create && !has_link && !has_result {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        if has_link
            && value.get("type").and_then(|t| t.as_str()) == Some("pr-link")
            && let Ok(link) = serde_json::from_value::<PrLink>(value.clone())
        {
            links.push(link);
        }

        let Some(blocks) = value.pointer("/message/content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => {
                    let command = block
                        .pointer("/input/command")
                        .and_then(|c| c.as_str())
                        .unwrap_or_default();
                    if !command.contains("gh pr create") {
                        continue;
                    }
                    if let (Some(id), Some(title)) = (
                        block.get("id").and_then(|i| i.as_str()),
                        title_argument(command),
                    ) {
                        titles_by_call.insert(id.to_owned(), title);
                    }
                }
                Some("tool_result") => {
                    let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str()) else {
                        continue;
                    };
                    let Some(title) = titles_by_call.get(id) else {
                        continue;
                    };
                    for url in pull_request_urls(&result_text(block)) {
                        titles_by_url.insert(url, title.clone());
                    }
                }
                _ => {}
            }
        }
    }

    let mut seen = HashMap::new();
    for link in links {
        // A session re-records the same link on later turns; the first sighting
        // is the one whose timestamp means anything.
        if seen.contains_key(&link.pr_url) {
            continue;
        }
        let title = titles_by_url
            .get(&link.pr_url)
            .cloned()
            // A pull request opened outside `gh pr create` — by hand, or by a
            // script — still deserves review. Naming it by its address beats
            // dropping it, and beats inventing a title for it.
            .unwrap_or_else(|| describe(&link.pr_url, link.pr_repository.as_deref()));
        seen.insert(
            link.pr_url.clone(),
            Deliverable {
                kind: "pull_request".to_owned(),
                title,
                url: link.pr_url.clone(),
                repo: link.pr_repository.clone(),
                session_id: link
                    .session_id
                    .as_deref()
                    .and_then(|s| Uuid::parse_str(s).ok()),
                at: link
                    .timestamp
                    .as_deref()
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&Utc)),
            },
        );
    }

    let mut found: Vec<Deliverable> = seen.into_values().collect();
    // Oldest first, so a backfill numbers versions in the order things happened.
    found.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.url.cmp(&b.url)));
    found
}

/// The value of `--title` in a shell command, honouring either quote.
///
/// Written by hand rather than with a shell parser because the input is one
/// known flag in commands this tool wrote itself, and a real parser would
/// still have to cope with the heredocs the `--body` beside it uses.
fn title_argument(command: &str) -> Option<String> {
    let rest = command.split("--title").nth(1)?.trim_start();
    let mut chars = rest.chars();
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        // Unquoted: a bare word, ending at whitespace.
        let word = rest.split_whitespace().next()?;
        return (!word.is_empty()).then(|| word.to_owned());
    }
    let mut title = String::new();
    let mut escaped = false;
    for ch in chars {
        if escaped {
            title.push(ch);
            escaped = false;
        } else if ch == '\\' && quote == '"' {
            escaped = true;
        } else if ch == quote {
            return (!title.is_empty()).then_some(title);
        } else {
            title.push(ch);
        }
    }
    None
}

/// The text of a tool result, whichever shape it arrived in.
fn result_text(block: &serde_json::Value) -> String {
    match block.get("content") {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Every GitHub pull request URL in a piece of text.
fn pull_request_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for start in text.match_indices("https://github.com/").map(|(i, _)| i) {
        let tail = &text[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\\' || c == ')')
            .unwrap_or(tail.len());
        let url = tail[..end].trim_end_matches(['.', ',']).to_owned();
        if url.contains("/pull/") {
            urls.push(url);
        }
    }
    urls
}

/// A name for a pull request whose title the transcript never recorded.
fn describe(url: &str, repo: Option<&str>) -> String {
    let number = url.rsplit('/').next().unwrap_or_default();
    match repo {
        Some(repo) => format!("{repo}#{number}"),
        None => format!("Pull request {number}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_use(id: &str, command: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "tool_use", "id": id, "name": "Bash",
                 "input": {"command": command}}
            ]}
        })
        .to_string()
    }

    fn tool_result(id: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": id, "content": text}
            ]}
        })
        .to_string()
    }

    fn pr_link(url: &str, repo: &str, at: &str) -> String {
        serde_json::json!({
            "type": "pr-link",
            "prUrl": url,
            "prRepository": repo,
            "sessionId": "d123a1ab-25d2-4fa2-825e-051b624561be",
            "timestamp": at,
        })
        .to_string()
    }

    #[test]
    fn a_pull_request_is_found_with_the_title_it_was_opened_under() {
        let found = scan_transcript([
            tool_use(
                "t1",
                "gh pr create --base main --title \"fix(sync): bound the wait\" --body x",
            ),
            tool_result("t1", "https://github.com/ferrosadb/ferrosa-memory/pull/234"),
            pr_link(
                "https://github.com/ferrosadb/ferrosa-memory/pull/234",
                "ferrosadb/ferrosa-memory",
                "2026-08-25T12:00:00.000Z",
            ),
        ]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "fix(sync): bound the wait");
        assert_eq!(found[0].kind, "pull_request");
        assert_eq!(found[0].repo.as_deref(), Some("ferrosadb/ferrosa-memory"));
        assert!(
            found[0].session_id.is_some(),
            "feedback needs somewhere to go"
        );
    }

    /// A session re-records the same link every later turn. Proposing one claim
    /// per sighting would put the same pull request in the queue 40 times.
    #[test]
    fn the_same_pull_request_is_reported_once() {
        let url = "https://github.com/ferrosadb/ferrosa/pull/320";
        let found = scan_transcript([
            pr_link(url, "ferrosadb/ferrosa", "2026-08-25T12:00:00.000Z"),
            pr_link(url, "ferrosadb/ferrosa", "2026-08-25T12:05:00.000Z"),
            pr_link(url, "ferrosadb/ferrosa", "2026-08-25T13:00:00.000Z"),
        ]);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].at.expect("a time").to_rfc3339(),
            "2026-08-25T12:00:00+00:00",
            "the first sighting is when it was opened"
        );
    }

    /// Several pull requests in one session must each keep their own title.
    /// Pairing by order would swap them whenever results interleave.
    #[test]
    fn titles_stay_with_their_own_pull_request() {
        let found = scan_transcript([
            tool_use("a", "gh pr create --title 'first thing'"),
            tool_use("b", "gh pr create --title 'second thing'"),
            tool_result("b", "https://github.com/o/r/pull/2"),
            tool_result("a", "https://github.com/o/r/pull/1"),
            pr_link(
                "https://github.com/o/r/pull/1",
                "o/r",
                "2026-08-25T12:00:00.000Z",
            ),
            pr_link(
                "https://github.com/o/r/pull/2",
                "o/r",
                "2026-08-25T12:01:00.000Z",
            ),
        ]);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].title, "first thing");
        assert_eq!(found[1].title, "second thing");
    }

    /// A pull request opened by hand still deserves review; it is named by its
    /// address rather than dropped or given an invented title.
    #[test]
    fn a_pull_request_with_no_recorded_title_is_named_by_its_address() {
        let found = scan_transcript([pr_link(
            "https://github.com/ferrosadb/forge/pull/7",
            "ferrosadb/forge",
            "2026-08-25T12:00:00.000Z",
        )]);
        assert_eq!(found[0].title, "ferrosadb/forge#7");
    }

    /// A session that produced nothing produces no claims. Inventing one would
    /// put a queue in front of a person that nothing put there.
    #[test]
    fn a_session_with_no_deliverables_yields_none() {
        let found = scan_transcript([
            tool_use("t1", "cargo test --workspace"),
            tool_result("t1", "test result: ok. 208 passed"),
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#
                .to_owned(),
        ]);
        assert!(found.is_empty());
    }

    /// A `gh pr create` that failed produced no pull request, so there is
    /// nothing to review — the title alone must not become a claim.
    #[test]
    fn a_failed_creation_is_not_a_deliverable() {
        let found = scan_transcript([
            tool_use("t1", "gh pr create --title \"never landed\""),
            tool_result(
                "t1",
                "pull request create failed: no commits between main and head",
            ),
        ]);
        assert!(found.is_empty());
    }

    /// Malformed lines are skipped rather than ending the scan: these files are
    /// appended to live, and the last line can be half-written.
    #[test]
    fn a_truncated_line_does_not_stop_the_scan() {
        let found = scan_transcript([
            "{\"type\":\"pr-link\",\"prUrl\":\"https://github.com/o/r/pu".to_owned(),
            pr_link(
                "https://github.com/o/r/pull/9",
                "o/r",
                "2026-08-25T12:00:00.000Z",
            ),
        ]);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_title_argument_survives_either_quote_and_an_escape() {
        assert_eq!(
            title_argument(r#"gh pr create --title "a \"quoted\" thing" --body x"#).as_deref(),
            Some(r#"a "quoted" thing"#)
        );
        assert_eq!(
            title_argument("gh pr create --title 'single quoted'").as_deref(),
            Some("single quoted")
        );
        assert_eq!(title_argument("gh pr create --body only").as_deref(), None);
    }

    /// The result of a `gh pr create` often carries more than the URL, and a
    /// comment URL is not a pull request.
    #[test]
    fn only_pull_request_urls_are_taken_from_a_result() {
        let urls = pull_request_urls(
            "Warning: 3 uncommitted changes\nhttps://github.com/o/r/pull/12\n\
             see also https://github.com/o/r/issues/4",
        );
        assert_eq!(urls, vec!["https://github.com/o/r/pull/12"]);
    }
}
