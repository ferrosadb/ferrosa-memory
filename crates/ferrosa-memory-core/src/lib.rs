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
pub mod chains;
pub mod compression;
pub mod config;
pub mod cql_storage;
pub mod datalog;
pub mod dedup;
pub mod dispatch;
pub mod dream;
pub mod embedding;
pub mod enrich;
pub mod entity;
pub mod feedback;
pub mod fold;
pub mod graph;
pub mod http;
pub mod hybrid_search;
pub mod importance;
pub mod intention;
pub mod memo;
pub mod metrics;
pub mod migration;
pub mod ner;
pub mod pagerank;
pub mod plan;
pub mod promotion;
pub mod quota;
pub mod recursive_explore;
pub mod router;
pub mod scope;
pub mod skill;
#[cfg(test)]
mod security_tests;
pub mod session;
pub mod smart_ingest;
pub mod speculative;
pub mod spreading;
pub mod storage;
pub mod temporal;
pub mod test_cluster;
pub mod transport;
pub mod types;
pub mod vector;
pub mod viz;
pub mod warmth;
