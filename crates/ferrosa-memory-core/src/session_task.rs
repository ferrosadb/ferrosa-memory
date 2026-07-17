//! Durable active-task state for agent sessions.
//!
//! Responsibility: provide fmem-owned task identity, lifecycle helpers, scoped
//! aliases, and deterministic focus-stack recovery for clients that otherwise
//! keep only in-memory todo state.
//! Correctness: task ids are generated here, normal lifecycle never hard
//! deletes, and current-task recovery reads the explicit focus stack before
//! falling back to non-terminal task ordering.
//! Last revised: 2026-07-17
//! Last changed: add workspace-scoped active task recovery across sessions.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::storage::Storage;
use crate::types::{
    SessionTask, SessionTaskAlias, SessionTaskClient, SessionTaskEvent, SessionTaskFocusEntry,
    SessionTaskPolicy, SessionTaskStatus, TenantContext,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionTaskUpsert {
    pub session_id: Uuid,
    pub task_id: Option<Uuid>,
    pub title: String,
    pub description: Option<String>,
    pub status: Option<SessionTaskStatus>,
    pub priority: Option<i32>,
    pub tags: Vec<String>,
    pub parent_task_id: Option<Uuid>,
    pub client: SessionTaskClient,
    pub alias_scope: Option<String>,
    pub alias: Option<String>,
    pub focus: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTaskSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground: Option<SessionTask>,
    pub active_tasks: Vec<SessionTask>,
    pub focus_stack: Vec<SessionTaskFocusEntry>,
    pub recovery_hints: Vec<String>,
    pub coordination_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTaskWriteResult {
    pub task: SessionTask,
    pub action: String,
    pub generated_task_id: bool,
    pub snapshot: SessionTaskSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTaskLifecycleResult {
    pub task: SessionTask,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_candidate: Option<SessionTask>,
    pub snapshot: SessionTaskSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTaskObserveResult {
    pub action: String,
    pub hints: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<SessionTask>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_candidate: Option<SessionTask>,
    pub snapshot: SessionTaskSnapshot,
}

pub async fn put_task<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    input: SessionTaskUpsert,
) -> anyhow::Result<SessionTaskWriteResult> {
    validate_title(&input.title)?;
    let now = chrono::Utc::now();
    let generated_task_id = input.task_id.is_none();
    let task_id = input.task_id.unwrap_or_else(Uuid::now_v7);
    let existing = storage
        .session_task_get(ctx, input.session_id, task_id)
        .await?;
    let status = input.status.unwrap_or_else(|| {
        existing
            .as_ref()
            .map(|task| task.status.clone())
            .unwrap_or(SessionTaskStatus::InProgress)
    });
    let completed_at = if status.is_terminal() {
        existing
            .as_ref()
            .and_then(|task| task.completed_at)
            .or(Some(now))
    } else {
        None
    };

    let mut task = SessionTask {
        session_id: input.session_id,
        task_id,
        title: input.title,
        description: input.description.unwrap_or_default(),
        status: status.clone(),
        priority: input.priority.unwrap_or(100),
        tags: normalize_tags(input.tags),
        parent_task_id: input.parent_task_id,
        focus_rank: 0,
        client: input.client,
        outcome_summary: existing
            .as_ref()
            .and_then(|task| task.outcome_summary.clone()),
        created_at: existing.as_ref().map_or(now, |task| task.created_at),
        updated_at: now,
        completed_at,
    };

    if let Some(existing) = existing
        && !input.focus
    {
        task.focus_rank = existing.focus_rank;
    }

    storage.session_task_put(ctx, &task).await?;

    if let (Some(alias_scope), Some(alias)) = (input.alias_scope, input.alias) {
        put_alias(
            storage,
            ctx,
            task.session_id,
            task.task_id,
            &alias_scope,
            &alias,
        )
        .await?;
    }

    if input.focus {
        focus_task(storage, ctx, task.session_id, task.task_id, "put_task").await?;
    }

    record_event(
        storage,
        ctx,
        task.session_id,
        Some(task.task_id),
        "task_put",
        serde_json::json!({"generated_task_id": generated_task_id}),
    )
    .await?;

    let snapshot = current_tasks(storage, ctx, task.session_id).await?;
    Ok(SessionTaskWriteResult {
        task,
        action: if generated_task_id {
            "created".to_string()
        } else {
            "upserted".to_string()
        },
        generated_task_id,
        snapshot,
    })
}

pub async fn get_task<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    task_id: Uuid,
) -> anyhow::Result<Option<SessionTask>> {
    storage.session_task_get(ctx, session_id, task_id).await
}

pub async fn resolve_alias<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    alias_scope: &str,
    alias: &str,
) -> anyhow::Result<Option<SessionTask>> {
    let Some(alias) = storage
        .session_task_alias_get(ctx, session_id, alias_scope, alias)
        .await?
    else {
        return Ok(None);
    };
    storage
        .session_task_get(ctx, session_id, alias.task_id)
        .await
}

pub async fn list_tasks<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    status: Option<SessionTaskStatus>,
) -> anyhow::Result<Vec<SessionTask>> {
    storage.session_task_list(ctx, session_id, status).await
}

