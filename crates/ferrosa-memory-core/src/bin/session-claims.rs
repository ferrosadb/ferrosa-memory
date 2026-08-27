//! Module: File the deliverables a session produced as claims awaiting review.
//! Correctness: Correct when running it twice proposes nothing the second time,
//! when a session that produced nothing produces no claims, and when it says
//! what it skipped as loudly as what it filed.
//! Last revised: 2026-08-25
//! Last changed: New.
//!
//! # Why a bin rather than a server loop
//!
//! Nothing wrote claims. The knowledge lifecycle, its queues and its review
//! surface were all built, and the queue stayed empty because no producer
//! existed — the tier looked broken when it was merely unfed.
//!
//! The producer belongs at the end of a TURN, not in the server: the server
//! never sees a transcript, and the transcript is where authorship lives. A
//! Stop hook runs this against the session that just finished; `--all` walks
//! the archive once to catch up on everything already done.
//!
//! # Why it is safe to run repeatedly
//!
//! A claim is identified by the pull request it points at. Before proposing,
//! this reads what the tenant already holds and skips anything whose URL is
//! there — in any state. A rejected claim must not come back the next time the
//! hook runs, which is the difference between a review queue and a treadmill.
//!
//! # Why a deliverable may arrive already decided
//!
//! Everything this finds is filed `proposed` by default, which is what a claim
//! IS: something a model produced and nobody has judged.
//!
//! A backfill over an archive is the exception. Almost every pull request in
//! years of sessions has since been merged or closed, and filing those as
//! "awaiting your review" would put a queue of finished work in front of a
//! person — a review queue that is a lie is worse than an empty one. So
//! `--states` accepts what each pull request actually became, and the
//! deliverable is filed in the state that matches: a merge IS a person
//! ratifying it, and a close without a merge IS a refusal.
//!
//! The state file is separate from the scan on purpose. Which pull requests an
//! agent OPENED is a fact about the session and is read from it; what happened
//! to them afterwards is a fact only the forge holds, and mixing the two would
//! make this tool need network access to answer a question about a local file.
//!
//! # What it deliberately does not do
//!
//! It does not decide anything on its own. Without `--states` every deliverable
//! is a claim, and a state file only ever reports what already happened.

#![allow(deprecated)]

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use ferrosa_memory_core::knowledge::{
    ClaimDraft, CqlKnowledgeStore, KnowledgeState, KnowledgeStore,
};
use ferrosa_memory_core::session_scan::{Deliverable, scan_transcript};
use ferrosa_memory_core::types::TenantContext;
use scylla::SessionBuilder;

/// How long a filed claim has before it lapses.
///
/// Fourteen days rather than the thirty a decision gets: a proposal against a
/// codebase that has since moved is worth less for never having been read, and
/// a pull request goes stale faster than a ratified answer does.
const CLAIM_EXPIRY_DAYS: i64 = 14;

/// How many claims one run may file.
///
/// A bound on WORK, not on the answer: the archive holds years of sessions, and
/// a first run that filed every one of them at once would hand a person a queue
/// nobody could face. What it skips it SAYS, and the next run takes the next
/// batch.
const MAX_PER_RUN: usize = 200;

/// What became of a pull request, according to the forge that holds it.
#[derive(serde::Deserialize, Clone)]
struct Outcome {
    /// `open`, `merged` or `closed`.
    state: String,
    /// Who merged or closed it, when the forge recorded that.
    #[serde(default)]
    by: Option<String>,
}

/// Where a deliverable in this outcome belongs, and who decided it.
///
/// `None` leaves it a claim: `open` genuinely is awaiting review, and an
/// outcome this does not recognise must not be guessed into a decision.
fn decided_as(outcome: &Outcome) -> Option<KnowledgeState> {
    match outcome.state.to_ascii_lowercase().as_str() {
        // Merging IS the ratification. Someone read it and took it.
        "merged" => Some(KnowledgeState::Approved),
        // Closed without a merge is a refusal, which is a decision too.
        "closed" => Some(KnowledgeState::Rejected),
        _ => None,
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    let index = args.iter().position(|a| a == name)?;
    args.get(index + 1).cloned()
}

fn usage() -> &'static str {
    "usage: session-claims --tenant <uuid> [--transcript <path> | --all] [options]\n\
     \n\
     Read what an agent produced from the session that produced it, and file\n\
     each deliverable as a claim awaiting review.\n\
     \n\
     --transcript <path>  one session (what the Stop hook passes)\n\
     --all                every session under --projects, oldest first\n\
     --projects <dir>     where transcripts live (default ~/.claude/projects)\n\
     --tenant <uuid>      whose memory these claims belong to. No default: the\n\
                          task board shares this cluster under its own tenant,\n\
                          and filing into that one puts claims where no reviewer\n\
                          will look.\n\
     --host <host:port>   CQL address, or set FERROSA_CQL_PROXY_ADDR\n\
     --states <path>      JSON {url: {state, by}} saying what each pull\n\
                          request became. Without it everything is a claim.\n\
     --list-repos         print the repositories the scan found, one per line,\n\
                          and exit. Feeds scripts/pr-outcomes.sh, so the set of\n\
                          repositories comes from the sessions rather than a\n\
                          list someone has to remember to update.\n\
     --dry-run            report what WOULD be filed, write nothing\n\
     --limit <n>          cap this run (default 200)"
}

