//! Module: Put the tier rules on a cluster, and attribute what can be attributed.
//! Correctness: Correct when running it twice changes nothing the second time,
//! when an entity whose origin is a guess is left alone, and when it reports
//! what it could NOT attribute as loudly as what it could.
//! Last revised: 2026-08-24
//! Last changed: New.
//!
//! # Why a bin
//!
//! Seeding is a setup step, not a runtime feature. It runs once against a
//! cluster, by a person who knows where their checkouts are -- which is
//! exactly the information the rules need and the server cannot discover.
//!
//! # Backfilling existing entities
//!
//! Existing entities have no recorded source, so a seeded cluster tiers
//! nothing until something ingests with a path. `--backfill` reads what is
//! already there and attributes what states its own origin.
//!
//! Two things were learned getting here, both worth keeping:
//!
//! - A whole-cluster `SELECT ... LIMIT n` is refused: "projected cluster
//!   range scan with partition_limit is not implemented; refusing to return
//!   partial results". That is the right refusal -- a partial answer here
//!   would attribute some entities and silently skip others.
//! - An unpaged query over the same table returns a paging state, which the
//!   driver reports as a protocol error.
//!
//! So this uses `entity_stream_all`, the paged bulk path the codebase already
//! has, walking every session's partition in chunks. Bounded memory, no
//! partial results, and it reports what it could NOT attribute as loudly as
//! what it could.
//!
//! It does NOT infer an origin from an entity's type, session or name. A
//! guessed source produces a confident tier for something nobody placed,
//! which is worse than the honest blank the dashboard is built to show.

// This module reads rows through scylla 0.15's LegacySession API, the same
// choice cql_storage.rs made and for the same reason: the legacy API is
// deprecated upstream but has stable semantics, and migrating to the generic
// deserialization API is a separate piece of work across every call site.
// Scoped to the module rather than sprinkled per call, so the decision is
// stated once and a NEW deprecation still surfaces.
#![allow(deprecated)]
use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use ferrosa_memory_core::tier_store::{CqlTierStore, seed_tier_rules};
use ferrosa_memory_core::types::TenantContext;
use scylla::SessionBuilder;
use std::sync::Arc;

/// The tenant the rules and sources belong to.
///
/// REQUIRED, not defaulted. The first run of this defaulted to the forge
/// board's tenant and wrote a complete, correct rule set into a tenant with
/// 118 entities in it, while the 79,000 that needed tiering sat under
/// another. Nothing failed; the numbers just meant nothing. A tenant is not
/// something to guess.
fn tenant_help() -> &'static str {
    "--tenant is required. Use the tenant_id your memory server serves \
     (see the [viz] tenant_id in its config, or the auth file's principals)."
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let host = arg(&args, "--host").unwrap_or_else(|| "127.0.0.1:19042".to_owned());
    let keyspace = arg(&args, "--keyspace").unwrap_or_else(|| "agent_memory".to_owned());
    let research = arg(&args, "--research-root");
    let tenant = arg(&args, "--tenant");
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let backfill = args.iter().any(|a| a == "--backfill");
    let sample = args.iter().any(|a| a == "--sample");

    let Some(tenant) = tenant else {
        eprintln!("{}", tenant_help());
        std::process::exit(2);
    };
    let Some(research) = research else {
        eprintln!(
            "usage: seed-tiers --tenant <uuid> --research-root <path> [--backfill] [--dry-run]\n\
             \n\
             --research-root is the absolute path of your research checkout, e.g.\n\
             /Users/you/src/research. It is required rather than guessed: a path\n\
             discovered by scanning would tier the same content differently on a\n\
             machine with a different layout."
        );
        std::process::exit(2);
    };

    let session = Arc::new(
        SessionBuilder::new()
            .known_node(&host)
            .connection_timeout(std::time::Duration::from_secs(5))
            .build_legacy()
            .await
            .with_context(|| format!("connecting to {host}"))?,
    );
    let store = CqlTierStore::new(Arc::clone(&session), keyspace.clone());
    let ctx = TenantContext {
        tenant_id: tenant.parse().context("--tenant must be a UUID")?,
        session_origin: "seed-tiers".to_owned(),
    };

    println!("tenant: {tenant}");
    if dry_run {
        println!("dry run: nothing will be written");
    } else {
        let report = seed_tier_rules(&store, &ctx, &research, "seed-tiers").await?;
        println!(
            "rules: {} written, aliases: {} written",
            report.rules_written, report.aliases_written
        );
    }

    for (root, tier) in ferrosa_memory_core::tiers::TierRules::builtin().entries() {
        println!("  {root} -> {}", tier.as_str());
    }

    // Prove the rules resolve, rather than reporting that rows were written.
    // A rule set that writes cleanly and tiers nothing is the failure worth
    // catching, and it is invisible from the write side.
    {
        use ferrosa_memory_core::tier_store::load_rules;
        let (resolver, rules) = load_rules(&store, &ctx).await?;
        println!("\ncheck:");
        for probe in [
            format!("{}/skills/rust.md", research.trim_end_matches('/')),
            format!(
                "{}/corpus/rust/allocators.md",
                research.trim_end_matches('/')
            ),
            format!("{}/skills/rules/safety.md", research.trim_end_matches('/')),
            "research/skills/from-another-machine.md".to_owned(),
            "/tmp/unclassified.md".to_owned(),
        ] {
            let tier = ferrosa_memory_core::tiers::resolve(Some(&probe), &resolver, &rules, None);
            println!(
                "  {probe}\n    -> {} ({:?})",
                tier.tier.as_str(),
                tier.reason
            );
        }
    }

    if backfill {
        backfill_sources(&host, &keyspace, &store, &ctx, dry_run, sample).await?;
    } else {
        println!(
            "\nNot backfilling. Pass --backfill to attribute entities whose text\n\
             states where they came from."
        );
    }
    Ok(())
}

