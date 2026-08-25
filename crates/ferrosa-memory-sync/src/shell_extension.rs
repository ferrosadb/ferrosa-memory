//! Module: The wire surface for configured sessions.
//! Correctness: Correct when a device cannot run anything without a grant,
//! when a config survives a restart of the machine, and when output reaches
//! the device as it is produced rather than when the command finishes.
//! Last revised: 2026-08-25
//! Last changed: Adds bounded tenant-aware knowledge-tier frames.
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
    configs: Arc<Mutex<Vec<SessionConfig>>>,
    /// Where configs are written, so they survive a restart.
    store_path: PathBuf,
    runtime: Arc<SessionRuntime>,
    /// Per session: whether input is granted, and what is open.
    ///
    /// The grant lives HERE rather than being implied by an open session, so
    /// it can be withdrawn while a session is running. A model where the grant
    /// lasted as long as the session would have to be unpicked when real
    /// capability grants land, because an owner revoking access mid-command
    /// must stop it.
    /// Shared, because the status pump needs to know what this control
    /// session is holding open — an ephemeral session exists nowhere else.
    sessions: Arc<Mutex<HashMap<uuid::Uuid, ShellState>>>,
    /// Where the deferred-work board lives.
    ///
    /// Connected on FIRST USE rather than at startup: the board is a
    /// convenience, and a cluster that is down must not stop an operator
    /// reaching their agents. The failure then lands on the one screen that
    /// asked for it, with a reason.
    board_contact_points: Vec<String>,
    /// Which tenant's memory to read. `None` means nobody said, and the
    /// memory frames say so rather than reading someone else's tenant and
    /// reporting its emptiness as this one's.
    memory_tenant: Option<uuid::Uuid>,
    board: tokio::sync::OnceCell<Option<Arc<crate::task_board::TaskBoard>>>,
    /// The memory tiers, on the same terms as the board: connected on first
    /// use, and a cluster that is down costs one screen rather than the
    /// session.
    memory: tokio::sync::OnceCell<Option<Arc<crate::memory_view::MemoryView>>>,
}

#[derive(Default)]
struct ShellState {
    granted: bool,
    open: Option<Arc<RunningSession>>,
    /// Pumps output to the device until the session ends.
    pump: Option<tokio::task::JoinHandle<()>>,
    /// Ticks the roster's status frames while the control session lives.
    status_pump: Option<tokio::task::JoinHandle<()>>,
}

