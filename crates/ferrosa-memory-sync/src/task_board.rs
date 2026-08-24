//! Module: The deferred-work board, read for the repositories agents work in.
//! Correctness: Correct when a task is attributed to a repository the same way
//! whichever field carries it, when work waiting on a person sorts above work
//! waiting on nobody, and when an unreachable board says so instead of looking
//! like an empty board.
//! Last revised: 2026-08-23
//! Last changed: New.
//!
//! # Why the machine reads it and not the phone
//!
//! The board is a CQL table on the same cluster this listener already talks to.
//! The phone has no route to it, no credential for it, and no business holding
//! one — it is on a network away from the machine, which is the whole point of
//! the product. So the machine reads the board and sends what it found down the
//! control channel it already has.
//!
//! # Attribution is two-headed, and that is not this module's fault
//!
//! A task names its repository in `workspace_path`, or as a `repo=<path>` line
//! at the top of its body, depending on which tool captured it. Both are in the
//! live board today. Reading only one silently loses half the work, so this
//! reads both and says so out loud rather than picking a winner.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use scylla::frame::response::result::CqlValue;
use scylla::{LegacySession, SessionBuilder};

/// The single-user tenant forge writes under.
///
/// Hard-coded there too, in `forge/crates/tasks/src/store.rs`. Named here
/// rather than passed in so the coupling is visible: if forge ever becomes
/// multi-tenant this constant is the thing that has to change.
const TENANT_ID: &str = "00000000-0000-0000-0000-000000000001";

/// Statuses that mean the work is not finished.
///
/// `complete` and `archived` are excluded. Everything else — including
/// `triage`, the unreviewed deferred inbox — is outstanding.
const OPEN_STATUSES: [&str; 4] = ["triage", "ready", "in_progress", "blocked"];

/// One piece of outstanding work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardTask {
    pub id: String,
    pub title: String,
    pub status: String,
    pub priority: i32,
    /// Why it is blocked, when it is. The reason is the whole value of a
    /// blocked task: "blocked" alone tells the operator to go and look
    /// somewhere else, which is what putting it on screen was meant to avoid.
    pub block_reason: Option<String>,
    /// The repository it belongs to, however it was recorded.
    pub repo: String,
    pub updated_at: i64,
}

impl BoardTask {
    /// Whether this is waiting on a PERSON rather than on other work.
    ///
    /// A person-blocked task is the only kind the operator can clear by reading
    /// their phone, so it sorts first. Distinguished by the reason, because
    /// `status = blocked` covers both "waiting for a decision" and "waiting for
    /// another task", and only the first is theirs to unblock.
    pub fn waits_on_a_person(&self) -> bool {
        if self.status != "blocked" {
            return false;
        }
        let Some(reason) = &self.block_reason else {
            // Blocked with no reason recorded. Treated as needing a person,
            // because somebody has to find out what it is waiting for — and
            // that somebody is a person.
            return true;
        };
        let reason = reason.to_lowercase();
        [
            "decision",
            "review",
            "approval",
            "waiting on ben",
            "human",
            "ask ",
            "question",
            "confirm",
            "sign-off",
            "signoff",
        ]
        .iter()
        .any(|marker| reason.contains(marker))
    }
}

/// Sort as the operator reads: what needs them, then what matters most.
///
/// Ties break on recency and then on id, so the same board always produces the
/// same order. A list that reshuffles between two reads of unchanged data is a
/// list nobody can learn.
pub fn ranked(mut tasks: Vec<BoardTask>) -> Vec<BoardTask> {
    tasks.sort_by(|left, right| {
        right
            .waits_on_a_person()
            .cmp(&left.waits_on_a_person())
            .then(right.priority.cmp(&left.priority))
            .then(right.updated_at.cmp(&left.updated_at))
            .then(left.id.cmp(&right.id))
    });
    tasks
}

/// The repository a task belongs to, from whichever field carries it.
///
/// `workspace_path` when the task was created through the API, and a leading
/// `repo=<path>` line when it came from the `/defer` CLI or the capture hook.
/// Both shapes are in the live board.
pub fn repo_of(workspace_path: Option<&str>, body: Option<&str>) -> Option<String> {
    if let Some(path) = workspace_path
        && !path.is_empty()
    {
        return Some(path.trim_end_matches('/').to_owned());
    }
    let first = body?.lines().next()?.trim();
    first
        .strip_prefix("repo=")
        .map(|path| path.trim().trim_end_matches('/').to_owned())
        .filter(|path| !path.is_empty())
}

