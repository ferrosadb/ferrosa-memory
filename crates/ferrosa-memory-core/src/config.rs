//! Configuration parsing for `ferrosa-memory.toml`.
//!
//! The config file is located alongside the binary, or at a path specified
//! by the `FERROSA_MEMORY_CONFIG` environment variable. All fields have
//! sensible defaults so a minimal config is just `[ferrosa]` with contact points.
//!
//! ## Sections
//!
//! - `[server]` — transport mode, HTTP port, log level
//! - `[ferrosa]` — CQL connection parameters
//! - `[memory]` — TTL, compression, and gating thresholds
//! - `[embeddings]` — embedding provider configuration
//! - `[security]` — audit and anomaly detection settings
//! - `[routing]` — guideline version and batch schedule
//! - `[rmh]` — Resonant Memory Hierarchy warmth and exploration parameters
//! - `[datalog]` — Datalog inference engine limits and caching

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Top-level configuration.
#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub ferrosa: FerrosaCqlConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub embeddings: EmbeddingConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub graph: GraphDbConfig,
    #[serde(default)]
    pub sparql: SparqlConfig,
    #[serde(default)]
    pub viz: VizConfig,
    #[serde(default)]
    pub rmh: RmhConfig,
    #[serde(default)]
    pub datalog: DatalogConfig,
    #[serde(default)]
    pub promotion: PromotionConfig,
    #[serde(default)]
    pub enrich: EnrichConfig,
    #[serde(default)]
    pub judge: JudgeConfig,
    #[serde(default)]
    pub retrieval: RetrievalConfig,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub forget: ForgetConfig,
    #[serde(default)]
    pub consolidation: ConsolidationConfig,
}

/// Cross-replica consolidation coordination settings.
#[derive(Debug, Deserialize, Clone)]
pub struct ConsolidationConfig {
    /// Enable the global consolidation worker (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Poll interval for pending consolidation requests, in seconds (default: 20).
    #[serde(default = "default_consolidation_poll_seconds")]
    pub poll_seconds: u64,
    /// Lease duration granted to a worker; must exceed expected run time (default: 300).
    #[serde(default = "default_consolidation_lease_seconds")]
    pub lease_seconds: u64,
    /// Days after which unreinforced CO_OCCURS edges are pruned. 0 = never (default: 0).
    #[serde(default)]
    pub stale_edge_max_days: u64,
    /// Decay factor applied to CO_OCCURS edge weights each cycle (default: 0.95).
    #[serde(default = "default_decay_factor")]
    pub edge_decay_factor: f64,
    /// Run interval between tenant-scoped consolidation attempts, even when idle.
    /// Keeps the prior "idle_seconds" behavior without relying on actual inactivity.
    #[serde(default = "default_consolidation_min_interval_seconds")]
    pub min_interval_seconds: u64,
}

impl Default for ConsolidationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_seconds: default_consolidation_poll_seconds(),
            lease_seconds: default_consolidation_lease_seconds(),
            stale_edge_max_days: 0,
            edge_decay_factor: default_decay_factor(),
            min_interval_seconds: default_consolidation_min_interval_seconds(),
        }
    }
}

fn default_consolidation_poll_seconds() -> u64 {
    20
}

fn default_consolidation_lease_seconds() -> u64 {
    300
}

fn default_consolidation_min_interval_seconds() -> u64 {
    20
}

#[derive(Debug, Deserialize)]
pub struct GraphDbConfig {
    #[serde(default = "default_bolt_uri")]
    pub bolt_uri: String,
    #[serde(default = "default_graph_user")]
    pub username: String,
    #[serde(default = "default_graph_pass")]
    pub password: String,
    #[serde(default = "default_http_graph_url")]
    pub http_url: String,
}

impl Default for GraphDbConfig {
    fn default() -> Self {
        Self {
            bolt_uri: default_bolt_uri(),
            username: default_graph_user(),
            password: default_graph_pass(),
            http_url: default_http_graph_url(),
        }
    }
}

/// Public Ferrosa SPARQL listener used by the operator passthrough.
///
/// This must track Ferrosa's default sparql bind; deployments that change
/// that listener set sparql.http_url explicitly in their Memory config.
pub const DEFAULT_FERROSA_SPARQL_HTTP_URL: &str = "http://localhost:8080";

/// Public SPARQL endpoint configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct SparqlConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_sparql_url")]
    pub http_url: String,
}

impl Default for SparqlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            http_url: default_sparql_url(),
        }
    }
}

/// Visualization dashboard configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct VizConfig {
    #[serde(default = "default_viz_enabled")]
    pub enabled: bool,
    #[serde(default = "default_viz_port")]
    pub port: u16,
    /// Port to embed in rendered HTML for links pointing at viz (workbench → viz).
    /// Set when the viz listener sits behind a port mapping (e.g. podman 18766:8766).
    /// Defaults to `port` when unset.
    #[serde(default)]
    pub public_port: Option<u16>,
    /// Tenant UUID viz should read under when running in HTTP transport mode.
    ///
    /// Viz is unauthenticated, so the tenant cannot come from a request
    /// principal. In stdio mode this is unused — viz inherits the stdio tenant.
    /// In HTTP mode this is required if `enabled = true`.
    ///
    /// "loopback-only" used to be asserted here and was not true: the stdio and
    /// fallback arms bound 0.0.0.0. It is true now, enforced by
    /// `resolve_viz_bind` rather than by comment.
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// Explicit bind address for the viz listener. Unset means loopback,
    /// whatever the transport.
    ///
    /// A non-loopback value is REFUSED while viz cannot authenticate callers —
    /// it serves the whole graph. For a container, map the host side to
    /// 127.0.0.1 rather than binding the listener wide.
    #[serde(default)]
    pub bind_addr: Option<String>,
}

impl Default for VizConfig {
    fn default() -> Self {
        Self {
            enabled: default_viz_enabled(),
            port: default_viz_port(),
            public_port: None,
            tenant_id: None,
            bind_addr: None,
        }
    }
}

/// Resonant Memory Hierarchy configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct RmhConfig {
    #[serde(default = "default_warmth_boost")]
    pub warmth_boost_amount: f64,
    #[serde(default = "default_neighbor_ratio")]
    pub warmth_neighbor_ratio: f64,
    #[serde(default = "default_prune_threshold")]
    pub warmth_prune_threshold: f64,
    #[serde(default = "default_warmth_cap")]
    pub warmth_cap: f64,
    #[serde(default = "default_ppr_alpha")]
    pub ppr_alpha: f64,
    #[serde(default = "default_ppr_iterations")]
    pub ppr_iterations: usize,
    #[serde(default = "default_decay_lambda")]
    pub decay_lambda: f64,
    #[serde(default = "default_max_passes")]
    pub max_explore_passes: usize,
    #[serde(default = "default_convergence")]
    pub convergence_threshold: f64,
    #[serde(default = "default_max_explore_entities")]
    pub max_explore_entities: usize,
    #[serde(default = "default_forget_threshold")]
    pub forget_threshold: f64,
    #[serde(default = "default_decay_interval_hours")]
    pub decay_interval_hours: u32,
}

impl Default for RmhConfig {
    fn default() -> Self {
        Self {
            warmth_boost_amount: default_warmth_boost(),
            warmth_neighbor_ratio: default_neighbor_ratio(),
            warmth_prune_threshold: default_prune_threshold(),
            warmth_cap: default_warmth_cap(),
            ppr_alpha: default_ppr_alpha(),
            ppr_iterations: default_ppr_iterations(),
            decay_lambda: default_decay_lambda(),
            max_explore_passes: default_max_passes(),
            convergence_threshold: default_convergence(),
            max_explore_entities: default_max_explore_entities(),
            forget_threshold: default_forget_threshold(),
            decay_interval_hours: default_decay_interval_hours(),
        }
    }
}

/// Datalog inference engine configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct DatalogConfig {
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    #[serde(default = "default_max_facts")]
    pub max_facts: usize,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_seconds: u64,
    #[serde(default = "default_confidence_strategy")]
    pub confidence_combination: String,
}

impl Default for DatalogConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            max_facts: default_max_facts(),
            cache_ttl_seconds: default_cache_ttl(),
            confidence_combination: default_confidence_strategy(),
        }
    }
}

/// Promotion pipeline configuration (B10).
#[derive(Debug, Deserialize, Clone)]
pub struct PromotionConfig {
    /// Heat score threshold above which a predicate becomes a promotion candidate.
    #[serde(default = "default_promotion_threshold")]
    pub promotion_threshold: f64,
    /// Maximum total rows across all promoted predicates.
    #[serde(default = "default_size_budget")]
    pub size_budget_rows: usize,
    /// Number of days of heat data to consider.
    #[serde(default = "default_promotion_window_days")]
    pub window_days: u32,
    /// Multiplier applied to reuse benefit when scoring promotion candidates.
    #[serde(default = "default_reuse_factor")]
    pub reuse_factor: f64,
}

impl Default for PromotionConfig {
    fn default() -> Self {
        Self {
            promotion_threshold: default_promotion_threshold(),
            size_budget_rows: default_size_budget(),
            window_days: default_promotion_window_days(),
            reuse_factor: default_reuse_factor(),
        }
    }
}

/// LLM enrichment pipeline configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct EnrichConfig {
    /// Base URL for the OpenAI-compatible LLM API (e.g., LM Studio).
    #[serde(default = "default_enrich_url")]
    pub llm_base_url: String,
    /// Model name to use for enrichment.
    #[serde(default = "default_enrich_model")]
    pub llm_model: String,
    /// Number of entities per LLM batch call.
    #[serde(default = "default_enrich_batch")]
    pub batch_size: usize,
    /// Maximum tokens per LLM response.
    #[serde(default = "default_enrich_max_tokens")]
    pub max_tokens: u32,
}

impl Default for EnrichConfig {
    fn default() -> Self {
        Self {
            llm_base_url: default_enrich_url(),
            llm_model: default_enrich_model(),
            batch_size: default_enrich_batch(),
            max_tokens: default_enrich_max_tokens(),
        }
    }
}

fn default_enrich_url() -> String {
    "http://localhost:1234".into()
}
fn default_enrich_model() -> String {
    "google/gemma-4-31b".into()
}
fn default_enrich_batch() -> usize {
    10
}
fn default_enrich_max_tokens() -> u32 {
    2048
}