impl ShellExtension {
    pub fn new(
        workspace: impl Into<PathBuf>,
        store_path: impl Into<PathBuf>,
        board_contact_points: Vec<String>,
        memory_tenant: Option<uuid::Uuid>,
    ) -> Self {
        let store_path = store_path.into();
        let workspace = workspace.into();
        Self {
            configs: Arc::new(Mutex::new(load_configs(&store_path))),
            store_path,
            runtime: Arc::new(SessionRuntime::new(workspace)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            board_contact_points,
            memory_tenant,
            board: tokio::sync::OnceCell::new(),
            memory: tokio::sync::OnceCell::new(),
        }
    }

    /// The board, connected once. `None` if it could not be reached.
    async fn board(&self) -> Option<Arc<crate::task_board::TaskBoard>> {
        self.board
            .get_or_init(|| async {
                match crate::task_board::TaskBoard::connect(&self.board_contact_points).await {
                    Ok(board) => Some(Arc::new(board)),
                    Err(error) => {
                        // Loud, and not fatal. Agents keep working without it.
                        tracing::warn!(%error, "task board unavailable");
                        None
                    }
                }
            })
            .await
            .clone()
    }

    /// The memory tiers, connected once. `None` if they could not be reached.
    async fn memory(&self) -> Option<Arc<crate::memory_view::MemoryView>> {
        self.memory
            .get_or_init(|| async {
                let Some(tenant) = self.memory_tenant else {
                    tracing::warn!(
                        "memory tiers unavailable: no tenant configured. Pass \
                         --memory-tenant, or set server.tenant_id in the memory \
                         config, or FERROSA_MEMORY_TENANT_ID, to the tenant this \
                         machine's memory is stored under."
                    );
                    return None;
                };
                match crate::memory_view::MemoryView::connect(&self.board_contact_points, tenant)
                    .await
                {
                    Ok(view) => {
                        // Says which tenant, on the way in. An empty tier map
                        // has two explanations -- nothing seeded, or the wrong
                        // tenant -- and they are indistinguishable from the
                        // counts alone. This is the line that tells them apart
                        // without anyone having to query the cluster.
                        tracing::info!(%tenant, "memory tiers connected");
                        Some(Arc::new(view))
                    }
                    Err(error) => {
                        tracing::warn!(%error, %tenant, "memory tiers unavailable");
                        None
                    }
                }
            })
            .await
            .clone()
    }

    /// The DIKW map: four rows, always, plus what the plane does not know.
    ///
    /// Bounded — four counts and a handful of root names — so it is one frame.
    async fn memory_tiers_frame(&self) -> serde_json::Value {
        let Some(view) = self.memory().await else {
            // Two different failures. "Unreachable" sends an operator to the
            // cluster; an unset tenant is a configuration answer nobody gave,
            // and saying the wrong one costs a round of looking at a database
            // that was fine all along.
            let reason = if self.memory_tenant.is_none() {
                "no memory tenant is configured on this machine"
            } else {
                "the memory store could not be reached"
            };
            return serde_json::json!({
                "type": "shell_memory_tiers",
                "reachable": false,
                "reason": reason,
                "tiers": [],
            });
        };
        match view.map().await {
            Ok(map) => serde_json::json!({
                "type": "shell_memory_tiers",
                "reachable": true,
                "tiers": map.tiers.iter().map(|row| serde_json::json!({
                    "tier": row.tier.as_str(),
                    "count": row.count,
                    "roots": row.roots,
                })).collect::<Vec<_>>(),
                // Both numbers are the map's honesty. `sourced == 0` means
                // nothing records a source yet, which is a build step and not
                // an empty library; `unclassified` is the hole in the rules.
                "sourced": map.sourced,
                "unclassified": map.unclassified,
                "truncated": map.truncated,
                "sorts": crate::memory_view::AVAILABLE_SORTS,
            }),
            Err(error) => serde_json::json!({
                "type": "shell_memory_tiers",
                "reachable": false,
                "reason": bounded_reason(format_args!("{error:#}")),
                "tiers": [],
            }),
        }
    }

    /// One page of a tier's contents.
    ///
    /// Paged, and the page is small: this frame carries titles, and a tier can
    /// hold tens of thousands of them. The task list taught this the expensive
    /// way at 194 KiB.
    async fn memory_items_frames(
        &self,
        tier: &str,
        query: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Vec<serde_json::Value> {
        let Some(parsed) = ferrosa_memory_core::tiers::Tier::parse(tier) else {
            return vec![serde_json::json!({
                "type": "shell_memory_items",
                "tier": tier,
                "reachable": false,
                "reason": bounded_reason(format_args!("{} is not a tier this machine knows", bounded_reason(tier))),
                "items": [],
            })];
        };
        let Some(view) = self.memory().await else {
            return vec![serde_json::json!({
                "type": "shell_memory_items",
                "tier": tier,
                "reachable": false,
                "reason": "the memory store could not be reached",
                "items": [],
            })];
        };
        let limit = limit.clamp(1, MEMORY_PAGE_MAX);
        // A cursor the device sent back. Refused rather than ignored: falling
        // back to the first page would silently restart a list the operator
        // was halfway down, which reads as the list looping.
        let decoded = match cursor
            .map(crate::memory_view::TierCursor::decode)
            .transpose()
        {
            Ok(decoded) => decoded,
            Err(error) => {
                return vec![serde_json::json!({
                    "type": "shell_memory_items",
                    "tier": tier,
                    "reachable": false,
                    "reason": bounded_reason(format_args!("{error:#}")),
                    "items": [],
                })];
            }
        };
        let first_page = decoded.is_none();
        match view.items(parsed, query, decoded.as_ref(), limit).await {
            Ok(page) => {
                let rows: Vec<serde_json::Value> = page
                    .items
                    .iter()
                    .map(|item| {
                        // No per-item `tier`: every row in this frame is the
                        // tier named in the header. No `path`: the absolute
                        // path is the longest field by far and belongs on the
                        // detail, where one of them is being read rather than
                        // fifty being scrolled past.
                        serde_json::json!({
                            "id": item.entity_id,
                            "session": item.session_id,
                            "title": item.title,
                            "root": item.source_root,
                        })
                    })
                    .collect();
                // An empty page still sends one frame. Without it a search
                // that matches nothing looks identical to a search that never
                // answered, and the screen keeps its previous results.
                if rows.is_empty() {
                    return vec![serde_json::json!({
                        "type": "shell_memory_items",
                        "tier": tier, "reachable": true, "first": first_page,
                        "query": query,
                        "next_cursor": page.next_cursor.as_ref().map(|c| c.encode()),
                        "items": [],
                    })];
                }
                let frame_count = rows.len().div_ceil(MEMORY_ITEMS_PER_FRAME);
                rows.chunks(MEMORY_ITEMS_PER_FRAME)
                    .enumerate()
                    .map(|(index, chunk)| {
                        serde_json::json!({
                            "type": "shell_memory_items",
                            "tier": tier,
                            "reachable": true,
                            // Only the first frame of a first page replaces the
                            // list; the rest append. Without this every frame
                            // wipes the one before it and the tier shows its
                            // last three items.
                            "first": first_page && index == 0,
                            "query": query,
                            // Only the LAST frame of a page carries the
                            // cursor. On any earlier frame it would invite a
                            // device to request the next page while this one
                            // is still arriving, and skip the rest of it.
                            "next_cursor": (index + 1 == frame_count)
                                .then(|| page.next_cursor.as_ref().map(|c| c.encode()))
                                .flatten(),
                            "items": chunk,
                        })
                    })
                    .collect()
            }
            Err(error) => vec![serde_json::json!({
                "type": "shell_memory_items",
                "tier": tier,
                "reachable": false,
                "reason": bounded_reason(format_args!("{error:#}")),
                "items": [],
            })],
        }
    }

    /// One task in full, for the screen that shows a single one.
    ///
    /// Fetched on demand rather than carried with the list: the list is over a
    /// hundred titles, and shipping every body with them would put the board's
    /// whole prose through the data channel to render six rows.
    async fn task_detail_frames(&self, task_id: &str) -> Vec<serde_json::Value> {
        let Some(board) = self.board().await else {
            return vec![serde_json::json!({
                "type": "shell_task_detail",
                "task_id": task_id,
                "unavailable": "The task board could not be reached from this machine.",
            })];
        };
        match board.detail(task_id).await {
            // Gone between the list and the tap. A normal race, said plainly:
            // an empty detail screen would read as a task with no content.
            Ok(None) => vec![serde_json::json!({
                "type": "shell_task_detail",
                "task_id": task_id,
                "unavailable": "That task is no longer on the board.",
            })],
            Ok(Some(detail)) => {
                // The HEADER only. A task body is prose of no fixed length —
                // several kilobytes is normal — so it travels as text pages
                // like terminal output does, for exactly the same reason.
                let mut frames = vec![serde_json::json!({
                    "type": "shell_task_detail",
                    "task_id": task_id,
                    "title": detail.task.title,
                    "status": detail.task.status,
                    "priority": detail.task.priority,
                    "block_reason": detail.task.block_reason,
                    "needs_a_person": detail.task.waits_on_a_person(),
                    "repo": detail.task.repo,
                    "assignee": detail.assignee,
                    "summary": detail.summary,
                    // Which of this machine's agents could take it. Decided
                    // HERE because deciding it needs git — an agent working in
                    // a worktree is working on the repository, and only the
                    // machine can resolve that. Bounded, and the worst case is
                    // asserted with the other Bounded frames.
                    "agents": self.agents_for_repo(&detail.task.repo).await,
                })];
                // Related work, discovered while the detail is being read
                // rather than on a second request: the operator has already
                // asked "what is this", and "what else is about this" is the
                // same question one step out.
                if let Some(board) = self.board().await {
                    let related = board
                        .related_to(task_id, MAX_RELATED)
                        .await
                        .unwrap_or_default();
                    for page in related.chunks(TASKS_PER_FRAME) {
                        frames.push(serde_json::json!({
                            "type": "shell_task_related",
                            "task_id": task_id,
                            "related": page
                                .iter()
                                .map(|item| serde_json::json!({
                                    "reason": item.reason,
                                    "id": item.task.id,
                                    "title": item.task.title,
                                    "status": item.task.status,
                                    "needs_a_person": item.task.waits_on_a_person(),
                                }))
                                .collect::<Vec<_>>(),
                        }));
                    }
                }
                frames.extend(text_pages(task_id, "body", None, &detail.body));
                if let Some(result) = &detail.result {
                    frames.extend(text_pages(task_id, "result", None, result));
                }
                for comment in &detail.comments {
                    frames.extend(text_pages(
                        task_id,
                        "comment",
                        Some(&comment.author),
                        &comment.body,
                    ));
                }
                frames
            }
            Err(error) => vec![serde_json::json!({
                "type": "shell_task_detail",
                "task_id": task_id,
                "unavailable": bounded_reason(format_args!("The task board refused the read: {error}")),
            })],
        }
    }

    /// Agents whose working directory is in the task's repository.
    ///
    /// Capped, because this rides on a Bounded frame. A machine with more than
    /// a handful of agents for one repository is not a case that exists yet,
    /// and if it starts to, the frame-size test is what will say so.
    async fn agents_for_repo(&self, repo: &str) -> Vec<serde_json::Value> {
        if repo.is_empty() {
            return Vec::new();
        }
        let configs = self.configs.lock().await;
        let dirs: Vec<String> = configs
            .iter()
            .filter_map(|config| config.working_dir.clone())
            .collect();
        let mut matcher = crate::task_board::RepoMatcher::new(&dirs);
        // Asked once for the TASK's repository, then compared against each
        // agent — rather than a matcher per agent, which would re-run git for
        // every row.
        let in_repo = matcher.matches(repo);
        configs
            .iter()
            .filter(|config| {
                let Some(dir) = &config.working_dir else {
                    return false;
                };
                in_repo && crate::task_board::belongs_to(repo, dir)
            })
            .take(MAX_AGENTS_OFFERED)
            .map(|config| {
                serde_json::json!({
                    "id": config.id.to_string(),
                    "name": config.name,
                })
            })
            .collect()
    }

    /// Send an agent to work on a task.
    ///
    /// Opens the agent's session and types an instruction naming the task. The
    /// instruction is SHORT and names the id rather than pasting the body: the
    /// agent can read the board itself, and a few kilobytes of prose typed into
    /// a CLI is a way to corrupt a prompt, not a way to brief someone.
    ///
    /// The board is updated too — in_progress, assigned to the agent — because
    /// a dispatch nobody recorded is a dispatch that happens twice.
    async fn dispatch(
        &self,
        session: &SessionHandle,
        task_id: &str,
        config_id: uuid::Uuid,
    ) -> Result<(), String> {
        let config = {
            let configs = self.configs.lock().await;
            configs
                .iter()
                .find(|config| config.id == config_id)
                .cloned()
                .ok_or_else(|| "no agent with that id".to_owned())?
        };

        // The title, for an instruction a person reading the pane can follow.
        let title = match self.board().await {
            Some(board) => board
                .detail(task_id)
                .await
                .ok()
                .flatten()
                .map(|detail| detail.task.title),
            None => None,
        };

        // Through the ordinary open path, so a dispatch to a running tmux
        // agent resumes it rather than starting a second one beside it.
        self.open(session, &config.id.to_string()).await?;
        let running = self
            .sessions
            .lock()
            .await
            .get(&session.session_id())
            .and_then(|state| state.open.clone())
            .ok_or_else(|| "the session did not open".to_owned())?;

        let instruction = match &title {
            Some(title) => format!(
                "Please pick up forge task {task_id} — \"{title}\". \
                 Read it from the task board first, follow its acceptance criteria, \
                 and comment on the task with what you did."
            ),
            None => format!(
                "Please pick up forge task {task_id}. Read it from the task board first, \
                 follow its acceptance criteria, and comment on the task with what you did."
            ),
        };
        running
            .send(&instruction)
            .map_err(|error| error.to_string())?;
        // NOT submitted. The operator sees the instruction in the pane and
        // presses Return themselves — sending an agent to work is worth one
        // deliberate keystroke, and a harness that was mid-prompt would
        // otherwise have this run as an answer to something else.

        if let Some(board) = self.board().await
            && let Err(error) = board.assign(task_id, &config.name).await
        {
            // The agent HAS been briefed; only the record failed. Said out
            // loud rather than reported as a failed dispatch, which would
            // invite the operator to do it a second time.
            session
                .send(&envelope(serde_json::json!({
                    "type": "shell_notice",
                    "text": format!(
                        "{} was briefed, but the board was not updated: {error}",
                        config.name
                    ),
                })))
                .await?;
        }
        Ok(())
    }

    /// Outstanding work for the repositories this machine's agents work in.
    ///
    /// Derived from the agents' working directories, which is what makes this
    /// possible at all: before an agent could say where it starts, the machine
    /// had no idea which repositories it was being used for.
    ///
    /// # Why this is several frames
    ///
    /// One repository on this machine has 895 open tasks. As a single frame
    /// that is 194 KiB, on a channel where terminal output is chunked under
    /// 1100 bytes precisely because SCTP fragmentation fails on paths whose MTU
    /// discovery is broken. The list worked for an 11-task repository and then
    /// stopped the moment a large one was added — the frame went out and
    /// nothing arrived.
    ///
    /// So: a bounded number of tasks, ranked, split into datagram-sized pages,
    /// with the TRUE total on the first page so the device can say what it is
    /// not showing.
    async fn tasks_frames(
        &self,
        repo: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Vec<serde_json::Value> {
        let dirs: Vec<String> = {
            let configs = self.configs.lock().await;
            let mut dirs: Vec<String> = configs
                .iter()
                .filter_map(|config| config.working_dir.clone())
                .collect();
            dirs.sort();
            dirs.dedup();
            dirs
        };
        if dirs.is_empty() {
            // Not an error, and not an empty board either. No agent has said
            // where it works, so there is nothing to look up.
            return vec![serde_json::json!({
                "type": "shell_tasks",
                "first": true,
                "repo": "",
                "repo_total": 0,
                "offset": 0,
                "tasks": [],
                "unavailable": "No agent has a working directory yet, so there is no repository to look up.",
            })];
        }
        let Some(board) = self.board().await else {
            return vec![serde_json::json!({
                "type": "shell_tasks",
                "first": true,
                "repo": "",
                "repo_total": 0,
                "offset": 0,
                "tasks": [],
                "unavailable": "The task board could not be reached from this machine.",
            })];
        };
        let tasks = match board.open_work(&dirs).await {
            Ok(tasks) => tasks,
            Err(error) => {
                return vec![serde_json::json!({
                    "type": "shell_tasks",
                    "first": true,
                    "repo": "",
                    "repo_total": 0,
                    "offset": 0,
                    "tasks": [],
                    "unavailable": bounded_reason(format_args!("The task board refused the read: {error}")),
                })];
            }
        };

        // Per REPOSITORY, not globally. A page off a globally ranked list is a
        // page of whichever repository has the most work — 905 open tasks, 11
        // of them hippo, none in the first ten, so hippo vanished from a screen
        // that groups by repository. Each repository now has its own first ten
        // and its own next ten, which is also the only paging model that makes
        // sense on a screen where each repository is a section.
        let mut by_repo: std::collections::BTreeMap<String, Vec<&crate::task_board::BoardTask>> =
            std::collections::BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        for task in &tasks {
            if task.repo.is_empty() {
                continue;
            }
            if !by_repo.contains_key(&task.repo) {
                order.push(task.repo.clone());
            }
            by_repo.entry(task.repo.clone()).or_default().push(task);
        }

        // One repository when asked for one, every repository otherwise.
        let wanted: Vec<String> = match repo {
            Some(repo) => order.iter().filter(|name| *name == repo).cloned().collect(),
            None => order,
        };

        let mut frames = Vec::new();
        for name in wanted {
            let all = by_repo.get(&name).map(Vec::as_slice).unwrap_or_default();
            let listed: Vec<serde_json::Value> = all
                .iter()
                .skip(offset)
                .take(limit.min(MAX_TASKS_SENT))
                .map(|task| {
                    serde_json::json!({
                        "id": task.id,
                        "title": task.title,
                        "status": task.status,
                        "priority": task.priority,
                        "block_reason": task.block_reason,
                        "needs_a_person": task.waits_on_a_person(),
                        "repo": task.repo,
                    })
                })
                .collect();

            if listed.is_empty() {
                // Still carries the total and the offset: "no more" and "none
                // at all" are different answers, and a device that cannot tell
                // them apart either hides a working MORE button or shows one
                // that does nothing.
                frames.push(serde_json::json!({
                    "type": "shell_tasks",
                    "first": offset == 0,
                    "repo": name,
                    "repo_total": all.len(),
                    "offset": offset,
                    "tasks": [],
                }));
                continue;
            }
            for (index, page) in listed.chunks(TASKS_PER_FRAME).enumerate() {
                frames.push(serde_json::json!({
                    "type": "shell_tasks",
                    // Only the first frame of a repository's first page
                    // replaces that repository's list. Without this every frame
                    // would wipe the one before it and the section would show
                    // its last three tasks.
                    "first": offset == 0 && index == 0,
                    "repo": name,
                    "repo_total": all.len(),
                    "offset": offset,
                    "tasks": page,
                }));
            }
        }
        frames
    }

    /// Find tasks by id, across the whole board.
    ///
    /// By ID rather than by text: an id is what one agent hands another, what a
    /// commit message quotes, and what a person reads off a screen and wants to
    /// look up. Full-text search over bodies is a different feature with
    /// different costs.
    ///
    /// Prefix as well as exact, because ids are long enough that nobody types
    /// all of one. Not scoped to the agents' repositories: the point of looking
    /// an id up is that you do not know where it lives.
    async fn task_search_frames(&self, query: &str) -> Vec<serde_json::Value> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return vec![serde_json::json!({
                "type": "shell_task_search", "query": query, "tasks": [],
            })];
        }
        let Some(board) = self.board().await else {
            return vec![serde_json::json!({
                "type": "shell_task_search",
                "query": query,
                "tasks": [],
                "unavailable": "The task board could not be reached from this machine.",
            })];
        };
        let found = board.find_by_id(&query, DEFAULT_TASK_PAGE).await;
        let listed: Vec<serde_json::Value> = match &found {
            Ok(tasks) => tasks
                .iter()
                .map(|task| {
                    serde_json::json!({
                        "id": task.id,
                        "title": task.title,
                        "status": task.status,
                        "priority": task.priority,
                        "block_reason": task.block_reason,
                        "needs_a_person": task.waits_on_a_person(),
                        "repo": task.repo,
                    })
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        if listed.is_empty() {
            return vec![serde_json::json!({
                "type": "shell_task_search",
                "query": query,
                "tasks": [],
                "unavailable": found.err().map(|error| format!("Search failed: {error}")),
            })];
        }
        listed
            .chunks(TASKS_PER_FRAME)
            .enumerate()
            .map(|(index, page)| {
                serde_json::json!({
                    "type": "shell_task_search",
                    "query": query,
                    "first": index == 0,
                    "tasks": page,
                })
            })
            .collect()
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
                "working_dir": config.working_dir,
                // Whether a tmux session is already up, so the shell can say
                // "resume" rather than "run" and the operator knows a second
                // one is not about to start.
                "running": self.runtime.is_running(config).await,
            }));
        }
        serde_json::json!({ "type": "shell_configs", "configs": listed })
    }