pub async fn active_workspace_tasks<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    workspace: &str,
) -> anyhow::Result<Vec<SessionTask>> {
    let workspace = workspace.trim();
    if workspace.is_empty() {
        return Ok(Vec::new());
    }

    let mut tasks = Vec::new();
    for status in active_task_statuses() {
        tasks.extend(
            storage
                .session_task_list_by_workspace(ctx, workspace, status)
                .await?,
        );
    }
    tasks.retain(|task| {
        !task.status.is_terminal()
            && task
                .client
                .workspace
                .as_deref()
                .is_some_and(|candidate| candidate.trim() == workspace)
    });
    sort_session_tasks(&mut tasks);
    Ok(tasks)
}

pub async fn current_tasks<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
) -> anyhow::Result<SessionTaskSnapshot> {
    let focus_stack = storage.session_task_focus_get(ctx, session_id).await?;
    let active_tasks = active_tasks(storage, ctx, session_id).await?;

    let mut foreground = None;
    for entry in &focus_stack {
        if let Some(task) = storage
            .session_task_get(ctx, session_id, entry.task_id)
            .await?
            .filter(|task| !task.status.is_terminal())
        {
            foreground = Some(task);
            break;
        }
    }

    if foreground.is_none() {
        foreground = active_tasks.first().cloned();
    }

    Ok(SessionTaskSnapshot {
        foreground,
        active_tasks: active_tasks.clone(),
        focus_stack,
        recovery_hints: recovery_hints(&active_tasks),
        coordination_hint: coordination_hint(&active_tasks),
    })
}

pub async fn update_status<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    task_id: Uuid,
    status: SessionTaskStatus,
    outcome_summary: Option<String>,
) -> anyhow::Result<SessionTaskLifecycleResult> {
    let mut task = storage
        .session_task_get(ctx, session_id, task_id)
        .await?
        .with_context(|| format!("session task not found: {task_id}"))?;
    let now = chrono::Utc::now();
    task.status = status.clone();
    task.updated_at = now;
    if let Some(summary) = outcome_summary {
        task.outcome_summary = Some(summary);
    }
    if status.is_terminal() {
        task.completed_at = Some(now);
    }
    storage.session_task_put(ctx, &task).await?;

    if status.is_terminal() {
        remove_from_focus_stack(storage, ctx, session_id, task_id).await?;
    }

    record_event(
        storage,
        ctx,
        session_id,
        Some(task_id),
        "task_status",
        serde_json::json!({"status": status}),
    )
    .await?;

    let snapshot = current_tasks(storage, ctx, session_id).await?;
    let resume_candidate = if status.is_terminal() {
        snapshot.foreground.clone()
    } else {
        None
    };
    let policy = get_policy(storage, ctx, session_id).await?;
    let action = if status.is_terminal() {
        match (resume_candidate.is_some(), policy.auto_resume.as_str()) {
            (true, "inject") => "inject_auto_resume",
            (true, _) => "ask_user_to_resume",
            (false, _) => "completed_without_resume_candidate",
        }
    } else {
        "updated"
    }
    .to_string();

    Ok(SessionTaskLifecycleResult {
        task,
        action,
        resume_candidate,
        snapshot,
    })
}