/// Which repository a path is in, as git answers it.
///
/// The common git directory, NOT the working-tree root: two worktrees of one
/// repository share it, so an agent in `.wt-streaming-seam` is recognised as
/// working on `ferrosa-memory` rather than on something unrelated.
///
/// `None` when the path is not in a repository, or does not exist.
pub fn repo_identity(path: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            path,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let found = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if found.is_empty() {
        return None;
    }
    // Resolved, so a symlinked checkout and its target are one repository.
    Some(
        std::fs::canonicalize(&found)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or(found),
    )
}

/// Whether a task's repository is the one an agent works in.
///
/// Prefer [`RepoMatcher`] over calling this in a loop: each call may shell out
/// to git twice, and the board holds thousands of rows. Reading the board this
/// way took 117 seconds.
///
/// Compared by GIT REPOSITORY, not by path prefix. Path prefixes cannot tell
/// nested repositories apart, and this tree is full of them: `ferrosa-suite`
/// is a repository whose directory contains ~24 sibling repositories. The
/// prefix rule attributed every task filed against the suite to every agent
/// working under it — asking for ferrosa-mobile's work returned 1037 tasks,
/// most of the board.
///
/// Falls back to exact string equality when git cannot answer: a path that
/// does not exist, or is not a checkout, matches only itself. Guessing wider
/// than that is how the prefix rule went wrong.
pub fn belongs_to(task_repo: &str, working_dir: &str) -> bool {
    if task_repo.is_empty() || working_dir.is_empty() {
        return false;
    }
    let task_repo = task_repo.trim_end_matches('/');
    let working_dir = working_dir.trim_end_matches('/');
    if task_repo == working_dir {
        return true;
    }
    match (repo_identity(task_repo), repo_identity(working_dir)) {
        (Some(left), Some(right)) => left == right,
        // One of them is not a checkout. Only the exact match above applies.
        _ => false,
    }
}

/// Resolves paths to repositories once and remembers the answers.
///
/// `repo_identity` runs `git`, which is a process spawn. Calling it per row
/// against a few thousand tasks took 117 seconds to answer one question —
/// correct, and far too slow to sit in front of a phone waiting for its home
/// screen.
pub struct RepoMatcher {
    /// The agents' directories, with the repository each resolves to.
    wanted: Vec<(String, Option<String>)>,
    seen: std::collections::HashMap<String, Option<String>>,
}

impl RepoMatcher {
    pub fn new(working_dirs: &[String]) -> Self {
        let wanted = working_dirs
            .iter()
            .map(|dir| {
                let dir = dir.trim_end_matches('/').to_owned();
                let identity = repo_identity(&dir);
                (dir, identity)
            })
            .collect();
        Self {
            wanted,
            seen: std::collections::HashMap::new(),
        }
    }

    pub fn matches(&mut self, task_repo: &str) -> bool {
        let task_repo = task_repo.trim_end_matches('/');
        if task_repo.is_empty() {
            return false;
        }
        // The cheap answer first: an exact path match needs no git at all, and
        // it is the common case for tasks filed through the API.
        if self.wanted.iter().any(|(dir, _)| dir == task_repo) {
            return true;
        }
        // Only resolve if some agent's directory HAS a repository to compare
        // against. Otherwise this spawns git for every row on the board to
        // compare each answer against None.
        if !self.wanted.iter().any(|(_, identity)| identity.is_some()) {
            return false;
        }
        let identity = self
            .seen
            .entry(task_repo.to_owned())
            .or_insert_with(|| repo_identity(task_repo));
        let Some(identity) = identity else {
            return false;
        };
        self.wanted
            .iter()
            .any(|(_, wanted)| wanted.as_deref() == Some(identity.as_str()))
    }
}

/// Reads the board over CQL.
pub struct TaskBoard {
    session: LegacySession,
}

impl TaskBoard {
    pub async fn connect(contact_points: &[String]) -> Result<Self> {
        if contact_points.is_empty() {
            anyhow::bail!("no contact points for the task board");
        }
        // Bounded, because this runs while a control session is being set up.
        // An unreachable board must fail in seconds and say so, not hold the
        // listener while a phone waits.
        let session = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            SessionBuilder::new()
                .known_nodes(contact_points)
                .connection_timeout(std::time::Duration::from_secs(5))
                .build_legacy(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("task board did not answer within 10s"))?
        .context("connecting to the task board")?;
        Ok(Self { session })
    }