/// Judge model configuration for evaluation and reranker-feedback workflows.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct JudgeConfig {
    /// Enable judge-model reranking in live retrieval paths.
    ///
    /// Defaults to true because the local Ollama judge configuration below is
    /// also fully specified by default. If the configured judge endpoint is not
    /// actually available at runtime, retrieval still succeeds and reports the
    /// judge failure as a skipped rerank diagnostic rather than failing the
    /// search.
    #[serde(default = "default_judge_enabled")]
    pub enabled: bool,
    /// Provider family. Supported UI probes include `ollama`, `lmstudio`, and `openai_compatible`.
    #[serde(default = "default_judge_provider")]
    pub provider: String,
    /// Base URL for the model provider.
    #[serde(default = "default_judge_base_url")]
    pub base_url: String,
    /// Model name used for judge calls.
    #[serde(default = "default_judge_model")]
    pub model: String,
    /// Optional bearer/API token. Runtime GET endpoints redact this value.
    #[serde(default)]
    pub token: Option<String>,
    /// Request timeout for judge/model discovery calls.
    #[serde(default = "default_judge_timeout_seconds")]
    pub timeout_seconds: u64,
    /// Maximum candidates sent to the judge reranker per retrieval call.
    #[serde(default = "default_judge_max_rerank_candidates")]
    pub max_rerank_candidates: usize,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            enabled: default_judge_enabled(),
            provider: default_judge_provider(),
            base_url: default_judge_base_url(),
            model: default_judge_model(),
            token: None,
            timeout_seconds: default_judge_timeout_seconds(),
            max_rerank_candidates: default_judge_max_rerank_candidates(),
        }
    }
}

fn default_judge_max_rerank_candidates() -> usize {
    8
}

fn default_judge_enabled() -> bool {
    true
}

/// Runtime retrieval defaults shared by MCP tools that return ranked context.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct RetrievalConfig {
    /// Default number of ranked results when a retrieval call omits k/limit.
    #[serde(default = "default_retrieval_limit")]
    pub default_limit: usize,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            default_limit: default_retrieval_limit(),
        }
    }
}

fn default_retrieval_limit() -> usize {
    10
}

/// Search & rerank tunables (`[search]` section) that shape retrieval quality.
/// Promoted from former dispatch-layer constants so operators can tune them at
/// runtime via the workbench and persist them to the config file. Defaults
/// preserve the original hardcoded behaviour exactly.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct SearchConfig {
    /// Minimum number of candidates required before the LLM judge reranks.
    #[serde(default = "default_rerank_min_candidates")]
    pub rerank_min_candidates: usize,
    /// Hard cap on candidates sent to the judge reranker, regardless of request.
    #[serde(default = "default_rerank_max_candidates")]
    pub rerank_max_candidates: usize,
    /// Minimum number of scored candidates needed to trust judge score contrast.
    #[serde(default = "default_rerank_min_score_coverage")]
    pub rerank_min_score_coverage: usize,
    /// Batch size for chunked judge reranking of large candidate sets.
    #[serde(default = "default_rerank_batch_size")]
    pub rerank_batch_size: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            rerank_min_candidates: default_rerank_min_candidates(),
            rerank_max_candidates: default_rerank_max_candidates(),
            rerank_min_score_coverage: default_rerank_min_score_coverage(),
            rerank_batch_size: default_rerank_batch_size(),
        }
    }
}

fn default_rerank_min_candidates() -> usize {
    2
}
fn default_rerank_max_candidates() -> usize {
    50
}
fn default_rerank_min_score_coverage() -> usize {
    5
}
fn default_rerank_batch_size() -> usize {
    5
}

/// Configuration for the `forget` / `restore_forgotten` memory tools.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct ForgetConfig {
    /// Days a retracted (soft-forgotten) object remains restorable before the
    /// purge sweep hard-deletes it. Never hardcoded at call sites.
    #[serde(default = "default_retract_purge_days")]
    pub retract_purge_days: u32,
    /// Default number of candidates returned by a forget propose call.
    #[serde(default = "default_forget_candidate_limit")]
    pub candidate_limit: usize,
    /// Hard cap on candidates a single propose call may return.
    #[serde(default = "default_forget_candidate_max")]
    pub candidate_max: usize,
    /// Lifetime of a propose `forget_token` before confirm must re-propose.
    #[serde(default = "default_forget_token_ttl_seconds")]
    pub token_ttl_seconds: u64,
    /// Edge/reference count above which a candidate is flagged high_impact and
    /// requires explicit acknowledgement to forget.
    #[serde(default = "default_high_impact_edge_threshold")]
    pub high_impact_edge_threshold: usize,
}

impl Default for ForgetConfig {
    fn default() -> Self {
        Self {
            retract_purge_days: default_retract_purge_days(),
            candidate_limit: default_forget_candidate_limit(),
            candidate_max: default_forget_candidate_max(),
            token_ttl_seconds: default_forget_token_ttl_seconds(),
            high_impact_edge_threshold: default_high_impact_edge_threshold(),
        }
    }
}

fn default_retract_purge_days() -> u32 {
    7
}
fn default_forget_candidate_limit() -> usize {
    10
}
fn default_forget_candidate_max() -> usize {
    50
}
fn default_forget_token_ttl_seconds() -> u64 {
    600
}
fn default_high_impact_edge_threshold() -> usize {
    25
}