pub async fn focus_task<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    task_id: Uuid,
    reason: &str,
) -> anyhow::Result<SessionTaskSnapshot> {
    let task = storage
        .session_task_get(ctx, session_id, task_id)
        .await?
        .with_context(|| format!("session task not found: {task_id}"))?;
    if task.status.is_terminal() {
        anyhow::bail!("cannot focus terminal session task: {task_id}");
    }

    let now = chrono::Utc::now();
    let mut old_stack = storage.session_task_focus_get(ctx, session_id).await?;
    old_stack.retain(|entry| entry.task_id != task_id);
    let mut entries = Vec::with_capacity(old_stack.len() + 1);
    entries.push(SessionTaskFocusEntry {
        session_id,
        stack_index: 0,
        task_id,
        reason: reason.to_string(),
        created_at: now,
    });
    for (idx, mut entry) in old_stack.into_iter().enumerate() {
        entry.stack_index = (idx + 1) as i32;
        entries.push(entry);
    }
    storage
        .session_task_focus_set(ctx, session_id, &entries)
        .await?;
    refresh_focus_ranks(storage, ctx, session_id, &entries).await?;
    record_event(
        storage,
        ctx,
        session_id,
        Some(task_id),
        "task_focus",
        serde_json::json!({"reason": reason}),
    )
    .await?;
    current_tasks(storage, ctx, session_id).await
}

pub async fn observe<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    event_type: &str,
    title: Option<String>,
    task_id: Option<Uuid>,
    payload: serde_json::Value,
) -> anyhow::Result<SessionTaskObserveResult> {
    match event_type {
        "user_requested_new_task" | "user_requested_switch" => {
            let title = title.context("title is required for new task observation")?;
            let input = SessionTaskUpsert {
                session_id,
                title,
                description: payload
                    .get("description")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                tags: payload
                    .get("tags")
                    .and_then(|value| value.as_array())
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(|value| value.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                focus: true,
                ..SessionTaskUpsert::default_with_session(session_id)
            };
            let written = put_task(storage, ctx, input).await?;
            Ok(SessionTaskObserveResult {
                action: "push_and_focus_new".to_string(),
                hints: vec!["previous focus was pushed down the stack if it existed".to_string()],
                task: Some(written.task),
                resume_candidate: None,
                snapshot: written.snapshot,
            })
        }
        "task_completed" => {
            let task_id = task_id
                .or_else(|| {
                    payload
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .and_then(|s| Uuid::parse_str(s).ok())
                })
                .context("task_id is required for task_completed observation")?;
            let result = update_status(
                storage,
                ctx,
                session_id,
                task_id,
                SessionTaskStatus::Completed,
                payload
                    .get("outcome_summary")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            )
            .await?;
            Ok(SessionTaskObserveResult {
                action: result.action,
                hints: resume_hints(result.resume_candidate.as_ref()),
                task: Some(result.task),
                resume_candidate: result.resume_candidate,
                snapshot: result.snapshot,
            })
        }
        "agent_lost" | "context_reset" => {
            let snapshot = current_tasks(storage, ctx, session_id).await?;
            Ok(SessionTaskObserveResult {
                action: "refresh_current_before_writing".to_string(),
                hints: vec![
                    "call session_task_current before creating new tasks".to_string(),
                    "if the current task is not relevant, explicitly switch focus".to_string(),
                    "use temporal turn_chain/context windows from recovered task metadata when available".to_string(),
                ],
                task: snapshot.foreground.clone(),
                resume_candidate: snapshot.foreground.clone(),
                snapshot,
            })
        }
        _ => {
            let snapshot = current_tasks(storage, ctx, session_id).await?;
            Ok(SessionTaskObserveResult {
                action: "continue_current".to_string(),
                hints: vec!["no deterministic v1 task transition matched".to_string()],
                task: snapshot.foreground.clone(),
                resume_candidate: None,
                snapshot,
            })
        }
    }
}

pub async fn get_policy<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
) -> anyhow::Result<SessionTaskPolicy> {
    Ok(storage
        .session_task_policy_get(ctx, session_id)
        .await?
        .unwrap_or_else(|| SessionTaskPolicy {
            session_id,
            ..SessionTaskPolicy::default()
        }))
}