    /// Open work for these repositories, ranked.
    ///
    /// One query for the whole board rather than one per repository: the board
    /// is small, the partition is a single tenant, and CQL cannot filter on a
    /// prefix anyway. Filtering happens here, where both attribution shapes and
    /// the prefix rule live together.
    pub async fn open_work(&self, working_dirs: &[String]) -> Result<Vec<BoardTask>> {
        if working_dirs.is_empty() {
            return Ok(Vec::new());
        }
        let query = format!(
            "SELECT task_id, title, status, priority, block_reason, workspace_path, \
             body, updated_at FROM agent_memory.tasks WHERE tenant_id={TENANT_ID}"
        );
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(query, ())
            .await
            .context("reading the task board")?;
        let columns: BTreeMap<String, usize> = result
            .col_specs()
            .iter()
            .enumerate()
            .map(|(index, spec)| (spec.name().to_owned(), index))
            .collect();

        let mut matcher = RepoMatcher::new(working_dirs);
        let mut found: BTreeMap<String, BoardTask> = BTreeMap::new();
        for row in result.rows_or_empty() {
            let text = |name: &str| -> Option<String> {
                match row.columns.get(*columns.get(name)?)? {
                    Some(CqlValue::Text(value)) => Some(value.clone()),
                    Some(CqlValue::Ascii(value)) => Some(value.clone()),
                    _ => None,
                }
            };
            let number = |name: &str| -> Option<i64> {
                match row.columns.get(*columns.get(name)?)? {
                    Some(CqlValue::Int(value)) => Some(i64::from(*value)),
                    Some(CqlValue::BigInt(value)) => Some(*value),
                    _ => None,
                }
            };
            let (id, status, title, block_reason, workspace_path, body) = (
                text("task_id"),
                text("status"),
                text("title"),
                text("block_reason"),
                text("workspace_path"),
                text("body"),
            );
            let priority = number("priority").and_then(|value| i32::try_from(value).ok());
            let updated_at = number("updated_at");
            let (Some(id), Some(status)) = (id, status) else {
                continue;
            };
            if !OPEN_STATUSES.contains(&status.as_str()) {
                continue;
            }
            let Some(repo) = repo_of(workspace_path.as_deref(), body.as_deref()) else {
                continue;
            };
            if !matcher.matches(&repo) {
                continue;
            }
            found.insert(
                id.clone(),
                BoardTask {
                    id,
                    title: title.unwrap_or_default(),
                    status,
                    priority: priority.unwrap_or(0),
                    block_reason: block_reason.filter(|reason| !reason.is_empty()),
                    repo,
                    updated_at: updated_at.unwrap_or(0),
                },
            );
        }
        Ok(ranked(found.into_values().collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: &str, priority: i32, reason: Option<&str>) -> BoardTask {
        BoardTask {
            id: id.to_owned(),
            title: id.to_owned(),
            status: status.to_owned(),
            priority,
            block_reason: reason.map(str::to_owned),
            repo: "/repo".to_owned(),
            updated_at: 0,
        }
    }

    /// Both attribution shapes are in the live board, so both must resolve.
    #[test]
    fn a_repo_is_found_in_either_field() {
        assert_eq!(
            repo_of(Some("/src/ferrosa"), None),
            Some("/src/ferrosa".to_owned())
        );
        assert_eq!(
            repo_of(None, Some("repo=/src/ferrosa\n\nthe body")),
            Some("/src/ferrosa".to_owned())
        );
        assert_eq!(
            repo_of(Some(""), Some("repo=/src/ferrosa")),
            Some("/src/ferrosa".to_owned()),
            "an empty workspace_path must fall through to the body"
        );
    }

    /// `repo=` is only a marker on the FIRST line. A task whose prose happens
    /// to mention it later is not thereby attributed.
    #[test]
    fn repo_is_read_from_the_first_line_only() {
        assert_eq!(repo_of(None, Some("some prose\nrepo=/src/other")), None);
    }

    /// The same path is the same repository, without asking git.
    #[test]
    fn a_path_belongs_to_itself() {
        assert!(belongs_to("/src/ferrosa", "/src/ferrosa"));
        assert!(belongs_to("/src/ferrosa/", "/src/ferrosa"));
    }

    /// A CONTAINING directory is not the same repository.
    ///
    /// The case the prefix rule got wrong. `ferrosa-suite` is a repository
    /// whose directory holds ~24 other repositories, so "the task's path is a
    /// prefix of the agent's" attributed the whole suite board to every agent
    /// under it — 1037 tasks for ferrosa-mobile.
    ///
    /// Neither path exists, so this also pins the no-git fallback: unknown
    /// paths match only themselves.
    #[test]
    fn a_parent_directory_is_not_the_same_repository() {
        assert!(!belongs_to("/nope/suite", "/nope/suite/mobile"));
        assert!(!belongs_to("/nope/suite/mobile", "/nope/suite"));
    }

    /// The cache must not change the answer, only the cost.
    #[test]
    fn the_matcher_agrees_with_the_single_comparison() {
        let dirs = vec!["/nope/suite/mobile".to_owned()];
        let mut matcher = RepoMatcher::new(&dirs);
        for candidate in ["/nope/suite/mobile", "/nope/suite", "/nope/other", ""] {
            assert_eq!(
                matcher.matches(candidate),
                belongs_to(candidate, &dirs[0]),
                "matcher and belongs_to disagreed about {candidate}"
            );
        }
    }

    /// Asking twice gives the same answer — the cache is a cache, not a
    /// one-shot.
    #[test]
    fn the_matcher_is_stable_across_repeated_questions() {
        let mut matcher = RepoMatcher::new(&["/nope/suite/mobile".to_owned()]);
        assert!(matcher.matches("/nope/suite/mobile"));
        assert!(matcher.matches("/nope/suite/mobile"));
        assert!(!matcher.matches("/nope/suite"));
        assert!(!matcher.matches("/nope/suite"));
    }

    /// A neighbour sharing a prefix is not the same repository either.
    #[test]
    fn a_sibling_with_a_shared_prefix_does_not_belong() {
        assert!(!belongs_to("/nope/ferrosa", "/nope/ferrosa-memory"));
        assert!(!belongs_to("/nope/ferrosa-memory", "/nope/ferrosa"));
    }

    #[test]
    fn nothing_belongs_to_an_empty_directory() {
        assert!(!belongs_to("/src/ferrosa", ""));
        assert!(!belongs_to("", "/src/ferrosa"));
    }

    /// The ordering the operator asked for: what needs a person, then priority.
    #[test]
    fn work_waiting_on_a_person_sorts_above_higher_priority_work() {
        let order = ranked(vec![
            task("urgent", "ready", 90, None),
            task(
                "asks",
                "blocked",
                10,
                Some("waiting on a decision from Ben"),
            ),
        ]);
        assert_eq!(order.first().map(|t| t.id.as_str()), Some("asks"));
    }

    /// Blocked on another TASK is not blocked on a person — nobody can clear it
    /// by reading their phone, so it must not sit at the top pretending they
    /// can.
    #[test]
    fn work_blocked_on_other_work_does_not_claim_a_persons_attention() {
        let blocked_on_work = task("waits", "blocked", 50, Some("blocked by t_abc1234"));
        assert!(!blocked_on_work.waits_on_a_person());
    }

    /// Blocked with no reason still needs a person: someone has to find out
    /// what it is waiting for.
    #[test]
    fn blocked_with_no_reason_needs_a_person() {
        assert!(task("mystery", "blocked", 50, None).waits_on_a_person());
    }

    /// Same board, same order. A list that reshuffles between two reads of
    /// unchanged data cannot be learned by position.
    #[test]
    fn the_order_is_stable_for_equal_tasks() {
        let tasks = vec![
            task("b", "ready", 50, None),
            task("a", "ready", 50, None),
            task("c", "ready", 50, None),
        ];
        let first: Vec<String> = ranked(tasks.clone()).into_iter().map(|t| t.id).collect();
        let again: Vec<String> = ranked(tasks).into_iter().map(|t| t.id).collect();
        assert_eq!(first, again);
        assert_eq!(first, vec!["a", "b", "c"]);
    }

    /// Priority orders the rest, highest first.
    #[test]
    fn higher_priority_comes_first() {
        let order = ranked(vec![
            task("low", "ready", 10, None),
            task("high", "ready", 80, None),
        ]);
        assert_eq!(
            order.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["high", "low"]
        );
    }
}