fn default_judge_provider() -> String {
    "ollama".into()
}
fn default_judge_base_url() -> String {
    "http://127.0.0.1:11434".into()
}
fn default_judge_model() -> String {
    "qwen2.5-coder:7b".into()
}
fn default_judge_timeout_seconds() -> u64 {
    30
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_transport")]
    pub transport: String,
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// Port to embed in rendered HTML for cross-page links (workbench ⇄ viz).
    /// Set when the server sits behind a port mapping (e.g. podman 18765:8765):
    /// the process listens on `http_port` inside the container, but the browser
    /// needs `public_port` on the host. Defaults to `http_port` when unset.
    #[serde(default)]
    pub public_port: Option<u16>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub require_tls: bool,
    /// Path to the TLS certificate file (PEM format).
    pub cert_path: Option<String>,
    /// Path to the TLS private key file (PEM format).
    pub key_path: Option<String>,
    /// Path to the file-backed HTTP auth principal database.
    pub auth_file: Option<String>,
    /// Per-request HTTP timeout. Long enough to allow first-call local model loads.
    #[serde(default = "default_request_timeout_seconds")]
    pub request_timeout_seconds: u64,
    /// Connections allowed per minute from one client IP before HTTP 429.
    ///
    /// `-1` is unlimited, `0` blocks the address entirely, and any positive
    /// value is that many connections per minute.
    ///
    /// Unset derives from the bind address. A loopback bind defaults to
    /// unlimited: every process on the host shares the single address
    /// 127.0.0.1, so a per-IP budget is divided among co-operating local
    /// clients rather than applied to a remote caller. A network-exposed bind
    /// defaults to `EXPOSED_RATE_LIMIT_PER_MINUTE`.
    #[serde(default)]
    pub rate_limit_per_minute: Option<i64>,
    /// Per-IP connection budgets overriding `rate_limit_per_minute`, keyed by
    /// address, so one server can serve several rate tiers.
    ///
    /// Same encoding as `rate_limit_per_minute`: `-1` unlimited, `0` blocked,
    /// positive is a per-minute budget.
    ///
    /// ```toml
    /// [server.rate_limit_overrides]
    /// "203.0.113.7" = -1      # uncapped
    /// "198.51.100.4" = 100    # 100 per minute
    /// "198.51.100.9" = 0      # blocked
    /// ```
    #[serde(default)]
    pub rate_limit_overrides: std::collections::HashMap<String, i64>,
    /// Fixed tenant UUID for sharing data across sessions.
    /// If not set, a random UUID is generated per session.
    pub tenant_id: Option<String>,
    /// Fixed session UUID for cross-session memory continuity.
    /// If set, all tools default to this session_id when none is provided.
    pub session_id: Option<String>,
    /// Enable automatic dream consolidation after idle timeout (default: true).
    #[serde(default = "default_true")]
    pub idle_consolidation_enabled: bool,
    /// Seconds of inactivity before triggering idle consolidation (default: 20).
    #[serde(default = "default_idle_seconds")]
    pub idle_consolidation_seconds: u64,
    /// Days after which unreinforced CO_OCCURS edges are pruned. 0 = never prune (default: 0).
    #[serde(default)]
    pub stale_edge_max_days: u64,
    /// Decay factor applied to CO_OCCURS edge weights each consolidation cycle (default: 0.95).
    /// Unreinforced edges gradually lose strength; rediscovered edges are reset to full weight.
    #[serde(default = "default_decay_factor")]
    pub edge_decay_factor: f64,
    /// Hidden dev-only flag: when set, tool responses surface a `debug_stop`
    /// alert if any monitored component (DB quorum, embedding, reranker) is
    /// unhealthy, so the agent stops and investigates instead of building on a
    /// degraded cluster. Off in production. Seeds the runtime flag, which the
    /// LLM can also toggle in-session via the `config` tool.
    #[serde(default)]
    pub debug_stop: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: default_transport(),
            bind_addr: default_bind_addr(),
            http_port: default_http_port(),
            public_port: None,
            log_level: default_log_level(),
            require_tls: false,
            cert_path: None,
            key_path: None,
            auth_file: None,
            request_timeout_seconds: default_request_timeout_seconds(),
            rate_limit_per_minute: None,
            rate_limit_overrides: std::collections::HashMap::new(),
            tenant_id: None,
            session_id: None,
            idle_consolidation_enabled: true,
            idle_consolidation_seconds: default_idle_seconds(),
            stale_edge_max_days: 0,
            edge_decay_factor: default_decay_factor(),
            debug_stop: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FerrosaCqlConfig {
    pub contact_points: Vec<String>,
    #[serde(default = "default_keyspace")]
    pub keyspace: String,
    #[serde(default = "default_rf")]
    pub replication_factor: u8,
    #[serde(default = "default_consistency")]
    pub consistency: String,
    /// P0-11/W-02: In DBaaS mode, credentials come from `FERROSA_TENANT_ID` /
    /// `FERROSA_API_KEY` via `apply_dbaas_env_overrides`. In local-dev mode
    /// the TOML value is used as-is; the legacy default_ferrosa_user /
    /// default_ferrosa_password fns are removed — local configs must now
    /// set explicit values.
    #[serde(default = "default_cql_username")]
    pub username: String,
    #[serde(default = "default_cql_password")]
    pub password: String,
    /// Optional admin credentials used for a short-lived session that runs
    /// schema migrations (CREATE KEYSPACE / CREATE TABLE / etc.). When set,
    /// the runtime session still connects as `username`/`password` — only
    /// migration DDL uses the admin pair. Leave unset on auth-disabled
    /// clusters or when `username` already has DDL privileges.
    #[serde(default)]
    pub admin_username: Option<String>,
    #[serde(default)]
    pub admin_password: Option<String>,
    /// Path to the PEM CA bundle signing the Ferrosa cluster's CQL
    /// certificate. `Some` enables TLS and verifies the server against this
    /// CA; `None` connects in plaintext.
    ///
    /// A cluster started with `FERROSA_MODE=production` refuses to boot unless
    /// `cql_require_tls` is set, so this is required to reach one. Without it
    /// the TCP connection is accepted and the handshake then stalls until the
    /// connect timeout, with no error from either end explaining why.
    #[serde(default)]
    pub tls_ca_path: Option<String>,
    /// Skip server hostname verification while still requiring a CA-signed
    /// certificate. Only for reaching a cluster through a local port forward,
    /// where the dialled address is absent from the certificate's SAN list.
    #[serde(default)]
    pub tls_skip_hostname_verify: bool,
}

#[derive(Debug, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_ttl_days")]
    pub default_ttl_days: u32,
    #[serde(default = "default_fold_ttl")]
    pub fold_ttl_days: u32,
    #[serde(default = "default_archive_days")]
    pub archive_after_days: u32,
    #[serde(default = "default_compression_threshold")]
    pub compression_threshold_tokens: u32,
    #[serde(default = "default_confidence_gate")]
    pub confidence_gate: f64,
    #[serde(default = "default_max_memo")]
    pub max_memo_results: u32,
    #[serde(default = "default_max_entities")]
    pub max_entities: u32,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            default_ttl_days: default_ttl_days(),
            fold_ttl_days: default_fold_ttl(),
            archive_after_days: default_archive_days(),
            compression_threshold_tokens: default_compression_threshold(),
            confidence_gate: default_confidence_gate(),
            max_memo_results: default_max_memo(),
            max_entities: default_max_entities(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingConfig {
    /// Which runtime serves embeddings.
    ///
    /// `ollama` speaks Ollama's own `/api/embed`. Everything else in the
    /// OpenAI-compatible family — `openai`, `openai_compatible`, `lmstudio`,
    /// `llamacpp`, `vllm` — speaks `/v1/embeddings`. `synthetic` is for tests.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Base URL of the embedding server, for any provider.
    ///
    /// Preferred over `ollama_base_url`, which is kept so configs written
    /// before multi-runtime support keep working unchanged. When this is empty
    /// the client falls back to `ollama_base_url`.
    #[serde(default)]
    pub base_url: String,
    #[serde(default = "default_ollama_url")]
    pub ollama_base_url: String,
    #[serde(default = "default_embed_model")]
    pub model: String,
    #[serde(default = "default_dimensions")]
    pub dimensions: u32,
    /// Maximum characters sent to a single embedding request.
    ///
    /// Ollama tokenizes before rejecting over-context inputs, so client-side
    /// chunking prevents retry loops from turning a too-large text into CPU burn.
    #[serde(default = "default_embedding_max_input_chars")]
    pub max_input_chars: usize,
    #[serde(default = "default_ner_model")]
    pub ner_model: String,
}

impl EmbeddingConfig {
    /// The embedding endpoint to call, whichever setting supplied it.
    ///
    /// `base_url` is the general setting; `ollama_base_url` predates
    /// multi-runtime support and is still honoured so existing config files
    /// keep working. EVERY consumer must go through here — reading
    /// `ollama_base_url` directly means a config that only sets `base_url`
    /// silently gets an empty endpoint.
    pub fn resolved_base_url(&self) -> &str {
        if self.base_url.trim().is_empty() {
            &self.ollama_base_url
        } else {
            &self.base_url
        }
    }
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            base_url: String::new(),
            ollama_base_url: default_ollama_url(),
            model: default_embed_model(),
            dimensions: default_dimensions(),
            max_input_chars: default_embedding_max_input_chars(),
            ner_model: default_ner_model(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SecurityConfig {
    #[serde(default = "default_true")]
    pub audit_log_enabled: bool,
    #[serde(default = "default_true")]
    pub anomaly_detection_enabled: bool,
    #[serde(default = "default_sigma")]
    pub anomaly_sigma_threshold: f64,
    #[serde(default = "default_true")]
    pub anomaly_alerts_enabled: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            audit_log_enabled: true,
            anomaly_detection_enabled: true,
            anomaly_sigma_threshold: default_sigma(),
            anomaly_alerts_enabled: true,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RoutingConfig {
    #[serde(default = "default_guideline_version")]
    pub guideline_version: String,
    #[serde(default = "default_cron")]
    pub feedback_export_cron: String,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            guideline_version: default_guideline_version(),
            feedback_export_cron: default_cron(),
        }
    }
}

// --- defaults ---

fn default_transport() -> String {
    "stdio".into()
}
fn default_bind_addr() -> String {
    "0.0.0.0".into()
}
fn default_http_port() -> u16 {
    8765
}
fn default_request_timeout_seconds() -> u64 {
    30
}
fn default_log_level() -> String {
    "info".into()
}
fn default_keyspace() -> String {
    "agent_memory".into()
}
fn default_rf() -> u8 {
    3
}
fn default_consistency() -> String {
    "LOCAL_QUORUM".into()
}
fn default_ttl_days() -> u32 {
    7
}
fn default_fold_ttl() -> u32 {
    30
}
fn default_archive_days() -> u32 {
    30
}
fn default_compression_threshold() -> u32 {
    512
}
fn default_confidence_gate() -> f64 {
    0.7
}
fn default_max_memo() -> u32 {
    50
}
fn default_max_entities() -> u32 {
    10000
}
fn default_provider() -> String {
    "ollama".into()
}
fn default_ollama_url() -> String {
    "http://127.0.0.1:11434".into()
}
fn default_embed_model() -> String {
    "nomic-embed-text-v2-moe".into()
}
fn default_dimensions() -> u32 {
    768
}
fn default_embedding_max_input_chars() -> usize {
    6_000
}
fn default_ner_model() -> String {
    "qwen3.5:27b".into()
}
fn default_true() -> bool {
    true
}
fn default_sigma() -> f64 {
    3.0
}
fn default_guideline_version() -> String {
    "v1".into()
}
fn default_cron() -> String {
    "0 2 * * *".into()
}
fn default_idle_seconds() -> u64 {
    20
}
fn default_decay_factor() -> f64 {
    0.95
}
fn default_viz_enabled() -> bool {
    true
}
fn default_viz_port() -> u16 {
    8766
}
fn default_bolt_uri() -> String {
    "bolt://localhost:7687".into()
}
fn default_graph_user() -> String {
    "ferrosa_admin".into()
}
fn default_graph_pass() -> String {
    "ferrosa_admin".into()
}
fn default_http_graph_url() -> String {
    "http://localhost:7474".into()
}
fn default_sparql_url() -> String {
    DEFAULT_FERROSA_SPARQL_HTTP_URL.into()
}
/// P0-11/W-02: default CQL username for local-dev / non-DBaaS installs.
/// In production (FERROSA_DBAAS_MODE=true) `apply_dbaas_env_overrides`
/// overwrites this with the value from FERROSA_TENANT_ID.
fn default_cql_username() -> String {
    "ferrosa_user".into()
}
/// P0-11/W-02: default CQL password for local-dev / non-DBaaS installs.
/// In production (FERROSA_DBAAS_MODE=true) `apply_dbaas_env_overrides`
/// overwrites this with the value from FERROSA_API_KEY.
fn default_cql_password() -> String {
    "ferrosa_user".into()
}
fn default_warmth_boost() -> f64 {
    0.3
}
fn default_neighbor_ratio() -> f64 {
    0.5
}
fn default_prune_threshold() -> f64 {
    0.01
}
fn default_warmth_cap() -> f64 {
    10.0
}
fn default_ppr_alpha() -> f64 {
    0.45
}
fn default_ppr_iterations() -> usize {
    20
}
fn default_decay_lambda() -> f64 {
    0.1
}
fn default_max_passes() -> usize {
    3
}
fn default_convergence() -> f64 {
    0.1
}
fn default_max_explore_entities() -> usize {
    50
}
fn default_forget_threshold() -> f64 {
    0.05
}
fn default_decay_interval_hours() -> u32 {
    24
}
fn default_max_iterations() -> usize {
    100
}
fn default_max_facts() -> usize {
    50000
}
fn default_cache_ttl() -> u64 {
    3600
}
fn default_confidence_strategy() -> String {
    "min_parent_times_weight".to_string()
}
fn default_promotion_threshold() -> f64 {
    1000.0
}
fn default_size_budget() -> usize {
    100_000
}
fn default_promotion_window_days() -> u32 {
    7
}
fn default_reuse_factor() -> f64 {
    1.0
}

/// Resolve the config file path. Checks, in order:
/// 1. `FERROSA_MEMORY_CONFIG` env var
/// 2. `./ferrosa-memory.toml` (current directory)
/// 3. `~/.config/ferrosa/memory.toml`
pub fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FERROSA_MEMORY_CONFIG") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }

    let cwd = Path::new("ferrosa-memory.toml");
    if cwd.exists() {
        return Some(cwd.to_path_buf());
    }

    if let Some(config_dir) = dirs::config_dir() {
        let path = config_dir.join("ferrosa").join("memory.toml");
        if path.exists() {
            return Some(path);
        }
    }

    None
}

/// Load and parse config from the given TOML string.
pub fn parse_config(toml_str: &str) -> Result<Config, toml::de::Error> {
    toml::from_str(toml_str)
}

/// Sentinels delimiting the workbench-managed config block.
const MANAGED_BEGIN: &str = "# >>> ferrosa-memory workbench-managed (do not edit by hand) >>>";
const MANAGED_END: &str = "# <<< ferrosa-memory workbench-managed <<<";

/// Top-level TOML tables the workbench config editor owns and regenerates inside
/// the managed block. Hand-edited copies of these tables are removed on save so
/// the file stays valid (TOML forbids duplicate table headers).
const MANAGED_TABLES: [&str; 4] = ["judge", "search", "retrieval", "forget"];

/// Detect a top-level table header `[name]` (not an array-of-tables `[[name]]`),
/// returning the table name. Returns `None` for any other line.
fn parse_table_header(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.starts_with('[') && !line.starts_with("[[") && line.ends_with(']') {
        Some(line[1..line.len() - 1].trim())
    } else {
        None
    }
}

/// Remove the prior managed block (between sentinels, inclusive) and any
/// top-level managed tables, preserving every other line and comment verbatim.
fn strip_managed_and_tables(input: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut in_managed = false;
    let mut skipping_table = false;
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed == MANAGED_BEGIN {
            in_managed = true;
            continue;
        }
        if trimmed == MANAGED_END {
            in_managed = false;
            continue;
        }
        if in_managed {
            continue;
        }
        if let Some(header) = parse_table_header(trimmed) {
            let top = header.split('.').next().unwrap_or(header);
            skipping_table = MANAGED_TABLES.contains(&top);
        } else if trimmed.starts_with("[[") {
            skipping_table = false;
        }
        if skipping_table {
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

/// Render a single named TOML table (e.g. `[judge]`) from a serializable value.
fn render_managed_table<T: Serialize>(name: &str, value: &T) -> anyhow::Result<String> {
    let mut doc = toml::Table::new();
    doc.insert(name.to_string(), toml::Value::try_from(value)?);
    let body =
        toml::to_string_pretty(&doc).map_err(|e| anyhow::anyhow!("serialize [{name}]: {e}"))?;
    Ok(format!("\n{body}"))
}

/// Persist the workbench-managed config tables (`[judge]`, `[search]`,
/// `[retrieval]`, `[forget]`) to `path` inside a delimited managed block,
/// preserving all other file content and comments. Writes atomically via a
/// temp file + rename. Fails loudly if the file cannot be read or written.
pub fn write_managed_config_block(
    path: &Path,
    judge: &JudgeConfig,
    search: &SearchConfig,
    retrieval: &RetrievalConfig,
    forget: &ForgetConfig,
) -> anyhow::Result<()> {
    let existing = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read config {}: {e}", path.display()))?;
    let preserved = strip_managed_and_tables(&existing);

    let mut block = String::new();
    block.push_str(MANAGED_BEGIN);
    block.push('\n');
    block.push_str("# Generated by the Ferrosa Memory workbench config editor.\n");
    block.push_str("# Edits inside this block are overwritten on the next save;\n");
    block.push_str("# hand-tune other sections above this block.\n");
    block.push_str(&render_managed_table("judge", judge)?);
    block.push_str(&render_managed_table("search", search)?);
    block.push_str(&render_managed_table("retrieval", retrieval)?);
    block.push_str(&render_managed_table("forget", forget)?);
    block.push_str(MANAGED_END);
    block.push('\n');

    let mut out = preserved.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&block);

    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out.as_bytes())
        .map_err(|e| anyhow::anyhow!("write temp config {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| anyhow::anyhow!("commit config {}: {e}", path.display()))?;
    Ok(())
}

pub fn validate_shared_http_config(config: &Config) -> anyhow::Result<()> {
    if config.server.transport != "http" {
        return Ok(());
    }

    // Aggregate EVERY problem so an operator fixes the HTTP config in one pass
    // instead of restarting once per field. Previously each check bailed on the
    // first failure, which took ~5 restart/error cycles to bring a stdio config
    // up in HTTP mode (auth_file, bind_addr, tenant fallback, TLS secrets, viz).
    let mut problems: Vec<String> = Vec::new();
    if !config.server.require_tls && !is_loopback_bind_addr(&config.server.bind_addr) {
        problems.push(
            "HTTP transport requires TLS unless server.bind_addr is loopback-only".to_owned(),
        );
    }
    if config.server.require_tls
        && (config.server.cert_path.is_none() || config.server.key_path.is_none())
    {
        problems.push(
            "HTTP transport requires cert_path and key_path when require_tls is true".to_owned(),
        );
    }
    if config.server.auth_file.is_none() {
        problems.push("HTTP transport requires server.auth_file".to_owned());
    }
    // Both rate-limit fields are DECODED here so an invalid value is a startup
    // error naming the field, not a policy quietly guessed at request time.
    if let Err(error) = config.server.resolved_rate_limit_per_minute() {
        problems.push(error.to_string());
    }
    if let Err(error) = config.server.resolved_rate_limit_overrides() {
        problems.push(error.to_string());
    }
    if config.server.tenant_id.is_some() {
        problems.push("HTTP transport must not use server.tenant_id fallback".to_owned());
    }
    // Viz is unauthenticated and now loopback-bound, so under HTTP it cannot inherit a
    // request principal's tenant — it needs an explicit one. Surface it here so
    // it isn't yet another separate restart at viz-spawn time.
    if config.viz.enabled && config.viz.tenant_id.is_none() {
        problems.push("viz.tenant_id is required when viz.enabled is true in HTTP mode".to_owned());
    }

    if !problems.is_empty() {
        anyhow::bail!(
            "HTTP-mode config has {} problem(s); fix all before restarting:\n  - {}",
            problems.len(),
            problems.join("\n  - ")
        );
    }
    Ok(())
}

/// P0-11: process-wide counter of direct-loopback Ferrosa connections that
/// fmem made instead of going through the DBaaS proxy. Non-zero values in
/// production indicate the tenant connection path is bypassing the
/// metering / rate-limit / auth surface. The validator below refuses to
/// start with localhost contact points unless the operator has explicitly
/// opted in via `FERROSA_MEMORY_ALLOW_LOCALHOST=true`.
pub static LOOPBACK_CONNECTIONS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of the loopback-connection counter for tests and metrics scrape.
pub fn loopback_connection_count() -> u64 {
    LOOPBACK_CONNECTIONS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Returns true when *every* host portion of `endpoints` is a loopback
/// address (`localhost`, `127.0.0.1`, `::1`).
fn all_loopback(endpoints: &[String]) -> bool {
    !endpoints.is_empty()
        && endpoints.iter().all(|ep| {
            let host = ep.rsplit_once(':').map(|(h, _)| h).unwrap_or(ep.as_str());
            matches!(host.trim(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
        })
}

/// Returns true if any endpoint hostname looks like a loopback address.
fn any_loopback(endpoints: &[String]) -> bool {
    endpoints.iter().any(|ep| {
        let host = ep.rsplit_once(':').map(|(h, _)| h).unwrap_or(ep.as_str());
        matches!(host.trim(), "localhost" | "127.0.0.1" | "::1" | "[::1]")
    })
}

/// P0-11: refuse to load a config that points fmem at a localhost Ferrosa
/// cluster unless the operator has explicitly opted in via
/// `FERROSA_MEMORY_ALLOW_LOCALHOST=true`. The literal "true" wins (typo
/// defense matching P0-04/P0-05/P0-01).
///
/// In production, fmem must connect through the DBaaS proxy (which
/// enforces tenant auth, rate limiting, and metering), not directly to
/// loopback. The counter `LOOPBACK_CONNECTIONS` is incremented every
/// time we accept a loopback config so dashboards can observe drift.
pub fn validate_tenant_connection_path(config: &Config) -> anyhow::Result<()> {
    let endpoints = &config.ferrosa.contact_points;
    if endpoints.is_empty() {
        anyhow::bail!("ferrosa.contact_points is empty — cannot connect to any Ferrosa node");
    }
    if !any_loopback(endpoints) {
        return Ok(());
    }

    let allow = std::env::var("FERROSA_MEMORY_ALLOW_LOCALHOST")
        .ok()
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !allow {
        anyhow::bail!(
            "ferrosa.contact_points contains loopback addresses ({:?}) and \
             FERROSA_MEMORY_ALLOW_LOCALHOST is not set. Refusing to start: in production \
             fmem must connect through the DBaaS proxy. Set FERROSA_MEMORY_ALLOW_LOCALHOST=true \
             for local dev / CI.",
            endpoints
        );
    }

    LOOPBACK_CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if all_loopback(endpoints) {
        tracing::warn!(
            ?endpoints,
            "FERROSA_MEMORY_ALLOW_LOCALHOST=true and ALL contact_points are loopback — \
             fmem is bypassing the DBaaS proxy entirely. Local dev only."
        );
    } else {
        tracing::warn!(
            ?endpoints,
            "FERROSA_MEMORY_ALLOW_LOCALHOST=true and SOME contact_points are loopback — \
             a subset of connections will bypass the DBaaS proxy."
        );
    }
    Ok(())
}

/// Per-IP connection budget for a network-exposed bind, where distinct clients
/// have distinct addresses.
///
/// There is no loopback equivalent: a loopback bind is unlimited by default,
/// because every process on the host shares the address 127.0.0.1 and a budget
/// there is divided among co-operating local clients.
pub const EXPOSED_RATE_LIMIT_PER_MINUTE: usize = 50;

/// The configured value meaning "no limit".
pub const RATE_LIMIT_UNLIMITED: i64 = -1;

impl ServerConfig {
    /// Per-IP budget overrides, parsed into addresses.
    ///
    /// An unparseable address or a zero budget is an ERROR, not a skipped
    /// entry: a tier that silently disappears rations a client at the wrong
    /// rate, and the operator sees a working server doing the wrong thing.
    pub fn resolved_rate_limit_overrides(
        &self,
    ) -> anyhow::Result<std::collections::HashMap<std::net::IpAddr, Option<usize>>> {
        self.rate_limit_overrides
            .iter()
            .map(|(address, limit)| {
                let ip: std::net::IpAddr = address.trim().parse().map_err(|_| {
                    anyhow::anyhow!(
                        "server.rate_limit_overrides key {address:?} is not an IP address"
                    )
                })?;
                let budget = decode_rate_limit(*limit).ok_or_else(|| {
                    anyhow::anyhow!(
                        "server.rate_limit_overrides[{address:?}] is {limit}; \
                         use -1 for unlimited, 0 to block, or a positive budget"
                    )
                })?;
                Ok((ip, budget))
            })
            .collect()
    }

    /// The connection budget this server should enforce.
    ///
    /// `Ok(None)` is unlimited; `Ok(Some(0))` blocks every connection.
    pub fn resolved_rate_limit_per_minute(&self) -> anyhow::Result<Option<usize>> {
        match self.rate_limit_per_minute {
            Some(configured) => decode_rate_limit(configured).ok_or_else(|| {
                anyhow::anyhow!(
                    "server.rate_limit_per_minute is {configured}; \
                     use -1 for unlimited, 0 to block, or a positive budget"
                )
            }),
            None if is_loopback_bind_addr(&self.bind_addr) => Ok(None),
            None => Ok(Some(EXPOSED_RATE_LIMIT_PER_MINUTE)),
        }
    }
}

/// Decode a configured rate limit into a budget.
///
/// `-1` is unlimited (`None`), `0` blocks (`Some(0)`), positive values are the
/// budget itself. Any other negative number is rejected rather than clamped:
/// silently reading `-5` as unlimited or as blocked would apply a policy the
/// operator did not write, in opposite directions depending on the guess.
fn decode_rate_limit(configured: i64) -> Option<Option<usize>> {
    match configured {
        RATE_LIMIT_UNLIMITED => Some(None),
        budget if budget >= 0 => Some(Some(budget as usize)),
        _ => None,
    }
}

/// Where the viz listener should bind, and whether that is allowed.
///
/// Viz started as a debug dashboard and kept a debug posture. It serves the
/// whole graph -- `/viz`, `/viz/ws`, `/viz/snapshot`, `/viz/api/*` -- and it
/// authenticates nobody. The binding rule it grew was:
///
///     "stdio" => 0.0.0.0        // the DEFAULT transport
///     "http"  => 127.0.0.1
///     _       => 0.0.0.0        // and any typo, too
///
/// with `viz.enabled` defaulting to true. So a default install published the
/// user's knowledge graph on every interface, and an unrecognised transport
/// string failed OPEN.
///
/// This resolves the bind instead: loopback unless the operator says otherwise
/// IN WRITING, and an explicit non-loopback bind is refused while viz cannot
/// authenticate. Fails closed in every arm, including the fallback.
pub fn resolve_viz_bind(
    configured: Option<&str>,
    viz_can_authenticate: bool,
) -> Result<String, String> {
    let Some(requested) = configured.map(str::trim).filter(|value| !value.is_empty()) else {
        // No transport arm gets a non-loopback default any more. The old
        // stdio/fallback arms are the exposure.
        return Ok(LOOPBACK_BIND.to_owned());
    };

    if is_loopback_bind_addr(requested) {
        return Ok(requested.to_owned());
    }

    if viz_can_authenticate {
        return Ok(requested.to_owned());
    }

    Err(format!(
        "viz.bind_addr = {requested:?} would publish the graph on a non-loopback \
         interface, and viz cannot authenticate callers. Bind loopback, or set \
         viz.enabled = false. If a container port mapping already constrains \
         exposure, map the host side to 127.0.0.1 rather than binding the \
         listener wide."
    ))
}

/// The address viz binds when nothing else is configured.
pub const LOOPBACK_BIND: &str = "127.0.0.1";

fn is_loopback_bind_addr(bind_addr: &str) -> bool {
    matches!(
        bind_addr.trim(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]"
    )
}

/// P0-11/W-02: returns true when FERROSA_DBAAS_MODE=true (exact match,
/// case-insensitive). When true, `apply_dbaas_env_overrides` must succeed
/// at startup or the process exits with a clear error.
pub fn is_dbaas_mode() -> bool {
    std::env::var("FERROSA_DBAAS_MODE")
        .ok()
        .as_deref()
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// P0-11/W-02: Overwrite the CQL contact-points, graph URL, and
/// credentials from DBaaS-specific env vars. Called at startup when
/// `is_dbaas_mode()` is true. Fails loud if any required variable is absent.
///
/// Required env vars (all must be set in DBaaS mode):
/// - `FERROSA_CQL_PROXY_ADDR`  — comma-separated `host:port` list
/// - `FERROSA_GRAPH_PROXY_ADDR` — full URL, e.g. `http://proxy.dbaas.io:7474`
/// - `FERROSA_TENANT_ID`        — used as CQL username
/// - `FERROSA_API_KEY`          — used as CQL password
pub fn apply_dbaas_env_overrides(config: &mut Config) -> anyhow::Result<()> {
    let cql_addr = std::env::var("FERROSA_CQL_PROXY_ADDR").map_err(|_| {
        anyhow::anyhow!(
            "FERROSA_DBAAS_MODE=true but FERROSA_CQL_PROXY_ADDR is not set. \
             Set it to the CQL proxy address (e.g. 'proxy.dbaas.ferrosa.io:9042')."
        )
    })?;
    if cql_addr.trim().is_empty() {
        anyhow::bail!(
            "FERROSA_CQL_PROXY_ADDR is set but empty — cannot connect to any Ferrosa node"
        );
    }

    let graph_addr = std::env::var("FERROSA_GRAPH_PROXY_ADDR").map_err(|_| {
        anyhow::anyhow!(
            "FERROSA_DBAAS_MODE=true but FERROSA_GRAPH_PROXY_ADDR is not set. \
             Set it to the graph proxy URL (e.g. 'http://proxy.dbaas.ferrosa.io:7474')."
        )
    })?;
    if graph_addr.trim().is_empty() {
        anyhow::bail!(
            "FERROSA_GRAPH_PROXY_ADDR is set but empty — cannot connect to the graph proxy"
        );
    }

    let tenant_id = std::env::var("FERROSA_TENANT_ID").map_err(|_| {
        anyhow::anyhow!(
            "FERROSA_DBAAS_MODE=true but FERROSA_TENANT_ID is not set. \
             Set it to the tenant UUID issued by the DBaaS control plane."
        )
    })?;
    if tenant_id.trim().is_empty() {
        anyhow::bail!("FERROSA_TENANT_ID is set but empty");
    }

    let api_key = std::env::var("FERROSA_API_KEY").map_err(|_| {
        anyhow::anyhow!(
            "FERROSA_DBAAS_MODE=true but FERROSA_API_KEY is not set. \
             Set it to the API key issued by the DBaaS control plane."
        )
    })?;
    if api_key.trim().is_empty() {
        anyhow::bail!("FERROSA_API_KEY is set but empty");
    }

    // Rewrite contact_points from the env var (comma-separated).
    config.ferrosa.contact_points = cql_addr
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if config.ferrosa.contact_points.is_empty() {
        anyhow::bail!("FERROSA_CQL_PROXY_ADDR contained no valid addresses after parsing");
    }

    // Rewrite graph URL.
    config.graph.http_url = graph_addr;

    // Replace credentials with tenant identity.
    config.ferrosa.username = tenant_id;
    config.ferrosa.password = api_key;

    tracing::info!(
        contact_points = ?config.ferrosa.contact_points,
        graph_http_url = %config.graph.http_url,
        "DBaaS mode: CQL and graph addresses set from environment"
    );
    Ok(())
}

/// P0-11/W-02: Load config and, when FERROSA_DBAAS_MODE=true, apply the
/// DBaaS env overrides. Exits with a clear error if required vars are absent
/// in DBaaS mode.
pub fn load_config_with_dbaas() -> anyhow::Result<Config> {
    let mut config = load_config()?;
    if is_dbaas_mode() {
        apply_dbaas_env_overrides(&mut config)?;
    }
    Ok(config)
}

/// Load config from the resolved path, or return an error if not found.
pub fn load_config() -> anyhow::Result<Config> {
    let path = resolve_config_path().ok_or_else(|| {
        anyhow::anyhow!(
            "no config file found; set FERROSA_MEMORY_CONFIG or create ferrosa-memory.toml"
        )
    })?;
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("failed to read config at {}: {}", path.display(), e))?;
    parse_config(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse config at {}: {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    use serial_test::serial;

    /// P0-11: tenant connection path counter must be exposed.
    #[test]
    fn loopback_connection_count_exposes_counter() {
        let baseline = loopback_connection_count();
        assert!(loopback_connection_count() >= baseline);
    }

    #[test]
    fn search_config_defaults_match_legacy_rerank_constants() {
        let s = SearchConfig::default();
        assert_eq!(s.rerank_min_candidates, 2);
        assert_eq!(s.rerank_max_candidates, 50);
        assert_eq!(s.rerank_min_score_coverage, 5);
        assert_eq!(s.rerank_batch_size, 5);
    }

    #[test]
    fn managed_block_preserves_other_content_and_stays_valid_and_idempotent() {
        let path =
            std::env::temp_dir().join(format!("fmem_managed_block_{}.toml", std::process::id()));
        let original = "# hand-written header comment\n\
            [ferrosa]\nendpoint = \"127.0.0.1:9042\"\n\n\
            [judge]\nmodel = \"stale-model\"\n";
        std::fs::write(&path, original).unwrap();

        let judge = JudgeConfig {
            model: "fresh-model".into(),
            ..JudgeConfig::default()
        };
        write_managed_config_block(
            &path,
            &judge,
            &SearchConfig::default(),
            &RetrievalConfig::default(),
            &ForgetConfig::default(),
        )
        .unwrap();
        let after = std::fs::read_to_string(&path).unwrap();

        // Non-managed content + comments preserved.
        assert!(after.contains("# hand-written header comment"));
        assert!(after.contains("[ferrosa]"));
        assert!(after.contains("endpoint = \"127.0.0.1:9042\""));
        // Managed block written with fresh value; stale hand-written [judge] removed.
        assert!(after.contains(MANAGED_BEGIN));
        assert!(after.contains(MANAGED_END));
        assert!(after.contains("fresh-model"));
        assert!(!after.contains("stale-model"));
        assert_eq!(after.matches("[judge]").count(), 1);
        // Result must still be valid TOML (no duplicate-table error).
        let _: toml::Table = toml::from_str(&after).expect("managed output is valid TOML");

        // Idempotent: a second write keeps exactly one managed block / table.
        write_managed_config_block(
            &path,
            &judge,
            &SearchConfig::default(),
            &RetrievalConfig::default(),
            &ForgetConfig::default(),
        )
        .unwrap();
        let after2 = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after2.matches(MANAGED_BEGIN).count(), 1);
        assert_eq!(after2.matches("[judge]").count(), 1);
        assert_eq!(after2.matches("[search]").count(), 1);

        std::fs::remove_file(&path).ok();
    }

    /// P0-11: localhost contact_points are refused without the explicit
    /// dev opt-in.
    #[test]
    #[serial]
    fn validate_tenant_path_refuses_localhost_without_opt_in() {
        unsafe {
            std::env::remove_var("FERROSA_MEMORY_ALLOW_LOCALHOST");
        }
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
"#;
        let config = parse_config(toml).unwrap();
        let res = validate_tenant_connection_path(&config);
        assert!(
            res.is_err(),
            "loopback contact_points must refuse to start without opt-in: {res:?}"
        );
    }

    /// P0-11: explicit dev opt-in lets localhost contact_points through
    /// AND increments the counter.
    #[test]
    #[serial]
    fn validate_tenant_path_dev_opt_in_increments_counter() {
        unsafe {
            std::env::set_var("FERROSA_MEMORY_ALLOW_LOCALHOST", "true");
        }
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042", "localhost:19043", "localhost:19044"]
"#;
        let config = parse_config(toml).unwrap();
        let baseline = loopback_connection_count();
        let res = validate_tenant_connection_path(&config);
        unsafe {
            std::env::remove_var("FERROSA_MEMORY_ALLOW_LOCALHOST");
        }
        assert!(res.is_ok(), "dev opt-in must allow localhost: {res:?}");
        assert!(
            loopback_connection_count() > baseline,
            "loopback counter must advance when localhost is accepted"
        );
    }

    /// P0-11: typo defense — non-"true" values must NOT trigger opt-in.
    #[test]
    #[serial]
    fn validate_tenant_path_typo_does_not_opt_in() {
        unsafe {
            std::env::set_var("FERROSA_MEMORY_ALLOW_LOCALHOST", "yes");
        }
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
"#;
        let config = parse_config(toml).unwrap();
        let res = validate_tenant_connection_path(&config);
        unsafe {
            std::env::remove_var("FERROSA_MEMORY_ALLOW_LOCALHOST");
        }
        assert!(
            res.is_err(),
            "non-'true' values must not satisfy the opt-in: {res:?}"
        );
    }

    /// P0-11: production-shape config (non-loopback contact_points) passes
    /// validation regardless of the opt-in env.
    #[test]
    #[serial]
    fn validate_tenant_path_proxy_config_passes() {
        unsafe {
            std::env::remove_var("FERROSA_MEMORY_ALLOW_LOCALHOST");
        }
        let toml = r#"
[ferrosa]
contact_points = ["proxy.dbaas.ferrosa.io:9042"]
"#;
        let config = parse_config(toml).unwrap();
        let res = validate_tenant_connection_path(&config);
        assert!(res.is_ok(), "proxy contact_points must pass: {res:?}");
    }

    /// P0-11: empty contact_points always fails.
    #[test]
    #[serial]
    fn validate_tenant_path_empty_contact_points_fails() {
        let toml = r#"
[ferrosa]
contact_points = []
"#;
        let config = parse_config(toml).unwrap();
        let res = validate_tenant_connection_path(&config);
        assert!(res.is_err(), "empty contact_points must fail: {res:?}");
    }

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse minimal config");
        assert_eq!(config.ferrosa.contact_points, vec!["localhost:9042"]);
        assert_eq!(config.server.transport, "stdio");
        assert_eq!(config.memory.default_ttl_days, 7);
        assert_eq!(config.embeddings.dimensions, 768);
        assert_eq!(config.retrieval.default_limit, 10);
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[server]
transport = "http"
http_port = 9999
log_level = "debug"

[ferrosa]
contact_points = ["node1:9042", "node2:9042"]
keyspace = "test_memory"
replication_factor = 1
consistency = "ONE"

[memory]
default_ttl_days = 14
fold_ttl_days = 60
archive_after_days = 90
compression_threshold_tokens = 256
confidence_gate = 0.8
max_memo_results = 100

[embeddings]
provider = "openai"
ollama_base_url = "http://gpu:11434"
model = "text-embedding-3-small"
dimensions = 1536

[security]
audit_log_enabled = false
anomaly_detection_enabled = false
anomaly_sigma_threshold = 2.5
anomaly_alerts_enabled = false

[routing]
guideline_version = "v2"
feedback_export_cron = "0 3 * * *"

[retrieval]
default_limit = 25
"#;
        let config = parse_config(toml).expect("should parse full config");
        assert_eq!(config.server.transport, "http");
        assert_eq!(config.server.bind_addr, "0.0.0.0");
        assert_eq!(config.server.http_port, 9999);
        assert_eq!(config.ferrosa.contact_points.len(), 2);
        assert_eq!(config.ferrosa.keyspace, "test_memory");
        assert_eq!(config.memory.default_ttl_days, 14);
        assert_eq!(config.memory.confidence_gate, 0.8);
        assert_eq!(config.retrieval.default_limit, 25);
        assert_eq!(config.embeddings.provider, "openai");
        assert_eq!(config.embeddings.dimensions, 1536);
        assert!(!config.security.audit_log_enabled);
        assert!(!config.security.anomaly_alerts_enabled);
        assert_eq!(config.routing.guideline_version, "v2");
    }

    #[test]
    fn parse_tls_config_fields_optional() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse without TLS fields");
        assert!(!config.server.require_tls);
        assert!(config.server.cert_path.is_none());
        assert!(config.server.key_path.is_none());
    }

    #[test]
    fn parse_tls_config_fields_present() {
        let toml = r#"
[server]
require_tls = true
cert_path = "/etc/ssl/cert.pem"
key_path = "/etc/ssl/key.pem"

[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse with TLS fields");
        assert!(config.server.require_tls);
        assert_eq!(
            config.server.cert_path.as_deref(),
            Some("/etc/ssl/cert.pem")
        );
        assert_eq!(config.server.key_path.as_deref(), Some("/etc/ssl/key.pem"));
    }

    #[test]
    fn release_sparql_config_uses_the_named_default_and_preserves_overrides() {
        let release_toml = include_str!("../../../config/ferrosa-memory.example.toml");
        let raw: toml::Value =
            toml::from_str(release_toml).expect("release config must be valid TOML");
        let release_sparql = raw
            .get("sparql")
            .and_then(toml::Value::as_table)
            .expect("release config must include [sparql]");
        assert!(
            !release_sparql.contains_key("http_url"),
            "release config must not duplicate the SPARQL default"
        );

        let release_config = parse_config(release_toml).expect("release config must parse");
        assert!(release_config.sparql.enabled);
        assert_eq!(
            release_config.sparql.http_url,
            DEFAULT_FERROSA_SPARQL_HTTP_URL
        );

        let override_config = parse_config(
            r#"
[ferrosa]
contact_points = ["localhost:9042"]

[sparql]
http_url = "http://ferrosa.internal:18080"
"#,
        )
        .expect("explicit SPARQL URL override must parse");
        assert_eq!(
            override_config.sparql.http_url,
            "http://ferrosa.internal:18080"
        );
    }

    #[test]
    fn parse_auth_file_field_present() {
        let toml = r#"
[server]
auth_file = "/etc/ferrosa/auth.toml"

[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse auth_file");
        assert_eq!(
            config.server.auth_file.as_deref(),
            Some("/etc/ferrosa/auth.toml")
        );
    }

    #[test]
    fn parse_invalid_config_missing_required() {
        let toml = r#"
[server]
transport = "stdio"
"#;
        let result = parse_config(toml);
        assert!(result.is_err(), "should fail without [ferrosa] section");
    }

    #[test]
    fn parse_invalid_config_bad_type() {
        let toml = r#"
[ferrosa]
contact_points = "not_an_array"
"#;
        let result = parse_config(toml);
        assert!(result.is_err(), "contact_points must be an array");
    }

    #[test]
    fn default_decay_factor_returns_0_95() {
        assert!((default_decay_factor() - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn server_config_default_stale_edge_max_days() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.stale_edge_max_days, 0);
    }

    #[test]
    fn server_config_default_edge_decay_factor() {
        let cfg = ServerConfig::default();
        assert!((cfg.edge_decay_factor - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn server_config_default_idle_consolidation_enabled() {
        let cfg = ServerConfig::default();
        assert!(cfg.idle_consolidation_enabled);
    }

    #[test]
    fn server_config_default_idle_seconds() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.idle_consolidation_seconds, 20);
    }

    #[test]
    fn server_config_default_transport_stdio() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.transport, "stdio");
    }

    #[test]
    fn server_config_default_http_port() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.http_port, 8765);
    }

    #[test]
    fn viz_binds_loopback_when_nothing_is_configured() {
        // The old rule gave stdio -- the DEFAULT transport -- 0.0.0.0, so a
        // default install published the graph on every interface.
        assert_eq!(resolve_viz_bind(None, false).unwrap(), "127.0.0.1");
        assert_eq!(resolve_viz_bind(Some(""), false).unwrap(), "127.0.0.1");
        assert_eq!(resolve_viz_bind(Some("   "), false).unwrap(), "127.0.0.1");
    }

    #[test]
    fn viz_refuses_a_wide_bind_while_it_cannot_authenticate() {
        // Fail LOUD and closed. Silently narrowing to loopback would leave an
        // operator believing a remote dashboard works when it does not.
        for wide in ["0.0.0.0", "::", "192.168.1.10"] {
            let error = resolve_viz_bind(Some(wide), false)
                .expect_err("a wide bind without auth must be refused");
            assert!(
                error.contains(wide),
                "the error must name the address: {error}"
            );
            assert!(error.contains("cannot authenticate"), "{error}");
        }
    }

    #[test]
    fn viz_allows_an_explicit_loopback_bind() {
        for local in ["127.0.0.1", "localhost", "::1"] {
            assert_eq!(resolve_viz_bind(Some(local), false).unwrap(), local);
        }
    }

    #[test]
    fn viz_allows_a_wide_bind_once_it_can_authenticate() {
        // The refusal is about the missing auth, not about the address. When
        // viz authenticates, a deliberate wide bind is the operator's call.
        assert_eq!(resolve_viz_bind(Some("0.0.0.0"), true).unwrap(), "0.0.0.0");
    }

    #[test]
    fn loopback_bind_is_unlimited_by_default() {
        // Every process on the host arrives as 127.0.0.1 and would otherwise
        // share a single budget.
        for bind in ["127.0.0.1", "::1", "localhost"] {
            let cfg = ServerConfig {
                bind_addr: bind.into(),
                ..Default::default()
            };
            assert_eq!(
                cfg.resolved_rate_limit_per_minute().unwrap(),
                None,
                "{bind}"
            );
        }
    }

    #[test]
    fn exposed_bind_keeps_a_conservative_default() {
        let cfg = ServerConfig {
            bind_addr: "0.0.0.0".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.resolved_rate_limit_per_minute().unwrap(),
            Some(EXPOSED_RATE_LIMIT_PER_MINUTE)
        );
    }

    #[test]
    fn minus_one_is_unlimited_zero_blocks_and_positive_is_a_budget() {
        let mut cfg = ServerConfig {
            bind_addr: "0.0.0.0".into(),
            rate_limit_per_minute: Some(RATE_LIMIT_UNLIMITED),
            ..Default::default()
        };
        assert_eq!(cfg.resolved_rate_limit_per_minute().unwrap(), None);

        // Blocking an address is the point of 0, not a misconfiguration.
        cfg.rate_limit_per_minute = Some(0);
        assert_eq!(cfg.resolved_rate_limit_per_minute().unwrap(), Some(0));

        cfg.rate_limit_per_minute = Some(120);
        assert_eq!(cfg.resolved_rate_limit_per_minute().unwrap(), Some(120));
    }

    #[test]
    fn a_negative_other_than_minus_one_is_rejected_not_guessed() {
        // Clamping -5 to unlimited or to blocked would apply a policy the
        // operator did not write, in opposite directions depending on the guess.
        let cfg = ServerConfig {
            bind_addr: "0.0.0.0".into(),
            rate_limit_per_minute: Some(-5),
            ..Default::default()
        };
        let error = cfg
            .resolved_rate_limit_per_minute()
            .expect_err("-5 is not a valid budget");
        assert!(error.to_string().contains("-5"), "{error}");
    }

    #[test]
    fn rate_limit_tiers_parse_per_ip_including_blocked_and_unlimited() {
        let cfg = ServerConfig {
            rate_limit_overrides: std::collections::HashMap::from([
                ("203.0.113.7".to_owned(), RATE_LIMIT_UNLIMITED),
                ("198.51.100.4".to_owned(), 100i64),
                ("198.51.100.9".to_owned(), 0i64),
            ]),
            ..Default::default()
        };
        let resolved = cfg.resolved_rate_limit_overrides().expect("valid tiers");
        assert_eq!(resolved[&"203.0.113.7".parse().unwrap()], None);
        assert_eq!(resolved[&"198.51.100.4".parse().unwrap()], Some(100));
        assert_eq!(
            resolved[&"198.51.100.9".parse().unwrap()],
            Some(0),
            "0 blocks the address"
        );
    }

    #[test]
    fn a_tier_keyed_by_a_non_address_is_an_error_not_a_dropped_entry() {
        // Silently skipping it would ration a paying client at the anonymous
        // rate while the server looks healthy.
        let cfg = ServerConfig {
            rate_limit_overrides: std::collections::HashMap::from([(
                "premium-customer".to_owned(),
                5000i64,
            )]),
            ..Default::default()
        };
        let error = cfg
            .resolved_rate_limit_overrides()
            .expect_err("a hostname is not an IP address");
        assert!(error.to_string().contains("premium-customer"), "{error}");
    }

    #[test]
    fn server_config_default_request_timeout_seconds() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.request_timeout_seconds, 30);
    }

    #[test]
    fn server_config_default_bind_addr() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.bind_addr, "0.0.0.0");
    }

    #[test]
    fn server_config_default_log_level() {
        let cfg = ServerConfig::default();
        assert_eq!(cfg.log_level, "info");
    }

    #[test]
    fn server_config_default_tls_disabled() {
        let cfg = ServerConfig::default();
        assert!(!cfg.require_tls);
        assert!(cfg.cert_path.is_none());
        assert!(cfg.key_path.is_none());
        assert!(cfg.auth_file.is_none());
    }

    #[test]
    fn server_config_default_tenant_session_none() {
        let cfg = ServerConfig::default();
        assert!(cfg.tenant_id.is_none());
        assert!(cfg.session_id.is_none());
    }

    #[test]
    fn validate_shared_http_requires_auth_file() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
require_tls = true
cert_path = "/etc/ssl/cert.pem"
key_path = "/etc/ssl/key.pem"
"#;
        let config = parse_config(toml).unwrap();
        let err = validate_shared_http_config(&config).unwrap_err();
        assert!(err.to_string().contains("auth_file"));
    }

    #[test]
    fn validate_shared_http_rejects_tenant_fallback() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
require_tls = true
cert_path = "/etc/ssl/cert.pem"
key_path = "/etc/ssl/key.pem"
auth_file = "/etc/ferrosa/auth.toml"
tenant_id = "00000000-0000-0000-0000-000000000001"
"#;
        let config = parse_config(toml).unwrap();
        let err = validate_shared_http_config(&config).unwrap_err();
        assert!(err.to_string().contains("tenant_id"));
    }

    #[test]
    fn validate_shared_http_accepts_required_fields() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
require_tls = true
cert_path = "/etc/ssl/cert.pem"
key_path = "/etc/ssl/key.pem"
auth_file = "/etc/ferrosa/auth.toml"
[viz]
enabled = false
"#;
        let config = parse_config(toml).unwrap();
        validate_shared_http_config(&config).expect("shared http config should validate");
    }

    #[test]
    fn validate_shared_http_accepts_loopback_without_tls() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