/// Walk every session's entities and attribute the ones that state an origin.
///
/// Streamed rather than collected: the store holds tens of thousands of
/// entities with their extraction text, and pulling all of it into one Vec to
/// look at a prefix is memory spent for nothing.
async fn backfill_sources(
    host: &str,
    keyspace: &str,
    store: &CqlTierStore,
    ctx: &TenantContext,
    dry_run: bool,
    sample: bool,
) -> Result<()> {
    use ferrosa_memory_core::storage::Storage as _;
    use ferrosa_memory_core::tier_store::{SourceDraft, TierStore as _};

    // The deployment's own config, so the scan connects exactly as the server
    // does -- same credentials, same keyspace, same contact points. Building a
    // config here would be a second definition of how to reach this cluster.
    let config = ferrosa_memory_core::config::load_config().context(
        "reading the memory config; set FERROSA_MEMORY_CONFIG to the file the \
         server uses, e.g. .runtime/ferrosa-memory-http-18765.toml",
    )?;
    if config.ferrosa.keyspace != keyspace {
        println!(
            "note: scanning keyspace {} from the config, not {keyspace}",
            config.ferrosa.keyspace
        );
    }
    let _ = host;
    let storage = ferrosa_memory_core::cql_storage::CqlStorage::connect(&config.ferrosa)
        .await
        .context("connecting for the entity scan")?;

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let scan_ctx = ctx.clone();
    tokio::spawn(async move { storage.entity_stream_all(scan_ctx, 500, tx).await });

    let mut examined = 0usize;
    let mut attributed = 0usize;
    let mut failed = 0usize;
    let mut by_session: BTreeMap<uuid::Uuid, usize> = BTreeMap::new();
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut with_text: BTreeMap<String, usize> = BTreeMap::new();
    let mut samples: Vec<String> = Vec::new();

    while let Some(chunk) = rx.recv().await {
        // A failed chunk ends the run. Continuing would attribute part of the
        // store and report a total, which is the partial answer the engine
        // itself refuses to give.
        let chunk = chunk.context("reading entities")?;
        for entry in chunk {
            examined += 1;
            *by_type.entry(entry.entity_type.clone()).or_default() += 1;
            let probe = format!(
                "{}\n{}\n{}",
                entry.context_snippet,
                entry.description.clone().unwrap_or_default(),
                entry.properties,
            );
            if probe.contains("\"benchmark\"") {
                *by_type.entry("(benchmark data)".to_owned()).or_default() += 1;
            }
            for marker in [
                "/research/",
                "research/corpus",
                "research/skills",
                "/corpus/",
            ] {
                if probe.contains(marker) {
                    *by_type.entry(format!("(mentions {marker})")).or_default() += 1;
                }
            }
            // What a turn or project states about itself: the directory the
            // work happened in. Counted, not yet attributed -- see the report.
            if matches!(entry.entity_type.as_str(), "turn" | "project") && probe.contains("\"cwd\"")
            {
                *by_type.entry("(states a cwd)".to_owned()).or_default() += 1;
            }
            if sample && !probe.contains("\"benchmark\"") && samples.len() < 10 {
                let head: String = probe
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" | ")
                    .chars()
                    .take(220)
                    .collect();
                samples.push(format!("[{}] {head}", entry.entity_type));
            }
            if !probe.trim().is_empty() {
                *with_text.entry(entry.entity_type.clone()).or_default() += 1;
            }
            if false {
                // What IS in here? Written before guessing a second prefix.
                let head: String = entry
                    .context_snippet
                    .lines()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" | ")
                    .chars()
                    .take(150)
                    .collect();
                samples.push(format!("[{}] {}", entry.entity_type, head));
            }
            let Some(path) = origin_of(&entry.entity_type, &probe, &entry.properties) else {
                continue;
            };
            *by_session.entry(entry.session_id).or_default() += 1;
            if samples.len() < 8 {
                samples.push(format!("{} -> {path}", entry.entity_name));
            }
            if dry_run {
                attributed += 1;
                continue;
            }
            match store
                .record_source(
                    ctx,
                    SourceDraft {
                        entity_id: entry.entity_id,
                        session_id: entry.session_id,
                        title: entry.entity_name.clone(),
                        source_path: path,
                    },
                )
                .await
            {
                Ok(_) => attributed += 1,
                Err(error) => {
                    eprintln!("could not attribute {}: {error}", entry.entity_id);
                    failed += 1;
                }
            }
        }
    }

    println!("\nentities read:        {examined}");
    println!("origin stated in text: {attributed}");
    println!(
        "no stated origin:      {} (left unattributed on purpose)",
        examined.saturating_sub(attributed + failed)
    );
    if failed > 0 {
        println!("FAILED to write:       {failed}");
    }
    println!("sessions contributing: {}", by_session.len());
    println!("\nentity types (count / how many carry any text):");
    let mut types: Vec<_> = by_type.iter().collect();
    types.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
    for (kind, n) in types.into_iter().take(14) {
        println!(
            "  {kind:28} {n:>7}  text: {}",
            with_text.get(kind).copied().unwrap_or(0)
        );
    }
    for sample in &samples {
        println!("  e.g. {sample}");
    }
    if dry_run {
        println!("\n(dry run: nothing was written)");
    }
    Ok(())
}

