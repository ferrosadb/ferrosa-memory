//! Module: Turn identifiers written in task prose into real links on the board.
//! Correctness: Correct when running it twice changes nothing the second time,
//! when a token nobody would follow is not linked, and when it says what it did
//! rather than reporting a number.
//! Last revised: 2026-08-24
//! Last changed: Scoped the legacy Scylla row API to this board-maintenance boundary.
//!
//! # Why this exists
//!
//! The board records links, and almost nothing uses them: one blocked task on
//! the live board had zero explicit links and ten other tasks talking about the
//! same ticket, several of them duplicates of each other. People cross-
//! reference by writing the identifier into the title — `Decide QA-0009: fix
//! the entity_type example` — because that is the only place there is to put
//! it.
//!
//! Reading those out on demand answers one question at a time. This writes them
//! down, so the answer exists before the question.
//!
//! # What it links
//!
//! Tasks that name the same identifier are linked to a HUB — the oldest open
//! task naming it — rather than to each other. Linking every pair is quadratic
//! and, for a token in fifty tasks, produces a graph nobody can read. A hub
//! answers the question that gets asked: "what else is about this".
//!
//! Idempotent. `task_links` is keyed by (src, type, dst), so re-running
//! rewrites the same rows; the run still reports what it would have created so
//! a second pass visibly does nothing.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use ferrosa_memory_sync::task_board::identifiers_in;
use scylla::frame::response::result::CqlValue;
// Match the task-board reader and ferrosa-memory-core's `CqlSession` until the
// shared dynamic row decoder is migrated as one unit.
#[allow(deprecated)]
use scylla::{LegacySession, SessionBuilder};

const TENANT_ID: &str = "00000000-0000-0000-0000-000000000001";
const OPEN_STATUSES: [&str; 4] = ["triage", "ready", "in_progress", "blocked"];

/// Identifiers named by more than this many tasks are reported, not linked.
///
/// A token in thirty tasks is a topic, not a cross-reference, and hanging
/// thirty links off one hub buries the ones that mean something. Reported
/// because a cluster that large is usually duplicates and worth a person's
/// attention — which is the finding, not a failure.
const CLUSTER_LIMIT: usize = 15;

struct Task {
    id: String,
    title: String,
    block_reason: String,
    updated_at: i64,
}

#[tokio::main]
#[allow(deprecated)]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|arg| arg == "--dry-run");
    let quiet = args.iter().any(|arg| arg == "--quiet");
    let host = args
        .iter()
        .position(|arg| arg == "--host")
        .and_then(|index| args.get(index + 1))
        .cloned()
        .or_else(|| std::env::var("FORGE_CQL_HOST").ok())
        .unwrap_or_else(|| "127.0.0.1:9042".to_owned());

    let session = SessionBuilder::new()
        .known_node(&host)
        .connection_timeout(std::time::Duration::from_secs(5))
        .build_legacy()
        .await
        .with_context(|| format!("connecting to the task board at {host}"))?;

    let tasks = read_open_tasks(&session).await?;
    if !quiet {
        println!("{} open task(s) on the board", tasks.len());
    }

    // Which tasks name which identifier. Title and block reason only: a body
    // quotes ids in passing, a title names what the task is ABOUT.
    let mut by_token: BTreeMap<String, Vec<&Task>> = BTreeMap::new();
    for task in &tasks {
        for token in identifiers_in(&format!("{} {}", task.title, task.block_reason)) {
            by_token.entry(token).or_default().push(task);
        }
    }

    let mut written = 0usize;
    let mut clusters = 0usize;
    for (token, mut naming) in by_token {
        if naming.len() < 2 {
            continue;
        }
        if naming.len() > CLUSTER_LIMIT {
            clusters += 1;
            if !quiet {
                println!(
                    "  {token}: named by {} tasks — too many to link, likely duplicates",
                    naming.len()
                );
            }
            continue;
        }
        // If the identifier IS a task on this board, that task is the hub.
        // Pointing "everything that mentions t_9192783e" at the oldest task
        // that happens to mention it, rather than at t_9192783e itself, is
        // exactly backwards — and it is the common case, because quoting
        // another task's id is how tasks reference each other here.
        naming.sort_by_key(|task| task.updated_at);
        let hub = match tasks.iter().find(|task| task.id == token) {
            Some(named) => named,
            None => naming[0],
        };
        for task in naming.iter().filter(|task| task.id != hub.id) {
            if !quiet {
                println!("  {} -> {} ({token})", task.id, hub.id);
            }
            if !dry_run {
                link(&session, &task.id, &format!("mentions:{token}"), &hub.id).await?;
            }
            written += 1;
        }
    }

    println!(
        "{} link(s) {}{}",
        written,
        if dry_run {
            "would be written"
        } else {
            "written"
        },
        if clusters > 0 {
            format!(", {clusters} cluster(s) left unlinked")
        } else {
            String::new()
        }
    );
    Ok(())
}

#[allow(deprecated)]
async fn read_open_tasks(session: &LegacySession) -> Result<Vec<Task>> {
    #[allow(deprecated)]
    let result = session
        .query_unpaged(
            format!(
                "SELECT task_id, title, status, block_reason, updated_at \
                 FROM agent_memory.tasks WHERE tenant_id={TENANT_ID}"
            ),
            (),
        )
        .await
        .context("reading the task board")?;
    let columns: BTreeMap<String, usize> = result
        .col_specs()
        .iter()
        .enumerate()
        .map(|(index, spec)| (spec.name().to_owned(), index))
        .collect();

    let mut tasks = Vec::new();
    for row in result.rows_or_empty() {
        let text = |name: &str| -> Option<String> {
            match row.columns.get(*columns.get(name)?)? {
                Some(CqlValue::Text(value)) | Some(CqlValue::Ascii(value)) => Some(value.clone()),
                _ => None,
            }
        };
        let (Some(id), Some(status)) = (text("task_id"), text("status")) else {
            continue;
        };
        if !OPEN_STATUSES.contains(&status.as_str()) {
            continue;
        }
        let updated_at = columns
            .get("updated_at")
            .and_then(|index| row.columns.get(*index))
            .and_then(|value| match value {
                Some(CqlValue::BigInt(value)) => Some(*value),
                Some(CqlValue::Int(value)) => Some(i64::from(*value)),
                _ => None,
            })
            .unwrap_or(0);
        tasks.push(Task {
            id,
            title: text("title").unwrap_or_default(),
            block_reason: text("block_reason").unwrap_or_default(),
            updated_at,
        });
    }
    Ok(tasks)
}

#[allow(deprecated)]
async fn link(session: &LegacySession, src: &str, kind: &str, dst: &str) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    #[allow(deprecated)]
    session
        .query_unpaged(
            format!(
                "INSERT INTO agent_memory.task_links \
                 (tenant_id, src_task_id, link_type, dst_task_id, created_at) \
                 VALUES ({TENANT_ID}, ?, ?, ?, ?)"
            ),
            (src, kind, dst, now),
        )
        .await
        .context("writing a task link")?;
    Ok(())
}
