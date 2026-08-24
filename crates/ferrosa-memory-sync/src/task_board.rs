//! Module: The deferred-work board, read for the repositories agents work in.
//! Correctness: Correct when a task is attributed to a repository the same way
//! whichever field carries it, when work waiting on a person sorts above work
//! waiting on nobody, and when an unreachable board says so instead of looking
//! like an empty board.
//! Last revised: 2026-08-23
//! Last changed: One task can be read in full — body and comments — because a
//! title is not enough to decide anything by.
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

/// One task read in full, for the screen that shows a single one.
///
/// Separate from [`BoardTask`] and fetched on demand. The list carries titles
/// for a hundred-odd tasks; carrying every body with them would put the whole
/// board's prose through the data channel to render six rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardTaskDetail {
    pub task: BoardTask,
    pub body: String,
    pub assignee: Option<String>,
    pub result: Option<String>,
    pub summary: Option<String>,
    pub comments: Vec<BoardComment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardComment {
    pub author: String,
    pub body: String,
    pub created_at: i64,
}

/// One related task, and why it is related.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Related {
    /// In the operator's terms: "parent", "mentions QA-0009", "refers to this".
    pub reason: String,
    pub task: BoardTask,
}

/// Identifier-shaped tokens in a piece of text.
///
/// Two shapes, both of which are how work actually cross-references here:
/// `t_393bc64e` (a board id) and `QA-0009`, `MAAS-T-35`, `FG-003` (a ticket in
/// some other scheme, written into the prose because there is nowhere else to
/// put it).
///
/// Deliberately NOT a general word index. A "related" list built from shared
/// words is a list of everything, and the reason these tokens are worth
/// following is that a person chose to write one down.
pub fn identifiers_in(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for raw in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')) {
        let token = raw.trim_matches(|c: char| c == '-' || c == '_');
        if token.len() < 4 || token.len() > 40 {
            continue;
        }
        let board_id = token.starts_with("t_")
            && token.len() >= 8
            && token[2..].chars().all(|c| c.is_ascii_hexdigit());
        // LETTERS-DIGITS, with any number of dashed parts: QA-0009, MAAS-T-35.
        let ticket = token.contains('-')
            && token.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && token.chars().last().is_some_and(|c| c.is_ascii_digit())
            && token
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-');
        if (board_id || ticket) && !found.iter().any(|seen| seen == token) {
            found.push(token.to_owned());
        }
    }
    found
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
        let columns = column_index(&result);

        let mut matcher = RepoMatcher::new(working_dirs);
        let mut found: BTreeMap<String, BoardTask> = BTreeMap::new();
        for row in result.rows_or_empty() {
            let text = |name: &str| text_at(&row, &columns, name);
            let (id, status, title, block_reason, workspace_path, body) = (
                text("task_id"),
                text("status"),
                text("title"),
                text("block_reason"),
                text("workspace_path"),
                text("body"),
            );
            let priority =
                number_at(&row, &columns, "priority").and_then(|value| i32::try_from(value).ok());
            let updated_at = number_at(&row, &columns, "updated_at");
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

impl TaskBoard {
    /// Everything about one task, for the detail screen.
    ///
    /// `None` when the id is not on the board — a task completed or archived
    /// between the list and the tap. Reported as absent rather than as an
    /// error, because it is a normal race and not a fault.
    pub async fn detail(&self, task_id: &str) -> Result<Option<BoardTaskDetail>> {
        // Parameterised, unlike the list query: this value comes from a device.
        // Interpolating it would be CQL injection through the control channel.
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(
                format!(
                    "SELECT task_id, title, status, priority, block_reason, workspace_path, \
                     body, updated_at, assignee, result, summary FROM agent_memory.tasks \
                     WHERE tenant_id={TENANT_ID} AND task_id = ?"
                ),
                (task_id,),
            )
            .await
            .context("reading one task")?;
        let columns = column_index(&result);
        let Some(row) = result.rows_or_empty().into_iter().next() else {
            return Ok(None);
        };
        let text = |name: &str| text_at(&row, &columns, name);
        let Some(id) = text("task_id") else {
            return Ok(None);
        };
        let task = BoardTask {
            id,
            title: text("title").unwrap_or_default(),
            status: text("status").unwrap_or_else(|| "triage".to_owned()),
            priority: number_at(&row, &columns, "priority")
                .and_then(|value| i32::try_from(value).ok())
                .unwrap_or(0),
            block_reason: text("block_reason").filter(|reason| !reason.is_empty()),
            repo: repo_of(text("workspace_path").as_deref(), text("body").as_deref())
                .unwrap_or_default(),
            updated_at: number_at(&row, &columns, "updated_at").unwrap_or(0),
        };
        Ok(Some(BoardTaskDetail {
            body: text("body").unwrap_or_default(),
            assignee: text("assignee").filter(|value| !value.is_empty()),
            result: text("result").filter(|value| !value.is_empty()),
            summary: text("summary").filter(|value| !value.is_empty()),
            comments: self.comments(task_id).await.unwrap_or_default(),
            task,
        }))
    }

    /// Tasks whose id matches, exactly or by prefix.
    ///
    /// Two queries rather than one scan: an exact id is a primary-key lookup
    /// and answers instantly, which is the case that matters when someone
    /// pastes a full id. The prefix pass is a scan of the tenant partition,
    /// bounded by `limit`, and only runs when the exact lookup misses.
    ///
    /// Open work only. A completed task still answers by exact id — you looked
    /// it up on purpose — but a prefix should not fill with archived work.
    pub async fn find_by_id(&self, query: &str, limit: usize) -> Result<Vec<BoardTask>> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(found) = self.detail(&query).await? {
            return Ok(vec![found.task]);
        }

        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(
                format!(
                    "SELECT task_id, title, status, priority, block_reason, workspace_path, \
                     body, updated_at FROM agent_memory.tasks WHERE tenant_id={TENANT_ID}"
                ),
                (),
            )
            .await
            .context("searching the task board")?;
        let columns = column_index(&result);
        let mut found = Vec::new();
        for row in result.rows_or_empty() {
            let text = |name: &str| text_at(&row, &columns, name);
            let (Some(id), Some(status)) = (text("task_id"), text("status")) else {
                continue;
            };
            if !id.to_lowercase().contains(&query) {
                continue;
            }
            if !OPEN_STATUSES.contains(&status.as_str()) {
                continue;
            }
            found.push(BoardTask {
                repo: repo_of(text("workspace_path").as_deref(), text("body").as_deref())
                    .unwrap_or_default(),
                id,
                title: text("title").unwrap_or_default(),
                status,
                priority: number_at(&row, &columns, "priority")
                    .and_then(|value| i32::try_from(value).ok())
                    .unwrap_or(0),
                block_reason: text("block_reason").filter(|reason| !reason.is_empty()),
                updated_at: number_at(&row, &columns, "updated_at").unwrap_or(0),
            });
        }
        // Ranked like everything else, then cut. Cutting first would drop a
        // blocked task in favour of an arbitrary one.
        let mut ranked = ranked(found);
        ranked.truncate(limit);
        Ok(ranked)
    }

    /// Mark a task finished.
    ///
    /// Parameterised throughout: the id comes from a device, and the note is
    /// free text a person typed. Interpolating either into CQL would be
    /// injection through the control channel.
    ///
    /// Writes `status` and `result` and nothing else. Not a general edit: this
    /// is one operator saying one thing about one task, and the narrower the
    /// write the less there is to get wrong from a phone.
    pub async fn complete(&self, task_id: &str, note: Option<&str>) -> Result<()> {
        let now = now_millis();
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                format!(
                    "UPDATE agent_memory.tasks SET status = ?, result = ?, updated_at = ? \
                     WHERE tenant_id={TENANT_ID} AND task_id = ?"
                ),
                (
                    "complete",
                    note.unwrap_or("Marked complete from the control room."),
                    now,
                    task_id,
                ),
            )
            .await
            .context("marking the task complete")?;
        Ok(())
    }

    /// Record that an agent has been sent to work on a task.
    ///
    /// Status and assignee together, because either alone is a half-truth: an
    /// assignee on a `triage` task does not read as "being worked on", and
    /// `in_progress` with nobody named does not say who to ask.
    pub async fn assign(&self, task_id: &str, assignee: &str) -> Result<()> {
        let now = now_millis();
        #[allow(deprecated)]
        self.session
            .query_unpaged(
                format!(
                    "UPDATE agent_memory.tasks SET status = ?, assignee = ?, updated_at = ? \
                     WHERE tenant_id={TENANT_ID} AND task_id = ?"
                ),
                ("in_progress", assignee, now, task_id),
            )
            .await
            .context("assigning the task")?;
        Ok(())
    }

    /// Other tasks that mention a token in their title, body or block reason.
    ///
    /// The board has explicit links, and it also has the way people actually
    /// cross-reference: writing the id in the prose. Both are real
    /// relationships and only one of them is in `task_links`, so a "related"
    /// list built from links alone would miss most of what relates.
    ///
    /// Case-insensitive, because ids get typed in whatever case is remembered.
    pub async fn mentions_of(&self, token: &str, limit: usize) -> Result<Vec<BoardTask>> {
        let needle = token.trim().to_lowercase();
        if needle.len() < 4 {
            // A short token matches everything and means nothing. Refusing
            // beats a page of noise that looks like an answer.
            return Ok(Vec::new());
        }
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(
                format!(
                    "SELECT task_id, title, status, priority, block_reason, workspace_path, \
                     body, updated_at FROM agent_memory.tasks WHERE tenant_id={TENANT_ID}"
                ),
                (),
            )
            .await
            .context("scanning the board for mentions")?;
        let columns = column_index(&result);
        let mut found = Vec::new();
        for row in result.rows_or_empty() {
            let text = |name: &str| text_at(&row, &columns, name);
            let Some(id) = text("task_id") else { continue };
            // Not the task itself.
            if id.to_lowercase() == needle {
                continue;
            }
            let haystack = format!(
                "{} {} {}",
                text("title").unwrap_or_default(),
                text("body").unwrap_or_default(),
                text("block_reason").unwrap_or_default()
            )
            .to_lowercase();
            if !haystack.contains(&needle) {
                continue;
            }
            found.push(BoardTask {
                repo: repo_of(text("workspace_path").as_deref(), text("body").as_deref())
                    .unwrap_or_default(),
                id,
                title: text("title").unwrap_or_default(),
                status: text("status").unwrap_or_else(|| "triage".to_owned()),
                priority: number_at(&row, &columns, "priority")
                    .and_then(|value| i32::try_from(value).ok())
                    .unwrap_or(0),
                block_reason: text("block_reason").filter(|reason| !reason.is_empty()),
                updated_at: number_at(&row, &columns, "updated_at").unwrap_or(0),
            });
        }
        let mut ranked = ranked(found);
        ranked.truncate(limit);
        Ok(ranked)
    }

    /// Explicitly linked tasks, in both directions.
    ///
    /// Both, because from a task on screen the operator wants "what is this
    /// attached to", not "what did this attach itself to". A parent knows its
    /// children; a child only knows its parents if you ask the other way.
    pub async fn links_of(&self, task_id: &str) -> Result<Vec<(String, BoardTask)>> {
        let mut related: Vec<(String, String)> = Vec::new();

        #[allow(deprecated)]
        let outgoing = self
            .session
            .query_unpaged(
                format!(
                    "SELECT link_type, dst_task_id FROM agent_memory.task_links \
                     WHERE tenant_id={TENANT_ID} AND src_task_id = ?"
                ),
                (task_id,),
            )
            .await
            .context("reading task links")?;
        let columns = column_index(&outgoing);
        for row in outgoing.rows_or_empty() {
            if let (Some(kind), Some(dst)) = (
                text_at(&row, &columns, "link_type"),
                text_at(&row, &columns, "dst_task_id"),
            ) {
                related.push((kind, dst));
            }
        }

        // The reverse needs a scan: `task_links` is keyed by source, so "what
        // points at me" is not a lookup. Bounded by the board, which is small.
        #[allow(deprecated)]
        let incoming = self
            .session
            .query_unpaged(
                format!(
                    "SELECT src_task_id, link_type, dst_task_id FROM agent_memory.task_links \
                     WHERE tenant_id={TENANT_ID}"
                ),
                (),
            )
            .await
            .context("reading reverse task links")?;
        let columns = column_index(&incoming);
        for row in incoming.rows_or_empty() {
            let (Some(src), Some(kind), Some(dst)) = (
                text_at(&row, &columns, "src_task_id"),
                text_at(&row, &columns, "link_type"),
                text_at(&row, &columns, "dst_task_id"),
            ) else {
                continue;
            };
            if dst == task_id {
                related.push((format!("{kind} of"), src));
            }
        }

        let mut resolved = Vec::new();
        for (kind, id) in related {
            if let Ok(Some(detail)) = self.detail(&id).await {
                resolved.push((kind, detail.task));
            }
        }
        Ok(resolved)
    }

    /// Everything related to a task: what links to it, and what talks about it.
    ///
    /// Explicit links first, because someone stated them. Then tasks that
    /// mention this task's id, then tasks that mention the same identifiers it
    /// does — which is how the cross-referencing here is actually done. On the
    /// live board the first two are frequently empty and the third is not: one
    /// blocked task named QA-0009 in its title and ten other tasks turned out
    /// to be about the same thing, several of them duplicates of each other.
    ///
    /// Bounded, and deduplicated by task, so a task related three ways appears
    /// once with the strongest reason.
    pub async fn related_to(&self, task_id: &str, limit: usize) -> Result<Vec<Related>> {
        let Some(detail) = self.detail(task_id).await? else {
            return Ok(Vec::new());
        };

        let mut out: Vec<Related> = Vec::new();
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::from([task_id.to_owned()]);

        for (kind, task) in self.links_of(task_id).await.unwrap_or_default() {
            if seen.insert(task.id.clone()) {
                out.push(Related { reason: kind, task });
            }
        }

        for task in self.mentions_of(task_id, limit).await.unwrap_or_default() {
            if seen.insert(task.id.clone()) {
                out.push(Related {
                    reason: "refers to this".to_owned(),
                    task,
                });
            }
        }

        // Identifiers this task names, from its title and its block reason —
        // not its whole body. A body quotes ids in passing; a title and a
        // blocking reason name the thing the task is ABOUT.
        let named = identifiers_in(&format!(
            "{} {}",
            detail.task.title,
            detail.task.block_reason.clone().unwrap_or_default()
        ));
        for token in named {
            if out.len() >= limit {
                break;
            }
            for task in self.mentions_of(&token, limit).await.unwrap_or_default() {
                if out.len() >= limit {
                    break;
                }
                if seen.insert(task.id.clone()) {
                    out.push(Related {
                        reason: format!("mentions {token}"),
                        task,
                    });
                }
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// A task's comments, oldest first.
    ///
    /// Failure here does NOT fail the detail: the body is most of the value,
    /// and losing the whole screen because the discussion could not be read
    /// would be a worse trade than showing it without them.
    async fn comments(&self, task_id: &str) -> Result<Vec<BoardComment>> {
        #[allow(deprecated)]
        let result = self
            .session
            .query_unpaged(
                format!(
                    "SELECT author, body, created_at FROM agent_memory.task_comments \
                     WHERE tenant_id={TENANT_ID} AND task_id = ?"
                ),
                (task_id,),
            )
            .await
            .context("reading task comments")?;
        let columns = column_index(&result);
        Ok(result
            .rows_or_empty()
            .into_iter()
            .map(|row| BoardComment {
                author: text_at(&row, &columns, "author").unwrap_or_else(|| "unknown".to_owned()),
                body: text_at(&row, &columns, "body").unwrap_or_default(),
                created_at: number_at(&row, &columns, "created_at").unwrap_or(0),
            })
            .collect())
    }
}

/// Column name to position, so rows can be read by name.
fn column_index(result: &scylla::LegacyQueryResult) -> BTreeMap<String, usize> {
    result
        .col_specs()
        .iter()
        .enumerate()
        .map(|(index, spec)| (spec.name().to_owned(), index))
        .collect()
}

fn text_at(
    row: &scylla::frame::response::result::Row,
    columns: &BTreeMap<String, usize>,
    name: &str,
) -> Option<String> {
    match row.columns.get(*columns.get(name)?)? {
        Some(CqlValue::Text(value)) | Some(CqlValue::Ascii(value)) => Some(value.clone()),
        _ => None,
    }
}

fn number_at(
    row: &scylla::frame::response::result::Row,
    columns: &BTreeMap<String, usize>,
    name: &str,
) -> Option<i64> {
    match row.columns.get(*columns.get(name)?)? {
        Some(CqlValue::Int(value)) => Some(i64::from(*value)),
        Some(CqlValue::BigInt(value)) => Some(*value),
        _ => None,
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

/// Milliseconds since the epoch, as the board stores timestamps.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod identifier_tests {
    use super::identifiers_in;

    /// The two shapes work actually cross-references by.
    #[test]
    fn board_ids_and_ticket_ids_are_found() {
        let found = identifiers_in("Decide QA-0009 before t_393bc64e, see MAAS-T-35");
        assert!(found.contains(&"QA-0009".to_owned()));
        assert!(found.contains(&"t_393bc64e".to_owned()));
        assert!(found.contains(&"MAAS-T-35".to_owned()));
    }

    /// Ordinary prose is not an identifier. A related list built from words is
    /// a list of everything.
    #[test]
    fn prose_is_not_an_identifier() {
        let found = identifiers_in(
            "Fix the entity_type example in the onboarding docs, it is wrong and \
             blocks the release",
        );
        assert!(found.is_empty(), "matched prose: {found:?}");
    }

    /// A hyphenated lowercase word is not a ticket. `well-known` and `QA-0009`
    /// differ by case and by ending in a digit, and both tests matter.
    #[test]
    fn hyphenated_words_are_not_tickets() {
        assert!(identifiers_in("a well-known follow-up").is_empty());
        assert!(identifiers_in("ferrosa-memory").is_empty());
    }

    /// A version number is not a ticket either.
    #[test]
    fn versions_are_not_tickets() {
        assert!(identifiers_in("upgrade to 1-2-3").is_empty());
    }

    /// Each identifier once, however often it is written.
    #[test]
    fn repeats_collapse() {
        let found = identifiers_in("QA-0009 and QA-0009 again, QA-0009");
        assert_eq!(found, vec!["QA-0009".to_owned()]);
    }

    /// A truncated id is not followed. Four characters of hex matches half the
    /// board.
    #[test]
    fn a_short_token_is_not_an_id() {
        assert!(identifiers_in("t_39").is_empty());
    }
}