pub async fn put_policy<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    mut policy: SessionTaskPolicy,
) -> anyhow::Result<SessionTaskPolicy> {
    policy.updated_at = chrono::Utc::now();
    storage.session_task_policy_put(ctx, &policy).await?;
    Ok(policy)
}

impl SessionTaskUpsert {
    pub fn default_with_session(session_id: Uuid) -> Self {
        Self {
            session_id,
            task_id: None,
            title: String::new(),
            description: None,
            status: Some(SessionTaskStatus::InProgress),
            priority: Some(100),
            tags: Vec::new(),
            parent_task_id: None,
            client: SessionTaskClient::default(),
            alias_scope: None,
            alias: None,
            focus: true,
        }
    }
}

async fn put_alias<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    task_id: Uuid,
    alias_scope: &str,
    alias: &str,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    storage
        .session_task_alias_put(
            ctx,
            &SessionTaskAlias {
                session_id,
                alias_scope: alias_scope.to_string(),
                alias: alias.to_string(),
                task_id,
                created_at: now,
                updated_at: now,
            },
        )
        .await
}

async fn remove_from_focus_stack<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    task_id: Uuid,
) -> anyhow::Result<()> {
    let old_stack = storage.session_task_focus_get(ctx, session_id).await?;
    let mut entries = Vec::new();
    for entry in old_stack
        .into_iter()
        .filter(|entry| entry.task_id != task_id)
    {
        if let Some(task) = storage
            .session_task_get(ctx, session_id, entry.task_id)
            .await?
            .filter(|task| !task.status.is_terminal())
        {
            entries.push(SessionTaskFocusEntry {
                stack_index: entries.len() as i32,
                ..entry
            });
            let mut task = task;
            task.focus_rank = entries.len() as i32 - 1;
            task.updated_at = chrono::Utc::now();
            storage.session_task_put(ctx, &task).await?;
        }
    }
    storage
        .session_task_focus_set(ctx, session_id, &entries)
        .await
}

async fn refresh_focus_ranks<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    entries: &[SessionTaskFocusEntry],
) -> anyhow::Result<()> {
    for entry in entries {
        if let Some(mut task) = storage
            .session_task_get(ctx, session_id, entry.task_id)
            .await?
        {
            task.focus_rank = entry.stack_index;
            task.updated_at = chrono::Utc::now();
            storage.session_task_put(ctx, &task).await?;
        }
    }
    Ok(())
}

async fn active_tasks<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
) -> anyhow::Result<Vec<SessionTask>> {
    let mut tasks = Vec::new();
    for status in active_task_statuses() {
        tasks.extend(
            storage
                .session_task_list(ctx, session_id, Some(status))
                .await?,
        );
    }
    tasks.retain(|task| !task.status.is_terminal());
    sort_session_tasks(&mut tasks);
    Ok(tasks)
}

fn active_task_statuses() -> [SessionTaskStatus; 3] {
    [
        SessionTaskStatus::InProgress,
        SessionTaskStatus::Blocked,
        SessionTaskStatus::Pending,
    ]
}

fn sort_session_tasks(tasks: &mut [SessionTask]) {
    tasks.sort_by(|a, b| {
        a.focus_rank
            .cmp(&b.focus_rank)
            .then(a.priority.cmp(&b.priority))
            .then(b.updated_at.cmp(&a.updated_at))
            .then(a.task_id.cmp(&b.task_id))
    });
}

async fn record_event<S: Storage>(
    storage: &S,
    ctx: &TenantContext,
    session_id: Uuid,
    task_id: Option<Uuid>,
    event_type: &str,
    payload: serde_json::Value,
) -> anyhow::Result<()> {
    storage
        .session_task_event_put(
            ctx,
            &SessionTaskEvent {
                session_id,
                event_id: Uuid::now_v7(),
                task_id,
                event_type: event_type.to_string(),
                payload,
                created_at: chrono::Utc::now(),
            },
        )
        .await
}