bind_addr = "127.0.0.1"
require_tls = false
auth_file = "/etc/ferrosa/auth.toml"
[viz]
enabled = false
"#;
        let config = parse_config(toml).unwrap();
        validate_shared_http_config(&config).expect("loopback-only http config should validate");
    }

    #[test]
    fn validate_shared_http_aggregates_all_problems_in_one_error() {
        // A stdio-style config flipped to HTTP transport with nothing else set:
        // missing auth_file, non-loopback bind without TLS, and viz enabled
        // (default) without a viz tenant. The single error must name them all so
        // the operator fixes everything in one pass (no restart-per-field).
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
bind_addr = "0.0.0.0"
require_tls = false
"#;
        let config = parse_config(toml).unwrap();
        let err = validate_shared_http_config(&config)
            .expect_err("incomplete HTTP config must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("auth_file"), "must report auth_file: {msg}");
        assert!(
            msg.contains("loopback-only"),
            "must report bind_addr/TLS: {msg}"
        );
        assert!(
            msg.contains("viz.tenant_id"),
            "must report viz tenant: {msg}"
        );
        // All three surfaced together, not one-at-a-time.
        assert!(msg.contains("3 problem(s)"), "must aggregate count: {msg}");
    }

    #[test]
    fn validate_shared_http_rejects_non_loopback_without_tls() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:19042"]
[server]
transport = "http"
bind_addr = "0.0.0.0"
require_tls = false
auth_file = "/etc/ferrosa/auth.toml"
"#;
        let config = parse_config(toml).unwrap();
        let err = validate_shared_http_config(&config).unwrap_err();
        assert!(err.to_string().contains("loopback-only"));
    }

    #[test]
    fn parse_toml_with_stale_edge_and_decay() {
        let toml = r#"
[server]
stale_edge_max_days = 30
edge_decay_factor = 0.85

[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse stale edge config");
        assert_eq!(config.server.stale_edge_max_days, 30);
        assert!((config.server.edge_decay_factor - 0.85).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_toml_with_idle_consolidation() {
        let toml = r#"
[server]
idle_consolidation_enabled = false
idle_consolidation_seconds = 60

[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse idle consolidation config");
        assert!(!config.server.idle_consolidation_enabled);
        assert_eq!(config.server.idle_consolidation_seconds, 60);
    }

    #[test]
    fn parse_toml_with_graph_config() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[graph]
bolt_uri = "bolt://remote:7687"
username = "admin"
password = "secret"
http_url = "http://remote:7474"
"#;
        let config = parse_config(toml).expect("should parse graph config");
        assert_eq!(config.graph.bolt_uri, "bolt://remote:7687");
        assert_eq!(config.graph.username, "admin");
        assert_eq!(config.graph.password, "secret");
        assert_eq!(config.graph.http_url, "http://remote:7474");
    }

    #[test]
    fn graph_config_defaults() {
        let cfg = GraphDbConfig::default();
        assert_eq!(cfg.bolt_uri, "bolt://localhost:7687");
        assert_eq!(cfg.username, "ferrosa_admin");
        assert_eq!(cfg.password, "ferrosa_admin");
        assert_eq!(cfg.http_url, "http://localhost:7474");
    }

    #[test]
    fn parse_toml_with_viz_config() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[viz]
enabled = false
port = 9999
"#;
        let config = parse_config(toml).expect("should parse viz config");
        assert!(!config.viz.enabled);
        assert_eq!(config.viz.port, 9999);
    }

    #[test]
    fn viz_config_defaults() {
        let cfg = VizConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.port, 8766);
    }

    #[test]
    fn memory_config_defaults() {
        let cfg = MemoryConfig::default();
        assert_eq!(cfg.default_ttl_days, 7);
        assert_eq!(cfg.fold_ttl_days, 30);
        assert_eq!(cfg.archive_after_days, 30);
        assert_eq!(cfg.compression_threshold_tokens, 512);
        assert!((cfg.confidence_gate - 0.7).abs() < f64::EPSILON);
        assert_eq!(cfg.max_memo_results, 50);
        assert_eq!(cfg.max_entities, 10000);
    }

    #[test]
    fn embedding_config_defaults() {
        let cfg = EmbeddingConfig::default();
        assert_eq!(cfg.provider, "ollama");
        assert_eq!(cfg.ollama_base_url, "http://127.0.0.1:11434");
        assert_eq!(cfg.model, "nomic-embed-text-v2-moe");
        assert_eq!(cfg.dimensions, 768);
    }

    #[test]
    fn judge_config_defaults_to_local_ollama_without_token() {
        let cfg = JudgeConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.provider, "ollama");
        assert_eq!(cfg.base_url, "http://127.0.0.1:11434");
        assert_eq!(cfg.model, "qwen2.5-coder:7b");
        assert_eq!(cfg.token, None);
        assert_eq!(cfg.timeout_seconds, 30);
        assert_eq!(cfg.max_rerank_candidates, 8);
    }

    #[test]
    fn parse_judge_config_missing_enabled_defaults_to_live_rerank() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[judge]
