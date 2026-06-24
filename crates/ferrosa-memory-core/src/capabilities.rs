//! Startup capability gate for the connected Ferrosa.
//!
//! The memory schema hard-depends on Ferrosa-specific features — most notably
//! native full-text search (`fts_match`, used by ddl/040 + ddl/043 for lexical
//! recall). Against a Ferrosa that lacks them, the system otherwise fails
//! cryptically (a migration `CREATE INDEX … USING 'fulltext'` error) or, worse,
//! returns silently-wrong search results. Ferrosa's `system.local.release_version`
//! is a fixed Cassandra-compat marker (`"5.1.0-ferrosa"`), so it can't reveal
//! feature support — we PROBE the actual behavior instead.
//!
//! Tiered: a missing REQUIRED capability is fail-loud (caller refuses to serve);
//! an ambiguous/transient probe error is a warning (don't block on a blip).

use crate::cql_storage::CqlSession;

/// Verdict from probing a Ferrosa capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityStatus {
    /// The capability works.
    Supported,
    /// Definitively missing — a permanent incompatibility (fail loud).
    Unsupported(String),
    /// Probe errored ambiguously (transient/unrelated) — warn, don't block.
    Inconclusive(String),
}

/// Classify an `fts_match` probe error. Pure and testable.
///
/// Only a clear "`fts_match` / fulltext is not a supported feature" signal is
/// [`CapabilityStatus::Unsupported`]; everything else is
/// [`CapabilityStatus::Inconclusive`] so a transient blip never triggers a
/// fail-loud refusal.
pub fn classify_fts_probe_error(err: &str) -> CapabilityStatus {
    let e = err.to_lowercase();
    let mentions_fts = e.contains("fts_match") || e.contains("fulltext") || e.contains("full-text");
    let says_missing = e.contains("unknown")
        || e.contains("unsupported")
        || e.contains("not supported")
        || e.contains("no such function")
        || e.contains("does not exist")
        || e.contains("unimplemented")
        || (e.contains("invalid") && e.contains("function"));
    if mentions_fts && says_missing {
        CapabilityStatus::Unsupported(err.to_string())
    } else {
        CapabilityStatus::Inconclusive(err.to_string())
    }
}

/// Probe whether the connected Ferrosa supports native full-text search
/// (`fts_match`) — REQUIRED for lexical recall. Runs a zero-row `fts_match`
/// query against the already-indexed `entity_name` column (ddl/040). Returns
/// [`CapabilityStatus::Supported`] when the query executes (even with no rows).
pub async fn probe_native_fts(session: &CqlSession, keyspace: &str) -> CapabilityStatus {
    let nil = uuid::Uuid::nil();
    let q = format!(
        "SELECT entity_id FROM {keyspace}.entity_store \
         WHERE tenant_id = ? AND session_id = ? \
           AND entity_name = fts_match('__ferrosa_fts_capability_probe__') \
         LIMIT 1 ALLOW FILTERING"
    );
    #[allow(deprecated)]
    match session.query_unpaged(q, (nil, nil)).await {
        Ok(_) => CapabilityStatus::Supported,
        Err(e) => classify_fts_probe_error(&e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fts_function_is_unsupported() {
        for msg in [
            "Unknown function fts_match",
            "fts_match: no such function",
            "function fts_match is unsupported on this server",
            "fulltext indexes are not supported",
            "Invalid query: unknown function 'fts_match'",
        ] {
            assert!(
                matches!(
                    classify_fts_probe_error(msg),
                    CapabilityStatus::Unsupported(_)
                ),
                "should be Unsupported: {msg}"
            );
        }
    }

    #[test]
    fn transient_or_unrelated_errors_are_inconclusive() {
        for msg in [
            "Connection timed out",
            "Coordinator node overloaded",
            "keyspace agent_memory does not exist", // schema-not-ready, not a capability gap
            "request timeout",
        ] {
            assert!(
                matches!(
                    classify_fts_probe_error(msg),
                    CapabilityStatus::Inconclusive(_)
                ),
                "should be Inconclusive: {msg}"
            );
        }
    }
}