fn validate_title(title: &str) -> anyhow::Result<()> {
    if title.trim().is_empty() {
        anyhow::bail!("session task title must not be empty");
    }
    Ok(())
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut tags: Vec<_> = tags
        .into_iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

fn recovery_hints(active_tasks: &[SessionTask]) -> Vec<String> {
    let mut hints = vec![
        "authoritative current task state came from session_task_current".to_string(),
        "call session_task_get for full task detail before writing after compaction".to_string(),
        "use temporal turn_chain/context windows from task-linked events when reconstructing prior turns".to_string(),
    ];
    if active_tasks.len() > 5 {
        hints.push(
            "active task count is high; recommend sub-agents or task decomposition".to_string(),
        );
    }
    hints
}

fn coordination_hint(active_tasks: &[SessionTask]) -> String {
    if active_tasks.len() > 5 {
        "recommend_subagents".to_string()
    } else {
        "continue_current".to_string()
    }
}

fn resume_hints(candidate: Option<&SessionTask>) -> Vec<String> {
    if let Some(task) = candidate {
        vec![format!(
            "resume candidate: {} ({})",
            task.title, task.task_id
        )]
    } else {
        vec!["no suspended task remains on the focus stack".to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::mock::MockStorage;

    fn ctx() -> TenantContext {
        TenantContext {
            tenant_id: Uuid::new_v4(),
            session_origin: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn put_generates_task_id_and_focuses_task() {
        let store = MockStorage::new();
        let ctx = ctx();
        let session_id = Uuid::new_v4();

        let result = put_task(
            &store,
            &ctx,
            SessionTaskUpsert {
                session_id,
                title: "Implement durable tasks".to_string(),
                description: Some("Keep current work across compaction".to_string()),
                tags: vec!["Hermes".to_string(), "hermes".to_string()],
                alias_scope: Some("thread:test".to_string()),
                alias: Some("current".to_string()),
                ..SessionTaskUpsert::default_with_session(session_id)
            },
        )
        .await
        .unwrap();

        assert!(result.generated_task_id);
        assert_ne!(result.task.task_id, Uuid::nil());
        assert_eq!(result.task.tags, vec!["hermes"]);
        assert_eq!(
            result.snapshot.foreground.as_ref().map(|task| task.task_id),
            Some(result.task.task_id)
        );

        let resolved = resolve_alias(&store, &ctx, session_id, "thread:test", "current")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resolved.task_id, result.task.task_id);
    }

    #[tokio::test]
    async fn focus_pushes_old_task_and_complete_returns_resume_candidate() {
        let store = MockStorage::new();
        let ctx = ctx();
        let session_id = Uuid::new_v4();
        let first = put_task(
            &store,
            &ctx,
            SessionTaskUpsert {
                session_id,
                title: "First task".to_string(),
                ..SessionTaskUpsert::default_with_session(session_id)
            },
        )
        .await
        .unwrap()
        .task;
        let second = put_task(
            &store,
            &ctx,
            SessionTaskUpsert {
                session_id,
                title: "Second task".to_string(),
                ..SessionTaskUpsert::default_with_session(session_id)
            },
        )
        .await
        .unwrap()
        .task;

        let current = current_tasks(&store, &ctx, session_id).await.unwrap();
        assert_eq!(current.foreground.unwrap().task_id, second.task_id);
        assert_eq!(current.focus_stack[1].task_id, first.task_id);

        let completed = update_status(
            &store,
            &ctx,
            session_id,
            second.task_id,
            SessionTaskStatus::Completed,
            Some("done".to_string()),
        )
        .await
        .unwrap();

        assert_eq!(completed.action, "ask_user_to_resume");
        assert_eq!(completed.resume_candidate.unwrap().task_id, first.task_id);
        assert_eq!(
            completed.snapshot.foreground.unwrap().task_id,
            first.task_id
        );
    }
}