provider = "ollama"
base_url = "http://127.0.0.1:11434"
model = "qwen2.5-coder:7b"
"#;
        let config = parse_config(toml).expect("should parse judge config");
        assert!(config.judge.enabled);
    }

    #[test]
    fn parse_judge_config_explicit_false_disables_live_rerank() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[judge]
enabled = false
provider = "ollama"
base_url = "http://127.0.0.1:11434"
model = "qwen2.5-coder:7b"
"#;
        let config = parse_config(toml).expect("should parse judge config");
        assert!(!config.judge.enabled);
    }

    #[test]
    fn parse_judge_config_fields() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[judge]
enabled = true
provider = "lmstudio"
base_url = "http://127.0.0.1:1234"
model = "qwen3"
token = "secret"
timeout_seconds = 12
max_rerank_candidates = 25
"#;
        let config = parse_config(toml).expect("should parse judge config");
        assert!(config.judge.enabled);
        assert_eq!(config.judge.provider, "lmstudio");
        assert_eq!(config.judge.base_url, "http://127.0.0.1:1234");
        assert_eq!(config.judge.model, "qwen3");
        assert_eq!(config.judge.max_rerank_candidates, 25);
        assert_eq!(config.judge.token.as_deref(), Some("secret"));
        assert_eq!(config.judge.timeout_seconds, 12);
    }

    #[test]
    fn security_config_defaults() {
        let cfg = SecurityConfig::default();
        assert!(cfg.audit_log_enabled);
        assert!(cfg.anomaly_detection_enabled);
        assert!((cfg.anomaly_sigma_threshold - 3.0).abs() < f64::EPSILON);
        assert!(cfg.anomaly_alerts_enabled);
    }

    #[test]
    fn routing_config_defaults() {
        let cfg = RoutingConfig::default();
        assert_eq!(cfg.guideline_version, "v1");
        assert_eq!(cfg.feedback_export_cron, "0 2 * * *");
    }

    #[test]
    fn parse_toml_defaults_for_optional_sections() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse with all defaults");
        // Verify all default sections are populated
        assert_eq!(config.server.transport, "stdio");
        assert!(config.viz.enabled);
        assert_eq!(config.graph.bolt_uri, "bolt://localhost:7687");
        assert_eq!(config.memory.default_ttl_days, 7);
        assert_eq!(config.embeddings.provider, "ollama");
        assert!(config.security.audit_log_enabled);
        assert_eq!(config.routing.guideline_version, "v1");
        assert!((config.rmh.warmth_boost_amount - 0.3).abs() < f64::EPSILON);
        assert_eq!(config.datalog.max_iterations, 100);
    }

    #[test]
    fn embedding_config_has_ner_model_default() {
        let config = EmbeddingConfig::default();
        assert_eq!(config.ner_model, "qwen3.5:27b");
    }

    #[test]
    fn parse_toml_with_ner_model() {
        let toml_str = r#"
[server]
transport = "stdio"

[ferrosa]
contact_points = ["localhost:9042"]

[embeddings]
ner_model = "llama3:8b"
"#;
        let config = parse_config(toml_str).unwrap();
        assert_eq!(config.embeddings.ner_model, "llama3:8b");
    }

    #[test]
    fn parse_toml_with_tenant_and_session_ids() {
        let toml = r#"
[server]
tenant_id = "550e8400-e29b-41d4-a716-446655440000"
session_id = "660e8400-e29b-41d4-a716-446655440000"

[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse tenant/session IDs");
        assert_eq!(
            config.server.tenant_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            config.server.session_id.as_deref(),
            Some("660e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn rmh_config_defaults() {
        let cfg = RmhConfig::default();
        assert!((cfg.warmth_boost_amount - 0.3).abs() < f64::EPSILON);
        assert!((cfg.warmth_neighbor_ratio - 0.5).abs() < f64::EPSILON);
        assert!((cfg.warmth_prune_threshold - 0.01).abs() < f64::EPSILON);
        assert!((cfg.warmth_cap - 10.0).abs() < f64::EPSILON);
        assert!((cfg.ppr_alpha - 0.45).abs() < f64::EPSILON);
        assert_eq!(cfg.ppr_iterations, 20);
        assert!((cfg.decay_lambda - 0.1).abs() < f64::EPSILON);
        assert_eq!(cfg.max_explore_passes, 3);
        assert!((cfg.convergence_threshold - 0.1).abs() < f64::EPSILON);
        assert_eq!(cfg.max_explore_entities, 50);
    }

    #[test]
    fn datalog_config_defaults() {
        let cfg = DatalogConfig::default();
        assert_eq!(cfg.max_iterations, 100);
        assert_eq!(cfg.max_facts, 50000);
        assert_eq!(cfg.cache_ttl_seconds, 3600);
        assert_eq!(cfg.confidence_combination, "min_parent_times_weight");
    }

    #[test]
    fn test_config_defaults_rmh() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse with rmh defaults");
        assert!((config.rmh.warmth_boost_amount - 0.3).abs() < f64::EPSILON);
        assert!((config.rmh.ppr_alpha - 0.45).abs() < f64::EPSILON);
        assert_eq!(config.rmh.ppr_iterations, 20);
        assert_eq!(config.rmh.max_explore_passes, 3);
        assert!((config.rmh.warmth_cap - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_defaults_datalog() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse with datalog defaults");
        assert_eq!(config.datalog.max_iterations, 100);
        assert_eq!(config.datalog.max_facts, 50000);
        assert_eq!(config.datalog.cache_ttl_seconds, 3600);
        assert_eq!(
            config.datalog.confidence_combination,
            "min_parent_times_weight"
        );
    }

    #[test]
    fn test_config_rmh_overrides() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[rmh]
warmth_boost_amount = 0.5
ppr_alpha = 0.85
max_explore_passes = 5
"#;
        let config = parse_config(toml).expect("should parse with rmh overrides");
        assert!((config.rmh.warmth_boost_amount - 0.5).abs() < f64::EPSILON);
        assert!((config.rmh.ppr_alpha - 0.85).abs() < f64::EPSILON);
        assert_eq!(config.rmh.max_explore_passes, 5);
        // Non-overridden fields keep defaults
        assert!((config.rmh.warmth_neighbor_ratio - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_datalog_overrides() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[datalog]
max_iterations = 200
max_facts = 100000
cache_ttl_seconds = 7200
confidence_combination = "product"
"#;
        let config = parse_config(toml).expect("should parse with datalog overrides");
        assert_eq!(config.datalog.max_iterations, 200);
        assert_eq!(config.datalog.max_facts, 100000);
        assert_eq!(config.datalog.cache_ttl_seconds, 7200);
        assert_eq!(config.datalog.confidence_combination, "product");
    }

    #[test]
    fn promotion_config_defaults() {
        let cfg = PromotionConfig::default();
        assert!((cfg.promotion_threshold - 1000.0).abs() < f64::EPSILON);
        assert_eq!(cfg.size_budget_rows, 100_000);
        assert_eq!(cfg.window_days, 7);
        assert!((cfg.reuse_factor - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_defaults_promotion() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]
"#;
        let config = parse_config(toml).expect("should parse with promotion defaults");
        assert!((config.promotion.promotion_threshold - 1000.0).abs() < f64::EPSILON);
        assert_eq!(config.promotion.size_budget_rows, 100_000);
        assert_eq!(config.promotion.window_days, 7);
        assert!((config.promotion.reuse_factor - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_promotion_overrides() {
        let toml = r#"
[ferrosa]
contact_points = ["localhost:9042"]

[promotion]
promotion_threshold = 2000.0
size_budget_rows = 50000
window_days = 14
reuse_factor = 1.5
"#;
        let config = parse_config(toml).expect("should parse with promotion overrides");
        assert!((config.promotion.promotion_threshold - 2000.0).abs() < f64::EPSILON);
        assert_eq!(config.promotion.size_budget_rows, 50000);
        assert_eq!(config.promotion.window_days, 14);
        assert!((config.promotion.reuse_factor - 1.5).abs() < f64::EPSILON);
    }

    // ── W-02 tests ───────────────────────────────────────────────────────────

    /// P0-11/W-02: is_dbaas_mode returns true only on literal "true".
    #[test]
    #[serial]
    fn dbaas_mode_requires_exact_true() {
        unsafe { std::env::set_var("FERROSA_DBAAS_MODE", "true") }
        assert!(
            is_dbaas_mode(),
            "FERROSA_DBAAS_MODE=true must enable DBaaS mode"
        );

        unsafe { std::env::set_var("FERROSA_DBAAS_MODE", "TRUE") }
        assert!(
            is_dbaas_mode(),
            "case-insensitive TRUE must enable DBaaS mode"
        );

        unsafe { std::env::set_var("FERROSA_DBAAS_MODE", "yes") }
        assert!(
            !is_dbaas_mode(),
            "non-'true' values must not enable DBaaS mode"
        );

        unsafe { std::env::remove_var("FERROSA_DBAAS_MODE") }
        assert!(!is_dbaas_mode(), "absent var must not enable DBaaS mode");
    }

    fn minimal_config_with_proxy() -> Config {
        let toml = r#"
[ferrosa]
contact_points = ["proxy.dbaas.ferrosa.io:9042"]
"#;
        parse_config(toml).expect("minimal proxy config must parse")
    }

    /// P0-11/W-02: apply_dbaas_env_overrides populates config from env vars.
    #[test]
    #[serial]
    fn dbaas_env_overrides_populate_config() {
        unsafe {
            std::env::set_var("FERROSA_CQL_PROXY_ADDR", "proxy.dbaas.ferrosa.io:9042");
            std::env::set_var(
                "FERROSA_GRAPH_PROXY_ADDR",
                "http://graph.dbaas.ferrosa.io:7474",
            );
            std::env::set_var("FERROSA_TENANT_ID", "tenant-abc-123");
            std::env::set_var("FERROSA_API_KEY", "sk-test-key");
        }
        let mut config = minimal_config_with_proxy();
        apply_dbaas_env_overrides(&mut config)
            .expect("env overrides must succeed when all vars set");
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::remove_var("FERROSA_TENANT_ID");
            std::env::remove_var("FERROSA_API_KEY");
        }
        assert_eq!(
            config.ferrosa.contact_points,
            vec!["proxy.dbaas.ferrosa.io:9042"]
        );
        assert_eq!(config.graph.http_url, "http://graph.dbaas.ferrosa.io:7474");
        assert_eq!(config.ferrosa.username, "tenant-abc-123");
        assert_eq!(config.ferrosa.password, "sk-test-key");
    }

    /// P0-11/W-02: apply_dbaas_env_overrides parses comma-separated CQL addrs.
    #[test]
    #[serial]
    fn dbaas_env_overrides_multi_contact_points() {
        unsafe {
            std::env::set_var(
                "FERROSA_CQL_PROXY_ADDR",
                "proxy1.dbaas.ferrosa.io:9042, proxy2.dbaas.ferrosa.io:9042",
            );
            std::env::set_var(
                "FERROSA_GRAPH_PROXY_ADDR",
                "http://graph.dbaas.ferrosa.io:7474",
            );
            std::env::set_var("FERROSA_TENANT_ID", "tenant-multi");
            std::env::set_var("FERROSA_API_KEY", "sk-multi-key");
        }
        let mut config = minimal_config_with_proxy();
        apply_dbaas_env_overrides(&mut config).expect("multi-addr must succeed");
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::remove_var("FERROSA_TENANT_ID");
            std::env::remove_var("FERROSA_API_KEY");
        }
        assert_eq!(config.ferrosa.contact_points.len(), 2);
        assert_eq!(
            config.ferrosa.contact_points[0],
            "proxy1.dbaas.ferrosa.io:9042"
        );
        assert_eq!(
            config.ferrosa.contact_points[1],
            "proxy2.dbaas.ferrosa.io:9042"
        );
    }

    /// P0-11/W-02: missing FERROSA_CQL_PROXY_ADDR causes clear startup failure.
    #[test]
    #[serial]
    fn dbaas_env_overrides_missing_cql_addr_fails() {
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::set_var(
                "FERROSA_GRAPH_PROXY_ADDR",
                "http://graph.dbaas.ferrosa.io:7474",
            );
            std::env::set_var("FERROSA_TENANT_ID", "tenant-xyz");
            std::env::set_var("FERROSA_API_KEY", "sk-xyz");
        }
        let mut config = minimal_config_with_proxy();
        let err = apply_dbaas_env_overrides(&mut config).expect_err("missing CQL addr must fail");
        unsafe {
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::remove_var("FERROSA_TENANT_ID");
            std::env::remove_var("FERROSA_API_KEY");
        }
        let msg = err.to_string();
        assert!(
            msg.contains("FERROSA_CQL_PROXY_ADDR"),
            "error must name the missing variable, got: {msg}"
        );
    }

    /// P0-11/W-02: missing FERROSA_GRAPH_PROXY_ADDR causes clear startup failure.
    #[test]
    #[serial]
    fn dbaas_env_overrides_missing_graph_addr_fails() {
        unsafe {
            std::env::set_var("FERROSA_CQL_PROXY_ADDR", "proxy.dbaas.ferrosa.io:9042");
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::set_var("FERROSA_TENANT_ID", "tenant-xyz");
            std::env::set_var("FERROSA_API_KEY", "sk-xyz");
        }
        let mut config = minimal_config_with_proxy();
        let err = apply_dbaas_env_overrides(&mut config).expect_err("missing graph addr must fail");
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::remove_var("FERROSA_TENANT_ID");
            std::env::remove_var("FERROSA_API_KEY");
        }
        let msg = err.to_string();
        assert!(
            msg.contains("FERROSA_GRAPH_PROXY_ADDR"),
            "error must name the missing variable, got: {msg}"
        );
    }

    /// P0-11/W-02: missing FERROSA_TENANT_ID causes clear startup failure.
    #[test]
    #[serial]
    fn dbaas_env_overrides_missing_tenant_id_fails() {
        unsafe {
            std::env::set_var("FERROSA_CQL_PROXY_ADDR", "proxy.dbaas.ferrosa.io:9042");
            std::env::set_var(
                "FERROSA_GRAPH_PROXY_ADDR",
                "http://graph.dbaas.ferrosa.io:7474",
            );
            std::env::remove_var("FERROSA_TENANT_ID");
            std::env::set_var("FERROSA_API_KEY", "sk-xyz");
        }
        let mut config = minimal_config_with_proxy();
        let err = apply_dbaas_env_overrides(&mut config).expect_err("missing tenant id must fail");
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::remove_var("FERROSA_API_KEY");
        }
        let msg = err.to_string();
        assert!(
            msg.contains("FERROSA_TENANT_ID"),
            "error must name the missing variable, got: {msg}"
        );
    }

    /// P0-11/W-02: missing FERROSA_API_KEY causes clear startup failure.
    #[test]
    #[serial]
    fn dbaas_env_overrides_missing_api_key_fails() {
        unsafe {
            std::env::set_var("FERROSA_CQL_PROXY_ADDR", "proxy.dbaas.ferrosa.io:9042");
            std::env::set_var(
                "FERROSA_GRAPH_PROXY_ADDR",
                "http://graph.dbaas.ferrosa.io:7474",
            );
            std::env::set_var("FERROSA_TENANT_ID", "tenant-xyz");
            std::env::remove_var("FERROSA_API_KEY");
        }
        let mut config = minimal_config_with_proxy();
        let err = apply_dbaas_env_overrides(&mut config).expect_err("missing api key must fail");
        unsafe {
            std::env::remove_var("FERROSA_CQL_PROXY_ADDR");
            std::env::remove_var("FERROSA_GRAPH_PROXY_ADDR");
            std::env::remove_var("FERROSA_TENANT_ID");
        }
        let msg = err.to_string();
        assert!(
            msg.contains("FERROSA_API_KEY"),
            "error must name the missing variable, got: {msg}"
        );
    }
}
