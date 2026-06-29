//! # ferrosa-memory-core
//!
//! Shared library for the Ferrosa Memory MCP system. Contains configuration,
//! metrics, authentication, storage traits, and tool implementations that are
//! shared between the MCP server and the batch job.
//!
//! ## Module layout
//!
//! - [`config`] — TOML configuration parsing (`ferrosa-memory.toml`)
//! - [`metrics`] — Prometheus counters and histograms for observability
//! - [`auth`] — Tenant authentication and context extraction
//! - [`storage`] — Storage trait abstraction over CQL
//! - [`embedding`] — HTTP client for embedding endpoints (Ollama, etc.)
//! - [`memo`] — Memoization cache tool handlers
//! - [`plan`] — Plan state tool handlers
//! - [`types`] — Shared domain types

//! # ferrosa-memory-core
//!
//! Shared library for the Ferrosa Memory MCP system.

pub mod audit;
pub mod auth;
pub mod batch;
pub mod capabilities;
pub mod chains;
pub mod compression;
pub mod confidence;
pub mod config;
pub mod context_segment;
pub mod contradiction;
pub mod cql_storage;
pub mod datalog;
pub mod datalog_filter_expr;
pub mod dedup;
pub mod dispatch;
pub mod document_chunking;
pub mod dream;
pub mod embedding;
pub mod enrich;
pub mod entity;
pub mod expert_system;
pub mod feedback;
pub mod fold;
pub mod forget;
pub mod graph;
pub mod graph_write;
pub mod http;
pub mod hybrid_search;
pub mod importance;
pub mod intention;
pub mod memo;
pub mod metrics;
pub mod migration;
pub mod migration_backfill;
pub mod migration_backfill_cql;
pub mod ner;
pub mod pagerank;
pub mod plan;
pub mod promotion;
pub mod quota;
pub mod reconcile;
pub mod recursive_explore;
pub mod remote_identity;
pub mod remotes;
pub mod router;
pub mod scope;
#[cfg(test)]
mod security_tests;
pub mod session;
pub mod session_task;
pub mod skill;
pub mod smart_ingest;
pub mod speculative;
pub mod spreading;
pub mod storage;
pub mod system_describe;
pub mod temporal;
pub mod tenant_provision;
pub mod test_cluster;
pub mod transport;
pub mod turn_chain;
pub mod types;
pub mod vector;
pub mod viz;
pub mod warmth;