    async fn add_config(&self, body: &serde_json::Value) -> Result<(), String> {
        let config = SessionConfig::create(
            text(body, "name"),
            text(body, "kind"),
            command_list(body),
            body.get("working_dir").and_then(serde_json::Value::as_str),
        )
        .map_err(|error| error.to_string())?;
        let mut configs = self.configs.lock().await;
        configs.push(config);
        save_configs(&self.store_path, &configs);
        Ok(())
    }

    /// Change an existing config.
    ///
    /// A tmux session already running keeps running the OLD command — it was
    /// started with it, and nothing here can retroactively change what a
    /// process was launched as. Reported so the operator knows the edit takes
    /// effect next time, rather than believing a running build picked up their
    /// change.
    async fn update_config(&self, body: &serde_json::Value) -> Result<bool, String> {
        let id = config_id(body)?;
        let name = text(body, "name");
        let kind = text(body, "kind");
        let command = command_list(body);

        let mut configs = self.configs.lock().await;
        let position = configs
            .iter()
            .position(|config| config.id == id)
            .ok_or_else(|| "no config with that id".to_owned())?;
        // An ABSENT working_dir means "leave it alone"; an EMPTY one means "use
        // the machine's workspace". A client too old to know about the field
        // sends neither, and treating that as a clear would silently move an
        // agent to a different directory the next time anyone renamed it.
        let working_dir = match body.get("working_dir").and_then(serde_json::Value::as_str) {
            Some(dir) => dir,
            None => configs[position].working_dir.as_deref().unwrap_or(""),
        };
        let updated = configs[position]
            .edited(name, kind, command, Some(working_dir))
            .map_err(|error| error.to_string())?;
        let was_running = self.runtime.is_running(&configs[position]).await;
        configs[position] = updated;
        save_configs(&self.store_path, &configs);
        Ok(was_running)
    }

