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
//! # It does not backfill, and that was tried
//!
//! Existing entities have no recorded source, so a freshly seeded cluster
//! tiers nothing until something ingests with a path. A backfill was written
//! and removed. Two reasons, both worth keeping:
//!
//! - The provenance corpus documents carry ("Corpus file:", "Corpus path:")
//!   lives in their CHUNK text, not in `entity_store`, so the entity scan
//!   cannot see it.
//! - A whole-cluster `SELECT ... LIMIT n` is refused by the engine --
//!   "projected cluster range scan with partition_limit is not implemented;
//!   refusing to return partial results" -- which is the right refusal and
//!   means a backfill has to walk sessions rather than the cluster.
//!
//! Inferring an origin from an entity's type, session or name would avoid all
//! of that and is exactly what must not happen: a guessed source produces a
//! confident tier for something nobody placed.

use anyhow::{Context as _, Result};
use ferrosa_memory_core::tier_store::{CqlTierStore, seed_tier_rules};
use ferrosa_memory_core::types::TenantContext;
use scylla::SessionBuilder;
use std::sync::Arc;

const TENANT_ID: &str = "00000000-0000-0000-0000-000000000001";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let host = arg(&args, "--host").unwrap_or_else(|| "127.0.0.1:19042".to_owned());
    let keyspace = arg(&args, "--keyspace").unwrap_or_else(|| "agent_memory".to_owned());
    let research = arg(&args, "--research-root");
    let dry_run = args.iter().any(|a| a == "--dry-run");

    let Some(research) = research else {
        eprintln!(
            "usage: seed-tiers --research-root <path> [--dry-run]\n\
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
        tenant_id: TENANT_ID.parse()?,
        session_origin: "seed-tiers".to_owned(),
    };

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

    Ok(())
}

/// The origin an entity states in its own text, if it states one.
///
/// Kept, unused by this binary, because it is the only exact rule we have for
/// recovering an existing entity's origin and a session-walking backfill will
/// need it. Exact prefixes only: anything looser would attribute an entity by
/// the first filename someone happened to mention in its prose.
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