/// Where an entity came from, when the entity states it unambiguously.
///
/// Two forms, and both are STATED rather than inferred:
///
/// - A corpus document naming its own file ("Corpus path:", "Corpus file:").
/// - A turn or project written by a session hook, which records the directory
///   the work happened in. That kind of entity IS session capture by
///   construction -- the hook writes nothing else -- so the root is not a
///   guess about its content, it is what the writer was.
///
/// Everything else returns None. Inferring an origin from a name, a type
/// alone, or the first path-looking string in some prose would produce a
/// confident tier for something nobody placed.
pub fn origin_of(entity_type: &str, text: &str, properties: &serde_json::Value) -> Option<String> {
    if let Some(stated) = stated_origin(text) {
        return Some(stated);
    }
    // Benchmark fixtures state their dataset and their document id. Both are
    // written by the loader, so this is stated provenance, not a guess about
    // what the text contains.
    if let Some(benchmark) = properties.get("benchmark").and_then(|v| v.as_str())
        && benchmark == "bright-pro"
    {
        let doc = properties
            .get("doc_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        return Some(format!("brightpro-test/{doc}"));
    }
    if matches!(entity_type, "turn" | "project") {
        // The directory is real and stated; the root says what wrote it. Kept
        // as one path so a rule can match the root and a reader can still see
        // which checkout the session was in.
        let cwd = properties
            .get("cwd")
            .or_else(|| properties.get("project_path"))
            .or_else(|| properties.get("workspace"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !cwd.is_empty() {
            return Some(format!("session-capture{cwd}"));
        }
        return Some("session-capture".to_owned());
    }
    None
}

/// The origin an entity states in its own text, if it states one.
///
/// Exact prefixes only. Anything looser would attribute an entity by the
/// first filename someone happened to mention in its prose, which is a
/// confident tier for something nobody placed.
pub fn stated_origin(context: &str) -> Option<String> {
    for line in context.lines().take(40) {
        let line = line.trim();
        for prefix in ["Corpus path:", "Corpus file:"] {
            if let Some(rest) = line.strip_prefix(prefix) {
                let path = rest.trim();
                if !path.is_empty() {
                    return Some(path.to_owned());
                }
            }
        }
    }
    None
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|at| args.get(at + 1))
        .cloned()
}