    /// Remove a config.
    ///
    /// Refused while its tmux session is running. Deleting would leave a
    /// process alive that nothing can name, attribute or stop — an orphan the
    /// operator would find later in `tmux ls` with no idea what started it.
    /// Killing it silently is worse: "delete this config" is not "stop my
    /// build".
    async fn delete_config(&self, body: &serde_json::Value) -> Result<(), String> {
        let id = config_id(body)?;
        let mut configs = self.configs.lock().await;
        let position = configs
            .iter()
            .position(|config| config.id == id)
            .ok_or_else(|| "no config with that id".to_owned())?;
        if self.runtime.is_running(&configs[position]).await {
            return Err(
                "this config's session is still running — detach and stop it before deleting"
                    .to_owned(),
            );
        }
        configs.remove(position);
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
/// The most raw terminal bytes one `shell_output` frame may carry.
///
/// Deliberately small. A data channel is SCTP over DTLS over UDP, and SCTP
/// fragments a large message to the path MTU it believes in. On a path where
/// MTU discovery does not work — mobile carriers routinely drop the ICMP that
/// makes it work — those fragments are silently lost, so a big message never
/// arrives while small ones do.
///
/// That is the shape of the bug this bounds: video kept working (RTP packetises
/// to about 1200 bytes of its own accord) and small control frames kept
/// working, while terminal output alone vanished off WiFi. 512 bytes of raw
/// input becomes roughly 700 of base64 plus the envelope, which stays inside a
/// conservative MTU with room to spare.
///
/// The channel is reliable and ORDERED, so splitting changes nothing the
/// emulator sees: it receives the same bytes in the same sequence, in more
/// pieces.
const MAX_OUTPUT_BYTES_PER_FRAME: usize = 512;

/// How long a running job must be still before it counts as waiting.
///
/// Forty-five seconds. Long enough that a compile or a long think is not
/// mistaken for a question, short enough that an agent stuck on a prompt is
/// noticed while the operator still remembers asking. An agent that is genuinely
/// working prints something well inside this.
const QUIET_SECONDS: u64 = 45;

/// How many tasks travel in one frame.
///
/// Three. A fat task row — long title, long path, a block reason — runs about
/// 300 bytes of JSON, and four of them came to 1284 against a 1100-byte safe
/// datagram. The number came from the test, not from arithmetic on a typical
/// row; typical rows are what made the single-frame version look fine.
const TASKS_PER_FRAME: usize = 3;

/// How many tasks a request returns per REPOSITORY when it does not say.
///
/// Ten each. Ten overall was the wrong unit: on a screen grouped by repository,
/// a global page is a page of the largest repository and the others are simply
/// absent.
const DEFAULT_TASK_PAGE: usize = 10;

/// How many memory items one REQUEST returns, and the hard cap on a request.
///
/// Twenty by default. This is a page, not a frame: it arrives across several
/// frames, exactly as a page of tasks does.
const MEMORY_PAGE_DEFAULT: usize = 20;
const MEMORY_PAGE_MAX: usize = 50;

/// How long a failure reason may be on the wire.
///
/// Every `reason` and `unavailable` field is built from an error, and an
/// anyhow chain is unbounded: it carries every context layer, and the CQL
/// driver's errors can carry a whole statement. The size proof modelled a
/// 120-byte reason while production could write megabytes, so the proof
/// passed and the frame did not arrive -- a frame over
/// MAX_CONTROL_FRAME_BYTES is dropped, which tells the device nothing at
/// exactly the moment something has gone wrong.
///
/// 120 bytes, which is not an invention: it is exactly what
/// `every_bounded_frame_fits_a_datagram` already assumes every string field
/// is worth. Setting the bound to the proof's own figure is what makes the
/// proof cover production -- raising it to 240 was tried and immediately
/// showed `shell_task_detail` at 1355 bytes against an 1100 datagram, because
/// that frame carries two such fields and was already close to the line.
///
/// Long enough to name a failure and its immediate cause. An error that needs
/// more than a sentence belongs in the machine's log, which is not size-bound,
/// and the log line is emitted next to every one of these.
const MAX_REASON_BYTES: usize = 120;

/// Clamp a failure reason to something a frame can carry.
///
/// Truncates on a character boundary. A cut through a multi-byte character
/// would produce a string that does not serialise, turning a reportable
/// failure into an unreportable one.
fn bounded_reason(value: impl std::fmt::Display) -> String {
    let value = value.to_string();
    if value.len() <= MAX_REASON_BYTES {
        return value;
    }
    let mut end = MAX_REASON_BYTES.saturating_sub(3);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

/// How many memory items travel in ONE frame.
///
/// Four. The first attempt sent a 50-item page as a single frame and the size
/// proof measured it at 14,830 bytes against a 1,100-byte safe datagram — the
/// same mistake as the task list, in a frame written after learning it. A tier
/// item is not lighter than a task row: an entity id, a session id, a sentence
/// of title and an absolute path came to about 295 bytes, and three of those
/// still measured 1,106 against 1,100. The row was trimmed instead — no
/// per-item tier, no absolute path — which is what makes four fit.
const MEMORY_ITEMS_PER_FRAME: usize = 4;

// Checked by the compiler rather than by a test: both sides are constants, so
// the comparison has an answer before anything runs, and a test that asserts it
// can only ever pass. These are the invariants the size proof below depends on
// — a default above the cap, or a frame claiming more than a whole page, would
// make that proof about a frame nobody sends.
const _: () = assert!(MEMORY_PAGE_DEFAULT > 0);
const _: () = assert!(
    MEMORY_PAGE_DEFAULT <= MEMORY_PAGE_MAX,
    "the default memory page is above its own cap"
);
const _: () = assert!(
    MEMORY_ITEMS_PER_FRAME <= MEMORY_PAGE_MAX,
    "one frame claims to carry more than a whole page"
);

/// How many agents a task detail offers to dispatch.
///
/// Four. The frame that carries them is Bounded, and this is the number the
/// size test is asserted against.
const MAX_AGENTS_OFFERED: usize = 4;

/// How many related tasks a detail carries.
///
/// Twelve. One blocked task on the live board has ten others talking about the
/// same identifier; a bound below that would hide the duplicate cluster, which
/// is the most useful thing the list has to say.
const MAX_RELATED: usize = 12;

/// The most a single request will return, however large a limit it asks for.
///
/// A cap on WORK, not on truth: the real total travels with every page, so a
/// device showing 10 of 906 can say so.
const MAX_TASKS_SENT: usize = 200;

// Compile-time for the same reason. The upper bound is a judgement, not a
// protocol limit: 200 tasks is 67 frames, and a cap that costs more frames than
// an operator will ever scroll has stopped bounding anything.
const _: () = assert!(MAX_TASKS_SENT > 0);
const _: () = assert!(
    MAX_TASKS_SENT <= 500,
    "a cap this high stops being a cap: it is over 160 frames"
);

/// How often the roster's status is refreshed.
///
/// Each tick runs one `capture-pane` per tmux job. Slow enough that a handful
/// of jobs costs nothing, fast enough that a glance at the roster is current.
const STATUS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// Tell the device what every job is doing, until the control session ends.
///
/// Tell the device what changed, and when.
///
/// A DELTA, not a snapshot. Two reasons, and the second is the important one:
///
/// - Frames stay small. A full snapshot of every job's status on every tick
///   would grow past the datagram budget the output path had to be split to
///   respect, and would do it silently as jobs are added.
/// - A change is an EVENT. Sending only what moved, with the time it moved,
///   gives the device a stream it can interleave into a feed across jobs —
///   which a repeated snapshot cannot, because it cannot tell a line that just
///   appeared from one that has been there for an hour.
///
/// The first tick sends everything, because a device that just connected has
/// no prior state to diff against.
async fn pump_status(
    configs: Arc<Mutex<Vec<SessionConfig>>>,
    runtime: Arc<SessionRuntime>,
    sessions: Arc<Mutex<HashMap<uuid::Uuid, ShellState>>>,
    session: SessionHandle,
) {
    let session_id = session.session_id();
    // One watcher per job. It holds the previous pane AND how often each row
    // changes, which is how a spinner is told apart from something said.
    let mut watchers: HashMap<uuid::Uuid, crate::session_runtime::PaneWatcher> = HashMap::new();
    // When each job last moved, so one that has gone quiet can be noticed.
    let mut last_moved: HashMap<uuid::Uuid, u64> = HashMap::new();
    // Jobs already reported as waiting, so it is said once and not every tick.
    let mut announced: std::collections::HashSet<uuid::Uuid> = std::collections::HashSet::new();
    // Jobs last seen running, so a state change is reported once.
    let mut was_running: std::collections::HashSet<uuid::Uuid> = std::collections::HashSet::new();
    let mut first = true;
    // Which pids belong to which job. Resolved once per job and kept, because
    // walking a process tree is several spawns and a pane's harness does not
    // change pid while it runs.
    let mut pids: HashMap<uuid::Uuid, Vec<u32>> = HashMap::new();

    loop {
        // One call for every job, not one per job. A harness that reports its
        // own state is believed over anything read off the screen — see
        // `harness_state`.
        let reported = crate::harness_state::claude_agents().await;
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0);

        let snapshot: Vec<SessionConfig> = configs.lock().await.clone();
        // What this control session is holding open. An EPHEMERAL session is a
        // process held by this listener and nothing else — tmux has never heard
        // of it — so without this it reports off-shift for its whole life, and
        // a home screen that hides off-shift jobs hides it completely.
        let held_by_this_session: Option<uuid::Uuid> = sessions
            .lock()
            .await
            .get(&session_id)
            .and_then(|state| state.open.as_ref())
            .map(|open| open.config().id);

        let mut jobs = Vec::new();
        for config in &snapshot {
            let running =
                runtime.is_running(config).await || held_by_this_session == Some(config.id);
            if !running {
                if watchers.remove(&config.id).is_some() || was_running.contains(&config.id) {
                    // It was running and is not. That IS news.
                    jobs.push(serde_json::json!({
                        "id": config.id.to_string(),
                        "running": false,
                        "changed_at": at,
                    }));
                }
                last_moved.remove(&config.id);
                announced.remove(&config.id);
                pids.remove(&config.id);
                was_running.remove(&config.id);
                continue;
            }

            // What the harness says about ITSELF, if it says anything.
            let owned = match pids.get(&config.id) {
                Some(known) => known.clone(),
                None => {
                    let found = crate::harness_state::pane_process_tree(
                        runtime.tmux_binary(),
                        &config.tmux_session_name(),
                    )
                    .await;
                    pids.insert(config.id, found.clone());
                    found
                }
            };
            let says = owned.iter().find_map(|pid| reported.get(pid).copied());

            let Some(pane) = runtime.pane_text(config).await else {
                // No pane to read: an ephemeral session's output goes straight
                // to the device and tmux holds nothing to scrape. Say it is
                // up — skipping here is what left it off the roster even once
                // it was known to be running.
                if first || !was_running.contains(&config.id) {
                    jobs.push(serde_json::json!({
                        "id": config.id.to_string(),
                        "running": true,
                        "changed_at": at,
                    }));
                }
                was_running.insert(config.id);
                continue;
            };
            was_running.insert(config.id);
            let moved = watchers.entry(config.id).or_default().observe(&pane);

            // A harness that reports itself waiting is waiting, now — no quiet
            // timer, no guessing. This is the case the whole pane-watching
            // exercise could not reach: a pane waiting for input looks exactly
            // like a pane thinking.
            if says == Some(crate::harness_state::HarnessState::Waiting) {
                if announced.insert(config.id) {
                    jobs.push(serde_json::json!({
                        "id": config.id.to_string(),
                        "running": true,
                        "waiting": true,
                        "said_so": true,
                        "changed_at": at,
                    }));
                }
                if let Some(line) = moved {
                    last_moved.insert(config.id, at);
                    jobs.push(serde_json::json!({
                        "id": config.id.to_string(),
                        "running": true,
                        "last_line": line,
                        "changed_at": at,
                    }));
                }
                continue;
            }
            if says == Some(crate::harness_state::HarnessState::Busy) {
                // Working, whatever the screen looks like. Clearing the
                // announcement here is what lets a second wait be reported
                // after the operator answers the first.
                announced.remove(&config.id);
                last_moved.insert(config.id, at);
            }

            if let Some(line) = moved {
                last_moved.insert(config.id, at);
                announced.remove(&config.id);
                jobs.push(serde_json::json!({
                    "id": config.id.to_string(),
                    "running": true,
                    "last_line": line,
                    "changed_at": at,
                }));
            } else if first {
                // Nothing has moved yet and this is the first look. Say it is
                // up, so a job started before the app connected is not missing
                // from the roster until it happens to print something.
                jobs.push(serde_json::json!({
                    "id": config.id.to_string(),
                    "running": true,
                    "changed_at": at,
                }));
            } else if says.is_none()
                && let Some(since) = last_moved.get(&config.id)
                && at.saturating_sub(*since) >= QUIET_SECONDS
                && announced.insert(config.id)
            {
                // Only for a harness that does NOT report itself — codex has no
                // equivalent of `claude agents`. Moved, then stopped, is a
                // guess; it is a decent one for an interactive harness, and it
                // is marked as a guess so the screen can word it differently.
                jobs.push(serde_json::json!({
                    "id": config.id.to_string(),
                    "running": true,
                    "waiting": true,
                    "said_so": false,
                    "quiet_for": at.saturating_sub(*since),
                    "changed_at": at,
                }));
            }
        }

        // Nothing moved. Say nothing — a tick that sends an empty list every
        // three seconds is a heartbeat pretending to be news.
        if !jobs.is_empty() {
            let frame = serde_json::json!({
                "version": 1,
                "frame_id": uuid::Uuid::now_v7().to_string(),
                "body": {
                    "type": "shell_status",
                    // The device clears its state on a full send; on a delta it
                    // merges. Without this a reconnect would leave stale jobs
                    // in the roster forever.
                    "full": first,
                    "jobs": jobs,
                },
            });
            if session.send(&frame.to_string()).await.is_err() {
                // The channel has gone; so has the reason to keep scraping.
                return;
            }
        }

        first = false;
        tokio::time::sleep(STATUS_INTERVAL).await;
    }
}

/// One `shell_output` frame carrying raw terminal bytes.
///
/// Base64, not a string. This is a raw terminal stream — escape sequences,
/// cursor moves, whatever encoding the program chose. A lossy UTF-8 conversion
/// would replace exactly the bytes the emulator needs to position anything.
async fn send_output(
    session: &SessionHandle,
    config_id: uuid::Uuid,
    bytes: &[u8],
) -> Result<(), ()> {
    use base64::Engine as _;

    let frame = serde_json::json!({
        "version": 1,
        "frame_id": uuid::Uuid::now_v7().to_string(),
        "body": {
            "type": "shell_output",
            "config_id": config_id.to_string(),
            "bytes": base64::engine::general_purpose::STANDARD.encode(bytes),
        },
    });
    session.send(&frame.to_string()).await.map_err(|_| ())
}

async fn pump_output(running: Arc<RunningSession>, session: SessionHandle, config_id: uuid::Uuid) {
    while let Some(bytes) = running.next_output().await {
        // Split rather than sent whole. See MAX_OUTPUT_BYTES_PER_FRAME.
        for piece in bytes.chunks(MAX_OUTPUT_BYTES_PER_FRAME) {
            if send_output(&session, config_id, piece).await.is_err() {
                // The channel has gone; so has the reason to keep reading.
                return;
            }
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
            "shell_update_config",
            "shell_delete_config",
            "shell_open",
            "shell_close",
            "shell_delete_session",
            "shell_resize",
            "shell_input",
            "shell_scroll",
            "shell_tasks",
            "shell_task",
            "shell_task_search",
            "shell_dispatch",
            "shell_task_complete",
            "shell_memory_tiers",
            "shell_memory_items",
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
            let state = sessions.entry(session_id).or_default();
            state.granted = true;
            // One pump per control session. Re-granting must not stack a second
            // one — two pumps means two status frames per tick and a roster
            // that flickers between them.
            if state.status_pump.is_none() {
                state.status_pump = Some(tokio::spawn(pump_status(
                    Arc::clone(&self.configs),
                    Arc::clone(&self.runtime),
                    // Cloning the handle needs no lock; the pump waits for the
                    // one held here, which is released a few lines below.
                    Arc::clone(&self.sessions),
                    session.clone(),
                )));
            }
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
            "shell_update_config" => {
                let was_running = self.update_config(body).await?;
                session.send(&envelope(self.configs_frame().await)).await?;
                if was_running {
                    // Said plainly. A running process cannot retroactively
                    // become the command it was just edited to be, and an
                    // operator who believes otherwise will trust output from a
                    // build they think they changed.
                    session
                        .send(&envelope(serde_json::json!({
                            "type": "shell_notice",
                            "text": "The running session still uses the previous command. \
                                     The change applies next time it starts.",
                        })))
                        .await?;
                }
                Ok(())
            }
            "shell_delete_config" => {
                self.delete_config(body).await?;
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
            "shell_resize" => {
                // A TUI lays out to the size it is told, so a wrong one wraps
                // or truncates every frame it draws. The device is the only
                // thing that knows how big its terminal is.
                let rows = body.get("rows").and_then(serde_json::Value::as_u64);
                let cols = body.get("cols").and_then(serde_json::Value::as_u64);
                let (Some(rows), Some(cols)) = (rows, cols) else {
                    return Err("resize needs rows and cols".to_owned());
                };
                // Refused rather than clamped. A zero here means the device
                // measured nothing, and telling the program it has a
                // zero-column terminal makes it draw nothing at all.
                if rows == 0 || cols == 0 {
                    return Err("a terminal cannot be zero rows or columns".to_owned());
                }
                let open = self
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .and_then(|state| state.open.clone())
                    .ok_or_else(|| "no session is open".to_owned())?;
                open.resize(
                    u16::try_from(rows.min(u64::from(u16::MAX))).unwrap_or(u16::MAX),
                    u16::try_from(cols.min(u64::from(u16::MAX))).unwrap_or(u16::MAX),
                )
                .map_err(|error| error.to_string())
            }
            "shell_delete_session" => {
                // Deliberately distinct from shell_close. Closing a persistent
                // session detaches and leaves it running, which is the whole
                // reason to choose that kind; this is the operator saying the
                // work is finished and the session should stop existing.
                let open = self
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .and_then(|state| state.open.clone())
                    .ok_or_else(|| "no session is open".to_owned())?;
                open.destroy().map_err(|error| error.to_string())?;
                self.close(session_id).await;
                // The list changes — the agent stops being "in use" — so send
                // it rather than leaving the device to guess.
                session.send(&envelope(self.configs_frame().await)).await
            }
            "shell_input" => {
                let open = self
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .and_then(|state| state.open.clone())
                    .ok_or_else(|| "no session is open".to_owned())?;

                // Read BEFORE the input, which is what clears it.
                let was_scrolled = open.is_scrolled();

                // Text and keys are separate fields, and a frame may carry
                // either or both. A bare Return — no text at all — is how a
                // CLI waiting for confirmation is answered, so "empty means
                // nothing to do" would make the app unable to say yes.
                if let Some(text) = body.get("text").and_then(serde_json::Value::as_str)
                    && !text.is_empty()
                {
                    open.send(text).map_err(|error| error.to_string())?;
                }

                // Raw bytes from a client-side emulator. This is the normal
                // path now that the device runs a real terminal; `text` and
                // `keys` remain for the on-screen key bar and for any client
                // without an emulator.
                let raw = body.get("bytes").and_then(serde_json::Value::as_str);
                if let Some(encoded) = raw {
                    use base64::Engine as _;
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|error| format!("input bytes are not base64: {error}"))?;
                    open.send_bytes(&bytes).map_err(|error| error.to_string())?;
                }

                let keys = body.get("keys").and_then(serde_json::Value::as_array);
                if let Some(keys) = keys {
                    for name in keys.iter().filter_map(serde_json::Value::as_str) {
                        // Refused, not guessed. `send-keys` reads its argument
                        // as tmux's command language, so forwarding an unknown
                        // name would hand the device remote tmux control rather
                        // than a keypress.
                        let key = crate::session_runtime::NamedKey::from_wire(name)
                            .ok_or_else(|| format!("{name} is not a key this machine sends"))?;
                        open.send_key(key).map_err(|error| error.to_string())?;
                    }
                }

                if body.get("text").is_none() && keys.is_none() && raw.is_none() {
                    return Err("input needs bytes, text or keys".to_owned());
                }
                // Typing leaves copy mode, so the device's LIVE control has to
                // stop claiming the pane is scrolled. Sent only on the CHANGE:
                // a frame per keystroke is a frame per keystroke.
                if was_scrolled && !open.is_scrolled() {
                    session
                        .send(&envelope(serde_json::json!({
                            "type": "shell_scrolled",
                            "scrolled": false,
                        })))
                        .await?;
                }
                Ok(())
            }
            "shell_scroll" => {
                let open = self
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .and_then(|state| state.open.clone())
                    .ok_or_else(|| "no session is open".to_owned())?;

                let name = body
                    .get("motion")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "scroll needs a motion".to_owned())?;
                // Closed set, same reason as keys: the value reaches a tmux
                // command line, and an unvalidated one is remote tmux control.
                let motion = crate::session_runtime::ScrollMotion::from_wire(name)
                    .ok_or_else(|| format!("{name} is not a scroll this machine makes"))?;
                open.scroll(motion).map_err(|error| error.to_string())?;
                session
                    .send(&envelope(serde_json::json!({
                        "type": "shell_scrolled",
                        "scrolled": open.is_scrolled(),
                    })))
                    .await
            }
            "shell_tasks" => {
                let offset = body
                    .get("offset")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize;
                let limit = body
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .map(|value| value as usize)
                    .unwrap_or(DEFAULT_TASK_PAGE);
                // Absent means every repository's first page; present means the
                // next page of one of them.
                let repo = body.get("repo").and_then(serde_json::Value::as_str);
                for frame in self.tasks_frames(repo, offset, limit).await {
                    session.send(&envelope(frame)).await?;
                }
                Ok(())
            }
            "shell_dispatch" => {
                let task_id = body
                    .get("task_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "dispatch needs a task_id".to_owned())?;
                let config_id = config_id(body)?;
                self.dispatch(session, task_id, config_id).await?;
                for frame in self.task_detail_frames(task_id).await {
                    session.send(&envelope(frame)).await?;
                }
                Ok(())
            }
            "shell_task_complete" => {
                let task_id = body
                    .get("task_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "completing needs a task_id".to_owned())?;
                let note = body.get("note").and_then(serde_json::Value::as_str);
                let board = self
                    .board()
                    .await
                    .ok_or_else(|| "the task board could not be reached".to_owned())?;
                board
                    .complete(task_id, note)
                    .await
                    .map_err(|error| error.to_string())?;
                // The task back, so the screen shows what the board now says
                // rather than what the app assumed it would say.
                for frame in self.task_detail_frames(task_id).await {
                    session.send(&envelope(frame)).await?;
                }
                Ok(())
            }
            "shell_task_search" => {
                let query = body
                    .get("query")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                for frame in self.task_search_frames(query).await {
                    session.send(&envelope(frame)).await?;
                }
                Ok(())
            }
            "shell_task" => {
                let id = body
                    .get("task_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "task needs a task_id".to_owned())?;
                for frame in self.task_detail_frames(id).await {
                    session.send(&envelope(frame)).await?;
                }
                Ok(())
            }
            "shell_memory_tiers" => {
                session
                    .send(&envelope(self.memory_tiers_frame().await))
                    .await
            }
            "shell_memory_items" => {
                let tier = body
                    .get("tier")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| "memory items need a tier".to_owned())?;
                // An empty search is not a search. Without this, clearing the
                // field would filter to titles containing "" — every row —
                // which is the same answer by accident and not by rule.
                let query = body
                    .get("query")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                // A cursor from the previous page's last frame. Absent means
                // the first page; `offset` is gone, because an offset into a
                // list this size is a scan and a cursor is a seek.
                let cursor = body
                    .get("cursor")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let limit = body
                    .get("limit")
                    .and_then(serde_json::Value::as_u64)
                    .map(|value| value as usize)
                    .unwrap_or(MEMORY_PAGE_DEFAULT);
                for frame in self.memory_items_frames(tier, query, cursor, limit).await {
                    session.send(&envelope(frame)).await?;
                }
                Ok(())
            }
            other => Err(format!("claimed {other} and cannot serve it")),
        }
    }

    async fn on_closed(&self, session_id: uuid::Uuid) {
        // The status pump outlives an open session but not the control session.
        // Leaving it running would scrape tmux forever for a device that has
        // gone, which is invisible and never stops.
        if let Some(state) = self.sessions.lock().await.get_mut(&session_id)
            && let Some(pump) = state.status_pump.take()
        {
            pump.abort();
        }
        self.close(session_id).await;
        // The grant goes with the session. An ephemeral session is killed by
        // dropping it; a tmux one keeps running, which is why it was chosen.
        self.sessions.lock().await.remove(&session_id);
    }
}

fn config_id(body: &serde_json::Value) -> Result<uuid::Uuid, String> {
    body.get("config_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| "that is not a config id".to_owned())
}

fn text<'a>(body: &'a serde_json::Value, key: &str) -> &'a str {
    body.get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

fn command_list(body: &serde_json::Value) -> Vec<String> {
    body.get("command")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Prose of any length, as datagram-sized pages.
///
/// Base64 over raw byte chunks, exactly as terminal output travels — the same
/// bound, and covered by the same proof. Chunking the STRING instead would need
/// a char-boundary rule and a guess at how much JSON escaping expands it; a
/// byte chunk of a known size has neither problem.
fn text_pages(
    task_id: &str,
    part: &str,
    author: Option<&str>,
    text: &str,
) -> Vec<serde_json::Value> {
    use base64::Engine as _;
    if text.is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let pages = bytes.len().div_ceil(MAX_OUTPUT_BYTES_PER_FRAME);
    bytes
        .chunks(MAX_OUTPUT_BYTES_PER_FRAME)
        .enumerate()
        .map(|(index, chunk)| {
            serde_json::json!({
                "type": "shell_task_text",
                "task_id": task_id,
                "part": part,
                "author": author,
                // Index and count so the reader can tell a missing page from a
                // short one. A body that silently loses its middle reads as a
                // task somebody wrote badly.
                "index": index,
                "pages": pages,
                "bytes": base64::engine::general_purpose::STANDARD.encode(chunk),
            })
        })
        .collect()
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
        // Absent in a file written before the field existed, which correctly
        // means "the machine's workspace" — the behaviour those configs had.
        working_dir: value
            .get("working_dir")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
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
                "working_dir": config.working_dir,
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

    /// Editing an agent from a client too old to know about working
    /// directories must not move that agent somewhere else.
    ///
    /// The old client sends no `working_dir` at all. Reading that as "clear it"
    /// would relocate the agent to the machine's workspace the next time anyone
    /// renamed it, with nothing in the frame saying so.
    #[test]
    fn an_absent_working_dir_on_edit_leaves_the_directory_alone() {
        let dir = std::env::temp_dir();
        let original = SessionConfig::create(
            "agent",
            "tmux",
            vec!["codex".into()],
            Some(&dir.to_string_lossy()),
        )
        .expect("valid");
        assert!(
            original.working_dir.is_some(),
            "fixture must set a directory"
        );

        // What an old client sends: everything except working_dir.
        let body = serde_json::json!({
            "config_id": original.id.to_string(),
            "name": "renamed",
            "kind": "tmux",
            "command": ["codex"],
        });
        let carried = match body.get("working_dir").and_then(serde_json::Value::as_str) {
            Some(dir) => dir,
            None => original.working_dir.as_deref().unwrap_or(""),
        };
        let edited = original
            .edited("renamed", "tmux", vec!["codex".into()], Some(carried))
            .expect("valid");

        assert_eq!(edited.working_dir, original.working_dir);
        assert_eq!(edited.name, "renamed", "the rename must still apply");
    }

    /// An EMPTY working_dir is a deliberate "use the machine's workspace",
    /// which absent is not. The two must stay distinguishable.
    #[test]
    fn an_empty_working_dir_on_edit_clears_it() {
        let dir = std::env::temp_dir();
        let original = SessionConfig::create(
            "agent",
            "tmux",
            vec!["codex".into()],
            Some(&dir.to_string_lossy()),
        )
        .expect("valid");
        let edited = original
            .edited("agent", "tmux", vec!["codex".into()], Some(""))
            .expect("valid");
        assert_eq!(edited.working_dir, None);
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
            SessionConfig::create("build", "tmux", vec!["cargo".into(), "test".into()], None)
                .expect("valid"),
            SessionConfig::create("shell", "bash", vec!["bash".into(), "-l".into()], None)
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
        let good = SessionConfig::create("keep", "bash", vec!["ls".into()], None).expect("valid");
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

#[cfg(test)]
mod output_framing_tests {
    use super::*;
    use base64::Engine as _;

    /// How a frame kind is kept inside a datagram.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Sizing {
        /// Content bounded by construction — a status word, an id, a count. A
        /// worst-case sample is asserted below.
        Bounded,
        /// Content of no fixed length, so it travels as pages. The PAGE size is
        /// asserted; the number of pages is not bounded and need not be.
        Paged,
    }

    /// EVERY frame kind this module emits, and how each stays inside a datagram.
    ///
    /// This exists because the same bug has now happened twice: a frame whose
    /// content is bounded in the common case and unbounded in the real one.
    /// First terminal output, fine until a command printed a lot; then the task
    /// list, fine for an 11-task repository and 194 KiB for an 895-task one. In
    /// both cases the frame was sent and nothing arrived — no error, no log,
    /// just a screen that never filled.
    ///
    /// `every_emitted_frame_kind_is_declared` fails if a new kind is emitted
    /// without being added here, so the next one is not discovered by someone
    /// staring at a blank screen.
    const EMITTED: &[(&str, Sizing)] = &[
        ("shell_configs", Sizing::Bounded),
        ("shell_status", Sizing::Bounded),
        ("shell_notice", Sizing::Bounded),
        ("shell_opened", Sizing::Bounded),
        ("shell_ended", Sizing::Bounded),
        ("shell_error", Sizing::Bounded),
        ("shell_output", Sizing::Paged),
        ("shell_tasks", Sizing::Paged),
        ("shell_task_detail", Sizing::Bounded),
        ("shell_task_text", Sizing::Paged),
        ("shell_task_search", Sizing::Paged),
        ("shell_task_related", Sizing::Paged),
        ("shell_scrolled", Sizing::Bounded),
        ("shell_memory_tiers", Sizing::Bounded),
        ("shell_memory_items", Sizing::Paged),
    ];

    /// A new frame kind cannot be emitted without a decision about its size.
    ///
    /// Reads the module's own source, because that is where the truth is. A
    /// list maintained by hand beside the code goes stale — which is how
    /// `shell_notice` and `shell_status` both fell out of the client's frame
    /// allowlist in one day.
    #[test]
    fn every_emitted_frame_kind_is_declared() {
        let source = include_str!("shell_extension.rs");
        let declared: std::collections::HashSet<&str> =
            EMITTED.iter().map(|(kind, _)| *kind).collect();

        let prefix = "\"type\": \"";
        let mut found = std::collections::BTreeSet::new();
        for (index, _) in source.match_indices(prefix) {
            let rest = &source[index + prefix.len()..];
            let Some(end) = rest.find('"') else { continue };
            let kind = &rest[..end];
            if kind.starts_with("shell_") {
                found.insert(kind);
            }
        }
        assert!(
            !found.is_empty(),
            "the source scan found no emitted frames — it has stopped working, and a \
             guard that cannot fail is not a guard"
        );

        let undeclared: Vec<&&str> = found.iter().filter(|k| !declared.contains(**k)).collect();
        assert!(
            undeclared.is_empty(),
            "emitted but not declared in EMITTED: {undeclared:?}. Decide whether each is \
             Bounded (assert a worst case) or Paged (chunk it). The last two frames that \
             skipped this step were sent and silently never arrived."
        );
    }

    /// Every BOUNDED frame kind, at its worst case, fits a datagram.
    #[test]
    fn every_bounded_frame_fits_a_datagram() {
        // The real bound, so the proof covers what production can write.
        let long = "a".repeat(MAX_REASON_BYTES);
        let path = "/Users/bkearns/src/ferrosa-suite/ferrosa-memory/crates/ferrosa-memory-sync";
        for (kind, sizing) in EMITTED {
            if *sizing != Sizing::Bounded {
                continue;
            }
            let body = match *kind {
                // The one genuinely variable Bounded frame: it carries configs,
                // of which a machine has a handful. If that stops being true,
                // this assertion is what says so.
                "shell_configs" => serde_json::json!({
                    "type": kind,
                    "configs": [{
                        "id": uuid::Uuid::now_v7().to_string(),
                        "name": long,
                        "kind": "tmux",
                        "command": ["codex", "--yolo"],
                        "working_dir": path,
                        "running": true,
                    }],
                }),
                "shell_status" => serde_json::json!({
                    "type": kind,
                    "full": true,
                    "jobs": [{
                        "id": uuid::Uuid::now_v7().to_string(),
                        "running": true,
                        "last_line": long,
                        "changed_at": 1_787_538_000.0,
                    }],
                }),
                "shell_task_detail" => serde_json::json!({
                    "type": kind,
                    "task_id": "t_0d313bb0",
                    "title": long,
                    "status": "blocked",
                    "priority": 95,
                    "block_reason": long,
                    "needs_a_person": true,
                    "repo": path,
                    "assignee": "ben",
                    "summary": long,
                    "agents": (0..MAX_AGENTS_OFFERED).map(|_| serde_json::json!({
                        "id": uuid::Uuid::now_v7().to_string(),
                        "name": "ferrosa-suite claude",
                    })).collect::<Vec<_>>(),
                }),
                _ => serde_json::json!({
                    "type": kind,
                    "text": long,
                    "reason": long,
                    "dropped": 12,
                    "resumable": true,
                }),
            };
            let frame = envelope(body);
            assert!(
                frame.len() <= SAFE_DATAGRAM_BYTES,
                "{kind} at its worst case is {} bytes, over the {} safe datagram — make \
                 it Paged",
                frame.len(),
                SAFE_DATAGRAM_BYTES
            );
        }
    }

    /// A page of task text fits, using the same bound as output.
    #[test]
    fn a_page_of_task_text_fits_a_datagram() {
        let text = "x".repeat(MAX_OUTPUT_BYTES_PER_FRAME * 3);
        let pages = text_pages("t_0d313bb0", "comment", Some("ben"), &text);
        assert_eq!(pages.len(), 3);
        for page in pages {
            let frame = envelope(page);
            assert!(
                frame.len() <= SAFE_DATAGRAM_BYTES,
                "a task text page is {} bytes",
                frame.len()
            );
        }
    }

    /// A page of related tasks fits too. Its rows carry a reason as well as a
    /// title, so it is fatter than a task row and needs its own assertion.
    #[test]
    fn a_page_of_related_tasks_fits_a_datagram() {
        let item = serde_json::json!({
            "reason": "mentions MAAS-T-35",
            "id": "t_393bc64e",
            "title": "Decide QA-0009: fix the entity_type example or the tool",
            "status": "blocked",
            "needs_a_person": true,
        });
        let page: Vec<serde_json::Value> = (0..TASKS_PER_FRAME).map(|_| item.clone()).collect();
        let frame = envelope(serde_json::json!({
            "type": "shell_task_related",
            "task_id": "t_393bc64e",
            "related": page,
        }));
        assert!(
            frame.len() <= SAFE_DATAGRAM_BYTES,
            "a related page is {} bytes",
            frame.len()
        );
    }

    /// A page of tasks must fit a datagram, like a page of output.
    ///
    /// The bug this pins: the task list was one frame. For a repository with
    /// 895 open tasks that is 194 KiB, and nothing arrived — the list worked
    /// for an 11-task repository and stopped the moment a large one was added.
    /// Asserted on the SERIALIZED frame, not on the task count, because the
    /// count is not what travels.
    #[test]
    fn a_page_of_tasks_fits_a_datagram() {
        // A deliberately fat row: a long title, a long path, a block reason.
        let task = serde_json::json!({
            "id": "t_0d313bb0",
            "title": "Review specs/decisions.md to confirm the 10 decision records match intent",
            "status": "blocked",
            "priority": 95,
            "block_reason": "waiting on a decision about where archived buffers live",
            "needs_a_person": true,
            "repo": "/Users/bkearns/src/ferrosa-suite/ferrosa-memory",
        });
        let page: Vec<serde_json::Value> = (0..TASKS_PER_FRAME).map(|_| task.clone()).collect();
        let frame = envelope(serde_json::json!({
            "type": "shell_tasks",
            "first": true,
            "total": 906,
            "tasks": page,
        }));
        assert!(
            frame.len() <= SAFE_DATAGRAM_BYTES,
            "a {}-task page serializes to {} bytes, over the {} safe datagram",
            TASKS_PER_FRAME,
            frame.len(),
            SAFE_DATAGRAM_BYTES
        );
    }

    /// The size proof for the memory list, which is the frame most likely to
    /// repeat the task list's mistake: bounded on a fixture, unbounded on a
    /// real store where every title is a sentence and every path absolute.
    #[test]
    fn a_page_of_memory_items_fits_a_datagram() {
        let item = serde_json::json!({
            "id": "6f1d2c3b-4a59-4e7f-8b21-9c0d1e2f3a4b",
            "session": "0f9e8d7c-6b5a-4938-8271-6a5b4c3d2e1f",
            "title": "Ferrosa write ceiling is CQL-path CPU overhead, not storage",
            "root": "research/consolidation",
        });
        let page: Vec<serde_json::Value> =
            (0..MEMORY_ITEMS_PER_FRAME).map(|_| item.clone()).collect();
        let frame = envelope(serde_json::json!({
            "type": "shell_memory_items",
            "tier": "knowledge",
            "reachable": true,
            "query": "write ceiling",
            "sort": "recent",
            "offset": 0,
            "matched": 1284,
            "truncated": false,
            "items": page,
        }));
        assert!(
            frame.len() <= SAFE_DATAGRAM_BYTES,
            "a {}-item frame serializes to {} bytes, over the {} safe datagram. \
             Lower MEMORY_ITEMS_PER_FRAME rather than hoping real titles are shorter.",
            MEMORY_ITEMS_PER_FRAME,
            frame.len(),
            SAFE_DATAGRAM_BYTES
        );
    }

    /// A conservative floor for path MTU.
    ///
    /// IPv6 guarantees 1280, and DTLS, UDP, IP and SCTP headers all come out of
    /// that. Staying under this means SCTP never has to fragment an output
    /// frame, which is the thing that was failing on a path where MTU discovery
    /// does not work.
    const SAFE_DATAGRAM_BYTES: usize = 1100;

    fn frame_len(payload: &[u8]) -> usize {
        serde_json::json!({
            "version": 1,
            "frame_id": uuid::Uuid::now_v7().to_string(),
            "body": {
                "type": "shell_output",
                "config_id": uuid::Uuid::now_v7().to_string(),
                "bytes": base64::engine::general_purpose::STANDARD.encode(payload),
            },
        })
        .to_string()
        .len()
    }

    /// The invariant that matters is the SERIALIZED frame, not the chunk size.
    ///
    /// Base64 inflates by a third and the envelope carries two UUIDs, so a
    /// chunk limit chosen without measuring the whole frame is a guess. This
    /// fails if anyone raises the chunk size or adds a field to the envelope
    /// without rechecking.
    #[test]
    fn a_full_output_frame_fits_in_one_datagram() {
        let payload = vec![0xffu8; MAX_OUTPUT_BYTES_PER_FRAME];
        let length = frame_len(&payload);
        assert!(
            length <= SAFE_DATAGRAM_BYTES,
            "a full output frame is {length} bytes, over the {SAFE_DATAGRAM_BYTES} \
             budget — SCTP will fragment it and it will vanish on a path with a \
             small MTU, which is the bug this bounds"
        );
    }

    /// Splitting must not lose or reorder anything: the channel is reliable and
    /// ordered, so reassembling the pieces has to give back the original.
    #[test]
    fn splitting_preserves_the_byte_stream() {
        let original: Vec<u8> = (0..5000u32).map(|byte| (byte % 256) as u8).collect();
        let rejoined: Vec<u8> = original
            .chunks(MAX_OUTPUT_BYTES_PER_FRAME)
            .flat_map(<[u8]>::to_vec)
            .collect();
        assert_eq!(rejoined, original);
    }

    /// Every piece is within budget, including the last short one.
    #[test]
    fn no_piece_exceeds_the_limit() {
        let original = vec![0u8; 5000];
        for piece in original.chunks(MAX_OUTPUT_BYTES_PER_FRAME) {
            assert!(piece.len() <= MAX_OUTPUT_BYTES_PER_FRAME);
            assert!(frame_len(piece) <= SAFE_DATAGRAM_BYTES);
        }
    }

    /// An error that goes on the wire must be bounded at the point it is
    /// written, not assumed short.
    ///
    /// The size proof above models `reason` with 120 bytes. Production writes
    /// `format!("{error:#}")` there -- an anyhow chain, which carries every
    /// context layer and, from the CQL driver, can carry a whole statement. A
    /// frame over MAX_CONTROL_FRAME_BYTES (64 KiB) does not arrive, so the
    /// device is told nothing at exactly the moment something went wrong.
    #[test]
    fn a_huge_error_still_fits_a_datagram() {
        // What a driver error with a large statement embedded looks like.
        let enormous = format!("cql: {}", "SELECT * FROM x WHERE y = 1 AND ".repeat(4_000));
        assert!(
            enormous.len() > crate::control_session::MAX_CONTROL_FRAME_BYTES,
            "the fixture must be oversized"
        );

        let reason = bounded_reason(&enormous);
        assert!(
            reason.len() <= MAX_REASON_BYTES,
            "a reason is {} bytes, over the {MAX_REASON_BYTES} bound",
            reason.len()
        );

        let frame = envelope(serde_json::json!({
            "type": "shell_memory_tiers",
            "reachable": false,
            "reason": reason,
            "tiers": [],
        }));
        assert!(
            frame.len() <= SAFE_DATAGRAM_BYTES,
            "an error frame is {} bytes, over the {} safe datagram",
            frame.len(),
            SAFE_DATAGRAM_BYTES
        );
    }

    /// Truncation must not split a character, or the frame stops being JSON.
    #[test]
    fn bounding_a_reason_respects_character_boundaries() {
        // Three bytes each, so the cut lands mid-character unless handled.
        let multibyte = "\u{542b}".repeat(MAX_REASON_BYTES);
        let reason = bounded_reason(&multibyte);
        assert!(reason.len() <= MAX_REASON_BYTES);
        assert!(
            serde_json::to_string(&reason).is_ok(),
            "a bounded reason must still serialise"
        );
    }

    /// A short reason passes through untouched -- the bound must not mangle
    /// the ordinary case.
    #[test]
    fn a_short_reason_is_left_alone() {
        assert_eq!(
            bounded_reason("the memory store could not be reached"),
            "the memory store could not be reached"
        );
    }
}