/// Every transcript under a projects directory.
fn transcripts(root: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    let mut found = Vec::new();
    for project in std::fs::read_dir(root)
        .with_context(|| format!("reading {}", root.display()))?
        .flatten()
    {
        if !project.path().is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(project.path())?.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                found.push(path);
            }
        }
    }
    // Oldest first, so a run that hits the cap takes the oldest work and the
    // next run continues rather than repeating.
    found.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    Ok(found)
}

/// Read one transcript, streaming: these run to hundreds of megabytes.
fn scan_file(path: &std::path::Path) -> Result<Vec<Deliverable>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    // A transcript is appended to while it is read — the session may still be
    // running — so an unreadable line is skipped rather than fatal.
    let lines = std::io::BufReader::new(file).lines().map_while(Result::ok);
    Ok(scan_transcript(lines))
}

/// Every pull request URL this tenant already holds a claim or a decision for.
///
/// Read across every state on purpose. Skipping only what is still `proposed`
/// would re-file everything a person had already rejected, on the next run.
async fn already_filed(store: &CqlKnowledgeStore, ctx: &TenantContext) -> Result<HashSet<String>> {
    let mut urls = HashSet::new();
    for state in [
        KnowledgeState::Proposed,
        KnowledgeState::Revisit,
        KnowledgeState::Approved,
        KnowledgeState::Rejected,
        KnowledgeState::Expired,
        KnowledgeState::Superseded,
    ] {
        for band in ["high", "low"] {
            let mut cursor = None;
            loop {
                let page = store.page(ctx, state, band, cursor.as_deref(), 200).await?;
                for item in &page.items {
                    for version in store.versions(ctx, item.knowledge_id).await? {
                        if let Some(url) = version.body_url {
                            urls.insert(url);
                        }
                    }
                }
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
    }
    Ok(urls)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let all = args.iter().any(|a| a == "--all");
    let transcript = arg(&args, "--transcript");
    let limit = arg(&args, "--limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(MAX_PER_RUN);

    let outcomes: std::collections::HashMap<String, Outcome> = match arg(&args, "--states") {
        Some(path) => {
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
            serde_json::from_str(&text).with_context(|| format!("parsing {path}"))?
        }
        None => std::collections::HashMap::new(),
    };

    let Some(tenant) = arg(&args, "--tenant") else {
        eprintln!("{}", usage());
        std::process::exit(2);
    };
    if transcript.is_none() && !all {
        eprintln!("{}", usage());
        std::process::exit(2);
    }
    let host = arg(&args, "--host")
        .or_else(|| std::env::var("FERROSA_CQL_PROXY_ADDR").ok())
        .unwrap_or_else(|| {
            eprintln!("{}", usage());
            std::process::exit(2);
        });

    let paths = match &transcript {
        Some(one) => vec![std::path::PathBuf::from(one)],
        None => {
            let root = arg(&args, "--projects")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                        .join(".claude/projects")
                });
            transcripts(&root)?
        }
    };

    let mut found: Vec<Deliverable> = Vec::new();
    let mut unreadable = 0usize;
    for path in &paths {
        match scan_file(path) {
            Ok(mut some) => found.append(&mut some),
            // Loudly, and keep going: one unreadable transcript must not cost
            // the archive.
            Err(error) => {
                unreadable += 1;
                eprintln!("!! could not read {}: {error:#}", path.display());
            }
        }
    }
    found.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.url.cmp(&b.url)));
    found.dedup_by(|a, b| a.url == b.url);

    // On stderr: `--list-repos` is meant to be piped, and a status line in the
    // middle of that pipe becomes a repository name nobody can query.
    eprintln!(
        "scanned {} session{}, found {} deliverable{}{}",
        paths.len(),
        if paths.len() == 1 { "" } else { "s" },
        found.len(),
        if found.len() == 1 { "" } else { "s" },
        if unreadable > 0 {
            format!(", {unreadable} unreadable")
        } else {
            String::new()
        }
    );

    if args.iter().any(|a| a == "--list-repos") {
        let mut repos: Vec<&str> = found.iter().filter_map(|d| d.repo.as_deref()).collect();
        repos.sort_unstable();
        repos.dedup();
        for repo in repos {
            println!("{repo}");
        }
        return Ok(());
    }

    let session = Arc::new(
        SessionBuilder::new()
            .known_node(&host)
            .connection_timeout(std::time::Duration::from_secs(5))
            .build_legacy()
            .await
            .with_context(|| format!("connecting to {host}"))?,
    );
    let store = CqlKnowledgeStore::new(session, "agent_memory");
    let ctx = TenantContext {
        tenant_id: tenant.parse().context("--tenant must be a UUID")?,
        session_origin: "session-claims".to_owned(),
    };

    let seen = already_filed(&store, &ctx).await?;
    let fresh: Vec<&Deliverable> = found.iter().filter(|d| !seen.contains(&d.url)).collect();
    println!(
        "  {} already filed, {} new",
        found.len() - fresh.len(),
        fresh.len()
    );

    let taking = fresh.len().min(limit);
    if fresh.len() > taking {
        // Never a silent cap: a run that quietly filed 200 of 900 would read as
        // "that is all there is".
        println!(
            "  filing {taking} of {} this run; run again for the rest",
            fresh.len()
        );
    }

    if !outcomes.is_empty() {
        let unknown = fresh
            .iter()
            .take(taking)
            .filter(|d| !outcomes.contains_key(&d.url))
            .count();
        if unknown > 0 {
            // Filed as claims, which is the safe default, but SAID: a merged
            // pull request landing in the review queue because its repository
            // was missing from the outcome file is a queue full of finished
            // work, and the cause is invisible from the queue itself.
            println!(
                "  {unknown} have no recorded outcome and will be filed as claims; \
                 check scripts/pr-outcomes.sh covers their repositories"
            );
        }
    }

    let mut filed_as: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for deliverable in fresh.iter().take(taking) {
        let outcome = outcomes.get(&deliverable.url);
        let decision = outcome.and_then(decided_as);
        let landing = match decision {
            Some(KnowledgeState::Approved) => "approved",
            Some(KnowledgeState::Rejected) => "rejected",
            _ => "claim",
        };
        *filed_as.entry(landing).or_default() += 1;
        if dry_run {
            println!(
                "  would file as {landing}: {} [{}]",
                deliverable.title,
                deliverable.repo.as_deref().unwrap_or("?")
            );
            continue;
        }
        let draft = ClaimDraft {
            title: deliverable.title.clone(),
            kind: deliverable.kind.clone(),
            body_url: Some(deliverable.url.clone()),
            summary: None,
            author_agent: Some("claude".to_owned()),
            author_session: deliverable.session_id,
            task_id: None,
            // Unset rather than guessed. A priority inferred from the title
            // would sort the queue by a number nobody chose.
            priority: 50,
            repo: deliverable.repo.clone(),
            expires_in_days: CLAIM_EXPIRY_DAYS,
        };
        let item = match store.propose(&ctx, draft).await {
            Ok(item) => item,
            Err(error) => {
                eprintln!("!! could not file {}: {error:#}", deliverable.url);
                continue;
            }
        };
        // A merge or a close already happened; recording it moves the row out
        // of the review queue rather than leaving finished work in it.
        if let Some(decision) = decision {
            let reviewer = outcome
                .and_then(|o| o.by.clone())
                .unwrap_or_else(|| "recorded from the forge".to_owned());
            if let Err(error) = store
                .decide(&ctx, item.knowledge_id, decision, Some(&reviewer), None)
                .await
            {
                eprintln!(
                    "!! filed {} but could not record its outcome: {error:#}",
                    item.knowledge_id
                );
                continue;
            }
        }
        println!("  {landing}: {}", item.title);
    }

    let summary: Vec<String> = filed_as
        .iter()
        .map(|(where_it_went, count)| format!("{count} {where_it_went}"))
        .collect();
    if !summary.is_empty() {
        println!(
            "{} {}",
            if dry_run { "would file:" } else { "filed:" },
            summary.join(", ")
        );
    }

    if dry_run {
        println!("dry run: nothing written");
    }
    Ok(())
}
